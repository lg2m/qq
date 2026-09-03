//! Reduces durable `SessionEvent`s into the client model: session summaries,
//! loaded transcript bodies, tool calls, live output tails, and the notices a
//! transition warrants. Every arm is idempotent against replay because the
//! caller has already deduplicated by cursor.

use qq_protocol::{
    MessageSnapshot, MessageState, RunOutcome, SessionEvent, SessionEventEnvelope, SessionId,
    SessionSummary, SnapshotRequest, ToolCallSnapshot, ToolCallState, WorkspaceGrantOutcome,
};

use super::{
    App, Attention, MAX_RECENT_TOOL_CALLS, NoticeLevel, PendingIntent, SNAPSHOT_MESSAGE_LIMIT,
    SNAPSHOT_SESSION_LIMIT, SessionView, format_bytes, model_context_window,
};
use crate::{
    ClientRequest,
    effect::{Effect, Effects},
    input::{Overlay, SessionConfirm},
    model::ApprovalPreview,
    viewport::View,
};

impl App {
    pub(super) fn reduce_event(&mut self, envelope: &SessionEventEnvelope) -> Effects {
        let mut effects = Effects::none();
        match &envelope.event {
            SessionEvent::SessionCreated { session } => {
                let mine = envelope
                    .caused_by
                    .and_then(|id| self.pending.get(&id))
                    .is_some_and(|intent| matches!(intent, PendingIntent::Create));
                self.upsert_summary(session.clone());
                if mine {
                    self.adopt_created_session(session.id);
                }
            }
            SessionEvent::SessionUpdated { session } => {
                self.upsert_summary(session.clone());
            }
            SessionEvent::SessionDeleted { session_id } => {
                effects.extend(self.remove_session(*session_id));
            }
            SessionEvent::PromptQueued {
                session, message, ..
            } => {
                self.upsert_summary(session.clone());
                self.push_message(message.clone());
            }
            SessionEvent::RunStarted { session, .. }
            | SessionEvent::CancellationRequested { session, .. } => {
                self.upsert_summary(session.clone());
                if let SessionEvent::RunStarted { run_id, .. } = &envelope.event
                    && let Some(view) = self.sessions.get_mut(&envelope.session_id)
                {
                    let cost_before = view
                        .summary
                        .accounting
                        .map(|accounting| accounting.direct.estimated_cost_usd_nanos)
                        .unwrap_or(view.summary.estimated_cost_usd_nanos);
                    let stats = view.runs.entry(*run_id).or_default();
                    stats.started_at_ms = Some(envelope.occurred_at_ms);
                    stats.cost_usd_nanos = cost_before;
                }
                if let SessionEvent::RunStarted { run_id, .. } = &envelope.event
                    && let Some(messages) = self
                        .sessions
                        .get_mut(&envelope.session_id)
                        .and_then(|session| session.messages.as_mut())
                {
                    for message in messages.iter_mut().filter(|message| {
                        message.run_id == *run_id && message.role == qq_protocol::MessageRole::User
                    }) {
                        message.state = MessageState::Complete;
                    }
                }
            }
            SessionEvent::RunActivityChanged { run_id, activity } => {
                if let Some(session) = self.sessions.get_mut(&envelope.session_id) {
                    session.activity = Some((*run_id, *activity));
                }
            }
            // Reasoning is display-only and must never enter the assistant
            // transcript. It accumulates per run for warm sessions so the
            // collapsed row above the run's message can expand on demand.
            SessionEvent::ReasoningStarted { run_id, .. } => {
                if let Some(session) = self
                    .sessions
                    .get_mut(&envelope.session_id)
                    .filter(|session| session.is_warm())
                {
                    session.reasoning.entry(*run_id).or_default().streaming = true;
                }
            }
            SessionEvent::ReasoningDelta { run_id, text, .. } => {
                if let Some(session) = self
                    .sessions
                    .get_mut(&envelope.session_id)
                    .filter(|session| session.is_warm())
                {
                    let reasoning = session.reasoning.entry(*run_id).or_default();
                    reasoning.streaming = true;
                    reasoning.append(text);
                }
            }
            SessionEvent::ReasoningCompleted { run_id, .. } => {
                if let Some(reasoning) = self
                    .sessions
                    .get_mut(&envelope.session_id)
                    .and_then(|session| session.reasoning.get_mut(run_id))
                {
                    reasoning.streaming = false;
                }
            }
            SessionEvent::AssistantMessageStarted { message } => {
                let shown = self.focused() == Some(envelope.session_id);
                if !shown && let Some(view) = self.sessions.get_mut(&envelope.session_id) {
                    view.unread += 1;
                }
                // A new turn's message means every earlier turn of the run
                // has committed; the server finalized those messages inside
                // the turn persist without a dedicated event.
                self.complete_streamed_turns(
                    envelope.session_id,
                    message.run_id,
                    message.turn_ordinal.saturating_sub(1),
                );
                if let Some(session) = self.sessions.get_mut(&envelope.session_id) {
                    session.live.set_tail(&message.output);
                }
                self.push_message(message.clone());
            }
            SessionEvent::TextAppended {
                message_id,
                channel,
                text,
            } => {
                if let Some(run_id) = envelope.run_id
                    && let Some(view) = self.sessions.get_mut(&envelope.session_id)
                {
                    let stats = view.runs.entry(run_id).or_default();
                    if stats.first_token_at_ms.is_none() {
                        stats.first_token_at_ms = Some(envelope.occurred_at_ms);
                    }
                }
                // Live status reduces for every session, warm or cold, so the
                // sidebar tracks children the user is not looking at.
                if *channel == qq_protocol::TextChannel::Output
                    && let Some(session) = self.sessions.get_mut(&envelope.session_id)
                {
                    session.live.append_tail(text);
                }
                if let Some(message) = self.message_mut(envelope.session_id, *message_id) {
                    match channel {
                        qq_protocol::TextChannel::Output => message.output.push_str(text),
                        qq_protocol::TextChannel::Refusal => message.refusal.push_str(text),
                    }
                }
            }
            SessionEvent::ToolApprovalRequested {
                tool_call,
                shell,
                edit,
            } => {
                if tool_call.state != ToolCallState::AwaitingApproval {
                    self.answered_approvals.remove(&tool_call.id);
                } else if let Some(session) = self.sessions.get(&envelope.session_id) {
                    effects.extend(self.attention(Attention::ApprovalRequested {
                        session_title: session.summary.title.clone(),
                    }));
                }
                if let Some(session) = self.sessions.get_mut(&envelope.session_id) {
                    if shell.is_some() || edit.is_some() {
                        session.approval_previews.insert(
                            tool_call.id,
                            ApprovalPreview {
                                shell: shell.clone(),
                                edit: edit.clone(),
                            },
                        );
                    }
                    session.live.note_tool_call(tool_call);
                }
                self.upsert_tool_call(tool_call.clone());
            }
            // Live tool output chunks are display-only: they feed the bounded
            // tail under a running call's line, and the call's authoritative
            // bounded result arrives on ToolCallFinished regardless.
            SessionEvent::ToolCallOutputDelta {
                tool_call_id,
                chunk,
            } => {
                if let Some(session) = self.sessions.get_mut(&envelope.session_id) {
                    session.append_live_tool_output(*tool_call_id, chunk);
                    session
                        .tool_timing
                        .entry(*tool_call_id)
                        .or_default()
                        .last_output_at_ms = Some(envelope.occurred_at_ms);
                }
            }
            SessionEvent::ToolCallRequested { tool_call }
            | SessionEvent::ToolApprovalResolved { tool_call, .. }
            | SessionEvent::ToolCallStarted { tool_call }
            | SessionEvent::ToolCallFinished { tool_call } => {
                if let Some(view) = self.sessions.get_mut(&envelope.session_id) {
                    let timing = view.tool_timing.entry(tool_call.id).or_default();
                    match &envelope.event {
                        SessionEvent::ToolCallStarted { .. } => {
                            timing.started_at_ms = Some(envelope.occurred_at_ms);
                        }
                        SessionEvent::ToolCallFinished { .. } => {
                            timing.finished_at_ms = Some(envelope.occurred_at_ms);
                            view.runs.entry(tool_call.run_id).or_default().tool_calls += 1;
                        }
                        _ => {}
                    }
                }
                if matches!(envelope.event, SessionEvent::ToolCallRequested { .. }) {
                    // Calls are persisted with their completed turn, so the
                    // turn's message (same ordinal) is finalized by then.
                    self.complete_streamed_turns(
                        envelope.session_id,
                        tool_call.run_id,
                        tool_call.turn_ordinal,
                    );
                }
                if tool_call.state != ToolCallState::AwaitingApproval {
                    self.answered_approvals.remove(&tool_call.id);
                }
                if let Some(session) = self.sessions.get_mut(&envelope.session_id) {
                    if tool_call.state != ToolCallState::AwaitingApproval {
                        session.approval_previews.remove(&tool_call.id);
                    }
                    if tool_call_state_is_terminal(tool_call.state) {
                        // The persisted bounded result takes over from the tail.
                        session.live_tool_output.remove(&tool_call.id);
                    }
                    session.live.note_tool_call(tool_call);
                }
                self.upsert_tool_call(tool_call.clone());
            }
            // Steering rows are user messages of the active run; the TUI
            // renders them in transcript order and tracks their state. The
            // composer sends them (`App::steer_run`); the row is installed
            // from the event so it is durable before it is shown.
            SessionEvent::SteeringQueued { message, .. } => {
                self.push_message(message.clone());
            }
            SessionEvent::SteeringApplied { message_id, .. }
            | SessionEvent::SteeringSuperseded { message_id, .. } => {
                let state = match &envelope.event {
                    SessionEvent::SteeringApplied { .. } => MessageState::Complete,
                    _ => MessageState::Cancelled,
                };
                if let Some(messages) = self
                    .sessions
                    .get_mut(&envelope.session_id)
                    .and_then(|session| session.messages.as_mut())
                    && let Some(message) = messages
                        .iter_mut()
                        .find(|message| message.id == *message_id)
                {
                    message.state = state;
                }
            }
            // The interrupted turn's tool calls arrive as ordinary finished
            // events; nothing else changes on screen.
            SessionEvent::RunInterrupted { .. } => {}
            // The follow-through of an approve-for-workspace decision. A
            // failure is informational: the session grant already stands.
            SessionEvent::WorkspaceGrantPromoted { outcome, .. } => {
                effects.push(Effect::Notice {
                    session: None,
                    level: NoticeLevel::Warning,
                    text: match outcome {
                        WorkspaceGrantOutcome::Written { path } => {
                            format!("grant written to {path}")
                        }
                        WorkspaceGrantOutcome::AlreadyPresent { path } => {
                            format!("grant already present in {path}")
                        }
                        WorkspaceGrantOutcome::Failed { message } => {
                            format!("workspace grant not saved: {message}")
                        }
                    },
                });
            }
            SessionEvent::SessionCompacted {
                session,
                before_bytes,
                after_bytes,
                ..
            } => {
                self.upsert_summary(session.clone());
                effects.push(Effect::Notice {
                    session: Some(envelope.session_id),
                    level: NoticeLevel::Info,
                    text: format!(
                        "compacted: {} -> {}",
                        format_bytes(*before_bytes),
                        format_bytes(*after_bytes)
                    ),
                });
            }
            SessionEvent::SessionCompactionRolledBack { session, remaining } => {
                self.upsert_summary(session.clone());
                effects.push(Effect::Notice {
                    session: Some(envelope.session_id),
                    level: NoticeLevel::Info,
                    text: format!("compaction rolled back; {remaining} retained"),
                });
            }
            // Run-level audit updates are not session state. In particular,
            // old persisted events may predate the authoritative session
            // field, so replaying one must not repopulate the meter.
            SessionEvent::ModelTurnCompleted { .. } | SessionEvent::RunContextUpdated { .. } => {}
            SessionEvent::SessionContextUpdated { context_tokens, .. } => {
                if let Some(session) = self.sessions.get_mut(&envelope.session_id) {
                    session.summary.context_tokens = *context_tokens;
                }
            }
            SessionEvent::RunFinished {
                session,
                run_id,
                outcome,
                usage,
                ..
            } => {
                let shown = self.focused() == Some(envelope.session_id);
                if !shown && let Some(view) = self.sessions.get_mut(&envelope.session_id) {
                    view.finished_unread = true;
                    view.unread += 1;
                }
                let cost_before = self
                    .sessions
                    .get(&envelope.session_id)
                    .and_then(|view| view.runs.get(run_id))
                    .and_then(|stats| stats.cost_usd_nanos);
                self.upsert_summary(session.clone());
                if let Some(view) = self.sessions.get_mut(&envelope.session_id) {
                    let cost_after = session
                        .accounting
                        .map(|accounting| accounting.direct.estimated_cost_usd_nanos)
                        .unwrap_or(session.estimated_cost_usd_nanos);
                    let stats = view.runs.entry(*run_id).or_default();
                    stats.finished_at_ms = Some(envelope.occurred_at_ms);
                    stats.outcome = Some(outcome.clone());
                    stats.usage = *usage;
                    stats.cost_usd_nanos = match (cost_before, cost_after) {
                        (Some(before), Some(after)) => Some(after.saturating_sub(before)),
                        _ => None,
                    };
                }
                effects.extend(self.attention(Attention::RunFinished {
                    session_title: session.title.clone(),
                }));
                if let Some(view) = self.sessions.get_mut(&envelope.session_id) {
                    view.activity = None;
                    view.live.active_tool = None;
                    view.live.awaiting_approval.clear();
                }
                if let Some(view) = self.sessions.get_mut(&envelope.session_id) {
                    if let Some(reasoning) = view.reasoning.get_mut(run_id) {
                        reasoning.streaming = false;
                    }
                    // Reasoning for runs whose messages were trimmed away is
                    // unreachable from the transcript; drop it.
                    if let Some(messages) = view.messages.as_ref() {
                        let retained: std::collections::HashSet<_> =
                            messages.iter().map(|message| message.run_id).collect();
                        view.reasoning.retain(|run, _| retained.contains(run));
                        view.runs.retain(|run, _| retained.contains(run));
                    }
                }
                if session.active_run_id.is_none() && session.queued_prompts == 0 {
                    effects.extend(self.flush_draft(envelope.session_id));
                }
                if let Some(messages) = self
                    .sessions
                    .get_mut(&envelope.session_id)
                    .and_then(|session| session.messages.as_mut())
                {
                    let state = match outcome {
                        RunOutcome::Completed => MessageState::Complete,
                        RunOutcome::Cancelled => MessageState::Cancelled,
                        RunOutcome::Interrupted | RunOutcome::BudgetExhausted { .. } => {
                            MessageState::Interrupted
                        }
                        RunOutcome::Failed { .. } => MessageState::Failed,
                    };
                    for message in messages
                        .iter_mut()
                        .filter(|message| message.run_id == *run_id)
                    {
                        // Turns finalized before the run ended keep their own
                        // state; the outcome only settles the still-streaming
                        // current turn (and queued rows).
                        let settled = message.role == qq_protocol::MessageRole::Assistant
                            && !matches!(
                                message.state,
                                MessageState::Queued | MessageState::Streaming
                            );
                        if !settled
                            && (message.role == qq_protocol::MessageRole::Assistant
                                || message.state == MessageState::Queued)
                        {
                            message.state = state;
                        }
                    }
                }
                match outcome {
                    RunOutcome::Failed { failure } => effects.push(Effect::Notice {
                        session: Some(envelope.session_id),
                        level: NoticeLevel::Error,
                        text: failure.message.clone(),
                    }),
                    RunOutcome::BudgetExhausted { exhaustion } => effects.push(Effect::Notice {
                        session: Some(envelope.session_id),
                        level: NoticeLevel::Error,
                        text: exhaustion.message.clone(),
                    }),
                    RunOutcome::Completed | RunOutcome::Cancelled | RunOutcome::Interrupted => {}
                }
            }
        }
        effects
    }

