use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_stream::stream;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select_biased};
use futures_core::Stream;
use futures_util::StreamExt;
use qq_protocol::{
    ApprovalDecision, ApprovalGrant, ApprovalMode, ApprovalResolution, CommandId, CommandOutcome,
    CommandReceipt, EditPreview, EventCursor, MessageId, MessageRole, MessageSnapshot,
    MessageState, ModelPricing, ModelSelection, RunFailure, RunFailureKind, RunId, RunOutcome,
    RunSnapshot, RunStatus, SessionCommand, SessionEvent, SessionEventEnvelope, SessionId,
    SessionSnapshot, SessionStatus, SessionSummary, ShellCommandPreview, SnapshotRequest, StoreId,
    SubscribeRequest, TextChannel, TokenUsage, ToolCallId, ToolCallSnapshot, ToolCallState,
    WorkspaceId, WorkspaceSnapshot, WorkspaceSummary,
};
use qq_provider::{ContentBlock, Message, Role};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Semaphore, mpsc, oneshot, watch};

use crate::{
    GateDecision, Runtime, RuntimeEvent, RuntimeToolCall, ToolGate, ToolGateFuture, approval,
    tools::{FileState, FileStateUpdate},
};

const CONTROL_QUEUE_CAPACITY: usize = 256;
const OUTPUT_QUEUE_CAPACITY: usize = 1024;
const MAX_PENDING_PROMPTS: u16 = 16;
const MAX_CONTEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_PROMPT_BYTES: usize = 128 * 1024;
const MAX_REPLAY_EVENTS: u16 = 128;
const MAX_SNAPSHOT_SESSIONS: u16 = 512;
const MAX_SNAPSHOT_MESSAGES: u16 = 256;
const MAX_SNAPSHOT_TOOL_CALLS: usize = 4_096;
const MAX_TEXT_CHUNK_BYTES: usize = 64 * 1024;
const MAX_FAILURE_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_WORKSPACES: u32 = 1024;
const MAX_SESSIONS_PER_WORKSPACE: u32 = 512;
const MAX_COMMANDS: u32 = 100_000;
const MAX_MODEL_SELECTION_BYTES: usize = 512;
const OUTPUT_BATCH_BYTES: usize = 8 * 1024;
const OUTPUT_BATCH_DELAY: Duration = Duration::from_millis(8);
const MAX_PERSISTED_EVENT_BYTES: usize = 1024 * 1024;
const MAX_GRANT_BYTES: usize = 256;
const MAX_SESSION_GRANTS: u32 = 256;
const MAX_SESSION_FILES: u32 = 4_096;
const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);
const INTERRUPTED_TOOL_RESULT: &str =
    "Tool execution was interrupted before a durable result was recorded.";

pub type RuntimeLoadFuture =
    Pin<Box<dyn Future<Output = Result<LoadedRuntime, RuntimeLoadError>> + Send + 'static>>;

#[derive(Clone)]
pub struct LoadedRuntime {
    pub runtime: Arc<Runtime>,
    pub pricing: Option<ModelPricing>,
}

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
    /// How long an approval request may wait for a client before it is denied.
    pub approval_timeout: Duration,
}

impl SessionRuntimeOptions {
    #[must_use]
    pub fn new(database_path: PathBuf) -> Self {
        Self {
            database_path,
            max_active_runs: 8,
            approval_timeout: DEFAULT_APPROVAL_TIMEOUT,
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
    approvals: Mutex<HashMap<ToolCallId, PendingApproval>>,
    approval_timeout: Duration,
    wakeups: Mutex<HashMap<WorkspaceId, watch::Sender<u64>>>,
    failed: watch::Sender<bool>,
}

struct PendingApproval {
    run_id: RunId,
    signal: oneshot::Sender<()>,
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
            approvals: Mutex::new(HashMap::new()),
            approval_timeout: options.approval_timeout,
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
        let signal_approval = match &command {
            SessionCommand::RespondToolApproval { tool_call_id, .. } => Some(*tool_call_id),
            _ => None,
        };
        let should_schedule = matches!(command, SessionCommand::SubmitPrompt { .. });
        let applied = self.inner.store.command(command_id, command).await?;
        self.inner.notify(applied.receipt.committed_through);

        if let Some(run_id) = signal_run {
            self.inner.cancel(run_id);
        }
        if let Some(tool_call_id) = signal_approval {
            self.inner.resolve_approval(tool_call_id);
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

    fn register_approval(&self, tool_call_id: ToolCallId, run_id: RunId) -> oneshot::Receiver<()> {
        let (signal, receiver) = oneshot::channel();
        if let Ok(mut approvals) = self.approvals.lock() {
            approvals.insert(tool_call_id, PendingApproval { run_id, signal });
        }
        receiver
    }

    fn resolve_approval(&self, tool_call_id: ToolCallId) {
        let Ok(mut approvals) = self.approvals.lock() else {
            return;
        };
        if let Some(pending) = approvals.remove(&tool_call_id) {
            let _ = pending.signal.send(());
        }
    }

    fn remove_approval(&self, tool_call_id: ToolCallId) {
        if let Ok(mut approvals) = self.approvals.lock() {
            approvals.remove(&tool_call_id);
        }
    }

    fn clear_run_approvals(&self, run_id: RunId) {
        if let Ok(mut approvals) = self.approvals.lock() {
            approvals.retain(|_, pending| pending.run_id != run_id);
        }
    }
}

/// Applies the session's approval policy to each requested tool call,
/// persisting approval state before publishing it and holding the run open
/// while a client decides.
struct SessionToolGate {
    inner: Arc<SessionRuntimeInner>,
    claimed: ClaimedRun,
    cancellation: watch::Receiver<bool>,
}

impl ToolGate for SessionToolGate {
    fn resolve(&self, call: &RuntimeToolCall) -> ToolGateFuture {
        let inner = Arc::clone(&self.inner);
        let claimed = self.claimed.clone();
        let call = call.clone();
        let mut cancellation = self.cancellation.clone();
        Box::pin(async move {
            let internal_denial = || GateDecision::Deny {
                message: "Tool approval state could not be persisted; the call was denied."
                    .to_owned(),
            };
            let Ok((mode, grants)) = inner.store.approval_policy(claimed.session_id).await else {
                return internal_denial();
            };
            let class = approval::classify(&call.name, &call.arguments);
            match approval::evaluate(mode, &call.name, &class, &grants) {
                approval::PolicyDecision::Execute => GateDecision::Execute,
                approval::PolicyDecision::Deny => {
                    let message = approval::POLICY_DENIED_RESULT.to_owned();
                    match inner
                        .store
                        .deny_tool_call(&claimed, call.id, message.clone())
                        .await
                    {
                        Ok(event) => {
                            inner.notify(event.cursor);
                            GateDecision::Deny { message }
                        }
                        Err(_) => internal_denial(),
                    }
                }
                approval::PolicyDecision::RequireApproval => {
                    let shell = match class {
                        approval::ToolClass::Shell { command, cwd } => {
                            Some(ShellCommandPreview { command, cwd })
                        }
                        _ => None,
                    };
                    let edit = approval::edit_preview(&call.name, &call.arguments);
                    // Register before publishing the request so a client
                    // response can never race past the waiting run.
                    let mut resolved = inner.register_approval(call.id, claimed.run_id);
                    match inner
                        .store
                        .request_tool_approval(&claimed, call.id, shell, edit)
                        .await
                    {
                        Ok(event) => inner.notify(event.cursor),
                        Err(_) => {
                            inner.remove_approval(call.id);
                            return internal_denial();
                        }
                    }
                    let timed_out = tokio::select! {
                        biased;
                        changed = cancellation.changed() => {
                            // Run cancellation or shutdown: leave the call
                            // awaiting so run completion interrupts it.
                            let _ = changed;
                            inner.remove_approval(call.id);
                            return GateDecision::Deny {
                                message: "The run stopped before this approval was resolved."
                                    .to_owned(),
                            };
                        }
                        result = &mut resolved => result.is_err(),
                        () = tokio::time::sleep(inner.approval_timeout) => true,
                    };
                    inner.remove_approval(call.id);
                    match inner
                        .store
                        .conclude_tool_approval(&claimed, call.id, timed_out)
                        .await
                    {
                        Ok(ConcludedApproval::Approved) => GateDecision::Execute,
                        Ok(ConcludedApproval::Denied { message, event }) => {
                            if let Some(event) = event {
                                inner.notify(event.cursor);
                            }
                            GateDecision::Deny { message }
                        }
                        Ok(ConcludedApproval::StillWaiting) | Err(_) => internal_denial(),
                    }
                }
            }
        })
    }
}

enum ConcludedApproval {
    Approved,
    Denied {
        message: String,
        event: Option<Box<SessionEventEnvelope>>,
    },
    StillWaiting,
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
    let loaded = tokio::select! {
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
        Err(error) => {
            finish_run(
                &inner,
                &claimed,
                persistence_failure("failed to persist the assistant message", &error),
            )
            .await;
            return;
        }
    }

    let tool_cancellation = Arc::new(AtomicBool::new(false));
    let gate = Arc::new(SessionToolGate {
        inner: Arc::clone(&inner),
        claimed: claimed.clone(),
        cancellation: cancellation.clone(),
    });
    // The session's durable file-state map seeds the run so read-before-write
    // tracking survives across runs (and server restarts) in one session.
    let file_state = match inner.store.session_file_state(claimed.session_id).await {
        Ok(entries) => Arc::new(FileState::with_entries(entries)),
        Err(error) => {
            finish_run(
                &inner,
                &claimed,
                persistence_failure("failed to load the session file state", &error),
            )
            .await;
            return;
        }
    };
    let mut events = loaded.runtime.run_loop(
        claimed.messages.clone(),
        PathBuf::from(&claimed.workspace),
        Arc::clone(&tool_cancellation),
        gate,
        file_state,
    );
    let mut accounting = RunAccountingAccumulator::new(loaded.pricing.clone());
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
                if let Err(error) =
                    flush_pending_text(&inner, &claimed, &mut pending_channel, &mut pending_text)
                        .await
                {
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
                    )
                    .await;
                    return;
                }
                flush_at = None;
            }
            stopped @ (RunInput::Cancelled | RunInput::Interrupted) => {
                tool_cancellation.store(true, Ordering::Release);
                if let Err(error) =
                    flush_pending_text(&inner, &claimed, &mut pending_channel, &mut pending_text)
                        .await
                {
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
                    )
                    .await;
                    return;
                }
                let outcome = if matches!(stopped, RunInput::Cancelled) {
                    RunOutcome::Cancelled
                } else {
                    RunOutcome::Interrupted
                };
                finish_run_accounted(&inner, &claimed, outcome, Some(accounting.snapshot())).await;
                return;
            }
            RunInput::Event(Some(RuntimeEvent::Started)) => {}
            RunInput::Event(Some(RuntimeEvent::AssistantTurnCompleted {
                turn_ordinal,
                message,
                usage,
                calls,
            })) => {
                if let Err(error) =
                    flush_pending_text(&inner, &claimed, &mut pending_channel, &mut pending_text)
                        .await
                {
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
                    )
                    .await;
                    return;
                }
                flush_at = None;
                accounting.record_turn(usage);
                if message.has_content() || !calls.is_empty() {
                    match inner
                        .store
                        .persist_model_turn(&claimed, turn_ordinal, message, calls)
                        .await
                    {
                        Ok(events) => {
                            for event in events {
                                inner.notify(event.cursor);
                            }
                        }
                        Err(error) => {
                            finish_run(
                                &inner,
                                &claimed,
                                persistence_failure(
                                    "failed to persist the completed model turn",
                                    &error,
                                ),
                            )
                            .await;
                            return;
                        }
                    }
                }
            }
            // Approval transitions (including denials) are persisted and
            // published by the tool gate before this event is emitted.
            RunInput::Event(Some(RuntimeEvent::ToolCallDenied { .. })) => {}
            RunInput::Event(Some(RuntimeEvent::ToolCallStarted { id })) => {
                match inner.store.start_tool_call(&claimed, id).await {
                    Ok(event) => inner.notify(event.cursor),
                    Err(error) => {
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist the started tool call", &error),
                        )
                        .await;
                        return;
                    }
                }
            }
            RunInput::Event(Some(RuntimeEvent::ToolCallFinished {
                id,
                result,
                is_error,
                file_state,
            })) => {
                match inner
                    .store
                    .finish_tool_call(&claimed, id, result, is_error, file_state)
                    .await
                {
                    Ok(event) => inner.notify(event.cursor),
                    Err(error) => {
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist the tool result", &error),
                        )
                        .await;
                        return;
                    }
                }
            }
            RunInput::Event(Some(
                event @ (RuntimeEvent::OutputTextDelta { .. } | RuntimeEvent::RefusalDelta { .. }),
            )) => {
                let (channel, text) = match event {
                    RuntimeEvent::OutputTextDelta { text } => (TextChannel::Output, text),
                    RuntimeEvent::RefusalDelta { text } => (TextChannel::Refusal, text),
                    _ => unreachable!("matched text event"),
                };
                if text.is_empty() {
                    continue;
                }
                if !persisted_first_text {
                    if let Err(error) = persist_text(&inner, &claimed, channel, text).await {
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist model output", &error),
                        )
                        .await;
                        return;
                    }
                    persisted_first_text = true;
                    continue;
                }
                if pending_channel.is_some_and(|pending| pending != channel)
                    && let Err(error) = flush_pending_text(
                        &inner,
                        &claimed,
                        &mut pending_channel,
                        &mut pending_text,
                    )
                    .await
                {
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
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
                    if let Err(error) = flush_pending_text(
                        &inner,
                        &claimed,
                        &mut pending_channel,
                        &mut pending_text,
                    )
                    .await
                    {
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist model output", &error),
                        )
                        .await;
                        return;
                    }
                    flush_at = None;
                }
            }
            RunInput::Event(Some(RuntimeEvent::Completed)) => {
                if let Err(error) =
                    flush_pending_text(&inner, &claimed, &mut pending_channel, &mut pending_text)
                        .await
                {
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
                    )
                    .await;
                    return;
                }
                finish_run_accounted(
                    &inner,
                    &claimed,
                    RunOutcome::Completed,
                    Some(accounting.snapshot()),
                )
                .await;
                return;
            }
            RunInput::Event(Some(RuntimeEvent::Failed { kind, message })) => {
                if let Err(error) =
                    flush_pending_text(&inner, &claimed, &mut pending_channel, &mut pending_text)
                        .await
                {
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
                    )
                    .await;
                    return;
                }
                finish_run_accounted(
                    &inner,
                    &claimed,
                    RunOutcome::Failed {
                        failure: RunFailure {
                            kind,
                            message: truncate_utf8(message, MAX_FAILURE_MESSAGE_BYTES),
                        },
                    },
                    Some(accounting.snapshot()),
                )
                .await;
                return;
            }
            RunInput::Event(None) => {
                if let Err(error) =
                    flush_pending_text(&inner, &claimed, &mut pending_channel, &mut pending_text)
                        .await
                {
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist model output", &error),
                    )
                    .await;
                    return;
                }
                finish_run_accounted(
                    &inner,
                    &claimed,
                    internal_failure("model stream ended without a terminal event"),
                    Some(accounting.snapshot()),
                )
                .await;
                return;
            }
        }
    }
}

