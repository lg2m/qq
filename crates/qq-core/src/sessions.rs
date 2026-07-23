use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_stream::stream;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select_biased};
use futures_core::Stream;
use futures_util::StreamExt;
use qq_protocol::{
    CommandId, CommandOutcome, CommandReceipt, EventCursor, MessageId, MessageRole,
    MessageSnapshot, MessageState, ModelSelection, RunFailure, RunFailureKind, RunId, RunOutcome,
    RunSnapshot, RunStatus, SessionCommand, SessionEvent, SessionEventEnvelope, SessionId,
    SessionSnapshot, SessionStatus, SessionSummary, SnapshotRequest, StoreId, SubscribeRequest,
    TextChannel, WorkspaceId, WorkspaceSnapshot, WorkspaceSummary,
};
use qq_provider::Message;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use thiserror::Error;
use tokio::sync::{Semaphore, mpsc, oneshot, watch};

use crate::{RunEvent, Runtime};

const CONTROL_QUEUE_CAPACITY: usize = 256;
const OUTPUT_QUEUE_CAPACITY: usize = 1024;
const MAX_PENDING_PROMPTS: u16 = 16;
const MAX_CONTEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_PROMPT_BYTES: usize = 128 * 1024;
const MAX_REPLAY_EVENTS: u16 = 128;
const MAX_SNAPSHOT_SESSIONS: u16 = 512;
const MAX_SNAPSHOT_MESSAGES: u16 = 256;
const MAX_TEXT_CHUNK_BYTES: usize = 64 * 1024;
const MAX_FAILURE_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_WORKSPACES: u32 = 1024;
const MAX_SESSIONS_PER_WORKSPACE: u32 = 512;
const MAX_COMMANDS: u32 = 100_000;
const MAX_MODEL_SELECTION_BYTES: usize = 512;
const OUTPUT_BATCH_BYTES: usize = 8 * 1024;
const OUTPUT_BATCH_DELAY: Duration = Duration::from_millis(8);
const MAX_PERSISTED_EVENT_BYTES: usize = 1024 * 1024;

pub type RuntimeLoadFuture =
    Pin<Box<dyn Future<Output = Result<Arc<Runtime>, RuntimeLoadError>> + Send + 'static>>;

pub trait RuntimeLoader: Send + Sync + 'static {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeLoadRequest {
    pub workspace: String,
    pub model: ModelSelection,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct RuntimeLoadError {
    pub kind: RunFailureKind,
    pub message: String,
}

pub type SessionEventStream =
    Pin<Box<dyn Stream<Item = Result<SessionEventEnvelope, SessionRuntimeError>> + Send + 'static>>;

#[derive(Debug, Clone)]
pub struct SessionRuntimeOptions {
    pub database_path: PathBuf,
    pub max_active_runs: usize,
}

impl SessionRuntimeOptions {
    #[must_use]
    pub fn new(database_path: PathBuf) -> Self {
        Self {
            database_path,
            max_active_runs: 8,
        }
    }
}

#[derive(Clone)]
pub struct SessionRuntime {
    inner: Arc<SessionRuntimeInner>,
}

struct SessionRuntimeInner {
    store: Store,
    loader: Arc<dyn RuntimeLoader>,
    permits: Arc<Semaphore>,
    schedule: mpsc::Sender<()>,
    cancellations: Mutex<HashMap<RunId, watch::Sender<bool>>>,
    wakeups: Mutex<HashMap<WorkspaceId, watch::Sender<u64>>>,
    failed: watch::Sender<bool>,
}

impl SessionRuntime {
    pub async fn open(
        options: SessionRuntimeOptions,
        loader: Arc<dyn RuntimeLoader>,
    ) -> Result<Self, SessionRuntimeError> {
        if options.max_active_runs == 0 {
            return Err(SessionRuntimeError::InvalidRunLimit);
        }
        let store = Store::open(options.database_path).await?;
        let recovered = store.recover_interrupted_runs().await?;
        let (schedule, receiver) = mpsc::channel(1);
        let (failed, _) = watch::channel(false);
        let inner = Arc::new(SessionRuntimeInner {
            store,
            loader,
            permits: Arc::new(Semaphore::new(options.max_active_runs)),
            schedule,
            cancellations: Mutex::new(HashMap::new()),
            wakeups: Mutex::new(HashMap::new()),
            failed,
        });
        for cursor in recovered {
            inner.notify(cursor);
        }
        tokio::spawn(schedule_runs(Arc::downgrade(&inner), receiver));
        let runtime = Self { inner };
        runtime.request_schedule();
        Ok(runtime)
    }

    pub async fn command(
        &self,
        command_id: CommandId,
        command: SessionCommand,
    ) -> Result<CommandReceipt, SessionRuntimeError> {
        if *self.inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        let signal_run = match command {
            SessionCommand::CancelRun { run_id } => Some(run_id),
            _ => None,
        };
        let should_schedule = matches!(command, SessionCommand::SubmitPrompt { .. });
        let applied = self.inner.store.command(command_id, command).await?;
        self.inner.notify(applied.receipt.committed_through);

        if let Some(run_id) = signal_run {
            self.inner.cancel(run_id);
        }
        if should_schedule || applied.schedule {
            self.request_schedule();
        }
        Ok(applied.receipt)
    }

    pub async fn snapshot(
        &self,
        request: SnapshotRequest,
    ) -> Result<WorkspaceSnapshot, SessionRuntimeError> {
        if *self.inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        if request.session_limit == 0
            || request.session_limit > MAX_SNAPSHOT_SESSIONS
            || request.message_limit == 0
            || request.message_limit > MAX_SNAPSHOT_MESSAGES
        {
            return Err(SessionRuntimeError::InvalidPageLimit);
        }
        self.inner.store.snapshot(request).await
    }

    pub fn subscribe(
        &self,
        request: SubscribeRequest,
    ) -> Result<SessionEventStream, SessionRuntimeError> {
        if *self.inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        if request.after.store_id != self.inner.store.store_id() {
            return Err(SessionRuntimeError::CursorStoreMismatch);
        }
        if request.after.workspace_id != request.workspace_id {
            return Err(SessionRuntimeError::CursorWorkspaceMismatch);
        }

        let store = self.inner.store.clone();
        let mut failed = self.inner.failed.subscribe();
        let mut wakeup = self
            .inner
            .subscribe(request.workspace_id, request.after.sequence)?;
        Ok(Box::pin(stream! {
            let mut after = request.after.sequence;
            loop {
                let events = match store
                    .events_after(request.workspace_id, after, MAX_REPLAY_EVENTS)
                    .await
                {
                    Ok(events) => events,
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                };
                if !events.is_empty() {
                    for event in events {
                        after = event.cursor.sequence;
                        yield Ok(event);
                    }
                    continue;
                }
                tokio::select! {
                    biased;
                    changed = failed.changed() => {
                        if changed.is_err() || *failed.borrow() {
                            yield Err(SessionRuntimeError::Unavailable);
                            return;
                        }
                    }
                    changed = wakeup.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    }
                }
            }
        }))
    }

    fn request_schedule(&self) {
        let _ = self.inner.schedule.try_send(());
    }
}

impl SessionRuntimeInner {
    fn notify(&self, cursor: EventCursor) {
        let Ok(mut wakeups) = self.wakeups.lock() else {
            return;
        };
        match wakeups.get(&cursor.workspace_id) {
            Some(sender) => {
                sender.send_replace(cursor.sequence);
            }
            None => {
                let (sender, _) = watch::channel(cursor.sequence);
                wakeups.insert(cursor.workspace_id, sender);
            }
        }
    }

    fn subscribe(
        &self,
        workspace_id: WorkspaceId,
        sequence: u64,
    ) -> Result<watch::Receiver<u64>, SessionRuntimeError> {
        let mut wakeups = self
            .wakeups
            .lock()
            .map_err(|_| SessionRuntimeError::Unavailable)?;
        let sender = wakeups.entry(workspace_id).or_insert_with(|| {
            let (sender, _) = watch::channel(sequence);
            sender
        });
        Ok(sender.subscribe())
    }

    fn cancel(&self, run_id: RunId) {
        let Ok(cancellations) = self.cancellations.lock() else {
            return;
        };
        if let Some(sender) = cancellations.get(&run_id) {
            sender.send_replace(true);
        }
    }
}

async fn schedule_runs(
    inner: std::sync::Weak<SessionRuntimeInner>,
    mut receiver: mpsc::Receiver<()>,
) {
    while receiver.recv().await.is_some() {
        let Some(inner) = inner.upgrade() else {
            return;
        };
        if *inner.failed.borrow() {
            return;
        }
        loop {
            let permit = match Arc::clone(&inner.permits).try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => break,
            };
            let claimed = match inner.store.claim_next_run().await {
                Ok(Some(claimed)) => claimed,
                Ok(None) => break,
                Err(_) => {
                    inner.failed.send_replace(true);
                    return;
                }
            };
            inner.notify(claimed.started.cursor);
            let (cancel, cancel_receiver) = watch::channel(false);
            if let Ok(mut cancellations) = inner.cancellations.lock() {
                cancellations.insert(claimed.run_id, cancel);
            }
            match inner.store.cancellation_requested(claimed.run_id).await {
                Ok(true) => inner.cancel(claimed.run_id),
                Ok(false) => {}
                Err(_) => {
                    inner.failed.send_replace(true);
                    return;
                }
            }
            let task_inner = Arc::clone(&inner);
            tokio::spawn(async move {
                execute_run(Arc::clone(&task_inner), claimed, cancel_receiver).await;
                drop(permit);
                let _ = task_inner.schedule.try_send(());
            });
        }
    }
}