    pub(super) fn upsert_summary(&mut self, summary: SessionSummary) {
        let context_window = model_context_window(&self.models, summary.model.as_deref());
        match self.sessions.entry(summary.id) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().set_summary(summary, context_window);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(SessionView::summary_only(summary, context_window, 0));
            }
        }
        self.refresh_session_picker();
    }

    /// Drops a deleted session from every client map, mirroring the server's
    /// cascade: its children become roots, its per-call display state and
    /// optimistic prompts are discarded, and a deleted focus moves to the
    /// nearest remaining session (or clears).
    pub(super) fn remove_session(&mut self, session_id: SessionId) -> Effects {
        let mut effects = Effects::none();
        if !self.sessions.contains_key(&session_id) {
            return effects;
        }
        // A shown deleted session gives way to its neighbour in thread order;
        // the replacement is fetched if cold.
        let showing = self.focused() == Some(session_id);
        let refocus = if showing {
            let order = self.sessions.thread_order();
            order
                .iter()
                .position(|candidate| *candidate == session_id)
                .and_then(|index| {
                    order
                        .get(index + 1)
                        .or_else(|| {
                            index
                                .checked_sub(1)
                                .and_then(|previous| order.get(previous))
                        })
                        .copied()
                })
        } else {
            None
        };
        let Some(removed) = self.sessions.remove(&session_id) else {
            return effects;
        };
        for call in removed.tool_calls.iter().flatten() {
            self.answered_approvals.remove(&call.id);
        }
        self.pending.retain(|_, intent| {
            !matches!(
                intent,
                PendingIntent::Prompt { session_id: target, .. } if *target == session_id
            )
        });
        // The server detaches children on delete; mirror it so they stay
        // reachable as roots until the next summary refresh.
        for session in self.sessions.values_mut() {
            if session.summary.parent_id == Some(session_id) {
                session.summary.parent_id = None;
            }
        }
        if showing {
            self.view = View::Transcript(refocus);
            let warm = refocus
                .and_then(|next| self.sessions.get(&next))
                .is_some_and(SessionView::is_warm);
            if let (Some(next), Some(workspace_id), false) = (refocus, self.workspace_id, warm) {
                effects.push(Effect::Send(ClientRequest::Snapshot(SnapshotRequest {
                    workspace_id,
                    focused_session_id: Some(next),
                    include_sessions: Vec::new(),
                    session_limit: SNAPSHOT_SESSION_LIMIT,
                    message_limit: SNAPSHOT_MESSAGE_LIMIT,
                })));
            }
        }
        if let Some(Overlay::Sessions { confirm, .. }) = &mut self.overlay
            && matches!(confirm, Some(SessionConfirm::Delete(pending)) if *pending == session_id)
        {
            *confirm = None;
        }
        self.refresh_session_picker();
        effects
    }

    /// Marks a run's still-streaming assistant messages complete through the
    /// given turn: the server finalizes a turn's message in the same
    /// transaction as the turn's tool calls, without a dedicated event.
    fn complete_streamed_turns(
        &mut self,
        session_id: SessionId,
        run_id: qq_protocol::RunId,
        through_turn: u16,
    ) {
        let Some(messages) = self
            .sessions
            .get_mut(&session_id)
            .and_then(|session| session.messages.as_mut())
        else {
            return;
        };
        for message in messages.iter_mut().filter(|message| {
            message.run_id == run_id
                && message.role == qq_protocol::MessageRole::Assistant
                && message.state == MessageState::Streaming
                && message.turn_ordinal <= through_turn
        }) {
            message.state = MessageState::Complete;
        }
    }

    pub(super) fn push_message(&mut self, message: MessageSnapshot) {
        let Some(messages) = self
            .sessions
            .get_mut(&message.session_id)
            .and_then(|session| session.messages.as_mut())
        else {
            return;
        };
        if messages
            .iter()
            .rev()
            .any(|candidate| candidate.id == message.id)
        {
            return;
        }
        // Server snapshots order messages by run first, then by ordinal
        // within the run, so a prompt queued mid-run sorts after that run's
        // later per-turn messages. Mirror that live: a message whose run is
        // already present slots in right after the run's last message, and a
        // new run appends (runs are created in queue order).
        let position = messages
            .iter()
            .rposition(|candidate| candidate.run_id == message.run_id)
            .map_or(messages.len(), |index| index + 1);
        messages.insert(position, message);
        retain_recent_messages(messages);
    }

    /// The streaming message is nearly always the newest, so scan from the
    /// tail: a text delta then costs one comparison rather than a walk over
    /// the retained history.
    fn message_mut(
        &mut self,
        session_id: SessionId,
        message_id: qq_protocol::MessageId,
    ) -> Option<&mut MessageSnapshot> {
        self.sessions
            .get_mut(&session_id)?
            .messages
            .as_mut()?
            .iter_mut()
            .rev()
            .find(|message| message.id == message_id)
    }

    pub(super) fn upsert_tool_call(&mut self, tool_call: ToolCallSnapshot) {
        let Some(tool_calls) = self
            .sessions
            .get_mut(&tool_call.session_id)
            .and_then(|session| session.tool_calls.as_mut())
        else {
            return;
        };
        // Updates target recent calls; scan from the tail.
        if let Some(existing) = tool_calls
            .iter_mut()
            .rev()
            .find(|existing| existing.id == tool_call.id)
        {
            *existing = tool_call;
        } else {
            tool_calls.push(tool_call);
            retain_recent_tool_calls(tool_calls);
        }
    }
}

pub(super) fn retain_recent_messages(messages: &mut Vec<MessageSnapshot>) {
    let excess = messages
        .len()
        .saturating_sub(usize::from(SNAPSHOT_MESSAGE_LIMIT));
    if excess > 0 {
        messages.drain(..excess);
    }
}

const fn tool_call_state_is_terminal(state: ToolCallState) -> bool {
    match state {
        ToolCallState::Completed
        | ToolCallState::Failed
        | ToolCallState::Denied
        | ToolCallState::Interrupted => true,
        ToolCallState::Requested | ToolCallState::AwaitingApproval | ToolCallState::Running => {
            false
        }
    }
}

pub(super) fn retain_recent_tool_calls(tool_calls: &mut Vec<ToolCallSnapshot>) {
    let excess = tool_calls.len().saturating_sub(MAX_RECENT_TOOL_CALLS);
    if excess > 0 {
        tool_calls.drain(..excess);
    }
}