enum RunInput {
    Event(Option<RuntimeEvent>),
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
    finish_run_accounted(inner, claimed, outcome, None).await;
}

async fn finish_run_accounted(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    outcome: RunOutcome,
    accounting: Option<RunAccounting>,
) {
    match inner.store.finish_run(claimed, outcome, accounting).await {
        Ok(event) => inner.notify(event.cursor),
        Err(_) => {
            inner.failed.send_replace(true);
        }
    }
    if let Ok(mut cancellations) = inner.cancellations.lock() {
        cancellations.remove(&claimed.run_id);
    }
    inner.clear_run_approvals(claimed.run_id);
}

#[derive(Clone)]
struct RunAccounting {
    usage: Option<TokenUsage>,
    estimated_cost_usd_nanos: Option<u64>,
}

struct RunAccountingAccumulator {
    usage: Option<TokenUsage>,
    estimated_cost_usd_nanos: Option<u64>,
    pricing: Option<ModelPricing>,
    saw_turn: bool,
}

impl RunAccountingAccumulator {
    fn new(pricing: Option<ModelPricing>) -> Self {
        Self {
            usage: Some(TokenUsage::default()),
            estimated_cost_usd_nanos: pricing.as_ref().map(|_| 0),
            pricing,
            saw_turn: false,
        }
    }

    fn record_turn(&mut self, usage: Option<TokenUsage>) {
        self.saw_turn = true;
        let Some(usage) = usage else {
            self.usage = None;
            self.estimated_cost_usd_nanos = None;
            return;
        };
        self.usage = self.usage.and_then(|total| add_usage(total, usage));
        if self.usage.is_none() {
            self.estimated_cost_usd_nanos = None;
            return;
        }
        self.estimated_cost_usd_nanos = self.estimated_cost_usd_nanos.and_then(|total| {
            run_cost(usage, self.pricing.as_ref()?).and_then(|cost| total.checked_add(cost))
        });
    }

    fn snapshot(&self) -> RunAccounting {
        RunAccounting {
            usage: self.saw_turn.then_some(self.usage).flatten(),
            estimated_cost_usd_nanos: self
                .saw_turn
                .then_some(self.estimated_cost_usd_nanos)
                .flatten(),
        }
    }
}

fn add_usage(left: TokenUsage, right: TokenUsage) -> Option<TokenUsage> {
    Some(TokenUsage {
        input_tokens: left.input_tokens.checked_add(right.input_tokens)?,
        cache_read_input_tokens: left
            .cache_read_input_tokens
            .checked_add(right.cache_read_input_tokens)?,
        cache_write_input_tokens: left
            .cache_write_input_tokens
            .checked_add(right.cache_write_input_tokens)?,
        output_tokens: left.output_tokens.checked_add(right.output_tokens)?,
    })
}

fn internal_failure(message: &str) -> RunOutcome {
    RunOutcome::Failed {
        failure: RunFailure {
            kind: RunFailureKind::Server,
            message: message.to_owned(),
        },
    }
}

