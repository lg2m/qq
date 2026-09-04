use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use crossbeam_channel::{Sender, TrySendError};
use rusqlite::Connection;
use tokio::sync::{Semaphore, mpsc, oneshot, watch};

use super::*;
use feed::WorkspaceFeed;
use worker::WorkerMessage;

mod schema;
mod worker;

#[cfg(test)]
pub(super) use schema::{has_column, open_database};

const CONTROL_QUEUE_CAPACITY: usize = 256;
const OUTPUT_QUEUE_CAPACITY: usize = 1024;
const CONTROL_BURST_LIMIT: usize = 4;

#[cfg(test)]
struct CommittedCommandHook {
    command_id: CommandId,
    entered: oneshot::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static COMMITTED_COMMAND_HOOK: Mutex<Option<CommittedCommandHook>> = Mutex::new(None);

#[cfg(test)]
static CANCELLATION_READ_FAILURES: Mutex<Vec<(RunId, SessionRuntimeError)>> =
    Mutex::new(Vec::new());

#[cfg(test)]
static RESERVED_SETTLEMENT_FAILURES: Mutex<Vec<(RunId, SessionRuntimeError)>> =
    Mutex::new(Vec::new());

#[cfg(test)]
static PREPARED_SETTLEMENT_FAILURES: Mutex<Vec<(RunId, SessionRuntimeError)>> =
    Mutex::new(Vec::new());

#[cfg(test)]
static RESERVED_RELOAD_FAILURES: Mutex<Vec<(RunId, SessionRuntimeError)>> = Mutex::new(Vec::new());

#[cfg(test)]
static RESERVED_START_FAILURES: Mutex<Vec<(RunId, SessionRuntimeError)>> = Mutex::new(Vec::new());

#[cfg(test)]
struct CancellationReadHook {
    run_id: RunId,
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

#[cfg(test)]
static CANCELLATION_READ_HOOKS: Mutex<Vec<CancellationReadHook>> = Mutex::new(Vec::new());

#[cfg(test)]
struct ReservedStartOverloadHook {
    run_id: RunId,
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

#[cfg(test)]
static RESERVED_START_OVERLOAD_HOOKS: Mutex<Vec<ReservedStartOverloadHook>> =
    Mutex::new(Vec::new());

#[cfg(test)]
pub(super) fn fail_cancellation_reads(
    run_id: RunId,
    failures: impl IntoIterator<Item = SessionRuntimeError>,
) {
    CANCELLATION_READ_FAILURES
        .lock()
        .unwrap()
        .extend(failures.into_iter().map(|failure| (run_id, failure)));
}

#[cfg(test)]
pub(super) fn fail_reserved_settlements(
    run_id: RunId,
    failures: impl IntoIterator<Item = SessionRuntimeError>,
) {
    RESERVED_SETTLEMENT_FAILURES
        .lock()
        .unwrap()
        .extend(failures.into_iter().map(|failure| (run_id, failure)));
}

#[cfg(test)]
pub(super) fn fail_prepared_settlements(
    run_id: RunId,
    failures: impl IntoIterator<Item = SessionRuntimeError>,
) {
    PREPARED_SETTLEMENT_FAILURES
        .lock()
        .unwrap()
        .extend(failures.into_iter().map(|failure| (run_id, failure)));
}

#[cfg(test)]
pub(super) fn fail_reserved_reloads(
    run_id: RunId,
    failures: impl IntoIterator<Item = SessionRuntimeError>,
) {
    RESERVED_RELOAD_FAILURES
        .lock()
        .unwrap()
        .extend(failures.into_iter().map(|failure| (run_id, failure)));
}

#[cfg(test)]
pub(super) fn fail_reserved_starts(
    run_id: RunId,
    failures: impl IntoIterator<Item = SessionRuntimeError>,
) {
    RESERVED_START_FAILURES
        .lock()
        .unwrap()
        .extend(failures.into_iter().map(|failure| (run_id, failure)));
}

#[cfg(test)]
pub(super) fn hold_cancellation_read(
    run_id: RunId,
) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (entered, entered_rx) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    CANCELLATION_READ_HOOKS
        .lock()
        .unwrap()
        .push(CancellationReadHook {
            run_id,
            entered,
            release: release_rx,
        });
    (entered_rx, release)
}

#[cfg(test)]
pub(super) fn hold_overloaded_reserved_start(
    run_id: RunId,
) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (entered, entered_rx) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    RESERVED_START_OVERLOAD_HOOKS
        .lock()
        .unwrap()
        .push(ReservedStartOverloadHook {
            run_id,
            entered,
            release: release_rx,
        });
    (entered_rx, release)
}

#[cfg(test)]
fn take_cancellation_read_hook(run_id: RunId) -> Option<CancellationReadHook> {
    let mut hooks = CANCELLATION_READ_HOOKS.lock().unwrap();
    let index = hooks.iter().position(|hook| hook.run_id == run_id)?;
    Some(hooks.remove(index))
}

#[cfg(test)]
fn take_reserved_start_overload_hook(run_id: RunId) -> Option<ReservedStartOverloadHook> {
    let mut hooks = RESERVED_START_OVERLOAD_HOOKS.lock().unwrap();
    let index = hooks.iter().position(|hook| hook.run_id == run_id)?;
    Some(hooks.remove(index))
}

#[cfg(test)]
fn take_targeted_failure(
    failures: &Mutex<Vec<(RunId, SessionRuntimeError)>>,
    run_id: RunId,
) -> Option<SessionRuntimeError> {
    let mut failures = failures.lock().unwrap();
    let index = failures.iter().position(|(target, _)| *target == run_id)?;
    Some(failures.remove(index).1)
}

#[cfg(test)]
pub(super) fn hold_committed_command(
    command_id: CommandId,
) -> (oneshot::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (entered, entered_rx) = oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    *COMMITTED_COMMAND_HOOK.lock().unwrap() = Some(CommittedCommandHook {
        command_id,
        entered,
        release: release_rx,
    });
    (entered_rx, release)
}

#[cfg(test)]
fn pause_after_committed_command(command_id: CommandId) {
    let hook = {
        let mut hook = COMMITTED_COMMAND_HOOK.lock().unwrap();
        if hook
            .as_ref()
            .is_some_and(|hook| hook.command_id == command_id)
        {
            hook.take()
        } else {
            None
        }
    };
    if let Some(hook) = hook {
        let _ = hook.entered.send(());
        let _ = hook.release.recv();
    }
}

#[derive(Clone)]
pub(super) struct Store {
    inner: Arc<StoreInner>,
    store_id: StoreId,
}

struct StoreInner {
    control: Sender<WorkerMessage>,
    output: Sender<WorkerMessage>,
    output_slots: Arc<Semaphore>,
    /// Live subscribers, fed by the worker after each committing job.
    feed: Arc<WorkspaceFeed>,
    /// Subscriber catch-up pages read, for tests that bound them.
    #[cfg(test)]
    catch_up_reads: std::sync::atomic::AtomicU64,
    shutdown: Sender<()>,
    closed: watch::Receiver<bool>,
    closing: AtomicBool,
    admission: Mutex<()>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

#[derive(Clone, Copy)]
pub(super) enum Priority {
    Control,
    Output,
}

impl Drop for StoreInner {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::Release);
        self.output_slots.close();
        let _ = self.shutdown.try_send(());
    }
}