async fn execute_run(
    inner: Arc<SessionRuntimeInner>,
    claimed: ClaimedRun,
    mut cancellation: watch::Receiver<bool>,
) {
    if *cancellation.borrow() {
        finish_run(&inner, &claimed, RunOutcome::Cancelled).await;
        return;
    }
    let mut load = inner.loader.load(RuntimeLoadRequest {
        workspace: claimed.workspace.clone(),
        model: claimed.model.clone(),
    });
    let runtime = tokio::select! {
        result = &mut load => match result {
            Ok(runtime) => runtime,
            Err(error) => {
                finish_run(&inner, &claimed, RunOutcome::Failed {
                    failure: RunFailure {
                        kind: error.kind,
                        message: truncate_utf8(error.message, MAX_FAILURE_MESSAGE_BYTES),
                    },
                }).await;
                return;
            }
        },
        changed = cancellation.changed() => {
            if changed.is_ok() && *cancellation.borrow() {
                finish_run(&inner, &claimed, RunOutcome::Cancelled).await;
                // Runtime construction may be blocking; retain the run permit until it exits.
                let _ = load.await;
                return;
            }
            return;
        }
    };
    if *cancellation.borrow() {
        finish_run(&inner, &claimed, RunOutcome::Cancelled).await;
        return;
    }

    match inner.store.start_assistant(&claimed).await {
        Ok(event) => inner.notify(event.cursor),
        Err(_) => {
            finish_run(
                &inner,
                &claimed,
                internal_failure("failed to persist the assistant message"),
            )
            .await;
            return;
        }
    }

    let messages = claimed
        .messages
        .iter()
        .map(|message| match message.role {
            MessageRole::User => Message::user(message.output.clone()),
            MessageRole::Assistant => Message::assistant(if message.output.is_empty() {
                message.refusal.clone()
            } else {
                message.output.clone()
            }),
        })
        .collect();
    let mut events = runtime.run_messages(messages);
    let mut pending_text = String::new();
    let mut pending_channel = None;
    let mut flush_at = None;
    let mut persisted_first_text = false;
    loop {
        let input = if let Some(deadline) = flush_at {
            tokio::select! {
                biased;
                changed = cancellation.changed() => {
                    if changed.is_ok() && *cancellation.borrow() {
                        RunInput::Cancelled
                    } else {
                        RunInput::Interrupted
                    }
                }
                () = tokio::time::sleep_until(deadline) => RunInput::Flush,
                event = events.next() => RunInput::Event(event),
            }
        } else {
            tokio::select! {
                biased;
                changed = cancellation.changed() => {
                    if changed.is_ok() && *cancellation.borrow() {
                        RunInput::Cancelled
                    } else {
                        RunInput::Interrupted
                    }
                }
                event = events.next() => RunInput::Event(event),
            }
        };
        match input {
            RunInput::Flush => {
                if flush_pending_text(&inner, &claimed, &mut pending_channel, &mut pending_text)
                    .await
                    .is_err()
                {
                    finish_run(
                        &inner,
                        &claimed,
                        internal_failure("failed to persist model output"),
                    )
                    .await;
                    return;
                }
                flush_at = None;
            }
            stopped @ (RunInput::Cancelled | RunInput::Interrupted) => {
                if flush_pending_text(&inner, &claimed, &mut pending_channel, &mut pending_text)
                    .await
                    .is_err()
                {
                    finish_run(
                        &inner,
                        &claimed,
                        internal_failure("failed to persist model output"),
                    )
                    .await;
                    return;
                }
                let outcome = if matches!(stopped, RunInput::Cancelled) {
                    RunOutcome::Cancelled
                } else {
                    RunOutcome::Interrupted
                };
                finish_run(&inner, &claimed, outcome).await;
                return;
            }
            RunInput::Event(Some(RunEvent::Started)) => {}
            RunInput::Event(Some(
                event @ (RunEvent::OutputTextDelta { .. } | RunEvent::RefusalDelta { .. }),
            )) => {
                let (channel, text) = match event {
                    RunEvent::OutputTextDelta { text } => (TextChannel::Output, text),
                    RunEvent::RefusalDelta { text } => (TextChannel::Refusal, text),
                    _ => unreachable!("matched text event"),
                };
                if text.is_empty() {
                    continue;
                }
                if !persisted_first_text {
                    if persist_text(&inner, &claimed, channel, text).await.is_err() {
                        finish_run(
                            &inner,
                            &claimed,
                            internal_failure("failed to persist model output"),
                        )
                        .await;
                        return;
                    }
                    persisted_first_text = true;
                    continue;
                }
                if pending_channel.is_some_and(|pending| pending != channel)
                    && flush_pending_text(&inner, &claimed, &mut pending_channel, &mut pending_text)
                        .await
                        .is_err()
                {
                    finish_run(
                        &inner,
                        &claimed,
                        internal_failure("failed to persist model output"),
                    )
                    .await;
                    return;
                }
                if pending_text.is_empty() {
                    pending_channel = Some(channel);
                    flush_at = Some(tokio::time::Instant::now() + OUTPUT_BATCH_DELAY);
                }
                pending_text.push_str(&text);
                if pending_text.len() >= OUTPUT_BATCH_BYTES {
                    if flush_pending_text(&inner, &claimed, &mut pending_channel, &mut pending_text)
                        .await
                        .is_err()
                    {
                        finish_run(
                            &inner,
                            &claimed,
                            internal_failure("failed to persist model output"),
                        )
                        .await;
                        return;
                    }
                    flush_at = None;
                }
            }
            RunInput::Event(Some(RunEvent::Completed)) => {
                if flush_pending_text(&inner, &claimed, &mut pending_channel, &mut pending_text)
                    .await
                    .is_err()
                {
                    finish_run(
                        &inner,
                        &claimed,
                        internal_failure("failed to persist model output"),
                    )
                    .await;
                    return;
                }
                finish_run(&inner, &claimed, RunOutcome::Completed).await;
                return;
            }
            RunInput::Event(Some(RunEvent::Failed { kind, message })) => {
                if flush_pending_text(&inner, &claimed, &mut pending_channel, &mut pending_text)
                    .await
                    .is_err()
                {
                    finish_run(
                        &inner,
                        &claimed,
                        internal_failure("failed to persist model output"),
                    )
                    .await;
                    return;
                }
                finish_run(
                    &inner,
                    &claimed,
                    RunOutcome::Failed {
                        failure: RunFailure {
                            kind,
                            message: truncate_utf8(message, MAX_FAILURE_MESSAGE_BYTES),
                        },
                    },
                )
                .await;
                return;
            }
            RunInput::Event(None) => {
                if flush_pending_text(&inner, &claimed, &mut pending_channel, &mut pending_text)
                    .await
                    .is_err()
                {
                    finish_run(
                        &inner,
                        &claimed,
                        internal_failure("failed to persist model output"),
                    )
                    .await;
                    return;
                }
                finish_run(
                    &inner,
                    &claimed,
                    internal_failure("model stream ended without a terminal event"),
                )
                .await;
                return;
            }
        }
    }
}

enum RunInput {
    Event(Option<RunEvent>),
    Flush,
    Cancelled,
    Interrupted,
}

async fn flush_pending_text(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    channel: &mut Option<TextChannel>,
    text: &mut String,
) -> Result<(), SessionRuntimeError> {
    let Some(channel) = channel.take() else {
        return Ok(());
    };
    persist_text(inner, claimed, channel, std::mem::take(text)).await
}

async fn persist_text(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    channel: TextChannel,
    text: String,
) -> Result<(), SessionRuntimeError> {
    let mut remaining = text.as_str();
    while !remaining.is_empty() {
        let mut end = remaining.len().min(MAX_TEXT_CHUNK_BYTES);
        while !remaining.is_char_boundary(end) {
            end -= 1;
        }
        let event = inner
            .store
            .append_text(
                claimed,
                claimed.assistant_message_id,
                channel,
                remaining[..end].to_owned(),
            )
            .await?;
        inner.notify(event.cursor);
        remaining = &remaining[end..];
    }
    Ok(())
}

async fn finish_run(inner: &SessionRuntimeInner, claimed: &ClaimedRun, outcome: RunOutcome) {
    match inner.store.finish_run(claimed, outcome).await {
        Ok(event) => inner.notify(event.cursor),
        Err(_) => {
            inner.failed.send_replace(true);
        }
    }
    if let Ok(mut cancellations) = inner.cancellations.lock() {
        cancellations.remove(&claimed.run_id);
    }
}

