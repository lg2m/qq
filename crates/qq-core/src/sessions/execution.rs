use super::*;
use super::{
    approvals::SessionToolGate, runtime::SessionRuntimeInner, subagents::SessionSubagentSpawner,
};

/// Denies every tool call. Compaction runs summarize existing context; a
/// call the instruction forbade costs one denied round trip and persists
/// nothing.
struct CompactionRunGate;

#[cfg(test)]
struct BufferedToolOutputHook {
    tool_call_id: ToolCallId,
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

#[cfg(test)]
static BUFFERED_TOOL_OUTPUT_HOOK: Mutex<Option<BufferedToolOutputHook>> = Mutex::new(None);

/// Holds the execution loop immediately after one call's live output enters
/// the bounded batch. Tests use this exact handoff to make cancellation-versus-
/// timer ordering deterministic without adding production sleeps or hooks.
#[cfg(test)]
pub(super) fn hold_buffered_tool_output(
    tool_call_id: ToolCallId,
) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (entered, entered_rx) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    *BUFFERED_TOOL_OUTPUT_HOOK.lock().unwrap() = Some(BufferedToolOutputHook {
        tool_call_id,
        entered,
        release: release_rx,
    });
    (entered_rx, release)
}

#[cfg(test)]
async fn pause_after_buffering_tool_output(tool_call_id: ToolCallId) {
    let hook = {
        let mut hook = BUFFERED_TOOL_OUTPUT_HOOK.lock().unwrap();
        if hook
            .as_ref()
            .is_some_and(|hook| hook.tool_call_id == tool_call_id)
        {
            hook.take()
        } else {
            None
        }
    };
    if let Some(hook) = hook {
        let _ = hook.entered.send(());
        let _ = hook.release.await;
    }
}

impl ToolGate for CompactionRunGate {
    fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
        Box::pin(std::future::ready(GateDecision::Deny {
            message: "Tools are unavailable during compaction; produce the summary directly."
                .to_owned(),
        }))
    }
}

const COMPACTION_OUTPUT_RESERVE_TOKENS: u32 = 2_048;

struct PreparedExecution {
    events: crate::RuntimeStream,
    audit: PreparedRunAudit,
    tool_cancellation: Arc<AtomicBool>,
}

async fn prepare_execution(
    inner: &Arc<SessionRuntimeInner>,
    claimed: &mut ClaimedRun,
    loaded: &LoadedRuntime,
    cancellation: &mut watch::Receiver<bool>,
) -> Result<PreparedExecution, RunOutcome> {
    let tool_cancellation = Arc::new(AtomicBool::new(false));
    let internal = claimed.kind == RunKind::Compaction;
    // Take the only full transcript before cloning run metadata into gates or
    // spawners. ClaimedRun clones after this point stay scalar/empty instead
    // of duplicating up to 4 MiB per tool call.
    let messages = std::mem::take(&mut claimed.messages);
    let gate: Arc<dyn ToolGate> = if internal {
        Arc::new(CompactionRunGate)
    } else {
        Arc::new(SessionToolGate::new(
            Arc::clone(inner),
            claimed.clone(),
            cancellation.clone(),
        ))
    };
    let file_state = tokio::select! {
        result = session_file_state_with_retry(inner, claimed.session_id) => match result {
            Ok(entries) => Arc::new(FileState::with_entries(entries)),
            Err(error) => {
                return Err(persistence_failure(
                    "failed to load the session file state",
                    &error,
                ));
            }
        },
        changed = cancellation.changed() => {
            tool_cancellation.store(true, Ordering::Release);
            return if changed.is_ok() && *cancellation.borrow() {
                Err(RunOutcome::Cancelled)
            } else {
                Err(RunOutcome::Interrupted)
            };
        }
    };
    let capabilities = if internal {
        RunCapabilities::restricted()
            .without_tools()
            .with_max_output_tokens(
                loaded
                    .resolved_model
                    .max_output_tokens
                    .min(COMPACTION_OUTPUT_RESERVE_TOKENS),
            )
    } else if !claimed.user_initiated {
        RunCapabilities::restricted()
    } else {
        let spawner = if claimed.child {
            None
        } else {
            Some(Arc::new(SessionSubagentSpawner::new(
                Arc::clone(inner),
                claimed.clone(),
            )) as Arc<dyn SubagentSpawner>)
        };
        RunCapabilities::user(spawner)
    }
    .with_literal_slash(claimed.literal_slash);
    let mut events = loaded.runtime.run_loop_with_spawner(
        messages,
        PathBuf::from(&claimed.workspace),
        Arc::clone(&tool_cancellation),
        gate,
        file_state,
        capabilities,
    );
    loop {
        let event = tokio::select! {
            biased;
            changed = cancellation.changed() => {
                tool_cancellation.store(true, Ordering::Release);
                return if changed.is_ok() && *cancellation.borrow() {
                    Err(RunOutcome::Cancelled)
                } else {
                    Err(RunOutcome::Interrupted)
                };
            }
            event = events.next() => event,
        };
        match event {
            Some(RuntimeEvent::Started) => {}
            Some(RuntimeEvent::Prepared {
                turn_ordinal: 1,
                identity: Some(prompt_identity),
                weight,
            }) => {
                let mut resolved_model = loaded.resolved_model.as_ref().clone();
                // Internal compaction deliberately reserves a smaller output
                // budget. Its immutable descriptor records the effective cap
                // actually sent on every provider turn, not the model's
                // larger configured ceiling.
                resolved_model.max_output_tokens = weight.max_output_tokens;
                return Ok(PreparedExecution {
                    events,
                    audit: PreparedRunAudit {
                        prompt_identity: Arc::new(prompt_identity),
                        resolved_model: Arc::new(resolved_model),
                        weight,
                    },
                    tool_cancellation,
                });
            }
            Some(RuntimeEvent::Failed { kind, message }) => {
                return Err(RunOutcome::Failed {
                    failure: RunFailure {
                        kind,
                        message: truncate_utf8(message, MAX_FAILURE_MESSAGE_BYTES),
                    },
                });
            }
            Some(_) | None => {
                return Err(internal_failure(
                    "runtime preparation ended without an initial prepared request",
                ));
            }
        }
    }
}

