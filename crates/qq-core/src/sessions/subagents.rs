use super::runtime::SessionRuntimeInner;
use super::*;

/// Executes `spawn_agent` calls for one parent run: creates a read-only child
/// session and queued task atomically (while preserving the ordinary durable
/// event stream), then resolves with the child run's final assistant text.
pub(super) struct SessionSubagentSpawner {
    inner: Arc<SessionRuntimeInner>,
    parent: ClaimedRun,
    /// Bounds this run's children in flight; excess spawn calls in one turn
    /// wait here rather than erroring.
    slots: Arc<Semaphore>,
    /// Children this run has spawned so far, capped at
    /// [`MAX_SPAWNED_CHILDREN_PER_RUN`].
    spawned: Arc<AtomicUsize>,
}

impl SessionSubagentSpawner {
    pub(super) fn new(inner: Arc<SessionRuntimeInner>, parent: ClaimedRun) -> Self {
        Self {
            inner,
            parent,
            slots: Arc::new(Semaphore::new(MAX_CONCURRENT_CHILDREN_PER_RUN)),
            spawned: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl SubagentSpawner for SessionSubagentSpawner {
    fn spawn(&self, task: String, model: Option<String>) -> SpawnAgentFuture {
        let inner = Arc::clone(&self.inner);
        let parent = self.parent.clone();
        let slots = Arc::clone(&self.slots);
        let spawned = Arc::clone(&self.spawned);
        Box::pin(async move { spawn_child_run(inner, parent, slots, spawned, task, model).await })
    }
}

fn spawn_error(content: impl Into<String>) -> SpawnAgentOutcome {
    SpawnAgentOutcome {
        content: content.into(),
        is_error: true,
    }
}

/// One `spawn_agent` execution. Every child failure mode — command errors, a
/// failed, cancelled, or empty child run — resolves to an error *tool result*
/// for the parent's model, never a parent run failure.
pub(super) async fn spawn_child_run(
    inner: Arc<SessionRuntimeInner>,
    parent: ClaimedRun,
    slots: Arc<Semaphore>,
    spawned: Arc<AtomicUsize>,
    task: String,
    model: Option<String>,
) -> SpawnAgentOutcome {
    // The budget counts attempts, so a failing spawn also consumes it.
    if spawned.fetch_add(1, Ordering::AcqRel) >= MAX_SPAWNED_CHILDREN_PER_RUN {
        return spawn_error(format!(
            "this run already spawned {MAX_SPAWNED_CHILDREN_PER_RUN} sub-agents; \
             continue with what they returned"
        ));
    }
    let mut selection = parent.model.clone();
    if let Some(model) = model.and_then(|model| {
        let model = model.trim().to_owned();
        (!model.is_empty()).then_some(model)
    }) {
        selection.model = Some(model);
    } else {
        selection = match inner
            .loader
            .resolve_worker_model(parent.workspace.clone(), selection)
            .await
        {
            Ok(selection) => selection,
            Err(error) => {
                return spawn_error(format!(
                    "the sub-agent model could not be resolved: {}",
                    truncate_utf8(error.message, MAX_FAILURE_MESSAGE_BYTES)
                ));
            }
        };
    }
    // The spawn-time choke point: every resolved route — explicit argument,
    // configured worker, or parent fallback — must be authenticated and in
    // the served model list right now, before any durable child state exists.
    if let Err(error) = inner
        .loader
        .validate_spawn_model(parent.workspace.clone(), selection.clone())
        .await
    {
        return spawn_error(format!(
            "the sub-agent model was rejected: {}",
            truncate_utf8(error.message, MAX_FAILURE_MESSAGE_BYTES)
        ));
    }
    // Validate and construct the selected runtime before creating durable
    // child state. A bad explicit/configured route must leave no orphan
    // session (and this runtime will be reused from the loader cache when the
    // queued child is claimed).
    if let Err(error) = inner
        .loader
        .load(RuntimeLoadRequest {
            workspace: parent.workspace.clone(),
            model: selection.clone(),
        })
        .await
    {
        return spawn_error(format!(
            "the sub-agent model could not be loaded: {}",
            truncate_utf8(error.message, MAX_FAILURE_MESSAGE_BYTES)
        ));
    }
    // Wait for a child slot before creating anything: spawn calls queued
    // behind the concurrency cap must not pile up sessions they cannot run.
    let Ok(_slot) = Arc::clone(&slots).acquire_owned().await else {
        return spawn_error("the sub-agent scheduler is unavailable");
    };
    // Shutdown owns the write side of this gate. Hold the read side across
    // the durable child transaction so shutdown observes either no child run
    // or the fully queued run it must settle; child admission can never cross
    // the shutdown scan.
    let lifecycle = inner.lifecycle.read().await;
    if *inner.shutdown.borrow() || *inner.failed.borrow() {
        return spawn_error("the session runtime is shutting down");
    }
    let created = match inner.store.create_child_run(&parent, selection, task).await {
        Ok(created) => created,
        Err(error) => {
            return spawn_error(format!(
                "the sub-agent session and task could not be created: {error}"
            ));
        }
    };
    let CreatedChildRun {
        session_id: _session_id,
        run_id,
        committed_through,
    } = created;
    let mut guard = CancelChildOnDrop {
        inner: Arc::clone(&inner),
        run_id: Some(run_id),
    };
    inner.notify(committed_through);
    // The run cannot start until scheduling below, so subscribing after the
    // atomic commit and before that signal cannot miss its completion.
    let Ok(mut wakeup) = inner.subscribe(parent.workspace_id, committed_through.sequence) else {
        return spawn_error("the sub-agent could not be awaited");
    };
    drop(lifecycle);
    let _ = inner.schedule.try_send(());
    // Parent cancellation reaches the child by drop: the cancelled parent's
    // tool loop is dropped wholesale, dropping this future mid-await, and the
    // guard then cancels the still-running child run.
    let outcome = loop {
        match inner.store.run_outcome(run_id).await {
            Ok(Some(outcome)) => break outcome,
            Ok(None) => {}
            Err(error) => {
                guard.disarm();
                return spawn_error(format!("the sub-agent outcome could not be read: {error}"));
            }
        }
        if wakeup.changed().await.is_err() {
            guard.disarm();
            return spawn_error("the session runtime shut down while the sub-agent was running");
        }
    };
    guard.disarm();
    match outcome {
        RunOutcome::Completed => match inner.store.run_final_text(run_id).await {
            Ok(text) if text.trim().is_empty() => {
                spawn_error("the sub-agent completed without producing any text")
            }
            Ok(text) => SpawnAgentOutcome {
                content: text,
                is_error: false,
            },
            Err(error) => spawn_error(format!("the sub-agent answer could not be read: {error}")),
        },
        RunOutcome::Cancelled => spawn_error("the sub-agent run was cancelled"),
        RunOutcome::Interrupted => spawn_error("the sub-agent run was interrupted"),
        RunOutcome::Failed { failure } => {
            spawn_error(format!("the sub-agent run failed: {}", failure.message))
        }
    }
}

/// Cancels a still-running child run when the spawn future awaiting it is
/// dropped before the child finished.
struct CancelChildOnDrop {
    inner: Arc<SessionRuntimeInner>,
    run_id: Option<RunId>,
}

impl CancelChildOnDrop {
    fn disarm(&mut self) {
        self.run_id = None;
    }
}

impl Drop for CancelChildOnDrop {
    fn drop(&mut self) {
        let Some(run_id) = self.run_id.take() else {
            return;
        };
        let inner = Arc::clone(&self.inner);
        // Drop cannot await; outside a runtime (process teardown) there is
        // nothing left to cancel for.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            let Ok(command_id) = CommandId::generate() else {
                return;
            };
            let Ok(applied) = inner
                .store
                .command_with_seed(
                    command_id,
                    SessionCommand::CancelRun { run_id },
                    WorkspaceGrantSeed::default(),
                    None,
                )
                .await
            else {
                return;
            };
            inner.notify(applied.receipt.committed_through);
            inner.cancel(run_id);
            if applied.schedule {
                let _ = inner.schedule.try_send(());
            }
        });
    }
}