/// Maps a store error during a run into a run outcome. The deliberate session
/// context budget surfaces as a user-meaningful policy failure; every other
/// error is an internal failure that carries the store error rather than
/// discarding it, since qq-core has no logging facility to record it.
fn persistence_failure(action: &str, error: &SessionRuntimeError) -> RunOutcome {
    match error {
        SessionRuntimeError::OutputTooLarge | SessionRuntimeError::ContextTooLarge => {
            RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::Policy,
                    message: format!(
                        "session context reached its {} MiB limit; start a new session to continue",
                        MAX_CONTEXT_BYTES / (1024 * 1024)
                    ),
                },
            }
        }
        error => RunOutcome::Failed {
            failure: RunFailure {
                kind: RunFailureKind::Server,
                message: format!("{action}: {error}"),
            },
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
    #[error("tool call was not found for that run")]
    ToolCallNotFound,
    #[error("tool call is not awaiting approval")]
    ApprovalNotPending,
    #[error("approval grant is empty or exceeds the session limit")]
    InvalidApprovalGrant,
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

    /// Persists a completed model turn together with every tool call it
    /// requested in one transaction. A crash must never commit the turn's
    /// ToolCall blocks without their tool_calls rows: such orphans would replay
    /// as `tool_use` without `tool_result` and poison every later request.
    async fn persist_model_turn(
        &self,
        claimed: &ClaimedRun,
        turn_ordinal: u16,
        message: Message,
        calls: Vec<RuntimeToolCall>,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            persist_model_turn(
                connection,
                store_id,
                &claimed,
                turn_ordinal,
                &message,
                &calls,
            )
        })
        .await
    }

    async fn start_tool_call(
        &self,
        claimed: &ClaimedRun,
        tool_call_id: ToolCallId,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            start_tool_call(connection, store_id, &claimed, tool_call_id)
        })
        .await
    }

    async fn finish_tool_call(
        &self,
        claimed: &ClaimedRun,
        tool_call_id: ToolCallId,
        result: String,
        is_error: bool,
        file_state: Option<FileStateUpdate>,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            finish_tool_call(
                connection,
                store_id,
                &claimed,
                tool_call_id,
                result,
                is_error,
                file_state,
            )
        })
        .await
    }

    async fn session_file_state(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<(String, String)>, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            let mut statement = connection
                .prepare("SELECT path, content_hash FROM session_files WHERE session_id = ?1")
                .map_err(|_| SessionRuntimeError::Persistence)?;
            statement
                .query_map([session_id.to_string()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|_| SessionRuntimeError::Persistence)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| SessionRuntimeError::Persistence)
        })
        .await
    }

    async fn approval_policy(
        &self,
        session_id: SessionId,
    ) -> Result<(ApprovalMode, approval::SessionGrants), SessionRuntimeError> {
        self.call(Priority::Output, move |connection| {
            load_approval_policy(connection, session_id)
        })
        .await
    }

    async fn deny_tool_call(
        &self,
        claimed: &ClaimedRun,
        tool_call_id: ToolCallId,
        message: String,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            deny_tool_call(connection, store_id, &claimed, tool_call_id, &message)
        })
        .await
    }

    async fn request_tool_approval(
        &self,
        claimed: &ClaimedRun,
        tool_call_id: ToolCallId,
        shell: Option<ShellCommandPreview>,
        edit: Option<EditPreview>,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            request_tool_approval(connection, store_id, &claimed, tool_call_id, shell, edit)
        })
        .await
    }

    async fn conclude_tool_approval(
        &self,
        claimed: &ClaimedRun,
        tool_call_id: ToolCallId,
        timed_out: bool,
    ) -> Result<ConcludedApproval, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            conclude_tool_approval(connection, store_id, &claimed, tool_call_id, timed_out)
        })
        .await
    }

    async fn finish_run(
        &self,
        claimed: &ClaimedRun,
        outcome: RunOutcome,
        accounting: Option<RunAccounting>,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            complete_run(connection, store_id, &claimed, outcome, accounting)
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
    let mut connection = Connection::open_with_flags(
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
                 approval_mode TEXT NOT NULL DEFAULT 'ask',
                 estimated_cost_usd_nanos INTEGER NOT NULL DEFAULT 0,
                 cost_known INTEGER NOT NULL DEFAULT 1,
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
                   usage_json TEXT,
                   estimated_cost_usd_nanos INTEGER,
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
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            create_tool_tables(&transaction)?;
            create_grant_table(&transaction)?;
            create_session_files_table(&transaction)?;
            transaction
                .execute(
                    "INSERT INTO metadata(key, value) VALUES ('schema_version', '5')",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("1") => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            if !has_column(&transaction, "runs", "cancel_requested")? {
                transaction
                    .execute(
                        "ALTER TABLE runs ADD COLUMN cancel_requested INTEGER NOT NULL DEFAULT 0",
                        [],
                    )
                    .map_err(|_| SessionRuntimeError::Persistence)?;
            }
            for statement in [
                "ALTER TABLE sessions ADD COLUMN estimated_cost_usd_nanos INTEGER NOT NULL DEFAULT 0",
                "ALTER TABLE sessions ADD COLUMN cost_known INTEGER NOT NULL DEFAULT 1",
                "ALTER TABLE sessions ADD COLUMN approval_mode TEXT NOT NULL DEFAULT 'ask'",
                "ALTER TABLE runs ADD COLUMN usage_json TEXT",
                "ALTER TABLE runs ADD COLUMN estimated_cost_usd_nanos INTEGER",
            ] {
                transaction
                    .execute(statement, [])
                    .map_err(|_| SessionRuntimeError::Persistence)?;
            }
            transaction
                .execute("UPDATE sessions SET cost_known = 0", [])
                .map_err(|_| SessionRuntimeError::Persistence)?;
            create_tool_tables(&transaction)?;
            create_grant_table(&transaction)?;
            create_session_files_table(&transaction)?;
            transaction
                .execute(
                    "UPDATE metadata SET value = '5' WHERE key = 'schema_version'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("2") => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .execute(
                    "ALTER TABLE sessions ADD COLUMN approval_mode TEXT NOT NULL DEFAULT 'ask'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            create_tool_tables(&transaction)?;
            create_grant_table(&transaction)?;
            create_session_files_table(&transaction)?;
            transaction
                .execute(
                    "UPDATE metadata SET value = '5' WHERE key = 'schema_version'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("3") => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            for statement in [
                "ALTER TABLE sessions ADD COLUMN approval_mode TEXT NOT NULL DEFAULT 'ask'",
                "ALTER TABLE tool_calls ADD COLUMN approval_resolution TEXT",
                "ALTER TABLE tool_calls ADD COLUMN resolved_at_ms INTEGER",
            ] {
                transaction
                    .execute(statement, [])
                    .map_err(|_| SessionRuntimeError::Persistence)?;
            }
            create_grant_table(&transaction)?;
            create_session_files_table(&transaction)?;
            transaction
                .execute(
                    "UPDATE metadata SET value = '5' WHERE key = 'schema_version'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("4") => {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            create_session_files_table(&transaction)?;
            transaction
                .execute(
                    "UPDATE metadata SET value = '5' WHERE key = 'schema_version'",
                    [],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            transaction
                .commit()
                .map_err(|_| SessionRuntimeError::Persistence)?;
        }
        Some("5") => {}
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

fn create_tool_tables(connection: &Connection) -> Result<(), SessionRuntimeError> {
    connection
        .execute_batch(
            "CREATE TABLE model_turns (
                 run_id TEXT NOT NULL REFERENCES runs(id),
                 turn_ordinal INTEGER NOT NULL,
                 assistant_content_json TEXT NOT NULL,
                 PRIMARY KEY(run_id, turn_ordinal)
             );
             CREATE TABLE tool_calls (
                 id TEXT PRIMARY KEY,
                 run_id TEXT NOT NULL REFERENCES runs(id),
                 turn_ordinal INTEGER NOT NULL,
                 call_ordinal INTEGER NOT NULL,
                 provider_call_id TEXT NOT NULL,
                 name TEXT NOT NULL,
                 arguments_json TEXT NOT NULL,
                 state TEXT NOT NULL,
                 result TEXT,
                 is_error INTEGER NOT NULL DEFAULT 0,
                 approval_resolution TEXT,
                 requested_at_ms INTEGER NOT NULL,
                 started_at_ms INTEGER,
                 resolved_at_ms INTEGER,
                 finished_at_ms INTEGER,
                 UNIQUE(run_id, turn_ordinal, provider_call_id),
                 UNIQUE(run_id, turn_ordinal, call_ordinal)
             );
             CREATE INDEX tool_calls_run_ordinal
                 ON tool_calls(run_id, turn_ordinal, call_ordinal);",
        )
        .map_err(|_| SessionRuntimeError::Persistence)
}

fn create_grant_table(connection: &Connection) -> Result<(), SessionRuntimeError> {
    connection
        .execute_batch(
            "CREATE TABLE session_grants (
                 session_id TEXT NOT NULL REFERENCES sessions(id),
                 kind TEXT NOT NULL,
                 value TEXT NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 UNIQUE(session_id, kind, value)
             );",
        )
        .map_err(|_| SessionRuntimeError::Persistence)
}

fn create_session_files_table(connection: &Connection) -> Result<(), SessionRuntimeError> {
    connection
        .execute_batch(
            "CREATE TABLE session_files (
                 session_id TEXT NOT NULL REFERENCES sessions(id),
                 path TEXT NOT NULL,
                 content_hash TEXT NOT NULL,
                 updated_at_ms INTEGER NOT NULL,
                 UNIQUE(session_id, path)
             );",
        )
        .map_err(|_| SessionRuntimeError::Persistence)
}

fn has_column(
    connection: &Connection,
    table: &str,
    column: &str,
) -> Result<bool, SessionRuntimeError> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(columns.iter().any(|candidate| candidate == column))
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
    messages: Vec<Message>,
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
            approval_mode,
        } => {
            if !model.model.as_ref().is_some_and(|value| {
                value.len() <= MAX_MODEL_SELECTION_BYTES
                    && value
                        .split_once('/')
                        .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
            }) || model.max_output_tokens == Some(0)
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
                        max_output_tokens, organization, approval_mode,
                        created_at_ms, updated_at_ms
                     ) VALUES (?1, ?2, ?3, 'New session', 'idle', ?4, ?5, ?6, ?7, ?8, ?8)",
                    params![
                        session_id.to_string(),
                        workspace_id.to_string(),
                        parent_id.map(|id| id.to_string()),
                        model.model,
                        model.max_output_tokens,
                        model.organization,
                        approval_mode_str(approval_mode),
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
                    "SELECT
                         (SELECT COALESCE(SUM(
                             length(CAST(output AS BLOB)) + length(CAST(refusal AS BLOB))
                         ), 0) FROM messages WHERE session_id = ?1)
                         +
                         (SELECT COALESCE(SUM(
                             length(CAST(t.arguments_json AS BLOB))
                             + length(CAST(COALESCE(t.result, '') AS BLOB))
                         ), 0) FROM tool_calls t JOIN runs r ON r.id = t.run_id
                            WHERE r.session_id = ?1)",
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
            let next_title = if ordinal == 1 {
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
        SessionCommand::RespondToolApproval {
            run_id,
            tool_call_id,
            decision,
        } => {
            let (call_run, state, resolution) = transaction
                .query_row(
                    "SELECT run_id, state, approval_resolution FROM tool_calls WHERE id = ?1",
                    [tool_call_id.to_string()],
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
                .ok_or(SessionRuntimeError::ToolCallNotFound)?;
            if parse_id::<RunId>(&call_run)? != run_id {
                return Err(SessionRuntimeError::ToolCallNotFound);
            }
            let session_id: SessionId = transaction
                .query_row(
                    "SELECT session_id FROM runs WHERE id = ?1",
                    [run_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .map_err(|_| SessionRuntimeError::Persistence)
                .and_then(|session| parse_id(&session))?;
            let workspace_id = session_workspace(&transaction, session_id)?;
            if let Some(resolution) = resolution {
                // Idempotent: a second response returns the recorded outcome
                // without touching the call again.
                let resolution = parse_approval_resolution(&resolution)?;
                let sequence = workspace_sequence(&transaction, workspace_id)?;
                (
                    CommandReceipt {
                        command_id,
                        committed_through: EventCursor {
                            store_id,
                            workspace_id,
                            sequence,
                        },
                        outcome: CommandOutcome::ToolApprovalResolved {
                            tool_call_id,
                            resolution,
                        },
                    },
                    false,
                )
            } else {
                if state != "awaiting_approval" {
                    return Err(SessionRuntimeError::ApprovalNotPending);
                }
                let resolution = match &decision {
                    ApprovalDecision::ApproveOnce => ApprovalResolution::ApprovedOnce,
                    ApprovalDecision::ApproveForSession { .. } => {
                        ApprovalResolution::ApprovedForSession
                    }
                    ApprovalDecision::Deny => ApprovalResolution::Denied,
                };
                match &decision {
                    ApprovalDecision::ApproveOnce | ApprovalDecision::ApproveForSession { .. } => {
                        transaction
                            .execute(
                                "UPDATE tool_calls
                                 SET state = 'requested', approval_resolution = ?2,
                                     resolved_at_ms = ?3
                                 WHERE id = ?1 AND state = 'awaiting_approval'",
                                params![
                                    tool_call_id.to_string(),
                                    approval_resolution_str(resolution),
                                    now,
                                ],
                            )
                            .map_err(|_| SessionRuntimeError::Persistence)?;
                    }
                    ApprovalDecision::Deny => {
                        transaction
                            .execute(
                                "UPDATE tool_calls
                                 SET state = 'denied', result = ?2, is_error = 1,
                                     approval_resolution = ?3, resolved_at_ms = ?4,
                                     finished_at_ms = ?4
                                 WHERE id = ?1 AND state = 'awaiting_approval'",
                                params![
                                    tool_call_id.to_string(),
                                    approval::USER_DENIED_RESULT,
                                    approval_resolution_str(resolution),
                                    now,
                                ],
                            )
                            .map_err(|_| SessionRuntimeError::Persistence)?;
                    }
                }
                if let ApprovalDecision::ApproveForSession { grant } = &decision {
                    let (kind, value) = match grant {
                        ApprovalGrant::Tool { name } => ("tool", name.trim()),
                        ApprovalGrant::ShellPrefix { prefix } => ("shell_prefix", prefix.trim()),
                    };
                    if value.is_empty() || value.len() > MAX_GRANT_BYTES {
                        return Err(SessionRuntimeError::InvalidApprovalGrant);
                    }
                    let grant_count: u32 = transaction
                        .query_row(
                            "SELECT COUNT(*) FROM session_grants WHERE session_id = ?1",
                            [session_id.to_string()],
                            |row| row.get(0),
                        )
                        .map_err(|_| SessionRuntimeError::Persistence)?;
                    if grant_count >= MAX_SESSION_GRANTS {
                        return Err(SessionRuntimeError::InvalidApprovalGrant);
                    }
                    transaction
                        .execute(
                            "INSERT OR IGNORE INTO session_grants(
                                 session_id, kind, value, created_at_ms
                             ) VALUES (?1, ?2, ?3, ?4)",
                            params![session_id.to_string(), kind, value, now],
                        )
                        .map_err(|_| SessionRuntimeError::Persistence)?;
                }
                let tool_call = load_tool_call(&transaction, tool_call_id)?;
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
                    SessionEvent::ToolApprovalResolved {
                        tool_call,
                        resolution,
                    },
                )?;
                (
                    CommandReceipt {
                        command_id,
                        committed_through: event.cursor,
                        outcome: CommandOutcome::ToolApprovalResolved {
                            tool_call_id,
                            resolution,
                        },
                    },
                    false,
                )
            }
        }
        SessionCommand::SetApprovalMode { session_id, mode } => {
            let workspace_id = session_workspace(&transaction, session_id)?;
            transaction
                .execute(
                    "UPDATE sessions SET approval_mode = ?2, updated_at_ms = ?3 WHERE id = ?1",
                    params![session_id.to_string(), approval_mode_str(mode), now],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            let sequence = workspace_sequence(&transaction, workspace_id)?;
            (
                CommandReceipt {
                    command_id,
                    committed_through: EventCursor {
                        store_id,
                        workspace_id,
                        sequence,
                    },
                    outcome: CommandOutcome::ApprovalModeSet { session_id, mode },
                },
                false,
            )
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
    let messages = load_model_context(&transaction, session_id, user_ordinal)?;
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
    ensure_context_capacity(&transaction, claimed.session_id, text.len())?;
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

fn persist_model_turn(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    turn_ordinal: u16,
    message: &Message,
    calls: &[RuntimeToolCall],
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
    if message.role() != Role::Assistant {
        return Err(SessionRuntimeError::Persistence);
    }
    let content = message
        .content()
        .iter()
        .map(PersistedContentBlock::from)
        .collect::<Vec<_>>();
    let content_json =
        serde_json::to_string(&content).map_err(|_| SessionRuntimeError::Persistence)?;
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let argument_bytes = calls.iter().fold(0_usize, |total, call| {
        total.saturating_add(call.arguments.len())
    });
    ensure_context_capacity(&transaction, claimed.session_id, argument_bytes)?;
    transaction
        .execute(
            "INSERT INTO model_turns(run_id, turn_ordinal, assistant_content_json)
             VALUES (?1, ?2, ?3)",
            params![claimed.run_id.to_string(), turn_ordinal, content_json],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let now = now_ms();
    let mut events = Vec::with_capacity(calls.len());
    for call in calls {
        transaction
            .execute(
                "INSERT INTO tool_calls(
                     id, run_id, turn_ordinal, call_ordinal, provider_call_id, name,
                     arguments_json, state, requested_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'requested', ?8)",
                params![
                    call.id.to_string(),
                    claimed.run_id.to_string(),
                    call.turn_ordinal,
                    call.call_ordinal,
                    call.provider_call_id,
                    call.name,
                    call.arguments,
                    now,
                ],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        let tool_call = load_tool_call(&transaction, call.id)?;
        events.push(append_event(
            &transaction,
            EventContext {
                store_id,
                workspace_id: claimed.workspace_id,
                session_id: claimed.session_id,
                run_id: Some(claimed.run_id),
                caused_by: Some(claimed.command_id),
                occurred_at_ms: now,
            },
            SessionEvent::ToolCallRequested { tool_call },
        )?);
    }
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(events)
}

fn start_tool_call(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    tool_call_id: ToolCallId,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let now = now_ms();
    let updated = transaction
        .execute(
            "UPDATE tool_calls SET state = 'running', started_at_ms = ?2
             WHERE id = ?1 AND run_id = ?3 AND state = 'requested'",
            params![tool_call_id.to_string(), now, claimed.run_id.to_string()],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if updated != 1 {
        return Err(SessionRuntimeError::Unavailable);
    }
    let tool_call = load_tool_call(&transaction, tool_call_id)?;
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
        SessionEvent::ToolCallStarted { tool_call },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(event)
}

fn finish_tool_call(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    tool_call_id: ToolCallId,
    result: String,
    is_error: bool,
    file_state: Option<FileStateUpdate>,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    ensure_context_capacity(&transaction, claimed.session_id, result.len())?;
    let now = now_ms();
    let state = if is_error { "failed" } else { "completed" };
    let updated = transaction
        .execute(
            "UPDATE tool_calls
             SET state = ?2, result = ?3, is_error = ?4, finished_at_ms = ?5
             WHERE id = ?1 AND run_id = ?6 AND state = 'running'",
            params![
                tool_call_id.to_string(),
                state,
                result,
                is_error,
                now,
                claimed.run_id.to_string(),
            ],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if updated != 1 {
        return Err(SessionRuntimeError::Unavailable);
    }
    if let Some(update) = file_state {
        record_session_file(&transaction, claimed.session_id, &update, now)?;
    }
    let tool_call = load_tool_call(&transaction, tool_call_id)?;
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
        SessionEvent::ToolCallFinished { tool_call },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(event)
}

/// Upserts one file-state entry, evicting the least-recently recorded paths
/// when the per-session bound is exceeded; an evicted file simply needs a
/// re-read before its next edit.
fn record_session_file(
    transaction: &Transaction<'_>,
    session_id: SessionId,
    update: &FileStateUpdate,
    now: u64,
) -> Result<(), SessionRuntimeError> {
    transaction
        .execute(
            "INSERT INTO session_files(session_id, path, content_hash, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session_id, path) DO UPDATE
             SET content_hash = excluded.content_hash,
                 updated_at_ms = excluded.updated_at_ms",
            params![session_id.to_string(), update.path, update.hash, now],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    transaction
        .execute(
            "DELETE FROM session_files
             WHERE session_id = ?1 AND rowid NOT IN (
                 SELECT rowid FROM session_files WHERE session_id = ?1
                 ORDER BY updated_at_ms DESC, rowid DESC LIMIT ?2
             )",
            params![session_id.to_string(), MAX_SESSION_FILES],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(())
}

fn load_approval_policy(
    connection: &mut Connection,
    session_id: SessionId,
) -> Result<(ApprovalMode, approval::SessionGrants), SessionRuntimeError> {
    let mode = connection
        .query_row(
            "SELECT approval_mode FROM sessions WHERE id = ?1",
            [session_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?
        .ok_or(SessionRuntimeError::SessionNotFound)?;
    let mode = parse_approval_mode(&mode)?;
    let mut statement = connection
        .prepare("SELECT kind, value FROM session_grants WHERE session_id = ?1")
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let rows = statement
        .query_map([session_id.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let mut grants = approval::SessionGrants::default();
    for (kind, value) in rows {
        match kind.as_str() {
            "tool" => {
                grants.tools.insert(value);
            }
            "shell_prefix" => grants.shell_prefixes.push(value),
            _ => return Err(SessionRuntimeError::Persistence),
        }
    }
    Ok((mode, grants))
}

fn deny_tool_call(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    tool_call_id: ToolCallId,
    message: &str,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let now = now_ms();
    let updated = transaction
        .execute(
            "UPDATE tool_calls
             SET state = 'denied', result = ?2, is_error = 1, finished_at_ms = ?3
             WHERE id = ?1 AND run_id = ?4 AND state = 'requested'",
            params![
                tool_call_id.to_string(),
                message,
                now,
                claimed.run_id.to_string(),
            ],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if updated != 1 {
        return Err(SessionRuntimeError::Unavailable);
    }
    let tool_call = load_tool_call(&transaction, tool_call_id)?;
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
        SessionEvent::ToolCallFinished { tool_call },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(event)
}

fn request_tool_approval(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    tool_call_id: ToolCallId,
    shell: Option<ShellCommandPreview>,
    edit: Option<EditPreview>,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let now = now_ms();
    let updated = transaction
        .execute(
            "UPDATE tool_calls SET state = 'awaiting_approval'
             WHERE id = ?1 AND run_id = ?2 AND state = 'requested'",
            params![tool_call_id.to_string(), claimed.run_id.to_string()],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if updated != 1 {
        return Err(SessionRuntimeError::Unavailable);
    }
    let tool_call = load_tool_call(&transaction, tool_call_id)?;
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
        SessionEvent::ToolApprovalRequested {
            tool_call,
            shell,
            edit,
        },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(event)
}

fn conclude_tool_approval(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    tool_call_id: ToolCallId,
    timed_out: bool,
) -> Result<ConcludedApproval, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let (state, resolution, result) = transaction
        .query_row(
            "SELECT state, approval_resolution, result FROM tool_calls
             WHERE id = ?1 AND run_id = ?2",
            params![tool_call_id.to_string(), claimed.run_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?
        .ok_or(SessionRuntimeError::ToolCallNotFound)?;
    if let Some(resolution) = resolution {
        // A client resolution won the race; its transaction already
        // persisted the state change and published the event.
        return match parse_approval_resolution(&resolution)? {
            ApprovalResolution::ApprovedOnce | ApprovalResolution::ApprovedForSession => {
                Ok(ConcludedApproval::Approved)
            }
            ApprovalResolution::Denied | ApprovalResolution::DeniedTimeout => {
                Ok(ConcludedApproval::Denied {
                    message: result.unwrap_or_else(|| approval::USER_DENIED_RESULT.to_owned()),
                    event: None,
                })
            }
        };
    }
    if !timed_out || state != "awaiting_approval" {
        return Ok(ConcludedApproval::StillWaiting);
    }
    let now = now_ms();
    transaction
        .execute(
            "UPDATE tool_calls
             SET state = 'denied', result = ?2, is_error = 1,
                 approval_resolution = 'denied_timeout', resolved_at_ms = ?3,
                 finished_at_ms = ?3
             WHERE id = ?1 AND state = 'awaiting_approval'",
            params![
                tool_call_id.to_string(),
                approval::TIMEOUT_DENIED_RESULT,
                now,
            ],
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let tool_call = load_tool_call(&transaction, tool_call_id)?;
    let event = append_event(
        &transaction,
        EventContext {
            store_id,
            workspace_id: claimed.workspace_id,
            session_id: claimed.session_id,
            run_id: Some(claimed.run_id),
            caused_by: None,
            occurred_at_ms: now,
        },
        SessionEvent::ToolApprovalResolved {
            tool_call,
            resolution: ApprovalResolution::DeniedTimeout,
        },
    )?;
    transaction
        .commit()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    Ok(ConcludedApproval::Denied {
        message: approval::TIMEOUT_DENIED_RESULT.to_owned(),
        event: Some(Box::new(event)),
    })
}

fn ensure_context_capacity(
    transaction: &Transaction<'_>,
    session_id: SessionId,
    additional: usize,
) -> Result<(), SessionRuntimeError> {
    let persisted_bytes: u64 = transaction
        .query_row(
            "SELECT
                 (SELECT COALESCE(SUM(
                     length(CAST(output AS BLOB)) + length(CAST(refusal AS BLOB))
                 ), 0) FROM messages WHERE session_id = ?1)
                 +
                 (SELECT COALESCE(SUM(
                     length(CAST(t.arguments_json AS BLOB))
                     + length(CAST(COALESCE(t.result, '') AS BLOB))
                 ), 0) FROM tool_calls t JOIN runs r ON r.id = t.run_id
                    WHERE r.session_id = ?1)",
            [session_id.to_string()],
            |row| row.get(0),
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    if usize::try_from(persisted_bytes)
        .unwrap_or(usize::MAX)
        .saturating_add(additional)
        > MAX_CONTEXT_BYTES
    {
        return Err(SessionRuntimeError::OutputTooLarge);
    }
    Ok(())
}

fn complete_run(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    outcome: RunOutcome,
    accounting: Option<RunAccounting>,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = connection
        .transaction()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let now = now_ms();
    let outcome = cancellation_wins(&transaction, claimed.run_id, outcome)?;
    interrupt_active_tool_calls(
        &transaction,
        store_id,
        claimed,
        Some(claimed.command_id),
        now,
    )?;
    let (run_status, message_state) = outcome_states(&outcome);
    let outcome_json =
        serde_json::to_string(&outcome).map_err(|_| SessionRuntimeError::Persistence)?;
    let usage = accounting.as_ref().and_then(|accounting| accounting.usage);
    let usage_json = usage
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let cost = accounting
        .as_ref()
        .and_then(|accounting| accounting.estimated_cost_usd_nanos)
        .and_then(|cost| i64::try_from(cost).ok());
    let (current_cost, current_cost_known) = transaction
        .query_row(
            "SELECT estimated_cost_usd_nanos, cost_known FROM sessions WHERE id = ?1",
            [claimed.session_id.to_string()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?)),
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let (next_cost, next_cost_known) = if accounting.is_some() {
        match cost.and_then(|cost| current_cost.checked_add(cost)) {
            Some(cost) if current_cost_known => (cost, true),
            _ => (current_cost, false),
        }
    } else {
        (current_cost, current_cost_known)
    };
    transaction
        .execute(
            "UPDATE runs
             SET status = ?2, outcome_json = ?3, finished_at_ms = ?4,
                 usage_json = ?5, estimated_cost_usd_nanos = ?6
             WHERE id = ?1 AND outcome_json IS NULL",
            params![
                claimed.run_id.to_string(),
                run_status,
                outcome_json,
                now,
                usage_json,
                cost
            ],
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
                  estimated_cost_usd_nanos = ?4,
                  cost_known = ?5,
                  updated_at_ms = ?2
             WHERE id = ?1 AND active_run_id = ?3",
            params![
                claimed.session_id.to_string(),
                now,
                claimed.run_id.to_string(),
                next_cost,
                next_cost_known,
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
            usage,
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
            usage: None,
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
    interrupt_active_tool_calls(transaction, store_id, claimed, None, now)?;
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
            usage: None,
        },
    )
}

fn interrupt_active_tool_calls(
    transaction: &Transaction<'_>,
    store_id: StoreId,
    claimed: &ClaimedRun,
    caused_by: Option<CommandId>,
    now: u64,
) -> Result<(), SessionRuntimeError> {
    let mut statement = transaction
        .prepare(
            "SELECT id FROM tool_calls
             WHERE run_id = ?1 AND state IN ('requested', 'awaiting_approval', 'running')
             ORDER BY turn_ordinal, call_ordinal",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let ids = statement
        .query_map([claimed.run_id.to_string()], |row| row.get::<_, String>(0))
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    drop(statement);
    for id in ids {
        let id = parse_id::<ToolCallId>(&id)?;
        transaction
            .execute(
                "UPDATE tool_calls
                 SET state = 'interrupted', result = ?2, is_error = 1, finished_at_ms = ?3
                 WHERE id = ?1 AND state IN ('requested', 'awaiting_approval', 'running')",
                params![id.to_string(), INTERRUPTED_TOOL_RESULT, now],
            )
            .map_err(|_| SessionRuntimeError::Persistence)?;
        let tool_call = load_tool_call(transaction, id)?;
        append_event(
            transaction,
            EventContext {
                store_id,
                workspace_id: claimed.workspace_id,
                session_id: claimed.session_id,
                run_id: Some(claimed.run_id),
                caused_by,
                occurred_at_ms: now,
            },
            SessionEvent::ToolCallFinished { tool_call },
        )?;
    }
    Ok(())
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
    let mut statement = transaction
        .prepare(
            "SELECT t.id FROM tool_calls t JOIN runs r ON r.id = t.run_id
             WHERE r.session_id = ?1
             ORDER BY r.created_at_ms DESC, t.turn_ordinal DESC, t.call_ordinal DESC
             LIMIT ?2",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let mut tool_call_ids = statement
        .query_map(
            params![
                session_id.to_string(),
                u64::try_from(MAX_SNAPSHOT_TOOL_CALLS + 1).expect("snapshot bound fits u64")
            ],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    drop(statement);
    let has_older_tool_calls = tool_call_ids.len() > MAX_SNAPSHOT_TOOL_CALLS;
    tool_call_ids.truncate(MAX_SNAPSHOT_TOOL_CALLS);
    tool_call_ids.reverse();
    let mut tool_calls = Vec::with_capacity(tool_call_ids.len());
    for id in tool_call_ids {
        tool_calls.push(load_tool_call(transaction, parse_id(&id)?)?);
    }
    Ok(SessionSnapshot {
        summary,
        messages,
        runs,
        tool_calls,
        has_older_tool_calls,
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
                     s.queued_prompts, s.model,
                     CASE WHEN s.cost_known = 1 THEN s.estimated_cost_usd_nanos END,
                     s.updated_at_ms,
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
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<u64>>(7)?,
                    row.get::<_, u64>(8)?,
                    row.get::<_, Option<String>>(9)?,
                ))
            },
        )
        .optional()
        .map_err(|_| SessionRuntimeError::Persistence)?
        .ok_or(SessionRuntimeError::SessionNotFound)
        .and_then(
            |(
                workspace,
                parent,
                title,
                status,
                active,
                queued,
                model,
                cost,
                updated,
                last_outcome,
            )| {
                Ok(SessionSummary {
                    id: session_id,
                    workspace_id: parse_id(&workspace)?,
                    parent_id: parent.as_deref().map(parse_id).transpose()?,
                    title,
                    status: parse_session_status(&status)?,
                    active_run_id: active.as_deref().map(parse_id).transpose()?,
                    queued_prompts: queued,
                    model,
                    estimated_cost_usd_nanos: cost,
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

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PersistedContentBlock {
    Text {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

impl From<&ContentBlock> for PersistedContentBlock {
    fn from(block: &ContentBlock) -> Self {
        match block {
            ContentBlock::Text { text } => Self::Text { text: text.clone() },
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => Self::ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            },
            ContentBlock::ToolResult {
                call_id,
                content,
                is_error,
            } => Self::ToolResult {
                call_id: call_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            },
        }
    }
}

impl From<PersistedContentBlock> for ContentBlock {
    fn from(block: PersistedContentBlock) -> Self {
        match block {
            PersistedContentBlock::Text { text } => Self::Text { text },
            PersistedContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => Self::ToolCall {
                id,
                name,
                arguments,
            },
            PersistedContentBlock::ToolResult {
                call_id,
                content,
                is_error,
            } => Self::ToolResult {
                call_id,
                content,
                is_error,
            },
        }
    }
}

fn load_model_context(
    transaction: &Transaction<'_>,
    session_id: SessionId,
    through_ordinal: u64,
) -> Result<Vec<Message>, SessionRuntimeError> {
    let mut statement = transaction
        .prepare(
            "SELECT id FROM messages
             WHERE session_id = ?1 AND ordinal <= ?2 AND state IN ('complete', 'interrupted')
             ORDER BY ordinal",
        )
        .map_err(|_| SessionRuntimeError::Persistence)?;
    let message_ids = statement
        .query_map(params![session_id.to_string(), through_ordinal], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|_| SessionRuntimeError::Persistence)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SessionRuntimeError::Persistence)?;
    drop(statement);

    let mut context = Vec::new();
    for id in message_ids {
        let snapshot = load_message(transaction, parse_id(&id)?)?;
        match snapshot.role {
            MessageRole::User => context.push(Message::user(snapshot.output)),
            MessageRole::Assistant => {
                let mut statement = transaction
                    .prepare(
                        "SELECT turn_ordinal, assistant_content_json FROM model_turns
                         WHERE run_id = ?1 ORDER BY turn_ordinal",
                    )
                    .map_err(|_| SessionRuntimeError::Persistence)?;
                let turns = statement
                    .query_map([snapshot.run_id.to_string()], |row| {
                        Ok((row.get::<_, u16>(0)?, row.get::<_, String>(1)?))
                    })
                    .map_err(|_| SessionRuntimeError::Persistence)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| SessionRuntimeError::Persistence)?;
                drop(statement);
                if turns.is_empty() {
                    let content = if snapshot.output.is_empty() {
                        snapshot.refusal
                    } else {
                        snapshot.output
                    };
                    if !content.trim().is_empty() {
                        context.push(Message::assistant(content));
                    }
                    continue;
                }
                for (turn_ordinal, content_json) in turns {
                    let content: Vec<ContentBlock> =
                        serde_json::from_str::<Vec<PersistedContentBlock>>(&content_json)
                            .map_err(|_| SessionRuntimeError::Persistence)?
                            .into_iter()
                            .map(ContentBlock::from)
                            .collect();

                    let mut statement = transaction
                        .prepare(
                            "SELECT provider_call_id, result, is_error FROM tool_calls
                             WHERE run_id = ?1 AND turn_ordinal = ?2 AND result IS NOT NULL
                             ORDER BY call_ordinal",
                        )
                        .map_err(|_| SessionRuntimeError::Persistence)?;
                    let mut recorded = statement
                        .query_map(params![snapshot.run_id.to_string(), turn_ordinal], |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                (row.get::<_, String>(1)?, row.get::<_, bool>(2)?),
                            ))
                        })
                        .map_err(|_| SessionRuntimeError::Persistence)?
                        .collect::<Result<HashMap<String, (String, bool)>, _>>()
                        .map_err(|_| SessionRuntimeError::Persistence)?;
                    drop(statement);
                    // Emit exactly one result per ToolCall block, in block order.
                    // A block without a recorded result (a crash between the
                    // turn commit and its tool_calls rows in an older store)
                    // gets an explicit interrupted result so replayed context
                    // stays provider-valid instead of poisoning the session.
                    let results = content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::ToolCall { id, .. } => Some(match recorded.remove(id) {
                                Some((content, is_error)) => ContentBlock::ToolResult {
                                    call_id: id.clone(),
                                    content,
                                    is_error,
                                },
                                None => ContentBlock::ToolResult {
                                    call_id: id.clone(),
                                    content: INTERRUPTED_TOOL_RESULT.to_owned(),
                                    is_error: true,
                                },
                            }),
                            ContentBlock::Text { .. } | ContentBlock::ToolResult { .. } => None,
                        })
                        .collect::<Vec<_>>();
                    context.push(Message::new(Role::Assistant, content));
                    if !results.is_empty() {
                        context.push(Message::tool_results(results));
                    }
                }
            }
        }
    }
    Ok(context)
}

fn load_tool_call(
    connection: &Connection,
    tool_call_id: ToolCallId,
) -> Result<ToolCallSnapshot, SessionRuntimeError> {
    connection
        .query_row(
            "SELECT r.session_id, t.run_id, t.turn_ordinal, t.call_ordinal,
                    t.provider_call_id, t.name, t.arguments_json, t.state, t.result, t.is_error
             FROM tool_calls t JOIN runs r ON r.id = t.run_id WHERE t.id = ?1",
            [tool_call_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u16>(2)?,
                    row.get::<_, u16>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, bool>(9)?,
                ))
            },
        )
        .map_err(|_| SessionRuntimeError::Persistence)
        .and_then(
            |(session, run, turn, call, provider_id, name, arguments, state, result, is_error)| {
                Ok(ToolCallSnapshot {
                    id: tool_call_id,
                    session_id: parse_id(&session)?,
                    run_id: parse_id(&run)?,
                    turn_ordinal: turn,
                    call_ordinal: call,
                    provider_call_id: provider_id,
                    name,
                    arguments,
                    state: parse_tool_call_state(&state)?,
                    result,
                    is_error,
                })
            },
        )
}

fn load_run(connection: &Connection, run_id: RunId) -> Result<RunSnapshot, SessionRuntimeError> {
    connection
        .query_row(
            "SELECT session_id, status, outcome_json, usage_json, estimated_cost_usd_nanos
             FROM runs WHERE id = ?1",
            [run_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<u64>>(4)?,
                ))
            },
        )
        .map_err(|_| SessionRuntimeError::Persistence)
        .and_then(|(session, status, outcome, usage, cost)| {
            Ok(RunSnapshot {
                id: run_id,
                session_id: parse_id(&session)?,
                status: parse_run_status(&status)?,
                outcome: outcome
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()
                    .map_err(|_| SessionRuntimeError::Persistence)?,
                usage: usage
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()
                    .map_err(|_| SessionRuntimeError::Persistence)?,
                estimated_cost_usd_nanos: cost,
            })
        })
}

fn run_cost(usage: TokenUsage, pricing: &ModelPricing) -> Option<u64> {
    let total_input = usage
        .input_tokens
        .checked_add(usage.cache_read_input_tokens)?
        .checked_add(usage.cache_write_input_tokens)?;
    let tier = pricing
        .context_tier
        .as_ref()
        .filter(|tier| total_input > tier.above_input_tokens);
    let input_rate = tier.map_or(pricing.input_usd_nanos_per_token, |tier| {
        tier.input_usd_nanos_per_token
    });
    let output_rate = tier.map_or(pricing.output_usd_nanos_per_token, |tier| {
        tier.output_usd_nanos_per_token
    });
    let cache_read_price = tier
        .and_then(|tier| tier.cache_read_usd_nanos_per_token)
        .or(pricing.cache_read_usd_nanos_per_token);
    let cache_write_price = tier
        .and_then(|tier| tier.cache_write_usd_nanos_per_token)
        .or(pricing.cache_write_usd_nanos_per_token);
    let cache_read_rate = if usage.cache_read_input_tokens == 0 {
        0
    } else {
        cache_read_price?
    };
    let cache_write_rate = if usage.cache_write_input_tokens == 0 {
        0
    } else {
        cache_write_price?
    };
    let total = u128::from(usage.input_tokens)
        .checked_mul(u128::from(input_rate))?
        .checked_add(u128::from(usage.output_tokens).checked_mul(u128::from(output_rate))?)?
        .checked_add(
            u128::from(usage.cache_read_input_tokens).checked_mul(u128::from(cache_read_rate))?,
        )?
        .checked_add(
            u128::from(usage.cache_write_input_tokens).checked_mul(u128::from(cache_write_rate))?,
        )?;
    u64::try_from(total).ok()
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

const fn approval_mode_str(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::ReadOnly => "read_only",
        ApprovalMode::Ask => "ask",
        ApprovalMode::Auto => "auto",
    }
}

fn parse_approval_mode(value: &str) -> Result<ApprovalMode, SessionRuntimeError> {
    match value {
        "read_only" => Ok(ApprovalMode::ReadOnly),
        "ask" => Ok(ApprovalMode::Ask),
        "auto" => Ok(ApprovalMode::Auto),
        _ => Err(SessionRuntimeError::Persistence),
    }
}

const fn approval_resolution_str(resolution: ApprovalResolution) -> &'static str {
    match resolution {
        ApprovalResolution::ApprovedOnce => "approved_once",
        ApprovalResolution::ApprovedForSession => "approved_for_session",
        ApprovalResolution::Denied => "denied",
        ApprovalResolution::DeniedTimeout => "denied_timeout",
    }
}

fn parse_approval_resolution(value: &str) -> Result<ApprovalResolution, SessionRuntimeError> {
    match value {
        "approved_once" => Ok(ApprovalResolution::ApprovedOnce),
        "approved_for_session" => Ok(ApprovalResolution::ApprovedForSession),
        "denied" => Ok(ApprovalResolution::Denied),
        "denied_timeout" => Ok(ApprovalResolution::DeniedTimeout),
        _ => Err(SessionRuntimeError::Persistence),
    }
}

fn parse_tool_call_state(value: &str) -> Result<ToolCallState, SessionRuntimeError> {
    match value {
        "requested" => Ok(ToolCallState::Requested),
        "awaiting_approval" => Ok(ToolCallState::AwaitingApproval),
        "running" => Ok(ToolCallState::Running),
        "completed" => Ok(ToolCallState::Completed),
        "failed" => Ok(ToolCallState::Failed),
        "denied" => Ok(ToolCallState::Denied),
        "interrupted" => Ok(ToolCallState::Interrupted),
        _ => Err(SessionRuntimeError::Persistence),
    }
}

fn prompt_title(prompt: &str) -> String {
    let mut title = String::new();
    let mut characters = 0;
    let mut pending_space = false;
    let mut truncated = false;
    for character in prompt.chars() {
        if character.is_whitespace() {
            pending_space = !title.is_empty();
            continue;
        }
        if character.is_control()
            || matches!(
                character,
                '\u{061c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
        {
            continue;
        }
        if pending_space {
            if characters + 1 >= 48 {
                truncated = true;
                break;
            }
            title.push(' ');
            characters += 1;
            pending_space = false;
        }
        if characters == 48 {
            truncated = true;
            break;
        }
        title.push(character);
        characters += 1;
    }
    if truncated {
        title.push_str("...");
    }
    if title.is_empty() {
        "New session".to_owned()
    } else {
        title
    }
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
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(runtime),
                        pricing: Some(ModelPricing {
                            input_usd_nanos_per_token: 1_000,
                            output_usd_nanos_per_token: 2_000,
                            cache_read_usd_nanos_per_token: Some(100),
                            cache_write_usd_nanos_per_token: Some(300),
                            context_tier: None,
                            provenance: "test".to_owned(),
                        }),
                    })
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
                Ok(qq_provider::ProviderEvent::Completed {
                    usage: Some(qq_provider::ProviderUsage {
                        input_tokens: 10,
                        cache_read_input_tokens: 2,
                        cache_write_input_tokens: 1,
                        output_tokens: 5,
                    }),
                }),
            ]))
        }
    }

    struct ChunkingLoader;

    impl RuntimeLoader for ChunkingLoader {
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            Box::pin(async {
                Runtime::new(ChunkingProvider, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(runtime),
                        pricing: None,
                    })
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
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
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
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(runtime),
                        pricing: None,
                    })
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

    struct ToolLoopLoader {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    impl RuntimeLoader for ToolLoopLoader {
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let requests = Arc::clone(&self.requests);
            Box::pin(async move {
                Runtime::new(ToolLoopProvider { requests }, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(runtime),
                        pricing: None,
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct ToolLoopProvider {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }

    impl Provider for ToolLoopProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut requests = self.requests.lock().unwrap();
            let turn = requests.len();
            requests.push(request);
            drop(requests);
            if turn == 0 {
                Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: "call_0".to_owned(),
                        name: "read_file".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: "call_0".to_owned(),
                        json: r#"{"path":"note.txt"}"#.to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                        id: "call_0".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed {
                        usage: Some(qq_provider::ProviderUsage {
                            input_tokens: 4,
                            cache_read_input_tokens: 0,
                            cache_write_input_tokens: 0,
                            output_tokens: 2,
                        }),
                    }),
                ]))
            } else {
                Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed {
                        usage: Some(qq_provider::ProviderUsage {
                            input_tokens: 6,
                            cache_read_input_tokens: 0,
                            cache_write_input_tokens: 0,
                            output_tokens: 1,
                        }),
                    }),
                ]))
            }
        }
    }

    impl Provider for DelayedProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            Box::pin(async_stream! {
                tokio::time::sleep(Duration::from_millis(20)).await;
                yield Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "answer".to_owned(),
                });
                yield Ok(qq_provider::ProviderEvent::Completed { usage: None });
            })
        }
    }

    /// Requests `tool` once per tool turn, then completes with text.
    struct ApprovalLoader {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        tool: &'static str,
        arguments: &'static str,
        tool_turns: usize,
    }

    impl RuntimeLoader for ApprovalLoader {
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let provider = ApprovalProvider {
                requests: Arc::clone(&self.requests),
                turn: StdMutex::new(0),
                tool: self.tool,
                arguments: self.arguments,
                tool_turns: self.tool_turns,
            };
            Box::pin(async move {
                Runtime::new(provider, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(runtime),
                        pricing: None,
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct ApprovalProvider {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        turn: StdMutex<usize>,
        tool: &'static str,
        arguments: &'static str,
        tool_turns: usize,
    }

    impl Provider for ApprovalProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            let mut current = self.turn.lock().unwrap();
            let turn = *current;
            *current += 1;
            drop(current);
            if turn < self.tool_turns {
                Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: format!("call_{turn}"),
                        name: self.tool.to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: format!("call_{turn}"),
                        json: self.arguments.to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                        id: format!("call_{turn}"),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                ]))
            } else {
                Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                ]))
            }
        }
    }

    /// Replays a fixed tool-call script per run: run N issues its scripted
    /// calls one per model turn, then completes with text.
    struct ScriptedRunsLoader {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        runs: Vec<Vec<(&'static str, String)>>,
        loads: StdMutex<usize>,
    }

    impl RuntimeLoader for ScriptedRunsLoader {
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let mut loads = self.loads.lock().unwrap();
            let script = self.runs.get(*loads).cloned().unwrap_or_default();
            *loads += 1;
            drop(loads);
            let provider = ScriptedRunProvider {
                requests: Arc::clone(&self.requests),
                script,
                turn: StdMutex::new(0),
            };
            Box::pin(async move {
                Runtime::new(provider, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(runtime),
                        pricing: None,
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct ScriptedRunProvider {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        script: Vec<(&'static str, String)>,
        turn: StdMutex<usize>,
    }

    impl Provider for ScriptedRunProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            let mut turn = self.turn.lock().unwrap();
            let current = *turn;
            *turn += 1;
            drop(turn);
            match self.script.get(current) {
                Some((name, arguments)) => Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: format!("call_{current}"),
                        name: (*name).to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: format!("call_{current}"),
                        json: arguments.clone(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                        id: format!("call_{current}"),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                ])),
                None => Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                ])),
            }
        }
    }

    struct ScriptedRunsHarness {
        _directory: TempDir,
        runtime: SessionRuntime,
        workspace_path: PathBuf,
        session_id: SessionId,
        events: SessionEventStream,
    }

    async fn scripted_runs_harness(
        mode: ApprovalMode,
        runs: Vec<Vec<(&'static str, String)>>,
    ) -> ScriptedRunsHarness {
        let directory = tempfile::tempdir().unwrap();
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(ScriptedRunsLoader {
                requests: Arc::new(StdMutex::new(Vec::new())),
                runs,
                loads: StdMutex::new(0),
            }),
        )
        .await
        .unwrap();
        let workspace_path = directory.path().to_owned();
        let (workspace_id, _) = resolve_workspace(&runtime, &workspace_path).await;
        let created = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: mode,
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        ScriptedRunsHarness {
            _directory: directory,
            runtime,
            workspace_path,
            session_id,
            events,
        }
    }

    async fn submit_prompt(harness: &ScriptedRunsHarness, prompt: &str) -> RunId {
        let queued = harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: harness.session_id,
                    prompt: prompt.to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
            panic!("unexpected receipt")
        };
        run_id
    }

    struct ApprovalHarness {
        _directory: TempDir,
        runtime: SessionRuntime,
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        run_id: RunId,
        events: SessionEventStream,
    }

    async fn approval_harness(
        mode: ApprovalMode,
        tool: &'static str,
        arguments: &'static str,
        tool_turns: usize,
        approval_timeout: Duration,
    ) -> ApprovalHarness {
        let directory = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions {
                database_path: directory.path().join("sessions.sqlite3"),
                max_active_runs: 1,
                approval_timeout,
            },
            Arc::new(ApprovalLoader {
                requests: Arc::clone(&requests),
                tool,
                arguments,
                tool_turns,
            }),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: mode,
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        let queued = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "mutate something".to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
            panic!("unexpected receipt")
        };
        ApprovalHarness {
            _directory: directory,
            runtime,
            requests,
            workspace_id,
            session_id,
            run_id,
            events,
        }
    }

    async fn collect_until_approval_requested(
        events: &mut SessionEventStream,
    ) -> (Vec<SessionEventEnvelope>, ToolCallSnapshot) {
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut observed = Vec::new();
            loop {
                let event = events.next().await.unwrap().unwrap();
                let requested = match &event.event {
                    SessionEvent::ToolApprovalRequested { tool_call, .. } => {
                        Some(tool_call.clone())
                    }
                    _ => None,
                };
                observed.push(event);
                if let Some(tool_call) = requested {
                    return (observed, tool_call);
                }
            }
        })
        .await
        .unwrap()
    }

    async fn respond_approval(
        runtime: &SessionRuntime,
        run_id: RunId,
        tool_call_id: ToolCallId,
        decision: ApprovalDecision,
    ) -> Result<CommandReceipt, SessionRuntimeError> {
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::RespondToolApproval {
                    run_id,
                    tool_call_id,
                    decision,
                },
            )
            .await
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

    #[test]
    fn version_one_migration_is_atomic_and_marks_historical_cost_unknown() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO metadata VALUES ('schema_version', '1');
                 CREATE TABLE workspaces (
                     id TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE,
                     next_sequence INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO workspaces VALUES ('workspace', '/workspace', 0);
                 CREATE TABLE sessions (
                     id TEXT PRIMARY KEY,
                     workspace_id TEXT NOT NULL REFERENCES workspaces(id),
                     parent_id TEXT REFERENCES sessions(id),
                     title TEXT NOT NULL, status TEXT NOT NULL, active_run_id TEXT,
                     queued_prompts INTEGER NOT NULL DEFAULT 0, model TEXT,
                     max_output_tokens INTEGER, organization TEXT,
                     created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
                 );
                 INSERT INTO sessions VALUES (
                     'old', 'workspace', NULL, 'Old', 'idle', NULL, 0,
                     'openai/gpt-test', 100, NULL, 1, 1
                 );
                 CREATE TABLE runs (
                     id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
                     command_id TEXT NOT NULL UNIQUE, user_message_id TEXT NOT NULL,
                     assistant_message_id TEXT NOT NULL, status TEXT NOT NULL,
                     cancel_requested INTEGER NOT NULL DEFAULT 0, outcome_json TEXT,
                     created_at_ms INTEGER NOT NULL, started_at_ms INTEGER,
                     finished_at_ms INTEGER
                 );",
            )
            .unwrap();
        drop(connection);

        let (connection, _) = open_database(&path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT value FROM metadata WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "5"
        );
        assert!(
            !connection
                .query_row(
                    "SELECT cost_known FROM sessions WHERE id = 'old'",
                    [],
                    |row| { row.get::<_, bool>(0) }
                )
                .unwrap()
        );
        assert!(has_column(&connection, "runs", "usage_json").unwrap());
        assert!(has_column(&connection, "tool_calls", "provider_call_id").unwrap());
        assert!(has_column(&connection, "model_turns", "assistant_content_json").unwrap());
        assert!(has_column(&connection, "tool_calls", "approval_resolution").unwrap());
        assert_eq!(
            connection
                .query_row(
                    "SELECT approval_mode FROM sessions WHERE id = 'old'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "ask"
        );
        assert!(has_column(&connection, "session_grants", "value").unwrap());
        assert!(has_column(&connection, "session_files", "content_hash").unwrap());
    }

    #[test]
    fn cost_uses_the_context_tier_and_cache_rates() {
        let pricing = ModelPricing {
            input_usd_nanos_per_token: 1,
            output_usd_nanos_per_token: 2,
            cache_read_usd_nanos_per_token: Some(1),
            cache_write_usd_nanos_per_token: Some(2),
            context_tier: Some(qq_protocol::ModelPricingTier {
                above_input_tokens: 10,
                input_usd_nanos_per_token: 10,
                output_usd_nanos_per_token: 20,
                cache_read_usd_nanos_per_token: Some(3),
                cache_write_usd_nanos_per_token: Some(4),
            }),
            provenance: "test".to_owned(),
        };
        assert_eq!(
            run_cost(
                TokenUsage {
                    input_tokens: 8,
                    cache_read_input_tokens: 2,
                    cache_write_input_tokens: 1,
                    output_tokens: 3,
                },
                &pricing,
            ),
            Some(8 * 10 + 2 * 3 + 4 + 3 * 20)
        );
    }

    #[test]
    fn prompt_titles_are_compact_and_bounded() {
        assert_eq!(
            prompt_title("  Fix the login\n\tredirect  "),
            "Fix the login redirect"
        );
        assert_eq!(
            prompt_title(&"x".repeat(49)),
            format!("{}...", "x".repeat(48))
        );
        assert_eq!(prompt_title("\0\u{1b}\u{202e}\u{2066}"), "New session");
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
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: ApprovalMode::default(),
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
    async fn only_the_first_prompt_names_a_session() {
        let (directory, runtime) = test_runtime().await;
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };

        for prompt in ["New session", "Do not replace the first title"] {
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

        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 1,
                message_limit: 4,
            })
            .await
            .unwrap();
        assert_eq!(snapshot.focused.unwrap().summary.title, "New session");
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
        assert!(matches!(
            &observed.last().unwrap().event,
            SessionEvent::RunFinished {
                usage: Some(TokenUsage {
                    input_tokens: 10,
                    cache_read_input_tokens: 2,
                    cache_write_input_tokens: 1,
                    output_tokens: 5,
                }),
                ..
            }
        ));
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
        assert_eq!(focused.summary.model.as_deref(), Some("test/model"));
        assert_eq!(focused.summary.estimated_cost_usd_nanos, Some(20_500));
        assert_eq!(
            focused.runs[0].usage,
            Some(TokenUsage {
                input_tokens: 10,
                cache_read_input_tokens: 2,
                cache_write_input_tokens: 1,
                output_tokens: 5,
            })
        );
        assert_eq!(focused.runs[0].estimated_cost_usd_nanos, Some(20_500));
        assert!(snapshot.cursor.sequence > initial.sequence);
    }

    #[tokio::test]
    async fn persists_tool_transitions_and_reconstructs_follow_up_context() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("note.txt"), "tool result\n").unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(ToolLoopLoader {
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
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "inspect the note".to_owned(),
                },
            )
            .await
            .unwrap();

        let observed = collect_through_finished(&mut events).await;
        let requested = observed
            .iter()
            .position(|event| matches!(event.event, SessionEvent::ToolCallRequested { .. }))
            .unwrap();
        let started = observed
            .iter()
            .position(|event| matches!(event.event, SessionEvent::ToolCallStarted { .. }))
            .unwrap();
        let finished = observed
            .iter()
            .position(|event| matches!(event.event, SessionEvent::ToolCallFinished { .. }))
            .unwrap();
        assert!(requested < started && started < finished);

        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 1,
                message_limit: 8,
            })
            .await
            .unwrap();
        let focused = snapshot.focused.unwrap();
        assert_eq!(focused.tool_calls.len(), 1);
        assert_eq!(focused.tool_calls[0].state, ToolCallState::Completed);
        assert_eq!(
            focused.tool_calls[0].result.as_deref(),
            Some("tool result\n")
        );
        assert_eq!(
            focused.runs[0].usage,
            Some(TokenUsage {
                input_tokens: 10,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                output_tokens: 3,
            })
        );

        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "what did you read?".to_owned(),
                },
            )
            .await
            .unwrap();
        let _ = collect_through_finished(&mut events).await;

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[2].messages().len(), 5);
        assert!(matches!(
            requests[2].messages()[1].content(),
            [ContentBlock::ToolCall { id, .. }] if id == "call_0"
        ));
        assert!(matches!(
            requests[2].messages()[2].content(),
            [ContentBlock::ToolResult { call_id, content, .. }]
                if call_id == "call_0" && content == "tool result\n"
        ));
    }

    #[tokio::test]
    async fn recovery_interrupts_running_tools_without_reexecuting_them() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let store = Store::open(database_path.clone()).await.unwrap();
        let resolved = store
            .command(
                CommandId::generate().unwrap(),
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
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: ApprovalMode::default(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
            panic!("unexpected receipt")
        };
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "read".to_owned(),
                },
            )
            .await
            .unwrap();
        let claimed = store.claim_next_run().await.unwrap().unwrap();
        let tool_call_id = ToolCallId::generate().unwrap();
        // A mutating call crashed mid-execution: replay must surface an
        // explicit interrupted result, never re-run the side effect.
        let call = RuntimeToolCall {
            id: tool_call_id,
            turn_ordinal: 1,
            call_ordinal: 1,
            provider_call_id: "provider-call".to_owned(),
            name: "edit_file".to_owned(),
            arguments: r#"{"path":"note.txt","old_string":"a","new_string":"b"}"#.to_owned(),
            argument_error: None,
        };
        store
            .persist_model_turn(
                &claimed,
                1,
                Message::new(
                    Role::Assistant,
                    vec![ContentBlock::ToolCall {
                        id: call.provider_call_id.clone(),
                        name: call.name.clone(),
                        arguments: serde_json::from_str(&call.arguments).unwrap(),
                    }],
                ),
                vec![call],
            )
            .await
            .unwrap();
        let started = store.start_tool_call(&claimed, tool_call_id).await.unwrap();
        drop(store);

        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(database_path),
            Arc::new(CapturingLoader {
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: started.cursor,
            })
            .unwrap();
        let recovered = collect_through_finished(&mut events).await;
        assert!(matches!(
            &recovered[0].event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.id == tool_call_id
                    && tool_call.state == ToolCallState::Interrupted
                    && tool_call.is_error
        ));
        assert!(matches!(
            &recovered[1].event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Interrupted,
                ..
            }
        ));
        let snapshot = runtime
            .snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: 1,
                message_limit: 4,
            })
            .await
            .unwrap();
        assert_eq!(
            snapshot.focused.unwrap().tool_calls[0].state,
            ToolCallState::Interrupted
        );

        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "continue".to_owned(),
                },
            )
            .await
            .unwrap();
        let _ = collect_through_finished(&mut events).await;

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(matches!(
            requests[0].messages()[2].content(),
            [ContentBlock::ToolResult {
                call_id,
                content,
                is_error: true,
            }] if call_id == "provider-call" && content == INTERRUPTED_TOOL_RESULT
        ));
    }

    #[tokio::test]
    async fn orphaned_tool_call_blocks_replay_with_synthesized_interrupted_results() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let store = Store::open(database_path.clone()).await.unwrap();
        let resolved = store
            .command(
                CommandId::generate().unwrap(),
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
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: ApprovalMode::default(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
            panic!("unexpected receipt")
        };
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "read".to_owned(),
                },
            )
            .await
            .unwrap();
        let claimed = store.claim_next_run().await.unwrap().unwrap();
        // Simulate the pre-fix crash window: the model turn committed with a
        // ToolCall block, but no tool_calls rows were ever written.
        store
            .persist_model_turn(
                &claimed,
                1,
                Message::new(
                    Role::Assistant,
                    vec![ContentBlock::ToolCall {
                        id: "orphan-call".to_owned(),
                        name: "read_file".to_owned(),
                        arguments: serde_json::json!({"path": "note.txt"}),
                    }],
                ),
                Vec::new(),
            )
            .await
            .unwrap();
        drop(store);

        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(database_path),
            Arc::new(CapturingLoader {
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.receipt.committed_through,
            })
            .unwrap();
        let _ = collect_through_finished(&mut events).await;
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "continue".to_owned(),
                },
            )
            .await
            .unwrap();
        let _ = collect_through_finished(&mut events).await;

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let messages = requests[0].messages();
        assert!(matches!(
            messages[1].content(),
            [ContentBlock::ToolCall { id, .. }] if id == "orphan-call"
        ));
        assert!(matches!(
            messages[2].content(),
            [ContentBlock::ToolResult {
                call_id,
                content,
                is_error: true,
            }] if call_id == "orphan-call" && content == INTERRUPTED_TOOL_RESULT
        ));
        // Provider validity: every ToolCall block must be answered by a
        // ToolResult with the same call ID in a later message.
        for (index, message) in messages.iter().enumerate() {
            for block in message.content() {
                if let ContentBlock::ToolCall { id, .. } = block {
                    assert!(messages[index + 1..].iter().any(|candidate| {
                        candidate.content().iter().any(|result| {
                            matches!(
                                result,
                                ContentBlock::ToolResult { call_id, .. } if call_id == id
                            )
                        })
                    }));
                }
            }
        }
    }

    struct ContextBudgetLoader;

    impl RuntimeLoader for ContextBudgetLoader {
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            Box::pin(async {
                Runtime::new(ContextBudgetProvider, "test-model", 256)
                    .map(|runtime| LoadedRuntime {
                        runtime: Arc::new(runtime),
                        pricing: None,
                    })
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    struct ContextBudgetProvider;

    impl Provider for ContextBudgetProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "x".repeat(MAX_CONTEXT_BYTES + 1),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ]))
        }
    }

    #[tokio::test]
    async fn exceeding_the_context_budget_fails_the_run_with_a_policy_outcome() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(ContextBudgetLoader),
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
                    prompt: "fill the context".to_owned(),
                },
            )
            .await
            .unwrap();

        let observed = collect_through_finished(&mut events).await;
        let finished = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::RunFinished { outcome, .. } => Some(outcome.clone()),
                _ => None,
            })
            .unwrap();
        assert!(matches!(
            finished,
            RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::Policy,
                    ref message,
                }
            } if message.contains("4 MiB limit")
        ));
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
                approval_timeout: DEFAULT_APPROVAL_TIMEOUT,
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
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: ApprovalMode::default(),
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
            .finish_run(&claimed, RunOutcome::Completed, None)
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
                approval_timeout: DEFAULT_APPROVAL_TIMEOUT,
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

    #[tokio::test]
    async fn approving_once_executes_the_tool_after_the_client_decides() {
        let mut harness = approval_harness(
            ApprovalMode::Ask,
            "__test_mutate",
            "{}",
            1,
            DEFAULT_APPROVAL_TIMEOUT,
        )
        .await;
        let (observed, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        assert!(
            observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolCallRequested { .. }))
        );
        assert_eq!(tool_call.state, ToolCallState::AwaitingApproval);

        let receipt = respond_approval(
            &harness.runtime,
            harness.run_id,
            tool_call.id,
            ApprovalDecision::ApproveOnce,
        )
        .await
        .unwrap();
        assert_eq!(
            receipt.outcome,
            CommandOutcome::ToolApprovalResolved {
                tool_call_id: tool_call.id,
                resolution: ApprovalResolution::ApprovedOnce,
            }
        );

        let observed = collect_through_finished(&mut harness.events).await;
        assert!(matches!(
            &observed[0].event,
            SessionEvent::ToolApprovalResolved {
                resolution: ApprovalResolution::ApprovedOnce,
                ..
            }
        ));
        assert!(
            observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolCallStarted { .. }))
        );
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.state == ToolCallState::Completed
                    && tool_call.result.as_deref() == Some("mutated")
        )));
        assert!(matches!(
            &observed.last().unwrap().event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        ));
        let requests = harness.requests.lock().unwrap();
        assert!(matches!(
            requests[1].messages()[2].content(),
            [ContentBlock::ToolResult {
                content,
                is_error: false,
                ..
            }] if content == "mutated"
        ));
    }

    #[tokio::test]
    async fn denial_returns_a_tool_error_and_the_run_still_completes() {
        let mut harness = approval_harness(
            ApprovalMode::Ask,
            "__test_mutate",
            "{}",
            1,
            DEFAULT_APPROVAL_TIMEOUT,
        )
        .await;
        let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        respond_approval(
            &harness.runtime,
            harness.run_id,
            tool_call.id,
            ApprovalDecision::Deny,
        )
        .await
        .unwrap();

        let observed = collect_through_finished(&mut harness.events).await;
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolApprovalResolved {
                tool_call,
                resolution: ApprovalResolution::Denied,
            } if tool_call.state == ToolCallState::Denied && tool_call.is_error
        )));
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolCallStarted { .. }))
        );
        assert!(matches!(
            &observed.last().unwrap().event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        ));
        let denied_result = {
            let requests = harness.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            match requests[1].messages()[2].content() {
                [
                    ContentBlock::ToolResult {
                        content,
                        is_error: true,
                        ..
                    },
                ] => content.clone(),
                other => panic!("unexpected tool result content {other:?}"),
            }
        };
        assert_eq!(denied_result, approval::USER_DENIED_RESULT);
        let snapshot = harness
            .runtime
            .snapshot(SnapshotRequest {
                workspace_id: harness.workspace_id,
                focused_session_id: Some(harness.session_id),
                session_limit: 1,
                message_limit: 8,
            })
            .await
            .unwrap();
        assert_eq!(
            snapshot.focused.unwrap().tool_calls[0].state,
            ToolCallState::Denied
        );
    }

    #[tokio::test]
    async fn responding_twice_returns_the_recorded_outcome_without_side_effects() {
        let mut harness = approval_harness(
            ApprovalMode::Ask,
            "__test_mutate",
            "{}",
            1,
            DEFAULT_APPROVAL_TIMEOUT,
        )
        .await;
        let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        respond_approval(
            &harness.runtime,
            harness.run_id,
            tool_call.id,
            ApprovalDecision::Deny,
        )
        .await
        .unwrap();
        let _ = collect_through_finished(&mut harness.events).await;

        // A retry with a different decision returns the recorded denial.
        let retry = respond_approval(
            &harness.runtime,
            harness.run_id,
            tool_call.id,
            ApprovalDecision::ApproveOnce,
        )
        .await
        .unwrap();
        assert_eq!(
            retry.outcome,
            CommandOutcome::ToolApprovalResolved {
                tool_call_id: tool_call.id,
                resolution: ApprovalResolution::Denied,
            }
        );
        let snapshot = harness
            .runtime
            .snapshot(SnapshotRequest {
                workspace_id: harness.workspace_id,
                focused_session_id: Some(harness.session_id),
                session_limit: 1,
                message_limit: 8,
            })
            .await
            .unwrap();
        assert_eq!(
            snapshot.focused.unwrap().tool_calls[0].state,
            ToolCallState::Denied
        );

        assert_eq!(
            respond_approval(
                &harness.runtime,
                harness.run_id,
                ToolCallId::generate().unwrap(),
                ApprovalDecision::ApproveOnce,
            )
            .await
            .unwrap_err(),
            SessionRuntimeError::ToolCallNotFound
        );
    }

    #[tokio::test]
    async fn unresolved_approvals_are_denied_by_timeout_with_a_distinct_error() {
        let mut harness = approval_harness(
            ApprovalMode::Ask,
            "__test_mutate",
            "{}",
            1,
            Duration::from_millis(50),
        )
        .await;
        let (_, _) = collect_until_approval_requested(&mut harness.events).await;
        let observed = collect_through_finished(&mut harness.events).await;
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolApprovalResolved {
                tool_call,
                resolution: ApprovalResolution::DeniedTimeout,
            } if tool_call.state == ToolCallState::Denied
        )));
        assert!(matches!(
            &observed.last().unwrap().event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        ));
        let requests = harness.requests.lock().unwrap();
        assert!(matches!(
            requests[1].messages()[2].content(),
            [ContentBlock::ToolResult {
                content,
                is_error: true,
                ..
            }] if content == approval::TIMEOUT_DENIED_RESULT
        ));
    }

    #[tokio::test]
    async fn approve_for_session_grants_cover_later_calls_without_prompting() {
        let mut harness = approval_harness(
            ApprovalMode::Ask,
            "__test_mutate",
            "{}",
            2,
            DEFAULT_APPROVAL_TIMEOUT,
        )
        .await;
        let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        respond_approval(
            &harness.runtime,
            harness.run_id,
            tool_call.id,
            ApprovalDecision::ApproveForSession {
                grant: ApprovalGrant::Tool {
                    name: "__test_mutate".to_owned(),
                },
            },
        )
        .await
        .unwrap();

        let observed = collect_through_finished(&mut harness.events).await;
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. })),
            "the session grant must cover the second call"
        );
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(
                    &event.event,
                    SessionEvent::ToolCallFinished { tool_call }
                        if tool_call.state == ToolCallState::Completed
                ))
                .count(),
            2
        );
        assert!(matches!(
            &observed.last().unwrap().event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn read_only_sessions_deny_mutating_tools_without_prompting() {
        let mut harness = approval_harness(
            ApprovalMode::ReadOnly,
            "__test_mutate",
            "{}",
            1,
            DEFAULT_APPROVAL_TIMEOUT,
        )
        .await;
        let observed = collect_through_finished(&mut harness.events).await;
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. }))
        );
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.state == ToolCallState::Denied
                    && tool_call.result.as_deref() == Some(approval::POLICY_DENIED_RESULT)
        )));
        assert!(matches!(
            &observed.last().unwrap().event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn shell_approval_requests_carry_the_command_and_auto_mode_asks_without_a_grant() {
        let mut harness = approval_harness(
            ApprovalMode::Auto,
            "__test_shell",
            r#"{"command":"cargo test --workspace","cwd":"crates"}"#,
            1,
            DEFAULT_APPROVAL_TIMEOUT,
        )
        .await;
        let (observed, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        let shell = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::ToolApprovalRequested { shell, .. } => shell.clone(),
                _ => None,
            })
            .expect("shell approval requests carry the command");
        assert_eq!(shell.command, "cargo test --workspace");
        assert_eq!(shell.cwd.as_deref(), Some("crates"));

        respond_approval(
            &harness.runtime,
            harness.run_id,
            tool_call.id,
            ApprovalDecision::ApproveForSession {
                grant: ApprovalGrant::ShellPrefix {
                    prefix: "cargo test".to_owned(),
                },
            },
        )
        .await
        .unwrap();
        let observed = collect_through_finished(&mut harness.events).await;
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.state == ToolCallState::Completed
        )));
    }

    #[tokio::test]
    async fn edit_approvals_carry_the_diff_preview_and_apply_after_approval() {
        let mut harness = scripted_runs_harness(
            ApprovalMode::Ask,
            vec![vec![
                ("read_file", r#"{"path":"note.txt"}"#.to_owned()),
                (
                    "edit_file",
                    r#"{"path":"note.txt","old_string":"hello world","new_string":"goodbye world"}"#
                        .to_owned(),
                ),
            ]],
        )
        .await;
        let note = harness.workspace_path.join("note.txt");
        std::fs::write(&note, "hello world\n").unwrap();
        let run_id = submit_prompt(&harness, "edit the note").await;

        let (observed, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        let edit = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::ToolApprovalRequested { edit, .. } => edit.clone(),
                _ => None,
            })
            .expect("edit approval requests carry the diff preview");
        assert_eq!(edit.path, "note.txt");
        assert_eq!(edit.diff, "- hello world\n+ goodbye world\n");
        assert_eq!(
            std::fs::read_to_string(&note).unwrap(),
            "hello world\n",
            "nothing may be applied before approval"
        );

        respond_approval(
            &harness.runtime,
            run_id,
            tool_call.id,
            ApprovalDecision::ApproveOnce,
        )
        .await
        .unwrap();
        let observed = collect_through_finished(&mut harness.events).await;
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.name == "edit_file" && tool_call.state == ToolCallState::Completed
        )));
        assert_eq!(std::fs::read_to_string(&note).unwrap(), "goodbye world\n");
    }

    #[tokio::test]
    async fn auto_mode_applies_edits_without_prompting_and_file_state_survives_runs() {
        let mut harness = scripted_runs_harness(
            ApprovalMode::Auto,
            vec![
                vec![("read_file", r#"{"path":"note.txt"}"#.to_owned())],
                vec![(
                    "edit_file",
                    r#"{"path":"note.txt","old_string":"hello","new_string":"goodbye"}"#.to_owned(),
                )],
            ],
        )
        .await;
        let note = harness.workspace_path.join("note.txt");
        std::fs::write(&note, "hello\n").unwrap();

        submit_prompt(&harness, "read the note").await;
        let first = collect_through_finished(&mut harness.events).await;
        assert!(first.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.name == "read_file" && tool_call.state == ToolCallState::Completed
        )));

        // The second run edits without re-reading: the read-before-write rule
        // is satisfied by the durable file-state map recorded by run one.
        submit_prompt(&harness, "now edit it").await;
        let second = collect_through_finished(&mut harness.events).await;
        assert!(
            !second
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. })),
            "auto mode must not prompt for workspace edits"
        );
        assert!(second.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.name == "edit_file" && tool_call.state == ToolCallState::Completed
        )));
        assert_eq!(std::fs::read_to_string(&note).unwrap(), "goodbye\n");
    }

    #[tokio::test]
    async fn auto_mode_executes_mutating_tools_after_a_mode_change() {
        let mut harness = approval_harness(
            ApprovalMode::ReadOnly,
            "__test_mutate",
            "{}",
            1,
            DEFAULT_APPROVAL_TIMEOUT,
        )
        .await;
        // The first run is denied by read-only policy.
        let _ = collect_through_finished(&mut harness.events).await;

        let receipt = harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SetApprovalMode {
                    session_id: harness.session_id,
                    mode: ApprovalMode::Auto,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            receipt.outcome,
            CommandOutcome::ApprovalModeSet {
                session_id: harness.session_id,
                mode: ApprovalMode::Auto,
            }
        );

        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: harness.session_id,
                    prompt: "mutate again".to_owned(),
                },
            )
            .await
            .unwrap();
        let observed = collect_through_finished(&mut harness.events).await;
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. }))
        );
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.state == ToolCallState::Completed
        )));
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_run_waiting_for_approval() {
        let mut harness = approval_harness(
            ApprovalMode::Ask,
            "__test_mutate",
            "{}",
            1,
            DEFAULT_APPROVAL_TIMEOUT,
        )
        .await;
        let (_, tool_call) = collect_until_approval_requested(&mut harness.events).await;
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CancelRun {
                    run_id: harness.run_id,
                },
            )
            .await
            .unwrap();
        let observed = collect_through_finished(&mut harness.events).await;
        assert!(observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::ToolCallFinished { tool_call: finished }
                if finished.id == tool_call.id
                    && finished.state == ToolCallState::Interrupted
        )));
        assert!(matches!(
            &observed.last().unwrap().event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Cancelled,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn recovery_marks_awaiting_approval_calls_interrupted() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("sessions.sqlite3");
        let store = Store::open(database_path.clone()).await.unwrap();
        let resolved = store
            .command(
                CommandId::generate().unwrap(),
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
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: ApprovalMode::Ask,
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
            panic!("unexpected receipt")
        };
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    prompt: "mutate".to_owned(),
                },
            )
            .await
            .unwrap();
        let claimed = store.claim_next_run().await.unwrap().unwrap();
        let tool_call_id = ToolCallId::generate().unwrap();
        let call = RuntimeToolCall {
            id: tool_call_id,
            turn_ordinal: 1,
            call_ordinal: 1,
            provider_call_id: "call_0".to_owned(),
            name: "__test_mutate".to_owned(),
            arguments: "{}".to_owned(),
            argument_error: None,
        };
        store
            .persist_model_turn(
                &claimed,
                1,
                Message::new(
                    Role::Assistant,
                    vec![ContentBlock::ToolCall {
                        id: call.provider_call_id.clone(),
                        name: call.name.clone(),
                        arguments: serde_json::from_str(&call.arguments).unwrap(),
                    }],
                ),
                vec![call],
            )
            .await
            .unwrap();
        let awaiting = store
            .request_tool_approval(&claimed, tool_call_id, None, None)
            .await
            .unwrap();
        drop(store);

        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(database_path),
            Arc::new(ScriptedLoader),
        )
        .await
        .unwrap();
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: awaiting.cursor,
            })
            .unwrap();
        let recovered = collect_through_finished(&mut events).await;
        assert!(matches!(
            &recovered[0].event,
            SessionEvent::ToolCallFinished { tool_call }
                if tool_call.id == tool_call_id
                    && tool_call.state == ToolCallState::Interrupted
                    && tool_call.is_error
        ));
        assert!(matches!(
            &recovered[1].event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Interrupted,
                ..
            }
        ));
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
