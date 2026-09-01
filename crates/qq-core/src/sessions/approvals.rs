use super::runtime::SessionRuntimeInner;
use super::*;

/// Applies the session's approval policy to each requested tool call,
/// persisting approval state before publishing it and holding the run open
/// while a client decides.
pub(super) struct SessionToolGate {
    inner: Arc<SessionRuntimeInner>,
    claimed: ClaimedRun,
    cancellation: watch::Receiver<bool>,
}

impl SessionToolGate {
    pub(super) fn new(
        inner: Arc<SessionRuntimeInner>,
        claimed: ClaimedRun,
        cancellation: watch::Receiver<bool>,
    ) -> Self {
        Self {
            inner,
            claimed,
            cancellation,
        }
    }
}

impl ToolGate for SessionToolGate {
    fn resolve(&self, call: &RuntimeToolCall) -> ToolGateFuture {
        let inner = Arc::clone(&self.inner);
        let claimed = self.claimed.clone();
        let call = call.clone();
        let mut cancellation = self.cancellation.clone();
        Box::pin(async move {
            let (mode, grants) = match inner.store.approval_policy(claimed.session_id).await {
                Ok(policy) => policy,
                Err(error) => return approval_persistence_failure(error),
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
                        Err(error) => approval_persistence_failure(error),
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
                        .request_tool_approval(&claimed, call.id, shell.clone(), edit.clone())
                        .await
                    {
                        Ok(event) => inner.notify(event.cursor),
                        Err(error) => {
                            inner.remove_approval(call.id);
                            return approval_persistence_failure(error);
                        }
                    }
                    // The reviewer adjudicates only under Auto — the mode
                    // whose held bucket is "dangerous-shaped but possibly
                    // fine". Ask means the human asked to decide everything;
                    // ReadOnly never reaches here. Any verdict other than a
                    // clear Approve leaves the human path exactly as it was.
                    let mut review: Option<ReviewFuture> = match (&inner.approval_reviewer, mode) {
                        (Some(reviewer), ApprovalMode::Auto) => {
                            Some(reviewer.review(ReviewRequest {
                                tool_name: call.name.clone(),
                                shell,
                                edit,
                                workspace: claimed.workspace.clone(),
                            }))
                        }
                        _ => None,
                    };
                    // One deadline for the whole wait: a reviewer escalation
                    // must not restart the human approval timeout.
                    let deadline = tokio::time::Instant::now() + inner.approval_timeout;
                    let timed_out = loop {
                        if let Some(pending_review) = review.as_mut() {
                            tokio::select! {
                                biased;
                                changed = cancellation.changed() => {
                                    let _ = changed;
                                    inner.remove_approval(call.id);
                                    return GateDecision::Deny {
                                        message: "The run stopped before this approval was resolved."
                                            .to_owned(),
                                    };
                                }
                                result = &mut resolved => break result.is_err(),
                                verdict = pending_review => {
                                    review = None;
                                    if matches!(verdict, ReviewVerdict::Approve) {
                                        match inner
                                            .store
                                            .resolve_approval_by_reviewer(&claimed, call.id)
                                            .await
                                        {
                                            Ok(Some(event)) => {
                                                inner.notify(event.cursor);
                                                inner.remove_approval(call.id);
                                                return GateDecision::Execute;
                                            }
                                            // A client resolution won the
                                            // race or the write failed:
                                            // fall through to conclude,
                                            // which reads the durable state.
                                            Ok(None) | Err(_) => break false,
                                        }
                                    }
                                    // Escalate or Deny: keep waiting for the
                                    // human on the remaining select arms.
                                }
                                () = tokio::time::sleep_until(deadline) => break true,
                            }
                        } else {
                            tokio::select! {
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
                                result = &mut resolved => break result.is_err(),
                                () = tokio::time::sleep_until(deadline) => break true,
                            }
                        }
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
                        Ok(ConcludedApproval::StillWaiting) => GateDecision::Fail {
                            kind: RunFailureKind::Server,
                            message:
                                "tool approval resolution disappeared before it could be applied"
                                    .to_owned(),
                        },
                        Err(error) => approval_persistence_failure(error),
                    }
                }
            }
        })
    }
}

fn approval_persistence_failure(error: SessionRuntimeError) -> GateDecision {
    match error {
        SessionRuntimeError::OutputTooLarge => GateDecision::Fail {
            kind: RunFailureKind::Policy,
            message: "the tool result would exceed the run's context capacity".to_owned(),
        },
        error => GateDecision::Fail {
            kind: RunFailureKind::Server,
            message: format!("tool approval state could not be persisted: {error}"),
        },
    }
}

pub(super) enum ConcludedApproval {
    Approved,
    Denied {
        message: String,
        event: Option<Box<SessionEventEnvelope>>,
    },
    StillWaiting,
}