pub(super) async fn execute_run(
    inner: Arc<SessionRuntimeInner>,
    mut claimed: ClaimedRun,
    mut cancellation: watch::Receiver<bool>,
) {
    if *cancellation.borrow() {
        finish_reserved_run(&inner, &claimed, RunOutcome::Cancelled).await;
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
                finish_reserved_run(&inner, &claimed, RunOutcome::Failed {
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
                finish_reserved_run(&inner, &claimed, RunOutcome::Cancelled).await;
                // Runtime construction may be blocking; retain the run permit until it exits.
                let _ = load.await;
                return;
            }
            return;
        }
    };
    if *cancellation.borrow() {
        finish_reserved_run(&inner, &claimed, RunOutcome::Cancelled).await;
        return;
    }

    if loaded.runtime.model.as_ref() != loaded.resolved_model.provider_model.as_str()
        || loaded.runtime.max_output_tokens != loaded.resolved_model.max_output_tokens
        || loaded.runtime.context_window() != loaded.resolved_model.context_window
    {
        finish_reserved_run(
            &inner,
            &claimed,
            RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::Configuration,
                    message: "loaded runtime does not match its resolved model descriptor"
                        .to_owned(),
                },
            },
        )
        .await;
        return;
    }
    loop {
        let prepared =
            match prepare_execution(&inner, &mut claimed, &loaded, &mut cancellation).await {
                Ok(prepared) => prepared,
                Err(outcome) => {
                    finish_reserved_run(&inner, &claimed, outcome).await;
                    return;
                }
            };
        let plan = context::plan(context::ContextInput {
            context_window: loaded.resolved_model.context_window,
            max_output_tokens: prepared.audit.weight.max_output_tokens,
            system_bytes: prepared.audit.weight.system_bytes,
            tool_schema_bytes: prepared.audit.weight.tool_schema_bytes,
            reducible_message_bytes: prepared.audit.weight.reducible_message_bytes,
            irreducible_message_bytes: prepared.audit.weight.irreducible_message_bytes,
            compatible_input_tokens: prepared.audit.weight.compatible_input_tokens,
            compaction: if claimed.kind == RunKind::Compaction
                || claimed.context_compaction_attempted
            {
                context::CompactionDisposition::AlreadyAttempted
            } else {
                context::CompactionDisposition::Eligible
            },
        });
        let repeats_known_overflow = matches!(plan, context::ContextPlan::Send { .. })
            && claimed
                .context_overflow_model
                .as_ref()
                .is_some_and(|model| {
                    same_context_request_shape(model.as_ref(), loaded.resolved_model.as_ref())
                })
            && claimed.kind == RunKind::Prompt;
        if repeats_known_overflow {
            if claimed.context_compaction_attempted {
                let context::ContextPlan::Send { estimate } = plan else {
                    unreachable!("known overflow override only applies to a send plan")
                };
                finish_prepared_run(
                    &inner,
                    &claimed,
                    &prepared.audit,
                    planned_context_failure(context::ContextPlan::Reject {
                        estimate,
                        reason: context::ContextRejectReason::ProviderReportedOverflow,
                    }),
                )
                .await;
                return;
            }
            let audit = prepared.audit.clone();
            drop(prepared);
            if !run_auto_compaction(&inner, &mut claimed, &loaded, audit, &mut cancellation).await {
                return;
            }
            continue;
        }
        match plan {
            context::ContextPlan::Send { .. } => {
                let started = loop {
                    if *inner.failed.borrow() {
                        finish_prepared_run(
                            &inner,
                            &claimed,
                            &prepared.audit,
                            internal_failure("session runtime failed before run start"),
                        )
                        .await;
                        return;
                    }
                    let result = inner
                        .store
                        .start_reserved_run(&claimed, prepared.audit.clone())
                        .await;
                    if matches!(result, Err(SessionRuntimeError::Overloaded)) {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                        continue;
                    }
                    break result;
                };
                let started = match started {
                    Ok(Some(started)) => started,
                    Ok(None) => {
                        finish_reserved_run(
                            &inner,
                            &claimed,
                            internal_failure("run preparation lost its durable reservation"),
                        )
                        .await;
                        return;
                    }
                    Err(error) => {
                        finish_reserved_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist prepared run state", &error),
                        )
                        .await;
                        return;
                    }
                };
                inner.notify(started.cursor);
                claimed.model = ModelSelection {
                    model: Some(prepared.audit.resolved_model.route.clone()),
                    max_output_tokens: Some(prepared.audit.resolved_model.max_output_tokens),
                    organization: prepared.audit.resolved_model.organization.clone(),
                };
                let cancelled =
                    match cancellation_requested_with_retry(&inner, claimed.run_id).await {
                        Ok(cancelled) => cancelled || *cancellation.borrow(),
                        Err(error) => {
                            finish_run(
                                &inner,
                                &claimed,
                                persistence_failure("failed to re-read run cancellation", &error),
                            )
                            .await;
                            return;
                        }
                    };
                if *inner.failed.borrow() {
                    prepared.tool_cancellation.store(true, Ordering::Release);
                    finish_run(
                        &inner,
                        &claimed,
                        internal_failure("session runtime failed before provider work"),
                    )
                    .await;
                    return;
                }
                if cancelled {
                    prepared.tool_cancellation.store(true, Ordering::Release);
                    finish_run(&inner, &claimed, RunOutcome::Cancelled).await;
                    return;
                }
                execute_started_run(
                    inner,
                    claimed,
                    cancellation,
                    prepared.events,
                    prepared.tool_cancellation,
                    prepared.audit.resolved_model,
                )
                .await;
                return;
            }
            context::ContextPlan::Compact { .. } if claimed.kind == RunKind::Prompt => {
                let audit = prepared.audit.clone();
                drop(prepared);
                if !run_auto_compaction(&inner, &mut claimed, &loaded, audit, &mut cancellation)
                    .await
                {
                    return;
                }
            }
            plan => {
                finish_prepared_run(
                    &inner,
                    &claimed,
                    &prepared.audit,
                    planned_context_failure(plan),
                )
                .await;
                return;
            }
        }
    }
}

