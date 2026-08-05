use super::*;
use super::{
    execution::{execute_run, finish_run, internal_failure},
    runtime::SessionRuntimeInner,
};

struct SchedulerStopGuard(watch::Sender<bool>);

impl Drop for SchedulerStopGuard {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

pub(super) async fn schedule_runs(
    inner: std::sync::Weak<SessionRuntimeInner>,
    mut receiver: mpsc::Receiver<()>,
    mut shutdown: watch::Receiver<bool>,
    stopped: watch::Sender<bool>,
) {
    let _stopped = SchedulerStopGuard(stopped);
    'scheduler: loop {
        let scheduled = tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break 'scheduler;
                }
                continue;
            }
            scheduled = receiver.recv() => scheduled,
        };
        if scheduled.is_none() {
            break;
        }
        let Some(inner) = inner.upgrade() else {
            break;
        };
        if *shutdown.borrow() || *inner.failed.borrow() {
            break;
        }
        // Root runs and child (sub-agent) runs are claimed from separate
        // queues against separate permit pools; see `child_permits` for why
        // sharing one pool would deadlock parents awaiting their children.
        for children in [false, true] {
            let pool = if children {
                &inner.child_permits
            } else {
                &inner.permits
            };
            loop {
                if *shutdown.borrow() {
                    break 'scheduler;
                }
                let permit = match Arc::clone(pool).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => break,
                };
                let claimed = match inner.store.claim_next_run(children).await {
                    Ok(Some(claimed)) => claimed,
                    Ok(None) => break,
                    Err(_) => {
                        inner.failed.send_replace(true);
                        break 'scheduler;
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
                        break 'scheduler;
                    }
                }
                let task_inner = Arc::clone(&inner);
                let panic_claimed = claimed.clone();
                tokio::spawn(async move {
                    let execution = AssertUnwindSafe(execute_run(
                        Arc::clone(&task_inner),
                        claimed,
                        cancel_receiver,
                    ))
                    .catch_unwind()
                    .await;
                    if execution.is_err() {
                        finish_run(
                            &task_inner,
                            &panic_claimed,
                            internal_failure(
                                "agent run task panicked; committed work was preserved",
                            ),
                        )
                        .await;
                    }
                    drop(permit);
                    if !*task_inner.shutdown.borrow() {
                        let _ = task_inner.schedule.try_send(());
                    }
                });
            }
        }
    }
}