fn internal_failure(message: &str) -> RunOutcome {
    RunOutcome::Failed {
        failure: RunFailure {
            kind: RunFailureKind::Server,
            message: message.to_owned(),
        },
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SessionRuntimeError {
    #[error("maximum active runs must be greater than zero")]
    InvalidRunLimit,
    #[error("page limits must be greater than zero")]
    InvalidPageLimit,
    #[error("workspace path must not be empty")]
    EmptyWorkspace,
    #[error("workspace path must identify an existing directory")]
    InvalidWorkspace,
    #[error("prompt must not be empty")]
    EmptyPrompt,
    #[error("prompt exceeds the session limit")]
    PromptTooLarge,
    #[error("workspace was not found")]
    WorkspaceNotFound,
    #[error("workspace limit reached")]
    WorkspaceLimitReached,
    #[error("session was not found")]
    SessionNotFound,
    #[error("workspace session limit reached")]
    SessionLimitReached,
    #[error("parent session does not belong to the workspace")]
    ParentWorkspaceMismatch,
    #[error("run was not found")]
    RunNotFound,
    #[error("session follow-up queue is full")]
    QueueFull,
    #[error("session context exceeds the size limit")]
    ContextTooLarge,
    #[error("model output exceeds the session size limit")]
    OutputTooLarge,
    #[error("session event exceeds the durable size limit")]
    EventTooLarge,
    #[error("command ID was reused with different content")]
    IdempotencyConflict,
    #[error("durable command limit reached")]
    CommandLimitReached,
    #[error("model selection exceeds the session limit")]
    InvalidModelSelection,
    #[error("event cursor belongs to another store")]
    CursorStoreMismatch,
    #[error("event cursor belongs to another workspace")]
    CursorWorkspaceMismatch,
    #[error("session runtime is overloaded")]
    Overloaded,
    #[error("session runtime is unavailable")]
    Unavailable,
    #[error("session persistence failed")]
    Persistence,
}

#[derive(Clone)]
struct Store {
    inner: Arc<StoreInner>,
    store_id: StoreId,
}

struct StoreInner {
    control: Sender<WorkerMessage>,
    output: Sender<WorkerMessage>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

type DatabaseJob = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

enum WorkerMessage {
    Run(DatabaseJob),
    Shutdown,
}

#[derive(Clone, Copy)]
enum Priority {
    Control,
    Output,
}

impl Drop for StoreInner {
    fn drop(&mut self) {
        let _ = self.control.send(WorkerMessage::Shutdown);
        if let Ok(worker) = self.worker.get_mut()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
    }
}

impl Store {
    async fn open(path: PathBuf) -> Result<Self, SessionRuntimeError> {
        let (control_tx, control_rx) = bounded(CONTROL_QUEUE_CAPACITY);
        let (output_tx, output_rx) = bounded(OUTPUT_QUEUE_CAPACITY);
        let (ready_tx, ready_rx) = oneshot::channel();
        let worker = thread::Builder::new()
            .name("qq-session-store".to_owned())
            .spawn(move || match open_database(&path) {
                Ok((mut connection, store_id)) => {
                    let _ = ready_tx.send(Ok(store_id));
                    database_worker(&mut connection, &control_rx, &output_rx);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            })
            .map_err(|_| SessionRuntimeError::Unavailable)?;
        let store_id = ready_rx
            .await
            .map_err(|_| SessionRuntimeError::Unavailable)??;
        Ok(Self {
            inner: Arc::new(StoreInner {
                control: control_tx,
                output: output_tx,
                worker: Mutex::new(Some(worker)),
            }),
            store_id,
        })
    }

    const fn store_id(&self) -> StoreId {
        self.store_id
    }

    async fn call<T, F>(&self, priority: Priority, operation: F) -> Result<T, SessionRuntimeError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, SessionRuntimeError> + Send + 'static,
    {
        let (reply, response) = oneshot::channel();
        let mut message = WorkerMessage::Run(Box::new(move |connection| {
            let _ = reply.send(operation(connection));
        }));
        let sender = match priority {
            Priority::Control => &self.inner.control,
            Priority::Output => &self.inner.output,
        };
        loop {
            match sender.try_send(message) {
                Ok(()) => break,
                Err(TrySendError::Full(returned)) if matches!(priority, Priority::Output) => {
                    message = returned;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                Err(TrySendError::Full(_)) => return Err(SessionRuntimeError::Overloaded),
                Err(TrySendError::Disconnected(_)) => return Err(SessionRuntimeError::Unavailable),
            }
        }
        response
            .await
            .map_err(|_| SessionRuntimeError::Unavailable)?
    }

    async fn recover_interrupted_runs(&self) -> Result<Vec<EventCursor>, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call(Priority::Control, move |connection| {
            recover_interrupted_runs(connection, store_id)
        })
        .await
    }

    async fn command(
        &self,
        command_id: CommandId,
        command: SessionCommand,
    ) -> Result<AppliedCommand, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call(Priority::Control, move |connection| {
            execute_command(connection, store_id, command_id, command)
        })
        .await
    }

    async fn snapshot(
        &self,
        request: SnapshotRequest,
    ) -> Result<WorkspaceSnapshot, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call(Priority::Control, move |connection| {
            load_snapshot(connection, store_id, request)
        })
        .await
    }

    async fn events_after(
        &self,
        workspace_id: WorkspaceId,
        sequence: u64,
        limit: u16,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            read_events(connection, workspace_id, sequence, limit)
        })
        .await
    }

    async fn claim_next_run(&self) -> Result<Option<ClaimedRun>, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call(Priority::Control, move |connection| {
            claim_next_run(connection, store_id)
        })
        .await
    }

    async fn cancellation_requested(&self, run_id: RunId) -> Result<bool, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            connection
                .query_row(
                    "SELECT cancel_requested FROM runs WHERE id = ?1",
                    [run_id.to_string()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|_| SessionRuntimeError::Persistence)?
                .ok_or(SessionRuntimeError::RunNotFound)
        })
        .await
    }

    async fn start_assistant(
        &self,
        claimed: &ClaimedRun,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            start_assistant(connection, store_id, &claimed)
        })
        .await
    }

    async fn append_text(
        &self,
        claimed: &ClaimedRun,
        message_id: MessageId,
        channel: TextChannel,
        text: String,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            append_text(connection, store_id, &claimed, message_id, channel, text)
        })
        .await
    }

    async fn finish_run(
        &self,
        claimed: &ClaimedRun,
        outcome: RunOutcome,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            complete_run(connection, store_id, &claimed, outcome)
        })
        .await
    }
}

fn database_worker(
    connection: &mut Connection,
    control: &Receiver<WorkerMessage>,
    output: &Receiver<WorkerMessage>,
) {
    loop {
        select_biased! {
            recv(control) -> message => if !run_worker_message(connection, message) { return; },
            recv(output) -> message => if !run_worker_message(connection, message) { return; },
        }
    }
}

fn run_worker_message(
    connection: &mut Connection,
    message: Result<WorkerMessage, crossbeam_channel::RecvError>,
) -> bool {
    match message {
        Ok(WorkerMessage::Run(job)) => {
            job(connection);
            true
        }
        Ok(WorkerMessage::Shutdown) | Err(_) => false,
    }
}