async fn run_auto_compaction(
    inner: &Arc<SessionRuntimeInner>,
    original: &mut ClaimedRun,
    loaded: &LoadedRuntime,
    original_audit: PreparedRunAudit,
    cancellation: &mut watch::Receiver<bool>,
) -> bool {
    let messages = loop {
        if *inner.failed.borrow() {
            finish_prepared_run(
                inner,
                original,
                &original_audit,
                internal_failure("session runtime failed before automatic compaction"),
            )
            .await;
            return false;
        }
        let result = inner
            .store
            .load_auto_compaction_messages(original.session_id)
            .await;
        if matches!(result, Err(SessionRuntimeError::Overloaded)) {
            tokio::time::sleep(Duration::from_millis(1)).await;
            continue;
        }
        break result;
    };
    let messages = match messages {
        Ok(messages) => messages,
        Err(error) => {
            finish_prepared_run(
                inner,
                original,
                &original_audit,
                persistence_failure("failed to assemble automatic compaction", &error),
            )
            .await;
            return false;
        }
    };
    let mut candidate = original.clone();
    candidate.run_id = match RunId::generate() {
        Ok(run_id) => run_id,
        Err(_) => {
            finish_prepared_run(
                inner,
                original,
                &original_audit,
                internal_failure("failed to allocate an automatic compaction run id"),
            )
            .await;
            return false;
        }
    };
    candidate.command_id = match CommandId::generate() {
        Ok(command_id) => command_id,
        Err(_) => {
            finish_prepared_run(
                inner,
                original,
                &original_audit,
                internal_failure("failed to allocate an automatic compaction command id"),
            )
            .await;
            return false;
        }
    };
    candidate.kind = RunKind::Compaction;
    candidate.user_initiated = false;
    candidate.literal_slash = false;
    candidate.messages = messages;
    candidate.context_compaction_attempted = true;
    let prepared = match prepare_execution(inner, &mut candidate, loaded, cancellation).await {
        Ok(prepared) => prepared,
        Err(outcome) => {
            finish_prepared_run(inner, original, &original_audit, outcome).await;
            return false;
        }
    };
    let plan = context::plan(context::ContextInput {
        context_window: loaded.resolved_model.context_window,
        max_output_tokens: prepared.audit.weight.max_output_tokens,
        system_bytes: prepared.audit.weight.system_bytes,
        tool_schema_bytes: prepared.audit.weight.tool_schema_bytes,
        reducible_message_bytes: prepared.audit.weight.reducible_message_bytes,
        irreducible_message_bytes: prepared.audit.weight.irreducible_message_bytes,
        compatible_input_tokens: prepared.audit.weight.compatible_input_tokens,
        compaction: context::CompactionDisposition::AlreadyAttempted,
    });
    if !matches!(plan, context::ContextPlan::Send { .. }) {
        finish_prepared_run(
            inner,
            original,
            &original_audit,
            planned_context_failure(plan),
        )
        .await;
        return false;
    }
    let started = loop {
        if *inner.failed.borrow() {
            finish_prepared_run(
                inner,
                original,
                &original_audit,
                internal_failure("session runtime failed before automatic compaction start"),
            )
            .await;
            return false;
        }
        let result = inner
            .store
            .start_auto_compaction(original, prepared.audit.clone())
            .await;
        if matches!(result, Err(SessionRuntimeError::Overloaded)) {
            tokio::time::sleep(Duration::from_millis(1)).await;
            continue;
        }
        break result;
    };
    let (mut compaction, started) = match started {
        Ok(Some(started)) => started,
        Ok(None) => {
            finish_prepared_run(
                inner,
                original,
                &original_audit,
                internal_failure("automatic compaction lost its prompt reservation"),
            )
            .await;
            return false;
        }
        Err(error) => {
            finish_prepared_run(
                inner,
                original,
                &original_audit,
                persistence_failure("failed to persist automatic compaction start", &error),
            )
            .await;
            return false;
        }
    };
    let (compaction_cancel, compaction_cancellation) = watch::channel(false);
    if let Ok(mut cancellations) = inner.cancellations.lock() {
        cancellations.insert(compaction.run_id, compaction_cancel);
    } else {
        inner.failed.send_replace(true);
        let outcome = internal_failure("run cancellation registry is unavailable");
        finish_run(inner, &compaction, outcome.clone()).await;
        finish_prepared_run(inner, original, &original_audit, outcome).await;
        return false;
    }
    inner.notify(started.cursor);
    compaction.model = ModelSelection {
        model: Some(prepared.audit.resolved_model.route.clone()),
        max_output_tokens: Some(prepared.audit.resolved_model.max_output_tokens),
        organization: prepared.audit.resolved_model.organization.clone(),
    };
    let cancelled = match cancellation_requested_with_retry(inner, compaction.run_id).await {
        Ok(cancelled) => cancelled,
        Err(error) => {
            let outcome = persistence_failure("failed to re-read compaction cancellation", &error);
            finish_run(inner, &compaction, outcome.clone()).await;
            finish_prepared_run(inner, original, &original_audit, outcome).await;
            return false;
        }
    };
    if *inner.failed.borrow() {
        prepared.tool_cancellation.store(true, Ordering::Release);
        let outcome = internal_failure("session runtime failed before compaction provider work");
        finish_run(inner, &compaction, outcome.clone()).await;
        finish_prepared_run(inner, original, &original_audit, outcome).await;
        return false;
    }
    let compaction_run_id = compaction.run_id;
    if cancelled {
        prepared.tool_cancellation.store(true, Ordering::Release);
        finish_run(inner, &compaction, RunOutcome::Cancelled).await;
    } else {
        execute_started_run(
            Arc::clone(inner),
            compaction,
            compaction_cancellation,
            prepared.events,
            prepared.tool_cancellation,
            Arc::clone(&prepared.audit.resolved_model),
        )
        .await;
    }
    if *inner.failed.borrow() {
        return false;
    }
    let committed = loop {
        let result = inner
            .store
            .compaction_committed(original.session_id, compaction_run_id)
            .await;
        if matches!(result, Err(SessionRuntimeError::Overloaded)) {
            tokio::time::sleep(Duration::from_millis(1)).await;
            continue;
        }
        break result;
    };
    let compacted = match committed {
        Ok(compacted) => compacted,
        Err(error) => {
            finish_prepared_run(
                inner,
                original,
                &original_audit,
                persistence_failure("failed to verify automatic compaction", &error),
            )
            .await;
            return false;
        }
    };
    loop {
        match inner.store.reload_reserved_messages(original).await {
            Ok(Some((messages, attempted))) => {
                original.messages = messages;
                original.context_compaction_attempted = attempted;
                if compacted {
                    original.context_overflow_model = None;
                }
                return true;
            }
            Ok(None) => {
                clear_run_registration(inner, original.run_id);
                return false;
            }
            Err(SessionRuntimeError::Overloaded) => {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            Err(error) => {
                finish_prepared_run(
                    inner,
                    original,
                    &original_audit,
                    persistence_failure(
                        "failed to reload the reserved prompt after automatic compaction",
                        &error,
                    ),
                )
                .await;
                return false;
            }
        }
    }
}

async fn cancellation_requested_with_retry(
    inner: &SessionRuntimeInner,
    run_id: RunId,
) -> Result<bool, SessionRuntimeError> {
    loop {
        if *inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        let result = inner.store.cancellation_requested(run_id).await;
        if *inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        match result {
            Err(SessionRuntimeError::Overloaded) => {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            result => return result,
        }
    }
}

async fn session_file_state_with_retry(
    inner: &SessionRuntimeInner,
    session_id: SessionId,
) -> Result<Vec<(String, String)>, SessionRuntimeError> {
    loop {
        if *inner.failed.borrow() {
            return Err(SessionRuntimeError::Unavailable);
        }
        match inner.store.session_file_state(session_id).await {
            Err(SessionRuntimeError::Overloaded) => {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            result => return result,
        }
    }
}

async fn finish_reserved_run(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    outcome: RunOutcome,
) {
    loop {
        match inner
            .store
            .finish_reserved_run(claimed, outcome.clone())
            .await
        {
            Ok(events) => {
                for event in events {
                    inner.notify(event.cursor);
                }
                inner
                    .settlements
                    .send_modify(|generation| *generation = generation.wrapping_add(1));
                clear_run_registration(inner, claimed.run_id);
                return;
            }
            Err(SessionRuntimeError::Overloaded) => {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            Err(_) => {
                inner.failed.send_replace(true);
                return;
            }
        }
    }
}

fn clear_run_registration(inner: &SessionRuntimeInner, run_id: RunId) {
    if let Ok(mut cancellations) = inner.cancellations.lock() {
        cancellations.remove(&run_id);
    }
    inner.clear_run_approvals(run_id);
}

async fn finish_prepared_run(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    audit: &PreparedRunAudit,
    outcome: RunOutcome,
) {
    loop {
        match inner
            .store
            .finish_prepared_run(claimed, audit.clone(), outcome.clone())
            .await
        {
            Ok(events) => {
                for event in events {
                    inner.notify(event.cursor);
                }
                inner
                    .settlements
                    .send_modify(|generation| *generation = generation.wrapping_add(1));
                clear_run_registration(inner, claimed.run_id);
                return;
            }
            Err(SessionRuntimeError::Overloaded) => {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            Err(error) => {
                // A trigger or storage failure on one descriptor/identity column
                // must still terminally settle the queued run without pretending
                // the failed audit write was durable.
                finish_reserved_run(
                    inner,
                    claimed,
                    persistence_failure("failed to persist prepared run state", &error),
                )
                .await;
                return;
            }
        }
    }
}

async fn execute_started_run(
    inner: Arc<SessionRuntimeInner>,
    claimed: ClaimedRun,
    mut cancellation: watch::Receiver<bool>,
    mut events: crate::RuntimeStream,
    tool_cancellation: Arc<AtomicBool>,
    resolved_model: Arc<ResolvedModel>,
) {
    let mut runtime_failed = inner.failed.subscribe();
    if *runtime_failed.borrow() {
        tool_cancellation.store(true, Ordering::Release);
        finish_run(
            &inner,
            &claimed,
            internal_failure("session runtime failed before provider work"),
        )
        .await;
        return;
    }
    let internal = claimed.kind == RunKind::Compaction;
    let mut accounting = RunAccountingAccumulator::new(resolved_model.pricing.clone());
    let mut pending_text = String::new();
    let mut pending_channel = None;
    let mut reasoning_kind = None;
    let mut reasoning_delta_persisted = false;
    let mut pending_reasoning_kind = None;
    let mut pending_reasoning_text = String::new();
    let mut flush_at = None;
    // One assistant message per model turn: the message row is created
    // lazily at the turn's first text delta (so call-only turns persist no
    // message row) and finalized when the turn's `persist_model_turn`
    // commits. `current_turn` is the 1-based ordinal of the turn currently
    // streaming; text deltas always belong to it.
    let mut current_turn: u16 = 1;
    let mut current_message: Option<MessageId> = None;
    // Live tool output batches on the same timer as model text. Text and tool
    // output never accumulate at the same time: a turn's text is fully
    // flushed when the turn completes, before any of its calls execute.
    let mut pending_tool_call: Option<ToolCallId> = None;
    let mut pending_tool_output = String::new();
    // An internal run's streamed output never joins the transcript; the
    // summary accumulates here and persists as a compaction row instead.
    let mut summary_text = String::new();
    loop {
        let input = if let Some(deadline) = flush_at {
            tokio::select! {
                biased;
                _ = runtime_failed.changed() => RunInput::RuntimeFailed,
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
                _ = runtime_failed.changed() => RunInput::RuntimeFailed,
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
        let (continues_text, continues_tool_output, continues_reasoning) = match &input {
            RunInput::Event(Some(event)) => (
                matches!(
                    event,
                    RuntimeEvent::OutputTextDelta { .. }
                        if pending_channel == Some(TextChannel::Output)
                ) || matches!(
                    event,
                    RuntimeEvent::RefusalDelta { .. }
                        if pending_channel == Some(TextChannel::Refusal)
                ),
                matches!(
                    event,
                    RuntimeEvent::ToolCallOutputDelta { id, .. }
                        if pending_tool_call == Some(*id)
                ),
                matches!(
                    event,
                    RuntimeEvent::ReasoningDelta { kind, .. }
                        if pending_reasoning_kind == Some(*kind)
                ),
            ),
            _ => (false, false, false),
        };
        if !pending_text.is_empty()
            && !continues_text
            && let Err(error) = flush_pending_text(
                &inner,
                &claimed,
                current_turn,
                &mut current_message,
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
        let stopped = matches!(
            &input,
            RunInput::Cancelled | RunInput::Interrupted | RunInput::RuntimeFailed
        );
        if !pending_tool_output.is_empty()
            && !continues_tool_output
            && !stopped
            && let Err(error) = flush_pending_tool_output(
                &inner,
                &claimed,
                &mut pending_tool_call,
                &mut pending_tool_output,
            )
            .await
        {
            finish_run(
                &inner,
                &claimed,
                persistence_failure("failed to persist tool output", &error),
            )
            .await;
            return;
        }
        if !pending_reasoning_text.is_empty()
            && !continues_reasoning
            && let Err(error) = flush_pending_reasoning(
                &inner,
                &claimed,
                &mut pending_reasoning_kind,
                &mut pending_reasoning_text,
            )
            .await
        {
            finish_run(
                &inner,
                &claimed,
                persistence_failure("failed to persist reasoning", &error),
            )
            .await;
            return;
        }
        if pending_text.is_empty()
            && pending_tool_output.is_empty()
            && pending_reasoning_text.is_empty()
        {
            flush_at = None;
        }
        match input {
            RunInput::Flush => {
                if let Err(error) = flush_pending_reasoning(
                    &inner,
                    &claimed,
                    &mut pending_reasoning_kind,
                    &mut pending_reasoning_text,
                )
                .await
                {
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist reasoning", &error),
                    )
                    .await;
                    return;
                }
                if let Err(error) = flush_pending_text(
                    &inner,
                    &claimed,
                    current_turn,
                    &mut current_message,
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
                if let Err(error) = flush_pending_tool_output(
                    &inner,
                    &claimed,
                    &mut pending_tool_call,
                    &mut pending_tool_output,
                )
                .await
                {
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist tool output", &error),
                    )
                    .await;
                    return;
                }
                flush_at = None;
            }
            stopped @ (RunInput::Cancelled | RunInput::Interrupted) => {
                tool_cancellation.store(true, Ordering::Release);
                if let Err(error) = flush_pending_reasoning(
                    &inner,
                    &claimed,
                    &mut pending_reasoning_kind,
                    &mut pending_reasoning_text,
                )
                .await
                {
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist reasoning", &error),
                    )
                    .await;
                    return;
                }
                if let Err(error) = flush_pending_text(
                    &inner,
                    &claimed,
                    current_turn,
                    &mut current_message,
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
                // Buffered live tool output is dropped rather than flushed:
                // the interrupted call's terminal result replaces it.
                let outcome = if matches!(stopped, RunInput::Cancelled) {
                    RunOutcome::Cancelled
                } else {
                    RunOutcome::Interrupted
                };
                finish_run_accounted(&inner, &claimed, outcome, Some(accounting.snapshot())).await;
                return;
            }
            RunInput::RuntimeFailed => {
                tool_cancellation.store(true, Ordering::Release);
                if let Err(error) = flush_pending_reasoning(
                    &inner,
                    &claimed,
                    &mut pending_reasoning_kind,
                    &mut pending_reasoning_text,
                )
                .await
                {
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist reasoning", &error),
                    )
                    .await;
                    return;
                }
                if let Err(error) = flush_pending_text(
                    &inner,
                    &claimed,
                    current_turn,
                    &mut current_message,
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
                finish_run_accounted(
                    &inner,
                    &claimed,
                    internal_failure("session runtime failed during provider work"),
                    Some(accounting.snapshot()),
                )
                .await;
                return;
            }
            RunInput::Event(Some(RuntimeEvent::Started)) => {}
            RunInput::Event(Some(RuntimeEvent::Prepared {
                turn_ordinal: _,
                identity,
                weight,
            })) => {
                if let Some(identity) = identity
                    && let Err(error) = inner.store.record_prompt_identity(&claimed, identity).await
                {
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist the run prompt identity", &error),
                    )
                    .await;
                    return;
                }
                let plan = context::plan(context::ContextInput {
                    context_window: resolved_model.context_window,
                    max_output_tokens: weight.max_output_tokens,
                    system_bytes: weight.system_bytes,
                    tool_schema_bytes: weight.tool_schema_bytes,
                    reducible_message_bytes: weight.reducible_message_bytes,
                    irreducible_message_bytes: weight.irreducible_message_bytes,
                    compatible_input_tokens: weight.compatible_input_tokens,
                    // Compaction is only legal between runs. The second slice
                    // will turn the first-turn Compact result into a reserved
                    // auto-compaction; later turns and compaction runs must
                    // fail closed without polling the provider.
                    compaction: if internal {
                        context::CompactionDisposition::AlreadyAttempted
                    } else {
                        context::CompactionDisposition::BetweenRunsOnly
                    },
                });
                match plan {
                    context::ContextPlan::Send { .. } => {}
                    plan => {
                        finish_run(&inner, &claimed, planned_context_failure(plan)).await;
                        return;
                    }
                }
            }
            RunInput::Event(Some(RuntimeEvent::ActivityChanged { activity })) => {
                if internal {
                    continue;
                }
                match inner.store.append_run_activity(&claimed, activity).await {
                    Ok(event) => inner.notify(event.cursor),
                    Err(error) => {
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist run activity", &error),
                        )
                        .await;
                        return;
                    }
                }
            }
            RunInput::Event(Some(RuntimeEvent::ReasoningStarted { kind })) => {
                if internal {
                    continue;
                }
                reasoning_kind = Some(kind);
                reasoning_delta_persisted = false;
                match inner
                    .store
                    .append_reasoning(&claimed, ReasoningEvent::Started { kind })
                    .await
                {
                    Ok(event) => inner.notify(event.cursor),
                    Err(error) => {
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist reasoning", &error),
                        )
                        .await;
                        return;
                    }
                }
            }
            RunInput::Event(Some(RuntimeEvent::ReasoningDelta { kind, text })) => {
                if internal || text.is_empty() {
                    continue;
                }
                if reasoning_kind != Some(kind) {
                    reasoning_kind = Some(kind);
                    reasoning_delta_persisted = false;
                }
                if !reasoning_delta_persisted {
                    match inner
                        .store
                        .append_reasoning(&claimed, ReasoningEvent::Delta { kind, text })
                        .await
                    {
                        Ok(event) => inner.notify(event.cursor),
                        Err(error) => {
                            finish_run(
                                &inner,
                                &claimed,
                                persistence_failure("failed to persist reasoning", &error),
                            )
                            .await;
                            return;
                        }
                    }
                    reasoning_delta_persisted = true;
                    continue;
                }
                if pending_reasoning_text.is_empty() {
                    pending_reasoning_kind = Some(kind);
                    if flush_at.is_none() {
                        flush_at = Some(tokio::time::Instant::now() + OUTPUT_BATCH_DELAY);
                    }
                }
                pending_reasoning_text.push_str(&text);
                if pending_reasoning_text.len() >= OUTPUT_BATCH_BYTES {
                    if let Err(error) = flush_pending_reasoning(
                        &inner,
                        &claimed,
                        &mut pending_reasoning_kind,
                        &mut pending_reasoning_text,
                    )
                    .await
                    {
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist reasoning", &error),
                        )
                        .await;
                        return;
                    }
                    if pending_text.is_empty() && pending_tool_output.is_empty() {
                        flush_at = None;
                    }
                }
            }
            RunInput::Event(Some(RuntimeEvent::ReasoningCompleted { kind })) => {
                if internal {
                    continue;
                }
                match inner
                    .store
                    .append_reasoning(&claimed, ReasoningEvent::Completed { kind })
                    .await
                {
                    Ok(event) => inner.notify(event.cursor),
                    Err(error) => {
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist reasoning", &error),
                        )
                        .await;
                        return;
                    }
                }
                reasoning_kind = None;
                reasoning_delta_persisted = false;
            }
            RunInput::Event(Some(RuntimeEvent::AssistantTurnCompleted {
                turn_ordinal,
                message,
                usage,
                calls,
            })) => {
                if internal {
                    // Usage, cost, and provider-turn identity persist like
                    // any run. The turn's text joins the summary instead of
                    // the transcript, and compaction tool calls remain
                    // non-authoritative and unpublished.
                    let turn_cost = usage.and_then(|usage| {
                        accounting
                            .pricing
                            .as_ref()
                            .and_then(|pricing| run_cost(usage, pricing))
                    });
                    accounting.record_turn(usage);
                    for block in message.content() {
                        if let ContentBlock::Text { text } = block {
                            if !summary_text.is_empty() {
                                summary_text.push('\n');
                            }
                            summary_text.push_str(text);
                        }
                    }
                    current_turn = turn_ordinal.saturating_add(1);
                    match inner
                        .store
                        .persist_model_turn(
                            &claimed,
                            ModelTurnCommit {
                                turn_ordinal,
                                message,
                                calls,
                                turn_message: None,
                                context_tokens: usage.map(turn_context_tokens),
                                usage,
                                estimated_cost_usd_nanos: turn_cost,
                                accounting: Some(accounting.snapshot()),
                            },
                        )
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
                                    "failed to persist the completed compaction turn",
                                    &error,
                                ),
                            )
                            .await;
                            return;
                        }
                    }
                    continue;
                }
                if let Err(error) = flush_pending_text(
                    &inner,
                    &claimed,
                    current_turn,
                    &mut current_message,
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
                let turn_cost = usage.and_then(|usage| {
                    accounting
                        .pricing
                        .as_ref()
                        .and_then(|pricing| run_cost(usage, pricing))
                });
                accounting.record_turn(usage);
                let turn_accounting = accounting.snapshot();
                // The completed turn's usage measures the context the run now
                // occupies; the turn transaction publishes it so the meter
                // moves while the tool loop is still running.
                let context_tokens = usage.map(turn_context_tokens);
                // The completed turn's message (if any) finalizes inside the
                // same transaction as the turn row; the next turn's text will
                // lazily start a fresh message.
                let turn_message = current_message.take();
                current_turn = turn_ordinal.saturating_add(1);
                match inner
                    .store
                    .persist_model_turn(
                        &claimed,
                        ModelTurnCommit {
                            turn_ordinal,
                            message,
                            calls,
                            turn_message,
                            context_tokens,
                            usage,
                            estimated_cost_usd_nanos: turn_cost,
                            accounting: Some(turn_accounting),
                        },
                    )
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
            // Approval transitions (including denials) are persisted and
            // published by the tool gate before this event is emitted.
            RunInput::Event(Some(RuntimeEvent::ToolCallDenied { .. })) => {}
            RunInput::Event(Some(RuntimeEvent::ToolCallStarted { id })) => {
                if internal {
                    continue;
                }
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
            RunInput::Event(Some(RuntimeEvent::ToolCallOutputDelta { id, chunk })) => {
                if internal || chunk.is_empty() {
                    continue;
                }
                if pending_tool_call.is_some_and(|pending| pending != id)
                    && let Err(error) = flush_pending_tool_output(
                        &inner,
                        &claimed,
                        &mut pending_tool_call,
                        &mut pending_tool_output,
                    )
                    .await
                {
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist tool output", &error),
                    )
                    .await;
                    return;
                }
                if pending_tool_output.is_empty() {
                    pending_tool_call = Some(id);
                    if flush_at.is_none() {
                        flush_at = Some(tokio::time::Instant::now() + OUTPUT_BATCH_DELAY);
                    }
                }
                pending_tool_output.push_str(&chunk);
                #[cfg(test)]
                pause_after_buffering_tool_output(id).await;
                if pending_tool_output.len() >= OUTPUT_BATCH_BYTES {
                    if let Err(error) = flush_pending_tool_output(
                        &inner,
                        &claimed,
                        &mut pending_tool_call,
                        &mut pending_tool_output,
                    )
                    .await
                    {
                        finish_run(
                            &inner,
                            &claimed,
                            persistence_failure("failed to persist tool output", &error),
                        )
                        .await;
                        return;
                    }
                    flush_at = None;
                }
            }
            RunInput::Event(Some(RuntimeEvent::ToolCallFinished {
                id,
                result,
                is_error,
                file_state,
                display,
            })) => {
                if internal {
                    continue;
                }
                // Any buffered live output flushes first so replay preserves
                // the chunk-then-result order.
                if let Err(error) = flush_pending_tool_output(
                    &inner,
                    &claimed,
                    &mut pending_tool_call,
                    &mut pending_tool_output,
                )
                .await
                {
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist tool output", &error),
                    )
                    .await;
                    return;
                }
                match inner
                    .store
                    .finish_tool_call(&claimed, id, result, is_error, file_state, display)
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
                // Internal runs stream no transcript text; the summary is
                // captured from the completed turn instead.
                if internal || text.is_empty() {
                    continue;
                }
                if current_message.is_none() {
                    // A turn's first delta persists immediately: it creates
                    // the turn's message row and publishes the new message
                    // without batching latency.
                    if let Err(error) = persist_text(
                        &inner,
                        &claimed,
                        current_turn,
                        &mut current_message,
                        channel,
                        text,
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
                    continue;
                }
                if pending_channel.is_some_and(|pending| pending != channel)
                    && let Err(error) = flush_pending_text(
                        &inner,
                        &claimed,
                        current_turn,
                        &mut current_message,
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
                        current_turn,
                        &mut current_message,
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
                if internal {
                    let summary = std::mem::take(&mut summary_text);
                    if summary.trim().is_empty() {
                        finish_run_accounted(
                            &inner,
                            &claimed,
                            RunOutcome::Failed {
                                failure: RunFailure {
                                    kind: RunFailureKind::ProviderResponse,
                                    message: "compaction produced an empty summary".to_owned(),
                                },
                            },
                            Some(accounting.snapshot()),
                        )
                        .await;
                        return;
                    }
                    match inner
                        .store
                        .finish_compaction_run(&claimed, summary, Some(accounting.snapshot()))
                        .await
                    {
                        Ok(events) => {
                            for event in events {
                                inner.notify(event.cursor);
                            }
                        }
                        Err(_) => {
                            inner.failed.send_replace(true);
                        }
                    }
                    if let Ok(mut cancellations) = inner.cancellations.lock() {
                        cancellations.remove(&claimed.run_id);
                    }
                    inner.clear_run_approvals(claimed.run_id);
                    return;
                }
                if let Err(error) = flush_pending_text(
                    &inner,
                    &claimed,
                    current_turn,
                    &mut current_message,
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
                if let Err(error) = flush_pending_text(
                    &inner,
                    &claimed,
                    current_turn,
                    &mut current_message,
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
                if let Err(error) = flush_pending_reasoning(
                    &inner,
                    &claimed,
                    &mut pending_reasoning_kind,
                    &mut pending_reasoning_text,
                )
                .await
                {
                    finish_run(
                        &inner,
                        &claimed,
                        persistence_failure("failed to persist reasoning", &error),
                    )
                    .await;
                    return;
                }
                if let Err(error) = flush_pending_text(
                    &inner,
                    &claimed,
                    current_turn,
                    &mut current_message,
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
    RuntimeFailed,
}

async fn flush_pending_reasoning(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    kind: &mut Option<qq_provider::ReasoningKind>,
    text: &mut String,
) -> Result<(), SessionRuntimeError> {
    let Some(kind) = kind.take() else {
        return Ok(());
    };
    let text = std::mem::take(text);
    if text.is_empty() {
        return Ok(());
    }
    let event = inner
        .store
        .append_reasoning(claimed, ReasoningEvent::Delta { kind, text })
        .await?;
    inner.notify(event.cursor);
    Ok(())
}

async fn flush_pending_text(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    current_turn: u16,
    current_message: &mut Option<MessageId>,
    channel: &mut Option<TextChannel>,
    text: &mut String,
) -> Result<(), SessionRuntimeError> {
    let Some(channel) = channel.take() else {
        return Ok(());
    };
    persist_text(
        inner,
        claimed,
        current_turn,
        current_message,
        channel,
        std::mem::take(text),
    )
    .await
}

/// Publishes any buffered live tool output as one batched
/// `ToolCallOutputDelta` event.
async fn flush_pending_tool_output(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    pending_call: &mut Option<ToolCallId>,
    pending_output: &mut String,
) -> Result<(), SessionRuntimeError> {
    let Some(tool_call_id) = pending_call.take() else {
        return Ok(());
    };
    let chunk = std::mem::take(pending_output);
    if chunk.is_empty() {
        return Ok(());
    }
    let event = inner
        .store
        .append_tool_output(claimed, tool_call_id, chunk)
        .await?;
    inner.notify(event.cursor);
    Ok(())
}

/// Persists model text into the current turn's assistant message, creating
/// that message on the turn's first chunk.
async fn persist_text(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    current_turn: u16,
    current_message: &mut Option<MessageId>,
    channel: TextChannel,
    text: String,
) -> Result<(), SessionRuntimeError> {
    let mut remaining = text.as_str();
    while !remaining.is_empty() {
        let mut end = remaining.len().min(MAX_TEXT_CHUNK_BYTES);
        while !remaining.is_char_boundary(end) {
            end -= 1;
        }
        let chunk = remaining[..end].to_owned();
        match *current_message {
            Some(message_id) => {
                let event = inner
                    .store
                    .append_text(claimed, message_id, channel, chunk)
                    .await?;
                inner.notify(event.cursor);
            }
            None => {
                let message_id =
                    MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
                let events = inner
                    .store
                    .begin_assistant_message(claimed, message_id, current_turn, channel, chunk)
                    .await?;
                for event in events {
                    inner.notify(event.cursor);
                }
                *current_message = Some(message_id);
            }
        }
        remaining = &remaining[end..];
    }
    Ok(())
}

pub(super) async fn finish_run(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    outcome: RunOutcome,
) {
    finish_run_accounted(inner, claimed, outcome, None).await;
}

async fn finish_run_accounted(
    inner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    outcome: RunOutcome,
    accounting: Option<RunAccounting>,
) {
    loop {
        match inner
            .store
            .finish_run(claimed, outcome.clone(), accounting.clone())
            .await
        {
            Ok(events) => {
                for event in events {
                    inner.notify(event.cursor);
                }
                inner
                    .settlements
                    .send_modify(|generation| *generation = generation.wrapping_add(1));
                clear_run_registration(inner, claimed.run_id);
                return;
            }
            Err(SessionRuntimeError::Overloaded) => {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            Err(_) => {
                inner.failed.send_replace(true);
                return;
            }
        }
    }
}

#[derive(Clone)]
pub(super) struct RunAccounting {
    pub(super) usage: Option<TokenUsage>,
    pub(super) context_tokens: Option<u64>,
    pub(super) estimated_cost_usd_nanos: Option<u64>,
    pub(super) saw_turn: bool,
}

struct RunAccountingAccumulator {
    usage: Option<TokenUsage>,
    context_tokens: Option<u64>,
    estimated_cost_usd_nanos: Option<u64>,
    pricing: Option<ModelPricing>,
    saw_turn: bool,
}

impl RunAccountingAccumulator {
    fn new(pricing: Option<ModelPricing>) -> Self {
        Self {
            usage: Some(TokenUsage::default()),
            context_tokens: None,
            estimated_cost_usd_nanos: pricing.as_ref().map(|_| 0),
            pricing,
            saw_turn: false,
        }
    }

    fn record_turn(&mut self, usage: Option<TokenUsage>) {
        self.saw_turn = true;
        let Some(usage) = usage else {
            self.usage = None;
            // A newer completed request without usage makes the run's and
            // session's current occupancy unknown. Retaining an older exact
            // turn would present stale state as authoritative.
            self.context_tokens = None;
            self.estimated_cost_usd_nanos = None;
            return;
        };
        // Context occupancy is the latest reported turn's input total, not a
        // sum: every model request re-sends the whole conversation, so the
        // last measured request describes what the context window held.
        self.context_tokens = Some(turn_context_tokens(usage));
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
            context_tokens: self.context_tokens,
            estimated_cost_usd_nanos: self
                .saw_turn
                .then_some(self.estimated_cost_usd_nanos)
                .flatten(),
            saw_turn: self.saw_turn,
        }
    }
}

/// The input-token total of one model turn (fresh input plus cache reads and
/// writes): what that turn's request occupied of the model context window.
const fn turn_context_tokens(usage: TokenUsage) -> u64 {
    usage
        .input_tokens
        .saturating_add(usage.cache_read_input_tokens)
        .saturating_add(usage.cache_write_input_tokens)
}

pub(super) fn add_usage(left: TokenUsage, right: TokenUsage) -> Option<TokenUsage> {
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

pub(super) fn internal_failure(message: &str) -> RunOutcome {
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
pub(super) fn persistence_failure(action: &str, error: &SessionRuntimeError) -> RunOutcome {
    match error {
        SessionRuntimeError::OutputTooLarge | SessionRuntimeError::ContextTooLarge => {
            context_budget_failure()
        }
        error => RunOutcome::Failed {
            failure: RunFailure {
                kind: RunFailureKind::Server,
                message: format!("{action}: {error}"),
            },
        },
    }
}

/// The deliberate context-budget policy failure. Since auto-compaction, a
/// prompt run reaches it only after one compaction attempt could not bring
/// the assembly back under the budget (or mid-run, when model output alone
/// pushes past it — never compacted mid-run).
fn context_budget_failure() -> RunOutcome {
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

fn planned_context_failure(plan: context::ContextPlan) -> RunOutcome {
    let Some(message) = context::rejection_message(plan) else {
        return internal_failure("context planner rejected a sendable request");
    };
    RunOutcome::Failed {
        failure: RunFailure {
            kind: RunFailureKind::Policy,
            message,
        },
    }
}

pub(super) struct ModelTurnCommit {
    pub(super) turn_ordinal: u16,
    pub(super) message: Message,
    pub(super) calls: Vec<RuntimeToolCall>,
    pub(super) turn_message: Option<MessageId>,
    pub(super) context_tokens: Option<u64>,
    pub(super) usage: Option<TokenUsage>,
    pub(super) estimated_cost_usd_nanos: Option<u64>,
    pub(super) accounting: Option<RunAccounting>,
}