impl Store {
    pub(super) async fn open(path: PathBuf) -> Result<Self, SessionRuntimeError> {
        let started = worker::start(path)?;
        let store_id = started
            .ready
            .await
            .map_err(|_| SessionRuntimeError::Unavailable)??;
        Ok(Self {
            inner: Arc::new(StoreInner {
                control: started.control,
                output: started.output,
                output_slots: Arc::new(Semaphore::new(OUTPUT_QUEUE_CAPACITY)),
                feed: Arc::new(WorkspaceFeed::default()),
                #[cfg(test)]
                catch_up_reads: std::sync::atomic::AtomicU64::new(0),
                shutdown: started.shutdown,
                closed: started.closed,
                closing: AtomicBool::new(false),
                admission: Mutex::new(()),
                worker: Mutex::new(Some(started.worker)),
            }),
            store_id,
        })
    }

    pub(super) const fn store_id(&self) -> StoreId {
        self.store_id
    }

    #[cfg(test)]
    pub(super) fn catch_up_reads(&self) -> u64 {
        self.inner.catch_up_reads.load(Ordering::Relaxed)
    }

    /// A live receiver for one workspace's committed events. Delivers only
    /// events published after this call; the caller catches up from
    /// `events_after` first.
    pub(super) fn feed(
        &self,
        workspace_id: WorkspaceId,
    ) -> Option<tokio::sync::broadcast::Receiver<Arc<feed::PublishedEvent>>> {
        self.inner.feed.subscribe(workspace_id)
    }