fn open_database(path: &PathBuf) -> Result<(Connection, StoreId), SessionRuntimeError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(SessionRuntimeError::Persistence);
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(SessionRuntimeError::Persistence),
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|_| SessionRuntimeError::Persistence)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS metadata (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS workspaces (
                 id TEXT PRIMARY KEY,
                 path TEXT NOT NULL UNIQUE,
                 next_sequence INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY,
                 workspace_id TEXT NOT NULL REFERENCES workspaces(id),
                 parent_id TEXT REFERENCES sessions(id),
                 title TEXT NOT NULL,
                 status TEXT NOT NULL,
                 active_run_id TEXT,
                 queued_prompts INTEGER NOT NULL DEFAULT 0,
                 model TEXT,
                 max_output_tokens INTEGER,
                 organization TEXT,
                 created_at_ms INTEGER NOT NULL,
                 updated_at_ms INTEGER NOT NULL
             );
              CREATE TABLE IF NOT EXISTS runs (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL REFERENCES sessions(id),
                 command_id TEXT NOT NULL UNIQUE,
                 user_message_id TEXT NOT NULL,
                 assistant_message_id TEXT NOT NULL,
                  status TEXT NOT NULL,
                  cancel_requested INTEGER NOT NULL DEFAULT 0,
                  outcome_json TEXT,
                 created_at_ms INTEGER NOT NULL,
                 started_at_ms INTEGER,
                 finished_at_ms INTEGER
             );
             CREATE TABLE IF NOT EXISTS messages (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL REFERENCES sessions(id),
                 run_id TEXT NOT NULL REFERENCES runs(id),
                 ordinal INTEGER NOT NULL,
                 role TEXT NOT NULL,
                 state TEXT NOT NULL,
                 output TEXT NOT NULL DEFAULT '',
                 refusal TEXT NOT NULL DEFAULT '',
                 created_at_ms INTEGER NOT NULL,
                 UNIQUE(session_id, ordinal)
             );
             CREATE TABLE IF NOT EXISTS events (
                 workspace_id TEXT NOT NULL REFERENCES workspaces(id),
                 sequence INTEGER NOT NULL,
                 envelope_json TEXT NOT NULL,
                 PRIMARY KEY(workspace_id, sequence)
             );
             CREATE TABLE IF NOT EXISTS commands (
                 id TEXT PRIMARY KEY,
                 request_json TEXT NOT NULL,
                 receipt_json TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS sessions_workspace_updated
                 ON sessions(workspace_id, updated_at_ms DESC);
              CREATE INDEX IF NOT EXISTS runs_ready
                  ON runs(status, created_at_ms);
              CREATE INDEX IF NOT EXISTS runs_session_started
                  ON runs(session_id, started_at_ms);
             CREATE INDEX IF NOT EXISTS messages_session_ordinal
                 ON messages(session_id, ordinal);",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let has_cancel_requested = {
        let mut statement = connection
            .prepare("PRAGMA table_info(runs)")
            .map_err(|_| SessionRuntimeError::Persistence)?;
        statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|_| SessionRuntimeError::Persistence)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| SessionRuntimeError::Persistence)?
            .iter()
            .any(|column| column == "cancel_requested")
    };
    if !has_cancel_requested {
        connection
            .execute(
                "ALTER TABLE runs ADD COLUMN cancel_requested INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
    }
    let schema_version = connection
        .query_row(
            "SELECT value FROM metadata WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    match schema_version.as_deref() {
        None => {
            connection
                .execute(
                    "INSERT INTO metadata(key, value) VALUES ('schema_version', '1')",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("1") => {}
        Some(_) => return Err(SessionRuntimeError::Persistence),
    }
    let stored = connection
        .query_row(
            "SELECT value FROM metadata WHERE key = 'store_id'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let store_id = match stored {
        Some(value) => value
            .parse()
            .map_err(|_| SessionRuntimeError::Persistence)?,
        None => {
            let id = StoreId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            connection
                .execute(
                    "INSERT INTO metadata(key, value) VALUES ('store_id', ?1)",
                    [id.to_string()],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            id
        }
    };
    Ok((connection, store_id))
}

#[derive(Clone)]
struct ClaimedRun {
    workspace_id: WorkspaceId,
    workspace: String,
    session_id: SessionId,
    run_id: RunId,
    command_id: CommandId,
    assistant_message_id: MessageId,
    model: ModelSelection,
    messages: Vec<MessageSnapshot>,
    started: SessionEventEnvelope,
}

struct AppliedCommand {
    receipt: CommandReceipt,
    schedule: bool,
}

fn execute_command(
    connection: &mut Connection,
    store_id: StoreId,
    command_id: CommandId,
    command: SessionCommand,
) -> Result<AppliedCommand, SessionRuntimeError> {
    let request_json =
        serde_json::to_string(&command).map_err(|_| SessionRuntimeError::Persistence)?;
    if let Some((stored_request, stored_receipt)) = connection
        .query_row(
            "SELECT request_json, receipt_json FROM commands WHERE id = ?1",
            [command_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?
    {
        if stored_request != request_json {
            return Err(SessionRuntimeError::IdempotencyConflict);
        }
        let receipt =
            serde_json::from_str(&stored_receipt).map_err(|_| SessionRuntimeError::Persistence)?;
        return Ok(AppliedCommand {
            receipt,
            schedule: false,
        });
    }
    let command_count: u32 = connection
        .query_row("SELECT COUNT(*) FROM commands", [], |row| row.get(0))
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if command_count >= MAX_COMMANDS {
        return Err(SessionRuntimeError::CommandLimitReached);
    }

    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let now = now_ms();
    let (receipt, schedule) = match command {
        SessionCommand::ResolveWorkspace { path } => {
            let path = path.trim();
            if path.is_empty() {
                return Err(SessionRuntimeError::EmptyWorkspace);
            }
            let canonical =
                std::fs::canonicalize(path).map_err(|_| SessionRuntimeError::InvalidWorkspace)?;
            if !canonical.is_dir() {
                return Err(SessionRuntimeError::InvalidWorkspace);
            }
            let path = canonical
                .to_str()
                .ok_or(SessionRuntimeError::InvalidWorkspace)?;
            let existing = transaction
                .query_row(
                    "SELECT id, next_sequence FROM workspaces WHERE path = ?1",
                    [path],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)),
                )
                .optional()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            let (workspace_id, sequence) = match existing {
                Some((id, sequence)) => (parse_id(&id)?, sequence),
                None => {
                    let workspace_count: u32 = transaction
                        .query_row("SELECT COUNT(*) FROM workspaces", [], |row| row.get(0))
                        .map_err(|_| SessionRuntimeError::Persistence)?;
                    if workspace_count >= MAX_WORKSPACES {
                        return Err(SessionRuntimeError::WorkspaceLimitReached);
                    }
                    let workspace_id =
                        WorkspaceId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
                    transaction
                        .execute(
                            "INSERT INTO workspaces(id, path, next_sequence) VALUES (?1, ?2, 0)",
                            params![workspace_id.to_string(), path],
                        )
                        .map_err(|_| SessionRuntimeError::Persistence)?;
                    (workspace_id, 0)
                }
            };
            (
                CommandReceipt {
                    command_id,
                    committed_through: EventCursor {
                        store_id,
                        workspace_id,
                        sequence,
                    },
                    outcome: CommandOutcome::WorkspaceResolved { workspace_id },
                },
                false,
            )
        }
        SessionCommand::CreateSession {
            workspace_id,
            parent_id,
            model,
        } => {
            if model
                .model
                .as_ref()
                .is_some_and(|value| value.len() > MAX_MODEL_SELECTION_BYTES)
                || model
                    .organization
                    .as_ref()
                    .is_some_and(|value| value.len() > MAX_MODEL_SELECTION_BYTES)
            {
                return Err(SessionRuntimeError::InvalidModelSelection);
            }
            ensure_workspace(&transaction, workspace_id)?;
            let session_count: u32 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM sessions WHERE workspace_id = ?1",
                    [workspace_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            if session_count >= MAX_SESSIONS_PER_WORKSPACE {
                return Err(SessionRuntimeError::SessionLimitReached);
            }
            if let Some(parent_id) = parent_id {
                let parent_workspace = transaction
                    .query_row(
                        "SELECT workspace_id FROM sessions WHERE id = ?1",
                        [parent_id.to_string()],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(|_| SessionRuntimeError::Persistence)?
                    .ok_or(SessionRuntimeError::SessionNotFound)?;
                if parse_id::<WorkspaceId>(&parent_workspace)? != workspace_id {
                    return Err(SessionRuntimeError::ParentWorkspaceMismatch);
                }
            }
            let session_id = SessionId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            transaction
                .execute(
                    "INSERT INTO sessions(
                        id, workspace_id, parent_id, title, status, model,
                        max_output_tokens, organization, created_at_ms, updated_at_ms
                     ) VALUES (?1, ?2, ?3, 'New session', 'idle', ?4, ?5, ?6, ?7, ?7)",
                    params![
                        session_id.to_string(),
                        workspace_id.to_string(),
                        parent_id.map(|id| id.to_string()),
                        model.model,
                        model.max_output_tokens,
                        model.organization,
                        now,
                    ],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            let summary = load_session_summary(&transaction, session_id)?;
            let event = append_event(
                &transaction,
                EventContext {
                    store_id,
                    workspace_id,
                    session_id,
                    run_id: None,
                    caused_by: Some(command_id),
                    occurred_at_ms: now,
                },
                SessionEvent::SessionCreated { session: summary },
            )?;
            (
                CommandReceipt {
                    command_id,
                    committed_through: event.cursor,
                    outcome: CommandOutcome::SessionCreated { session_id },
                },
                false,
            )
        }
        SessionCommand::SubmitPrompt { session_id, prompt } => {
            let prompt = prompt.trim().to_owned();
            if prompt.is_empty() {
                return Err(SessionRuntimeError::EmptyPrompt);
            }
            if prompt.len() > MAX_PROMPT_BYTES {
                return Err(SessionRuntimeError::PromptTooLarge);
            }
            let (workspace_id, queued, title) = transaction
                .query_row(
                    "SELECT workspace_id, queued_prompts, title FROM sessions WHERE id = ?1",
                    [session_id.to_string()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, u16>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()
                .map_err(|_| SessionRuntimeError::Persistence)?
                .ok_or(SessionRuntimeError::SessionNotFound)?;
            if queued >= MAX_PENDING_PROMPTS {
                return Err(SessionRuntimeError::QueueFull);
            }
            let context_bytes: u64 = transaction
                .query_row(
                    "SELECT COALESCE(SUM(
                         length(CAST(output AS BLOB)) + length(CAST(refusal AS BLOB))
                     ), 0)
                     FROM messages WHERE session_id = ?1",
                    [session_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            if usize::try_from(context_bytes)
                .unwrap_or(usize::MAX)
                .saturating_add(prompt.len())
                > MAX_CONTEXT_BYTES
            {
                return Err(SessionRuntimeError::ContextTooLarge);
            }
            let workspace_id = parse_id(&workspace_id)?;
            let run_id = RunId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            let message_id = MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            let assistant_message_id =
                MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
            let ordinal: u64 = transaction
                .query_row(
                    "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM messages WHERE session_id = ?1",
                    [session_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .execute(
                    "INSERT INTO runs(
                        id, session_id, command_id, user_message_id, assistant_message_id,
                        status, created_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6)",
                    params![
                        run_id.to_string(),
                        session_id.to_string(),
                        command_id.to_string(),
                        message_id.to_string(),
                        assistant_message_id.to_string(),
                        now,
                    ],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .execute(
                    "INSERT INTO messages(
                        id, session_id, run_id, ordinal, role, state, created_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, 'assistant', 'queued', ?5)",
                    params![
                        assistant_message_id.to_string(),
                        session_id.to_string(),
                        run_id.to_string(),
                        ordinal + 1,
                        now,
                    ],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .execute(
                    "INSERT INTO messages(
                        id, session_id, run_id, ordinal, role, state, output, created_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, 'user', 'queued', ?5, ?6)",
                    params![
                        message_id.to_string(),
                        session_id.to_string(),
                        run_id.to_string(),
                        ordinal,
                        prompt,
                        now,
                    ],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            let next_queued = queued + 1;
            let next_title = if title == "New session" {
                prompt_title(&prompt)
            } else {
                title
            };
            transaction
                .execute(
                    "UPDATE sessions
                     SET title = ?2, status = CASE WHEN active_run_id IS NULL THEN 'queued' ELSE status END,
                         queued_prompts = ?3, updated_at_ms = ?4
                     WHERE id = ?1",
                    params![session_id.to_string(), next_title, next_queued, now],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            let summary = load_session_summary(&transaction, session_id)?;
            let message = load_message(&transaction, message_id)?;
            let run = load_run(&transaction, run_id)?;
            let event = append_event(
                &transaction,
                EventContext {
                    store_id,
                    workspace_id,
                    session_id,
                    run_id: Some(run_id),
                    caused_by: Some(command_id),
                    occurred_at_ms: now,
                },
                SessionEvent::PromptQueued {
                    session: summary,
                    message,
                    run,
                    queue_position: next_queued,
                },
            )?;
            (
                CommandReceipt {
                    command_id,
                    committed_through: event.cursor,
                    outcome: CommandOutcome::PromptQueued {
                        session_id,
                        run_id,
                        queue_position: next_queued,
                    },
                },
                true,
            )
        }
        SessionCommand::CancelRun { run_id } => {
            let (session_id, status, stored_outcome) = transaction
                .query_row(
                    "SELECT session_id, status, outcome_json FROM runs WHERE id = ?1",
                    [run_id.to_string()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .optional()
                .map_err(|_| SessionRuntimeError::Persistence)?
                .ok_or(SessionRuntimeError::RunNotFound)?;
            let session_id = parse_id(&session_id)?;
            let workspace_id = session_workspace(&transaction, session_id)?;
            if let Some(outcome) = stored_outcome {
                let outcome =
                    serde_json::from_str(&outcome).map_err(|_| SessionRuntimeError::Persistence)?;
                let sequence = workspace_sequence(&transaction, workspace_id)?;
                (
                    CommandReceipt {
                        command_id,
                        committed_through: EventCursor {
                            store_id,
                            workspace_id,
                            sequence,
                        },
                        outcome: CommandOutcome::RunAlreadyFinished { run_id, outcome },
                    },
                    false,
                )
            } else {
                transaction
                    .execute(
                        "UPDATE runs SET cancel_requested = 1 WHERE id = ?1",
                        [run_id.to_string()],
                    )
                    .map_err(|_| SessionRuntimeError::Persistence)?;
                let summary = load_session_summary(&transaction, session_id)?;
                let requested = append_event(
                    &transaction,
                    EventContext {
                        store_id,
                        workspace_id,
                        session_id,
                        run_id: Some(run_id),
                        caused_by: Some(command_id),
                        occurred_at_ms: now,
                    },
                    SessionEvent::CancellationRequested {
                        session: summary,
                        run_id,
                    },
                )?;
                let cursor = if status == "queued" {
                    finish_queued_run(
                        &transaction,
                        store_id,
                        workspace_id,
                        session_id,
                        run_id,
                        now,
                    )?
                    .cursor
                } else {
                    requested.cursor
                };
                (
                    CommandReceipt {
                        command_id,
                        committed_through: cursor,
                        outcome: CommandOutcome::CancellationRequested { run_id },
                    },
                    status == "queued",
                )
            }
        }
    };
    let receipt_json =
        serde_json::to_string(&receipt).map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "INSERT INTO commands(id, request_json, receipt_json) VALUES (?1, ?2, ?3)",
            params![command_id.to_string(), request_json, receipt_json],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(AppliedCommand { receipt, schedule })
}

fn claim_next_run(
    connection: &mut Connection,
    store_id: StoreId,
) -> Result<Option<ClaimedRun>, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let row = transaction
        .query_row(
            "SELECT r.id, r.session_id, r.command_id, r.user_message_id,
                    r.assistant_message_id,
                    s.workspace_id, w.path, s.model, s.max_output_tokens, s.organization
             FROM runs r
             JOIN sessions s ON s.id = r.session_id
             JOIN workspaces w ON w.id = s.workspace_id
             WHERE r.status = 'queued' AND s.active_run_id IS NULL
             ORDER BY COALESCE((
                         SELECT MAX(previous.started_at_ms)
                         FROM runs previous
                         WHERE previous.session_id = r.session_id
                     ), 0),
                      r.created_at_ms, r.rowid
             LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<u32>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                ))
            },
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let Some((
        run,
        session,
        command,
        user_message,
        assistant_message,
        workspace,
        workspace_path,
        model,
        max_tokens,
        organization,
    )) = row
    else {
        return Ok(None);
    };
    let run_id: RunId = parse_id(&run)?;
    let session_id: SessionId = parse_id(&session)?;
    let command_id: CommandId = parse_id(&command)?;
    let user_message_id = parse_id::<MessageId>(&user_message)?;
    let assistant_message_id = parse_id::<MessageId>(&assistant_message)?;
    let workspace_id: WorkspaceId = parse_id(&workspace)?;
    let now = now_ms();
    transaction
        .execute(
            "UPDATE runs SET status = 'running', started_at_ms = ?2 WHERE id = ?1",
            params![run, now],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "UPDATE messages SET state = 'complete' WHERE id = ?1",
            [user_message],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "UPDATE sessions
             SET active_run_id = ?2, status = 'running', queued_prompts = queued_prompts - 1,
                 updated_at_ms = ?3
             WHERE id = ?1",
            params![session, run, now],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let user_ordinal: u64 = transaction
        .query_row(
            "SELECT ordinal FROM messages WHERE id = ?1",
            [user_message_id.to_string()],
            |row| row.get(0),
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let mut statement = transaction
        .prepare(
            "SELECT id FROM messages
             WHERE session_id = ?1 AND ordinal <= ?2 AND state = 'complete'
             ORDER BY ordinal",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let message_ids = statement
        .query_map(params![session_id.to_string(), user_ordinal], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    drop(statement);
    let mut messages = Vec::with_capacity(message_ids.len());
    for id in message_ids {
        messages.push(load_message(&transaction, parse_id(&id)?)?);
    }
    let summary = load_session_summary(&transaction, session_id)?;
    let started = append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id,
            session_id,
            run_id: Some(run_id),
            caused_by: None,
            occurred_at_ms: now,
        },
        SessionEvent::RunStarted {
            session: summary,
            run_id,
        },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(Some(ClaimedRun {
        workspace_id,
        workspace: workspace_path,
        session_id,
        run_id,
        command_id,
        assistant_message_id,
        model: ModelSelection {
            model,
            max_output_tokens: max_tokens,
            organization,
        },
        messages,
        started,
    }))
}

fn start_assistant(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let now = now_ms();
    let updated = transaction
        .execute(
            "UPDATE messages SET state = 'streaming'
             WHERE id = ?1 AND role = 'assistant' AND state = 'queued'",
            [claimed.assistant_message_id.to_string()],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if updated != 1 {
        return Err(SessionRuntimeError::Unavailable);
    }
    let message = load_message(&transaction, claimed.assistant_message_id)?;
    let event = append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id: claimed.workspace_id,
            session_id: claimed.session_id,
            run_id: Some(claimed.run_id),
            caused_by: Some(claimed.command_id),
            occurred_at_ms: now,
        },
        SessionEvent::AssistantMessageStarted { message },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(event)
}

fn append_text(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    message_id: MessageId,
    channel: TextChannel,
    text: String,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    if text.is_empty() {
        return Err(SessionRuntimeError::Persistence);
    }
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let persisted_bytes: u64 = transaction
        .query_row(
            "SELECT COALESCE(SUM(
                 length(CAST(output AS BLOB)) + length(CAST(refusal AS BLOB))
             ), 0)
             FROM messages WHERE session_id = ?1",
            [claimed.session_id.to_string()],
            |row| row.get(0),
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if usize::try_from(persisted_bytes)
        .unwrap_or(usize::MAX)
        .saturating_add(text.len())
        > MAX_CONTEXT_BYTES
    {
        return Err(SessionRuntimeError::OutputTooLarge);
    }
    let column = match channel {
        TextChannel::Output => "output",
        TextChannel::Refusal => "refusal",
    };
    let sql = format!(
        "UPDATE messages SET {column} = {column} || ?2 WHERE id = ?1 AND state = 'streaming'"
    );
    let updated = transaction
        .execute(&sql, params![message_id.to_string(), text])
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if updated != 1 {
        return Err(SessionRuntimeError::Unavailable);
    }
    let event = append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id: claimed.workspace_id,
            session_id: claimed.session_id,
            run_id: Some(claimed.run_id),
            caused_by: Some(claimed.command_id),
            occurred_at_ms: now_ms(),
        },
        SessionEvent::TextAppended {
            message_id,
            channel,
            text,
        },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(event)
}

fn complete_run(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    outcome: RunOutcome,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let now = now_ms();
    let outcome = cancellation_wins(&transaction, claimed.run_id, outcome)?;
    let (run_status, message_state) = outcome_states(&outcome);
    let outcome_json =
        serde_json::to_string(&outcome).map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "UPDATE runs
             SET status = ?2, outcome_json = ?3, finished_at_ms = ?4
             WHERE id = ?1 AND outcome_json IS NULL",
            params![claimed.run_id.to_string(), run_status, outcome_json, now],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "UPDATE messages SET state = ?2 WHERE run_id = ?1 AND role = 'assistant'",
            params![claimed.run_id.to_string(), message_state],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "UPDATE sessions
             SET active_run_id = NULL,
                 status = CASE WHEN queued_prompts > 0 THEN 'queued' ELSE 'idle' END,
                 updated_at_ms = ?2
             WHERE id = ?1 AND active_run_id = ?3",
            params![
                claimed.session_id.to_string(),
                now,
                claimed.run_id.to_string(),
            ],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let summary = load_session_summary(&transaction, claimed.session_id)?;
    let event = append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id: claimed.workspace_id,
            session_id: claimed.session_id,
            run_id: Some(claimed.run_id),
            caused_by: Some(claimed.command_id),
            occurred_at_ms: now,
        },
        SessionEvent::RunFinished {
            session: summary,
            run_id: claimed.run_id,
            outcome,
        },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(event)
}

fn finish_queued_run(
    transaction: &Transaction<'_>,
    store_id: StoreId,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    run_id: RunId,
    now: u64,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let outcome = RunOutcome::Cancelled;
    let outcome_json =
        serde_json::to_string(&outcome).map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "UPDATE runs
             SET status = 'cancelled', outcome_json = ?2, finished_at_ms = ?3
             WHERE id = ?1 AND status = 'queued'",
            params![run_id.to_string(), outcome_json, now],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "UPDATE messages SET state = 'cancelled' WHERE run_id = ?1",
            [run_id.to_string()],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "UPDATE sessions
             SET queued_prompts = queued_prompts - 1,
                 status = CASE
                     WHEN active_run_id IS NOT NULL THEN 'running'
                     WHEN queued_prompts > 1 THEN 'queued'
                     ELSE 'idle'
                 END,
                 updated_at_ms = ?2
             WHERE id = ?1",
            params![session_id.to_string(), now],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let summary = load_session_summary(transaction, session_id)?;
    append_event(
        transaction,
        EventContext {
            store_id,
            workspace_id,
            session_id,
            run_id: Some(run_id),
            caused_by: None,
            occurred_at_ms: now,
        },
        SessionEvent::RunFinished {
            session: summary,
            run_id,
            outcome,
        },
    )
}

fn recover_interrupted_runs(
    connection: &mut Connection,
    store_id: StoreId,
) -> Result<Vec<EventCursor>, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let mut statement = transaction
        .prepare(
            "SELECT r.id, r.session_id, s.workspace_id
             FROM runs r JOIN sessions s ON s.id = r.session_id
             WHERE r.status = 'running'",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    drop(statement);
    let mut cursors = Vec::with_capacity(rows.len());
    for (run, session, workspace) in rows {
        let run_id = parse_id(&run)?;
        let session_id = parse_id(&session)?;
        let workspace_id = parse_id(&workspace)?;
        let claimed = ClaimedRun {
            workspace_id,
            workspace: String::new(),
            session_id,
            run_id,
            command_id: CommandId::from_bytes([0; 16]),
            assistant_message_id: MessageId::from_bytes([0; 16]),
            model: ModelSelection::default(),
            messages: Vec::new(),
            started: SessionEventEnvelope {
                cursor: EventCursor {
                    store_id,
                    workspace_id,
                    sequence: 0,
                },
                session_id,
                run_id: Some(run_id),
                caused_by: None,
                occurred_at_ms: 0,
                event: SessionEvent::RunStarted {
                    session: load_session_summary(&transaction, session_id)?,
                    run_id,
                },
            },
        };
        let event =
            complete_run_in_transaction(&transaction, store_id, &claimed, RunOutcome::Interrupted)?;
        cursors.push(event.cursor);
    }
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(cursors)
}

fn complete_run_in_transaction(
    transaction: &Transaction<'_>,
    store_id: StoreId,
    claimed: &ClaimedRun,
    outcome: RunOutcome,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let now = now_ms();
    let outcome = cancellation_wins(transaction, claimed.run_id, outcome)?;
    let (run_status, message_state) = outcome_states(&outcome);
    let outcome_json =
        serde_json::to_string(&outcome).map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "UPDATE runs SET status = ?2, outcome_json = ?3, finished_at_ms = ?4 WHERE id = ?1",
            params![claimed.run_id.to_string(), run_status, outcome_json, now],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "UPDATE messages SET state = ?2 WHERE run_id = ?1 AND role = 'assistant'",
            params![claimed.run_id.to_string(), message_state],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "UPDATE sessions
             SET active_run_id = NULL,
                 status = CASE WHEN queued_prompts > 0 THEN 'queued' ELSE 'idle' END,
                 updated_at_ms = ?2
             WHERE id = ?1",
            params![claimed.session_id.to_string(), now],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let summary = load_session_summary(transaction, claimed.session_id)?;
    append_event(
        transaction,
        EventContext {
            store_id,
            workspace_id: claimed.workspace_id,
            session_id: claimed.session_id,
            run_id: Some(claimed.run_id),
            caused_by: None,
            occurred_at_ms: now,
        },
        SessionEvent::RunFinished {
            session: summary,
            run_id: claimed.run_id,
            outcome,
        },
    )
}

fn cancellation_wins(
    transaction: &Transaction<'_>,
    run_id: RunId,
    outcome: RunOutcome,
) -> Result<RunOutcome, SessionRuntimeError> {
    if matches!(outcome, RunOutcome::Cancelled) {
        return Ok(outcome);
    }
    let requested = transaction
        .query_row(
            "SELECT cancel_requested FROM runs WHERE id = ?1",
            [run_id.to_string()],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(if requested {
        RunOutcome::Cancelled
    } else {
        outcome
    })
}

fn outcome_states(outcome: &RunOutcome) -> (&'static str, &'static str) {
    match outcome {
        RunOutcome::Completed => ("completed", "complete"),
        RunOutcome::Cancelled => ("cancelled", "cancelled"),
        RunOutcome::Interrupted => ("interrupted", "interrupted"),
        RunOutcome::Failed { .. } => ("failed", "failed"),
    }
}

fn load_snapshot(
    connection: &mut Connection,
    store_id: StoreId,
    request: SnapshotRequest,
) -> Result<WorkspaceSnapshot, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let (path, sequence) = transaction
        .query_row(
            "SELECT path, next_sequence FROM workspaces WHERE id = ?1",
            [request.workspace_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?
        .ok_or(SessionRuntimeError::WorkspaceNotFound)?;
    let mut statement = transaction
        .prepare(
            "SELECT id FROM sessions WHERE workspace_id = ?1
             ORDER BY updated_at_ms DESC, rowid DESC LIMIT ?2",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let ids = statement
        .query_map(
            params![
                request.workspace_id.to_string(),
                u64::from(request.session_limit) + 1
            ],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    drop(statement);
    let has_older_sessions = ids.len() > usize::from(request.session_limit);
    let mut sessions = Vec::with_capacity(ids.len().min(usize::from(request.session_limit)));
    for id in ids.into_iter().take(usize::from(request.session_limit)) {
        sessions.push(load_session_summary(&transaction, parse_id(&id)?)?);
    }
    let focused = request
        .focused_session_id
        .map(|session_id| {
            if session_workspace(&transaction, session_id)? != request.workspace_id {
                return Err(SessionRuntimeError::SessionNotFound);
            }
            load_session_snapshot(&transaction, session_id, request.message_limit)
        })
        .transpose()?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(WorkspaceSnapshot {
        cursor: EventCursor {
            store_id,
            workspace_id: request.workspace_id,
            sequence,
        },
        workspace: WorkspaceSummary {
            id: request.workspace_id,
            path,
        },
        sessions,
        focused,
        has_older_sessions,
    })
}

fn load_session_snapshot(
    transaction: &Transaction<'_>,
    session_id: SessionId,
    message_limit: u16,
) -> Result<SessionSnapshot, SessionRuntimeError> {
    let summary = load_session_summary(transaction, session_id)?;
    let mut statement = transaction
        .prepare(
            "SELECT id FROM messages
             WHERE session_id = ?1 AND NOT (role = 'assistant' AND state = 'queued')
             ORDER BY ordinal DESC LIMIT ?2",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let mut message_ids = statement
        .query_map(
            params![session_id.to_string(), u64::from(message_limit) + 1],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    drop(statement);
    let has_older_messages = message_ids.len() > usize::from(message_limit);
    message_ids.truncate(usize::from(message_limit));
    message_ids.reverse();
    let mut messages = Vec::with_capacity(message_ids.len());
    for id in message_ids {
        messages.push(load_message(transaction, parse_id(&id)?)?);
    }
    let mut statement = transaction
        .prepare(
            "SELECT id FROM runs WHERE session_id = ?1
             ORDER BY created_at_ms DESC, rowid DESC LIMIT ?2",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let mut run_ids = statement
        .query_map(params![session_id.to_string(), message_limit], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    drop(statement);
    run_ids.reverse();
    let mut runs = Vec::with_capacity(run_ids.len());
    for id in run_ids {
        runs.push(load_run(transaction, parse_id(&id)?)?);
    }
    Ok(SessionSnapshot {
        summary,
        messages,
        runs,
        has_older_messages,
    })
}

fn read_events(
    connection: &mut Connection,
    workspace_id: WorkspaceId,
    after: u64,
    limit: u16,
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
    ensure_workspace(connection, workspace_id)?;
    let mut statement = connection
        .prepare(
            "SELECT envelope_json FROM events
             WHERE workspace_id = ?1 AND sequence > ?2
             ORDER BY sequence LIMIT ?3",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    statement
        .query_map(params![workspace_id.to_string(), after, limit], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|_| SessionRuntimeError::Persistence)?
        .map(|row| {
            let encoded = row.map_err(|_| SessionRuntimeError::Persistence)?;
            serde_json::from_str(&encoded).map_err(|_| SessionRuntimeError::Persistence)
        })
        .collect()
}

#[derive(Clone, Copy)]
struct EventContext {
    store_id: StoreId,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    run_id: Option<RunId>,
    caused_by: Option<CommandId>,
    occurred_at_ms: u64,
}

fn append_event(
    transaction: &Transaction<'_>,
    context: EventContext,
    event: SessionEvent,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    transaction
        .execute(
            "UPDATE workspaces SET next_sequence = next_sequence + 1 WHERE id = ?1",
            [context.workspace_id.to_string()],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let sequence = workspace_sequence(transaction, context.workspace_id)?;
    let envelope = SessionEventEnvelope {
        cursor: EventCursor {
            store_id: context.store_id,
            workspace_id: context.workspace_id,
            sequence,
        },
        session_id: context.session_id,
        run_id: context.run_id,
        caused_by: context.caused_by,
        occurred_at_ms: context.occurred_at_ms,
        event,
    };
    let encoded = serde_json::to_string(&envelope).map_err(|_| SessionRuntimeError::Persistence)?;
    if encoded.len() > MAX_PERSISTED_EVENT_BYTES {
        return Err(SessionRuntimeError::EventTooLarge);
    }
    transaction
        .execute(
            "INSERT INTO events(workspace_id, sequence, envelope_json) VALUES (?1, ?2, ?3)",
            params![context.workspace_id.to_string(), sequence, encoded],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(envelope)
}

fn load_session_summary(
    connection: &Connection,
    session_id: SessionId,
) -> Result<SessionSummary, SessionRuntimeError> {
    connection
        .query_row(
            "SELECT s.workspace_id, s.parent_id, s.title, s.status, s.active_run_id,
                     s.queued_prompts, s.updated_at_ms,
                     (SELECT outcome_json FROM runs
                      WHERE session_id = s.id AND outcome_json IS NOT NULL
                      ORDER BY finished_at_ms DESC, rowid DESC LIMIT 1)
              FROM sessions s WHERE s.id = ?1",
            [session_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, u16>(5)?,
                    row.get::<_, u64>(6)?,
                    row.get::<_, Option<String>>(7)?,
                ))
            },
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?
        .ok_or(SessionRuntimeError::SessionNotFound)
        .and_then(
            |(workspace, parent, title, status, active, queued, updated, last_outcome)| {
                Ok(SessionSummary {
                    id: session_id,
                    workspace_id: parse_id(&workspace)?,
                    parent_id: parent.as_deref().map(parse_id).transpose()?,
                    title,
                    status: parse_session_status(&status)?,
                    active_run_id: active.as_deref().map(parse_id).transpose()?,
                    queued_prompts: queued,
                    updated_at_ms: updated,
                    last_outcome: last_outcome
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()
                        .map_err(|_| SessionRuntimeError::Persistence)?,
                })
            },
        )
}

fn load_message(
    connection: &Connection,
    message_id: MessageId,
) -> Result<MessageSnapshot, SessionRuntimeError> {
    connection
        .query_row(
            "SELECT session_id, run_id, role, state, output, refusal, created_at_ms
             FROM messages WHERE id = ?1",
            [message_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, u64>(6)?,
                ))
            },
        )
        .map_err(|_| SessionRuntimeError::Persistence)
        .and_then(|(session, run, role, state, output, refusal, created)| {
            Ok(MessageSnapshot {
                id: message_id,
                session_id: parse_id(&session)?,
                run_id: parse_id(&run)?,
                role: parse_message_role(&role)?,
                state: parse_message_state(&state)?,
                output,
                refusal,
                created_at_ms: created,
            })
        })
}

fn load_run(connection: &Connection, run_id: RunId) -> Result<RunSnapshot, SessionRuntimeError> {
    connection
        .query_row(
            "SELECT session_id, status, outcome_json FROM runs WHERE id = ?1",
            [run_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .map_err(|_| SessionRuntimeError::Persistence)
        .and_then(|(session, status, outcome)| {
            Ok(RunSnapshot {
                id: run_id,
                session_id: parse_id(&session)?,
                status: parse_run_status(&status)?,
                outcome: outcome
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()
                    .map_err(|_| SessionRuntimeError::Persistence)?,
            })
        })
}

fn ensure_workspace(
    connection: &Connection,
    workspace_id: WorkspaceId,
) -> Result<(), SessionRuntimeError> {
    let found = connection
        .query_row(
            "SELECT 1 FROM workspaces WHERE id = ?1",
            [workspace_id.to_string()],
            |_| Ok(()),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    found.ok_or(SessionRuntimeError::WorkspaceNotFound)
}

fn session_workspace(
    connection: &Connection,
    session_id: SessionId,
) -> Result<WorkspaceId, SessionRuntimeError> {
    let workspace = connection
        .query_row(
            "SELECT workspace_id FROM sessions WHERE id = ?1",
            [session_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?
        .ok_or(SessionRuntimeError::SessionNotFound)?;
    parse_id(&workspace)
}

fn workspace_sequence(
    connection: &Connection,
    workspace_id: WorkspaceId,
) -> Result<u64, SessionRuntimeError> {
    connection
        .query_row(
            "SELECT next_sequence FROM workspaces WHERE id = ?1",
            [workspace_id.to_string()],
            |row| row.get(0),
        )
        .map_err(|_| SessionRuntimeError::Persistence)
}

fn parse_id<T>(value: &str) -> Result<T, SessionRuntimeError>
where
    T: std::str::FromStr,
{
    value.parse().map_err(|_| SessionRuntimeError::Persistence)
}

fn parse_session_status(value: &str) -> Result<SessionStatus, SessionRuntimeError> {
    match value {
        "idle" => Ok(SessionStatus::Idle),
        "queued" => Ok(SessionStatus::Queued),
        "running" => Ok(SessionStatus::Running),
        _ => Err(SessionRuntimeError::Persistence),
    }
}

fn parse_run_status(value: &str) -> Result<RunStatus, SessionRuntimeError> {
    match value {
        "queued" => Ok(RunStatus::Queued),
        "running" => Ok(RunStatus::Running),
        "completed" => Ok(RunStatus::Completed),
        "cancelled" => Ok(RunStatus::Cancelled),
        "failed" => Ok(RunStatus::Failed),
        "interrupted" => Ok(RunStatus::Interrupted),
        _ => Err(SessionRuntimeError::Persistence),
    }
}

fn parse_message_role(value: &str) -> Result<MessageRole, SessionRuntimeError> {
    match value {
        "user" => Ok(MessageRole::User),
        "assistant" => Ok(MessageRole::Assistant),
        _ => Err(SessionRuntimeError::Persistence),
    }
}

fn parse_message_state(value: &str) -> Result<MessageState, SessionRuntimeError> {
    match value {
        "queued" => Ok(MessageState::Queued),
        "streaming" => Ok(MessageState::Streaming),
        "complete" => Ok(MessageState::Complete),
        "cancelled" => Ok(MessageState::Cancelled),
        "failed" => Ok(MessageState::Failed),
        "interrupted" => Ok(MessageState::Interrupted),
        _ => Err(SessionRuntimeError::Persistence),
    }
}

fn prompt_title(prompt: &str) -> String {
    let first_line = prompt.lines().next().unwrap_or(prompt).trim();
    let mut title = first_line.chars().take(48).collect::<String>();
    if first_line.chars().count() > 48 {
        title.push_str("...");
    }
    title
}

fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use async_stream::stream as async_stream;
    use futures_util::{StreamExt, stream};
    use qq_provider::{ModelRequest, Provider, ProviderStream};
    use tempfile::TempDir;

    use super::*;

    struct ScriptedLoader;

    impl RuntimeLoader for ScriptedLoader {
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            Box::pin(async {
                Runtime::new(ScriptedProvider, "test-model", 256)
                    .map(Arc::new)
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct ScriptedProvider;

    impl Provider for ScriptedProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "hel".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "l".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "o".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed),
            ]))
        }
    }

    struct ChunkingLoader;

    impl RuntimeLoader for ChunkingLoader {
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            Box::pin(async {
                Runtime::new(ChunkingProvider, "test-model", 256)
                    .map(Arc::new)
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct ChunkingProvider;

    impl Provider for ChunkingProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: String::new(),
                }),
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "é".repeat(MAX_TEXT_CHUNK_BYTES / 2 + 8),
                }),
                Ok(qq_provider::ProviderEvent::Completed),
            ]))
        }
    }

    struct CapturingLoader {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    impl RuntimeLoader for CapturingLoader {
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let requests = Arc::clone(&self.requests);
            Box::pin(async move {
                Runtime::new(DelayedProvider { requests }, "test-model", 256)
                    .map(Arc::new)
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct DelayedProvider {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    impl Provider for DelayedProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            Box::pin(async_stream! {
                tokio::time::sleep(Duration::from_millis(20)).await;
                yield Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "answer".to_owned(),
                });
                yield Ok(qq_provider::ProviderEvent::Completed);
            })
        }
    }

    async fn test_runtime() -> (TempDir, SessionRuntime) {
        let directory = tempfile::tempdir().unwrap();
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(ScriptedLoader),
        )
        .await
        .unwrap();
        (directory, runtime)
    }

    async fn resolve_workspace(
        runtime: &SessionRuntime,
        path: &std::path::Path,
    ) -> (WorkspaceId, EventCursor) {
        let receipt = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::ResolveWorkspace {
                    path: path.to_str().unwrap().to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = receipt.outcome else {
            panic!("unexpected receipt")
        };
        (workspace_id, receipt.committed_through)
    }

    async fn create_session(
        runtime: &SessionRuntime,
        workspace_id: WorkspaceId,
        parent_id: Option<SessionId>,
    ) -> CommandReceipt {
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id,
                    model: ModelSelection::default(),
                },
            )
            .await
            .unwrap()
    }

    async fn collect_through_finished(
        events: &mut SessionEventStream,
    ) -> Vec<SessionEventEnvelope> {
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut observed = Vec::new();
            while let Some(event) = events.next().await {
                let event = event.unwrap();
                let finished = matches!(event.event, SessionEvent::RunFinished { .. });
                observed.push(event);
                if finished {
                    break;
                }
            }
            observed
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn creates_root_and_child_sessions_in_one_workspace_snapshot() {
        let (directory, runtime) = test_runtime().await;
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let root = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated {
            session_id: root_id,
        } = root.outcome
        else {
            panic!("unexpected receipt")
        };
        let child = create_session(&runtime, workspace_id, Some(root_id)).await;
        let CommandOutcome::SessionCreated {
            session_id: child_id,
        } = child.outcome
        else {
            panic!("unexpected receipt")
        };

        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(child_id),
                session_limit: 32,
                message_limit: 32,
            })
            .await
            .unwrap();

        assert_eq!(snapshot.sessions.len(), 2);
        assert_eq!(snapshot.focused.unwrap().summary.parent_id, Some(root_id));
    }

    #[tokio::test]
    async fn retries_return_the_original_durable_receipt() {
        let (directory, runtime) = test_runtime().await;
        let command_id = CommandId::generate().unwrap();
        let command = SessionCommand::ResolveWorkspace {
            path: directory.path().to_str().unwrap().to_owned(),
        };

        let first = runtime.command(command_id, command.clone()).await.unwrap();
        let retry = runtime.command(command_id, command).await.unwrap();

        assert_eq!(retry, first);
        assert_eq!(
            runtime
                .command(
                    command_id,
                    SessionCommand::ResolveWorkspace {
                        path: "/different".to_owned(),
                    },
                )
                .await
                .unwrap_err(),
            SessionRuntimeError::IdempotencyConflict
        );
    }

    #[tokio::test]
    async fn streams_committed_run_events_and_snapshots_the_result() {
        let (directory, runtime) = test_runtime().await;
        let (workspace_id, initial) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();

        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "Say hello".to_owned(),
                },
            )
            .await
            .unwrap();

        let mut observed = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(event) = events.next().await {
                let event = event.unwrap();
                let finished = matches!(event.event, SessionEvent::RunFinished { .. });
                observed.push(event);
                if finished {
                    break;
                }
            }
        })
        .await
        .unwrap();

        assert!(matches!(
            observed[0].event,
            SessionEvent::PromptQueued { .. }
        ));
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(event.event, SessionEvent::TextAppended { .. }))
                .count(),
            2
        );
        assert!(
            observed
                .windows(2)
                .all(|events| { events[1].cursor.sequence == events[0].cursor.sequence + 1 })
        );
        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 32,
                message_limit: 32,
            })
            .await
            .unwrap();
        let focused = snapshot.focused.unwrap();
        assert_eq!(focused.messages.len(), 2);
        assert_eq!(focused.messages[1].output, "hello");
        assert_eq!(focused.summary.status, SessionStatus::Idle);
        assert!(snapshot.cursor.sequence > initial.sequence);
    }

    #[tokio::test]
    async fn subscribers_converge_and_replay_from_an_intermediate_cursor() {
        let (directory, runtime) = test_runtime().await;
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let request = SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        };
        let mut first = runtime.subscribe(request).unwrap();
        let mut second = runtime.subscribe(request).unwrap();

        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "converge".to_owned(),
                },
            )
            .await
            .unwrap();

        let (first, second) = tokio::join!(
            collect_through_finished(&mut first),
            collect_through_finished(&mut second),
        );
        assert_eq!(first, second);
        assert!(first.len() > 2);

        let split = first.len() / 2;
        let mut replay = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: first[split - 1].cursor,
            })
            .unwrap();
        let replayed = tokio::time::timeout(Duration::from_secs(2), async {
            let mut replayed = Vec::new();
            for _ in split..first.len() {
                replayed.push(replay.next().await.unwrap().unwrap());
            }
            replayed
        })
        .await
        .unwrap();

        assert_eq!(replayed, first[split..]);
    }

    #[tokio::test]
    async fn scheduler_store_failure_disables_runtime_and_existing_subscribers() {
        let (directory, runtime) = test_runtime().await;
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        runtime
            .inner
            .store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "persist me".to_owned(),
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            events.next().await.unwrap().unwrap().event,
            SessionEvent::PromptQueued { .. }
        ));

        runtime
            .inner
            .store
            .inner
            .control
            .send(WorkerMessage::Shutdown)
            .unwrap();
        let worker = runtime
            .inner
            .store
            .inner
            .worker
            .lock()
            .unwrap()
            .take()
            .unwrap();
        tokio::task::spawn_blocking(move || worker.join().unwrap())
            .await
            .unwrap();

        let mut failed = runtime.inner.failed.subscribe();
        runtime.request_schedule();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !*failed.borrow() {
                failed.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        assert_eq!(
            events.next().await,
            Some(Err(SessionRuntimeError::Unavailable))
        );
        assert_eq!(
            runtime
                .snapshot(SnapshotRequest {
                    workspace_id,
                    focused_session_id: Some(session_id),
                    session_limit: 1,
                    message_limit: 1,
                })
                .await
                .unwrap_err(),
            SessionRuntimeError::Unavailable
        );
        assert_eq!(
            runtime
                .subscribe(SubscribeRequest {
                    workspace_id,
                    after: created.committed_through,
                })
                .err(),
            Some(SessionRuntimeError::Unavailable)
        );
    }

    #[tokio::test]
    async fn queues_follow_ups_without_reordering_conversation_context() {
        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions {
                database_path: directory.path().join("sessions.sqlite3"),
                max_active_runs: 1,
            },
            Arc::new(CapturingLoader {
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();

        for prompt in ["first", "second"] {
            runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::SubmitPrompt {
                        session_id,
                        prompt: prompt.to_owned(),
                    },
                )
                .await
                .unwrap();
        }
        let mut finished = 0;
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(event) = events.next().await {
                if matches!(event.unwrap().event, SessionEvent::RunFinished { .. }) {
                    finished += 1;
                    if finished == 2 {
                        break;
                    }
                }
            }
        })
        .await
        .unwrap();

        let captured = requests.lock().unwrap();
        assert_eq!(captured.len(), 2);
        assert_eq!(
            captured[1].messages(),
            [
                Message::user("first"),
                Message::assistant("answer"),
                Message::user("second"),
            ]
        );
    }

    #[tokio::test]
    async fn preserves_cancellation_requested_before_runtime_registration() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let command_id = CommandId::generate().unwrap();
        let resolved = store
            .command(
                command_id,
                SessionCommand::ResolveWorkspace {
                    path: directory.path().to_str().unwrap().to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.receipt.outcome else {
            panic!("unexpected receipt")
        };
        let created = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection::default(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
            panic!("unexpected receipt")
        };
        let queued = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "wait".to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::PromptQueued { run_id, .. } = queued.receipt.outcome else {
            panic!("unexpected receipt")
        };

        let claimed = store.claim_next_run().await.unwrap().unwrap();
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CancelRun { run_id },
            )
            .await
            .unwrap();

        assert!(store.cancellation_requested(run_id).await.unwrap());
        store
            .finish_run(&claimed, RunOutcome::Completed)
            .await
            .unwrap();
        let run = store
            .call(Priority::Control, move |connection| {
                load_run(connection, run_id)
            })
            .await
            .unwrap();
        assert_eq!(run.outcome, Some(RunOutcome::Cancelled));
    }

    #[tokio::test]
    async fn chunks_large_deltas_and_ignores_empty_deltas() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(ChunkingLoader),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "large".to_owned(),
                },
            )
            .await
            .unwrap();

        let mut chunks = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(event) = events.next().await {
                match event.unwrap().event {
                    SessionEvent::TextAppended { text, .. } => chunks.push(text),
                    SessionEvent::RunFinished { .. } => break,
                    _ => {}
                }
            }
        })
        .await
        .unwrap();

        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|chunk| !chunk.is_empty()));
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.len() <= MAX_TEXT_CHUNK_BYTES)
        );
        assert_eq!(chunks.concat(), "é".repeat(MAX_TEXT_CHUNK_BYTES / 2 + 8));
    }

    #[tokio::test]
    async fn rejects_cross_workspace_focus_and_oversized_pages() {
        let (directory, runtime) = test_runtime().await;
        let second = tempfile::tempdir().unwrap();
        let (first_workspace, _) = resolve_workspace(&runtime, directory.path()).await;
        let (second_workspace, _) = resolve_workspace(&runtime, second.path()).await;
        let created = create_session(&runtime, second_workspace, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };

        assert_eq!(
            runtime
                .snapshot(SnapshotRequest {
                    workspace_id: first_workspace,
                    focused_session_id: Some(session_id),
                    session_limit: 32,
                    message_limit: 32,
                })
                .await
                .unwrap_err(),
            SessionRuntimeError::SessionNotFound
        );
        assert_eq!(
            runtime
                .snapshot(SnapshotRequest {
                    workspace_id: first_workspace,
                    focused_session_id: None,
                    session_limit: MAX_SNAPSHOT_SESSIONS + 1,
                    message_limit: 1,
                })
                .await
                .unwrap_err(),
            SessionRuntimeError::InvalidPageLimit
        );
    }

    #[tokio::test]
    async fn schedules_ready_sessions_fairly() {
        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions {
                database_path: directory.path().join("sessions.sqlite3"),
                max_active_runs: 1,
            },
            Arc::new(CapturingLoader {
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let first = create_session(&runtime, workspace_id, None).await;
        let second = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated {
            session_id: first_session,
        } = first.outcome
        else {
            panic!("unexpected receipt")
        };
        let CommandOutcome::SessionCreated {
            session_id: second_session,
        } = second.outcome
        else {
            panic!("unexpected receipt")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: second.committed_through,
            })
            .unwrap();
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: first_session,
                    prompt: "first-a".to_owned(),
                },
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(event) = events.next().await {
                if matches!(event.unwrap().event, SessionEvent::RunStarted { .. }) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        for (session_id, prompt) in [(first_session, "first-b"), (second_session, "second-a")] {
            runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::SubmitPrompt {
                        session_id,
                        prompt: prompt.to_owned(),
                    },
                )
                .await
                .unwrap();
        }
        let mut finished = 0;
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(event) = events.next().await {
                if matches!(event.unwrap().event, SessionEvent::RunFinished { .. }) {
                    finished += 1;
                    if finished == 3 {
                        break;
                    }
                }
            }
        })
        .await
        .unwrap();

        let captured = requests.lock().unwrap();
        assert_eq!(captured.len(), 3);
        assert_eq!(
            captured[0].messages().last(),
            Some(&Message::user("first-a"))
        );
        assert_eq!(
            captured[1].messages().last(),
            Some(&Message::user("second-a"))
        );
        assert_eq!(
            captured[2].messages().last(),
            Some(&Message::user("first-b"))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_a_symlinked_database_and_uses_private_permissions() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("sessions.sqlite3");
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(database.clone()),
            Arc::new(ScriptedLoader),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::metadata(&database).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(runtime);

        let victim = directory.path().join("victim");
        std::fs::write(&victim, b"untouched").unwrap();
        let link = directory.path().join("linked.sqlite3");
        symlink(&victim, &link).unwrap();
        let error =
            match SessionRuntime::open(SessionRuntimeOptions::new(link), Arc::new(ScriptedLoader))
                .await
            {
                Ok(_) => panic!("symlinked database was accepted"),
                Err(error) => error,
            };
        assert_eq!(error, SessionRuntimeError::Persistence);
        assert_eq!(std::fs::read(victim).unwrap(), b"untouched");
    }
}