    pub(super) async fn call<T, F>(
        &self,
        priority: Priority,
        operation: F,
    ) -> Result<T, SessionRuntimeError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, SessionRuntimeError> + Send + 'static,
    {
        if self.inner.closing.load(Ordering::Acquire) {
            return Err(SessionRuntimeError::Unavailable);
        }
        let output_permit = match priority {
            Priority::Control => None,
            Priority::Output => Some(
                Arc::clone(&self.inner.output_slots)
                    .acquire_owned()
                    .await
                    .map_err(|_| SessionRuntimeError::Unavailable)?,
            ),
        };
        let (reply, response) = oneshot::channel();
        let feed = Arc::clone(&self.inner.feed);
        let message = WorkerMessage::Run {
            job: Box::new(move |connection| {
                let result = operation(connection);
                // Persist, then publish, then acknowledge. A failed operation
                // rolled back, so what it staged never reaches a subscriber.
                let staged = feed::take_staged();
                if result.is_ok() {
                    feed.publish(staged);
                }
                let _ = reply.send(result);
            }),
            _output_permit: output_permit,
        };
        let sender = match priority {
            Priority::Control => &self.inner.control,
            Priority::Output => &self.inner.output,
        };
        // Enqueue and close share this short, synchronous admission boundary.
        // If this call wins, the worker must drain it; if close wins, this
        // call cannot enter a queue after the shutdown signal.
        let admission = self
            .inner
            .admission
            .lock()
            .map_err(|_| SessionRuntimeError::Unavailable)?;
        if self.inner.closing.load(Ordering::Acquire) {
            return Err(SessionRuntimeError::Unavailable);
        }
        match sender.try_send(message) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(SessionRuntimeError::Overloaded),
            Err(TrySendError::Disconnected(_)) => {
                return Err(SessionRuntimeError::Unavailable);
            }
        }
        drop(admission);
        response
            .await
            .map_err(|_| SessionRuntimeError::Unavailable)?
    }

    /// Stops the database worker without synchronously joining it from an
    /// async executor thread. Runtime settlement remains a separate operation;
    /// after final close, every store call fails as unavailable.
    pub(super) async fn close(&self) -> Result<(), SessionRuntimeError> {
        {
            let _admission = self
                .inner
                .admission
                .lock()
                .map_err(|_| SessionRuntimeError::Unavailable)?;
            self.inner.closing.store(true, Ordering::Release);
            self.inner.output_slots.close();
            let _ = self.inner.shutdown.try_send(());
        }
        let mut closed = self.inner.closed.clone();
        tokio::time::timeout(SHUTDOWN_GRACE, async {
            while !*closed.borrow() {
                closed
                    .changed()
                    .await
                    .map_err(|_| SessionRuntimeError::Unavailable)?;
            }
            Ok::<(), SessionRuntimeError>(())
        })
        .await
        .map_err(|_| SessionRuntimeError::ShutdownTimedOut)??;
        let worker = self
            .inner
            .worker
            .lock()
            .map_err(|_| SessionRuntimeError::Unavailable)?
            .take();
        if let Some(worker) = worker {
            tokio::task::spawn_blocking(move || worker.join())
                .await
                .map_err(|_| SessionRuntimeError::Unavailable)?
                .map_err(|_| SessionRuntimeError::Unavailable)?;
        }
        Ok(())
    }

    pub(super) async fn recover_interrupted_runs(
        &self,
    ) -> Result<Vec<EventCursor>, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call(Priority::Control, move |connection| {
            recover_interrupted_runs(connection, store_id)
        })
        .await
    }

    pub(super) async fn unfinished_run_ids(&self) -> Result<Vec<RunId>, SessionRuntimeError> {
        self.call(Priority::Control, |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT id FROM runs
                     WHERE status IN ('queued', 'running')
                     ORDER BY created_at_ms, id",
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|_| SessionRuntimeError::Persistence)?
                .map(|row| {
                    let run = row.map_err(|_| SessionRuntimeError::Persistence)?;
                    parse_id(&run)
                })
                .collect()
        })
        .await
    }

    /// Applies one command with no config-grant seed; only CreateSession
    /// consults the seed, so this shorthand keeps non-seeding tests direct.
    #[cfg(test)]
    pub(super) async fn command(
        &self,
        command_id: CommandId,
        command: SessionCommand,
    ) -> Result<AppliedCommand, SessionRuntimeError> {
        self.command_with_seed(command_id, command, WorkspaceGrantSeed::default(), None)
            .await
    }

    pub(super) async fn command_with_seed(
        &self,
        command_id: CommandId,
        command: SessionCommand,
        seed: WorkspaceGrantSeed,
        promotion_wakeup: Option<mpsc::Sender<()>>,
    ) -> Result<AppliedCommand, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call(Priority::Control, move |connection| {
            let applied = execute_command(connection, store_id, command_id, command, &seed)?;
            if applied.grant_promotion_pending
                && let Some(wakeup) = promotion_wakeup
            {
                match wakeup.try_send(()) {
                    Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {}
                    // The outbox row remains authoritative and will be
                    // recovered when a healthy runtime next opens the store.
                    Err(mpsc::error::TrySendError::Closed(())) => {}
                }
            }
            #[cfg(test)]
            pause_after_committed_command(command_id);
            Ok(applied)
        })
        .await
    }

    pub(super) async fn create_child_run(
        &self,
        parent: &ClaimedRun,
        call_id: ToolCallId,
        admission: ChildAdmission,
    ) -> Result<CreatedChildRun, SessionRuntimeError> {
        let store_id = self.store_id;
        let parent = ChildRunParent {
            workspace_id: parent.workspace_id,
            session_id: parent.session_id,
            run_id: parent.run_id,
            tool_call_id: Some(call_id),
            depth: parent.depth,
            root_run_id: parent.root_run_id,
        };
        self.call(Priority::Control, move |connection| {
            create_child_run(connection, store_id, parent, admission)
        })
        .await
    }

    pub(super) async fn workspace_path(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<String, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            connection
                .query_row(
                    "SELECT path FROM workspaces WHERE id = ?1",
                    [workspace_id.to_string()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|_| SessionRuntimeError::Persistence)?
                .ok_or(SessionRuntimeError::WorkspaceNotFound)
        })
        .await
    }

    /// Returns the oldest accepted workspace promotion without removing it.
    /// SQLite is the queue; the runtime channel only coalesces wakeups.
    pub(super) async fn next_grant_promotion(
        &self,
    ) -> Result<Option<PendingGrantPromotion>, SessionRuntimeError> {
        self.call(Priority::Output, next_grant_promotion).await
    }

    /// Persists the fate of one approve-for-workspace promotion and removes
    /// its outbox row in the same transaction. A missing row means another
    /// runtime already settled the idempotent external write.
    pub(super) async fn settle_grant_promotion(
        &self,
        promotion: PendingGrantPromotion,
        outcome: WorkspaceGrantOutcome,
    ) -> Result<Option<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call(Priority::Output, move |connection| {
            settle_grant_promotion(connection, store_id, &promotion, outcome)
        })
        .await
    }

    pub(super) async fn snapshot(
        &self,
        request: SnapshotRequest,
    ) -> Result<WorkspaceSnapshot, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call(Priority::Control, move |connection| {
            load_snapshot(connection, store_id, request)
        })
        .await
    }

    #[cfg(test)]
    pub(super) async fn events_after(
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

    pub(super) async fn published_events_after(
        &self,
        workspace_id: WorkspaceId,
        sequence: u64,
        limit: u16,
    ) -> Result<Vec<Arc<feed::PublishedEvent>>, SessionRuntimeError> {
        #[cfg(test)]
        self.inner.catch_up_reads.fetch_add(1, Ordering::Relaxed);
        self.call(Priority::Control, move |connection| {
            read_published_events(connection, workspace_id, sequence, limit)
        })
        .await
    }

    /// Reserves the next queued run whose session sits at exactly `depth`
    /// (0 = roots). The scheduler reserves per depth so each permit pool
    /// stays independent.
    pub(super) async fn reserve_next_run_at_depth(
        &self,
        depth: u16,
    ) -> Result<Option<ClaimedRun>, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call(Priority::Control, move |connection| {
            reserve_next_run(connection, store_id, depth)
        })
        .await
    }

    /// Test convenience: roots, or depth-one children.
    #[cfg(test)]
    pub(super) async fn reserve_next_run(
        &self,
        children: bool,
    ) -> Result<Option<ClaimedRun>, SessionRuntimeError> {
        self.reserve_next_run_at_depth(u16::from(children)).await
    }

    #[cfg(test)]
    pub(super) async fn claim_next_run(
        &self,
        children: bool,
    ) -> Result<Option<ClaimedRun>, SessionRuntimeError> {
        let Some(claimed) = self.reserve_next_run(children).await? else {
            return Ok(None);
        };
        let audit = test_prepared_audit(&claimed);
        if self.start_reserved_run(&claimed, audit).await?.is_none() {
            return Err(SessionRuntimeError::Persistence);
        }
        Ok(Some(claimed))
    }

    pub(super) async fn cancellation_requested(
        &self,
        run_id: RunId,
    ) -> Result<bool, SessionRuntimeError> {
        #[cfg(test)]
        if let Some(hook) = take_cancellation_read_hook(run_id) {
            let _ = hook.entered.send(());
            let _ = hook.release.await;
        }
        #[cfg(test)]
        if let Some(failure) = take_targeted_failure(&CANCELLATION_READ_FAILURES, run_id) {
            return Err(failure);
        }
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

    pub(super) async fn start_reserved_run(
        &self,
        claimed: &ClaimedRun,
        audit: PreparedRunAudit,
    ) -> Result<Option<SessionEventEnvelope>, SessionRuntimeError> {
        #[cfg(test)]
        if let Some(hook) = take_reserved_start_overload_hook(claimed.run_id) {
            let _ = hook.entered.send(());
            let _ = hook.release.await;
            return Err(SessionRuntimeError::Overloaded);
        }
        #[cfg(test)]
        if let Some(failure) = take_targeted_failure(&RESERVED_START_FAILURES, claimed.run_id) {
            return Err(failure);
        }
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Control, move |connection| {
            start_reserved_run(connection, store_id, &claimed, &audit)
        })
        .await
    }

    pub(super) async fn start_auto_compaction(
        &self,
        original: &ClaimedRun,
        audit: PreparedRunAudit,
    ) -> Result<Option<(ClaimedRun, SessionEventEnvelope)>, SessionRuntimeError> {
        let store_id = self.store_id;
        let original = original.clone();
        self.call(Priority::Control, move |connection| {
            start_auto_compaction(connection, store_id, &original, &audit)
        })
        .await
    }

    pub(super) async fn load_auto_compaction_messages(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<Message>, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            load_auto_compaction_messages(connection, session_id)
        })
        .await
    }

    pub(super) async fn reload_reserved_messages(
        &self,
        claimed: &ClaimedRun,
    ) -> Result<Option<(Vec<Message>, bool)>, SessionRuntimeError> {
        #[cfg(test)]
        if let Some(failure) = take_targeted_failure(&RESERVED_RELOAD_FAILURES, claimed.run_id) {
            return Err(failure);
        }
        let claimed = claimed.clone();
        self.call(Priority::Control, move |connection| {
            reload_reserved_messages(connection, &claimed)
        })
        .await
    }

    /// Full-transcript recall for `search_history`, on the control lane so a
    /// saturated output queue cannot starve a running tool call.
    pub(super) async fn search_history(
        &self,
        session_id: SessionId,
        calling_run: RunId,
        query: String,
        limit: usize,
    ) -> Result<Vec<HistoryMatch>, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            search_session_history(&transaction, session_id, calling_run, &query, limit)
        })
        .await
    }

    pub(super) async fn compaction_committed(
        &self,
        session_id: SessionId,
        run_id: RunId,
    ) -> Result<bool, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            connection
                .query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM session_compactions
                         WHERE session_id = ?1 AND run_id = ?2
                     )",
                    params![session_id.to_string(), run_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|_| SessionRuntimeError::Persistence)
        })
        .await
    }

    pub(super) async fn finish_reserved_run(
        &self,
        claimed: &ClaimedRun,
        outcome: RunOutcome,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        #[cfg(test)]
        if let Some(failure) = take_targeted_failure(&RESERVED_SETTLEMENT_FAILURES, claimed.run_id)
        {
            return Err(failure);
        }
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Control, move |connection| {
            finish_reserved_run(connection, store_id, &claimed, outcome)
        })
        .await
    }

    pub(super) async fn finish_prepared_run(
        &self,
        claimed: &ClaimedRun,
        audit: PreparedRunAudit,
        outcome: RunOutcome,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        #[cfg(test)]
        if let Some(failure) = take_targeted_failure(&PREPARED_SETTLEMENT_FAILURES, claimed.run_id)
        {
            return Err(failure);
        }
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Control, move |connection| {
            finish_prepared_run(connection, store_id, &claimed, &audit, outcome)
        })
        .await
    }

    pub(super) async fn settle_panicked_execution(
        &self,
        original: &ClaimedRun,
        outcome: RunOutcome,
    ) -> Result<PanickedExecutionSettlement, SessionRuntimeError> {
        let store_id = self.store_id;
        let original = original.clone();
        self.call(Priority::Control, move |connection| {
            settle_panicked_execution(connection, store_id, &original, outcome)
        })
        .await
    }

    /// Creates the current turn's assistant message with its first text chunk
    /// in one transaction, emitting `AssistantMessageStarted` then
    /// `TextAppended`.
    pub(super) async fn begin_assistant_message(
        &self,
        claimed: &ClaimedRun,
        message_id: MessageId,
        turn_ordinal: u16,
        channel: TextChannel,
        text: String,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            begin_assistant_message(
                connection,
                store_id,
                &claimed,
                message_id,
                turn_ordinal,
                channel,
                &text,
            )
        })
        .await
    }

    pub(super) async fn append_text(
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
    /// The turn's assistant message (when the turn streamed text) is finalized
    /// in the same transaction so no crash window can leave a committed turn
    /// with a message still marked streaming.
    /// The turn's cumulative usage, cost, and reported context occupancy ride
    /// the same transaction. This keeps live budgets and run snapshots on the
    /// same committed boundary as the model work they measure.
    pub(super) async fn persist_model_turn(
        &self,
        claimed: &ClaimedRun,
        turn: ModelTurnCommit,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            persist_model_turn(connection, store_id, &claimed, &turn)
        })
        .await
    }

    pub(super) async fn append_run_activity(
        &self,
        claimed: &ClaimedRun,
        activity: RunActivity,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            append_run_activity(connection, store_id, &claimed, activity)
        })
        .await
    }

    /// Commits the exact system-prompt identity before the runtime is polled
    /// far enough to begin provider work.
    pub(super) async fn record_prompt_identity(
        &self,
        claimed: &ClaimedRun,
        identity: Arc<RunPromptIdentity>,
    ) -> Result<(), SessionRuntimeError> {
        let claimed = claimed.clone();
        let identity = serde_json::to_string(identity.as_ref())
            .map_err(|_| SessionRuntimeError::Persistence)?;
        self.call(Priority::Control, move |connection| {
            let changed = connection
                .execute(
                    "UPDATE runs
                     SET prompt_identity_json = ?3
                     WHERE id = ?1 AND session_id = ?2 AND status = 'running'
                       AND prompt_identity_json IS NULL",
                    params![
                        claimed.run_id.to_string(),
                        claimed.session_id.to_string(),
                        identity,
                    ],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            if changed != 1 {
                return Err(SessionRuntimeError::Persistence);
            }
            Ok(())
        })
        .await
    }

    pub(super) async fn apply_steering(
        &self,
        claimed: &ClaimedRun,
        message_id: MessageId,
        turn_ordinal: u16,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            apply_steering_message(connection, store_id, &claimed, message_id, turn_ordinal)
        })
        .await
    }

    pub(super) async fn record_interrupted(
        &self,
        claimed: &ClaimedRun,
        turn_ordinal: u16,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            record_run_interrupted(connection, store_id, &claimed, turn_ordinal)
        })
        .await
    }

    /// Records that the runtime is continuing past a turn the provider cut at
    /// its output token limit. The counter and the event commit together.
    pub(super) async fn record_output_truncated(
        &self,
        claimed: &ClaimedRun,
        turn_ordinal: u16,
        continuation: u16,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            record_run_output_truncated(connection, store_id, &claimed, turn_ordinal, continuation)
        })
        .await
    }

    /// The steering messages of a run that are recorded but not yet applied,
    /// with their provider-visible text, in order. Used once when the run
    /// loop starts so steering that arrived between claim and start is not
    /// stranded.
    pub(super) async fn pending_steering(
        &self,
        run_id: RunId,
    ) -> Result<Vec<crate::runtime::SteeringMessage>, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT id, output FROM messages
                     WHERE run_id = ?1 AND steering = 1 AND state = 'queued' ORDER BY ordinal",
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            let rows = statement
                .query_map([run_id.to_string()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|_| SessionRuntimeError::Persistence)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            rows.into_iter()
                .map(|(id, text)| {
                    Ok(crate::runtime::SteeringMessage {
                        message_id: parse_id(&id)?,
                        text,
                    })
                })
                .collect()
        })
        .await
    }

    pub(super) async fn append_reasoning(
        &self,
        claimed: &ClaimedRun,
        reasoning: ReasoningEvent,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            append_reasoning(connection, store_id, &claimed, reasoning)
        })
        .await
    }

    pub(super) async fn start_tool_call(
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

    /// Publishes one batched chunk of live tool output. Chunks are
    /// display/replay events only: they never touch the tool_call row, whose
    /// bounded result is persisted by `finish_tool_call`.
    pub(super) async fn append_tool_output(
        &self,
        claimed: &ClaimedRun,
        tool_call_id: ToolCallId,
        chunk: String,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            append_tool_call_output(connection, store_id, &claimed, tool_call_id, chunk)
        })
        .await
    }

    pub(super) async fn finish_tool_call(
        &self,
        claimed: &ClaimedRun,
        tool_call_id: ToolCallId,
        result: String,
        is_error: bool,
        file_state: Option<FileStateUpdate>,
        display: Option<ToolCallDisplay>,
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
                display,
            )
        })
        .await
    }

    pub(super) async fn session_file_state(
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

    pub(super) async fn approval_policy(
        &self,
        session_id: SessionId,
    ) -> Result<(ApprovalMode, approval::SessionGrants), SessionRuntimeError> {
        self.call(Priority::Output, move |connection| {
            load_approval_policy(connection, session_id)
        })
        .await
    }

    pub(super) async fn deny_tool_call(
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

    pub(super) async fn request_tool_approval(
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

    pub(super) async fn conclude_tool_approval(
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

    pub(super) async fn resolve_approval_by_reviewer(
        &self,
        claimed: &ClaimedRun,
        tool_call_id: ToolCallId,
    ) -> Result<Option<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            resolve_approval_by_reviewer(connection, store_id, &claimed, tool_call_id)
        })
        .await
    }

    /// Persists the final-answer audit record on the run and publishes it.
    pub(super) async fn record_audit(
        &self,
        claimed: &ClaimedRun,
        record: AuditRecord,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            record_run_audit(connection, store_id, &claimed, record)
        })
        .await
    }

    /// Settles a held `Supervised` call as denied by the reviewer.
    pub(super) async fn deny_approval_by_reviewer(
        &self,
        claimed: &ClaimedRun,
        tool_call_id: ToolCallId,
        message: String,
    ) -> Result<Option<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            deny_approval_by_reviewer(connection, store_id, &claimed, tool_call_id, &message)
        })
        .await
    }

    /// The bounded run context a review request carries.
    pub(super) async fn review_context(
        &self,
        claimed: &ClaimedRun,
    ) -> Result<(Option<String>, Vec<RecentAction>), SessionRuntimeError> {
        let claimed = claimed.clone();
        self.call(Priority::Control, move |connection| {
            load_review_context(connection, &claimed)
        })
        .await
    }

    pub(super) async fn finish_run(
        &self,
        claimed: &ClaimedRun,
        outcome: RunOutcome,
        accounting: Option<RunAccounting>,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            complete_run(connection, store_id, &claimed, outcome, accounting)
        })
        .await
    }

    /// Atomically commits a compaction summary with its cutoff marker and
    /// settles the internal run, publishing `RunFinished` and
    /// `SessionCompacted` from the same transaction.
    pub(super) async fn finish_compaction_run(
        &self,
        claimed: &ClaimedRun,
        summary: String,
        accounting: Option<RunAccounting>,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            complete_compaction(connection, store_id, &claimed, summary, accounting)
        })
        .await
    }

    /// The terminal outcome of one run, if it has reached one. Polled by
    /// spawn futures awaiting their child run.
    /// A settled run's outcome and estimated cost. The cost is `None` until
    /// the run settles and stays `None` when spend was unmeasurable.
    /// A settled run's outcome with the spend it is accountable for. Usage
    /// and cost are `None` when unknown, never zero.
    pub(super) async fn run_outcome(
        &self,
        run_id: RunId,
    ) -> Result<Option<(RunOutcome, SpawnAgentSpend)>, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            let (outcome, usage, cost) = connection
                .query_row(
                    "SELECT outcome_json, usage_json, estimated_cost_usd_nanos FROM runs WHERE id = ?1",
                    [run_id.to_string()],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, Option<u64>>(2)?,
                        ))
                    },
                )
                .optional()
                .map_err(|_| SessionRuntimeError::Persistence)?
                .ok_or(SessionRuntimeError::RunNotFound)?;
            let usage = usage
                .as_deref()
                .map(serde_json::from_str::<TokenUsage>)
                .transpose()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            outcome
                .as_deref()
                .map(|encoded| {
                    serde_json::from_str(encoded)
                        .map(|outcome| {
                            (
                                outcome,
                                SpawnAgentSpend {
                                    cost_usd_nanos: cost,
                                    usage,
                                },
                            )
                        })
                        .map_err(|_| SessionRuntimeError::Persistence)
                })
                .transpose()
        })
        .await
    }

    /// The final committed model turn's text/refusal. Earlier assistant turns
    /// remain in the child transcript but never enter the parent's tool
    /// result.
    pub(super) async fn run_final_text(
        &self,
        run_id: RunId,
    ) -> Result<String, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            let final_turn_message = connection
                .query_row(
                    "SELECT m.id
                     FROM model_turns t
                     LEFT JOIN messages m
                       ON m.run_id = t.run_id
                      AND m.turn_ordinal = t.turn_ordinal
                      AND m.role = 'assistant'
                      AND m.state = 'complete'
                     WHERE t.run_id = ?1
                     ORDER BY t.turn_ordinal DESC, m.ordinal DESC
                     LIMIT 1",
                    [run_id.to_string()],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            let message_id = match final_turn_message {
                Some(message_id) => message_id,
                None => connection
                    .query_row(
                        "SELECT id FROM messages
                         WHERE run_id = ?1 AND role = 'assistant' AND state = 'complete'
                         ORDER BY turn_ordinal DESC, ordinal DESC LIMIT 1",
                        [run_id.to_string()],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(|_| SessionRuntimeError::Persistence)?,
            };
            let Some(message_id) = message_id else {
                return Ok(String::new());
            };
            let message = load_message(connection, parse_id(&message_id)?)?;
            let output = message.output;
            let refusal = message.refusal;
            if output.is_empty() {
                return Ok(refusal);
            }
            if refusal.is_empty() {
                return Ok(output);
            }
            Ok(format!("{output}\n{refusal}"))
        })
        .await
    }

    #[cfg(test)]
    pub(super) fn stop_worker_for_test(&self) -> Option<std::thread::JoinHandle<()>> {
        self.inner.closing.store(true, Ordering::Release);
        self.inner.output_slots.close();
        let _ = self.inner.shutdown.try_send(());
        self.inner.worker.lock().ok()?.take()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    fn control_message(job: impl FnOnce(&mut Connection) + Send + 'static) -> WorkerMessage {
        WorkerMessage::Run {
            job: Box::new(job),
            _output_permit: None,
        }
    }

    #[tokio::test]
    async fn saturated_control_queue_reports_overload() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocked_store = store.clone();
        let blocked = tokio::spawn(async move {
            blocked_store
                .call(Priority::Control, move |_| {
                    let _ = entered_tx.send(());
                    release_rx
                        .recv()
                        .map_err(|_| SessionRuntimeError::Unavailable)
                })
                .await
        });
        entered_rx.await.unwrap();

        for _ in 0..CONTROL_QUEUE_CAPACITY {
            store
                .inner
                .control
                .try_send(control_message(|_| {}))
                .unwrap();
        }
        let error = store.call(Priority::Control, |_| Ok(())).await.unwrap_err();
        assert_eq!(error, SessionRuntimeError::Overloaded);

        release_tx.send(()).unwrap();
        blocked.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn saturated_output_submission_waits_for_a_capacity_wake() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocked_store = store.clone();
        let blocked = tokio::spawn(async move {
            blocked_store
                .call(Priority::Control, move |_| {
                    let _ = entered_tx.send(());
                    release_rx
                        .recv()
                        .map_err(|_| SessionRuntimeError::Unavailable)
                })
                .await
        });
        entered_rx.await.unwrap();

        for _ in 0..OUTPUT_QUEUE_CAPACITY {
            let permit = Arc::clone(&store.inner.output_slots)
                .try_acquire_owned()
                .unwrap();
            store
                .inner
                .output
                .try_send(WorkerMessage::Run {
                    job: Box::new(|_| {}),
                    _output_permit: Some(permit),
                })
                .unwrap();
        }
        let output_store = store.clone();
        let output =
            tokio::spawn(async move { output_store.call(Priority::Output, |_| Ok(7)).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!output.is_finished());

        release_tx.send(()).unwrap();

        blocked.await.unwrap().unwrap();
        assert_eq!(output.await.unwrap().unwrap(), 7);
    }

    #[tokio::test]
    async fn queued_output_receives_service_after_a_bounded_control_burst() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocked_store = store.clone();
        let blocked = tokio::spawn(async move {
            blocked_store
                .call(Priority::Control, move |_| {
                    let _ = entered_tx.send(());
                    release_rx
                        .recv()
                        .map_err(|_| SessionRuntimeError::Unavailable)
                })
                .await
        });
        entered_rx.await.unwrap();

        let controls = Arc::new(AtomicUsize::new(0));
        for _ in 0..CONTROL_BURST_LIMIT * 3 {
            let controls = Arc::clone(&controls);
            store
                .inner
                .control
                .try_send(control_message(move |_| {
                    controls.fetch_add(1, Ordering::SeqCst);
                }))
                .unwrap();
        }
        let permit = Arc::clone(&store.inner.output_slots)
            .try_acquire_owned()
            .unwrap();
        let (observed_tx, observed_rx) = oneshot::channel();
        let output_controls = Arc::clone(&controls);
        store
            .inner
            .output
            .try_send(WorkerMessage::Run {
                job: Box::new(move |_| {
                    let _ = observed_tx.send(output_controls.load(Ordering::SeqCst));
                }),
                _output_permit: Some(permit),
            })
            .unwrap();

        release_tx.send(()).unwrap();
        let observed = tokio::time::timeout(Duration::from_secs(1), observed_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(observed <= CONTROL_BURST_LIMIT);
        blocked.await.unwrap().unwrap();
        store.call(Priority::Control, |_| Ok(())).await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn final_close_waits_without_blocking_the_async_executor() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocked_store = store.clone();
        let blocked = tokio::spawn(async move {
            blocked_store
                .call(Priority::Control, move |_| {
                    let _ = entered_tx.send(());
                    release_rx
                        .recv()
                        .map_err(|_| SessionRuntimeError::Unavailable)
                })
                .await
        });
        entered_rx.await.unwrap();

        let closing_store = store.clone();
        let close = tokio::spawn(async move { closing_store.close().await });
        tokio::task::yield_now().await;
        assert!(!close.is_finished());
        release_tx.send(()).unwrap();

        blocked.await.unwrap().unwrap();
        close.await.unwrap().unwrap();
        assert_eq!(
            store.call(Priority::Control, |_| Ok(())).await.unwrap_err(),
            SessionRuntimeError::Unavailable
        );
    }

    #[tokio::test]
    async fn final_close_drains_every_job_accepted_before_admission_closed() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocked_store = store.clone();
        let blocked = tokio::spawn(async move {
            blocked_store
                .call(Priority::Control, move |_| {
                    let _ = entered_tx.send(());
                    release_rx
                        .recv()
                        .map_err(|_| SessionRuntimeError::Unavailable)
                })
                .await
        });
        entered_rx.await.unwrap();
        let (accepted_tx, accepted_rx) = oneshot::channel();
        let accepted_store = store.clone();
        let accepted = tokio::spawn(async move {
            accepted_store
                .call(Priority::Control, move |_| {
                    let _ = accepted_tx.send(());
                    Ok(())
                })
                .await
        });
        while store.inner.control.is_empty() {
            tokio::task::yield_now().await;
        }

        let closing_store = store.clone();
        let close = tokio::spawn(async move { closing_store.close().await });
        tokio::task::yield_now().await;
        release_tx.send(()).unwrap();

        blocked.await.unwrap().unwrap();
        accepted.await.unwrap().unwrap();
        close.await.unwrap().unwrap();
        accepted_rx
            .await
            .expect("an accepted job must execute before close returns");
    }

    #[tokio::test]
    async fn final_close_drains_every_output_call_accepted_before_admission_closed() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocked_store = store.clone();
        let blocked = tokio::spawn(async move {
            blocked_store
                .call(Priority::Control, move |_| {
                    let _ = entered_tx.send(());
                    release_rx
                        .recv()
                        .map_err(|_| SessionRuntimeError::Unavailable)
                })
                .await
        });
        entered_rx.await.unwrap();

        let (accepted_tx, accepted_rx) = oneshot::channel();
        let accepted_store = store.clone();
        let accepted = tokio::spawn(async move {
            accepted_store
                .call(Priority::Output, move |_| {
                    let _ = accepted_tx.send(());
                    Ok(())
                })
                .await
        });
        while store.inner.output.is_empty() {
            tokio::task::yield_now().await;
        }

        let closing_store = store.clone();
        let close = tokio::spawn(async move { closing_store.close().await });
        tokio::task::yield_now().await;
        release_tx.send(()).unwrap();

        blocked.await.unwrap().unwrap();
        accepted.await.unwrap().unwrap();
        close.await.unwrap().unwrap();
        accepted_rx
            .await
            .expect("an accepted output call must execute before close returns");
        assert_eq!(
            store.call(Priority::Output, |_| Ok(())).await.unwrap_err(),
            SessionRuntimeError::Unavailable
        );
    }
}
