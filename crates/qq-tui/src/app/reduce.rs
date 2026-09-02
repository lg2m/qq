//! Reduces durable `SessionEvent`s into the client model: session summaries,
//! loaded transcript bodies, tool calls, live output tails, and the notices a
//! transition warrants. Every arm is idempotent against replay because the
//! caller has already deduplicated by cursor.

use qq_protocol::{
    MessageSnapshot, MessageState, RunOutcome, SessionEvent, SessionEventEnvelope, SessionId,
    SessionSummary, SnapshotRequest, ToolCallSnapshot, ToolCallState, WorkspaceGrantOutcome,
};

use super::{
    App, MAX_LIVE_TOOL_OUTPUT_BYTES, MAX_RECENT_TOOL_CALLS, PendingIntent, SNAPSHOT_MESSAGE_LIMIT,
    SNAPSHOT_SESSION_LIMIT, SessionView, format_bytes, model_context_window,
};
use crate::{
    ClientRequest,
    input::{Overlay, SessionConfirm},
};

impl App {
    pub(super) fn reduce_event(&mut self, envelope: &SessionEventEnvelope) {
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
                self.remove_session(*session_id);
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
            // Reasoning has its own display channel. Until that channel is
            // rendered, these events still update liveness via
            // RunActivityChanged and must not enter the assistant transcript.
            SessionEvent::ReasoningStarted { .. }
            | SessionEvent::ReasoningDelta { .. }
            | SessionEvent::ReasoningCompleted { .. } => {}
            SessionEvent::AssistantMessageStarted { message } => {
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
                tool_call, edit, ..
            } => {
                if tool_call.state != ToolCallState::AwaitingApproval {
                    self.answered_approvals.remove(&tool_call.id);
                }
                if let Some(edit) = edit {
                    self.edit_previews.insert(tool_call.id, edit.clone());
                }
                if let Some(session) = self.sessions.get_mut(&envelope.session_id) {
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
                self.append_live_tool_output(*tool_call_id, chunk);
            }
            SessionEvent::ToolCallRequested { tool_call }
            | SessionEvent::ToolApprovalResolved { tool_call, .. }
            | SessionEvent::ToolCallStarted { tool_call }
            | SessionEvent::ToolCallFinished { tool_call } => {
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
                    self.edit_previews.remove(&tool_call.id);
                }
                if tool_call_state_is_terminal(tool_call.state) {
                    // The persisted bounded result takes over from the tail.
                    self.live_tool_output.remove(&tool_call.id);
                }
                if let Some(session) = self.sessions.get_mut(&envelope.session_id) {
                    session.live.note_tool_call(tool_call);
                }
                self.upsert_tool_call(tool_call.clone());
            }
            // The follow-through of an approve-for-workspace decision. A
            // failure is informational: the session grant already stands.
            SessionEvent::WorkspaceGrantPromoted { outcome, .. } => {
                self.set_warning(match outcome {
                    WorkspaceGrantOutcome::Written { path } => {
                        format!("grant written to {path}")
                    }
                    WorkspaceGrantOutcome::AlreadyPresent { path } => {
                        format!("grant already present in {path}")
                    }
                    WorkspaceGrantOutcome::Failed { message } => {
                        format!("workspace grant not saved: {message}")
                    }
                });
            }
            SessionEvent::SessionCompacted {
                session,
                before_bytes,
                after_bytes,
                ..
            } => {
                self.upsert_summary(session.clone());
                self.set_info_for(
                    Some(envelope.session_id),
                    format!(
                        "compacted: {} -> {}",
                        format_bytes(*before_bytes),
                        format_bytes(*after_bytes)
                    ),
                );
            }
            SessionEvent::SessionCompactionRolledBack { session, remaining } => {
                self.upsert_summary(session.clone());
                self.set_info_for(
                    Some(envelope.session_id),
                    format!("compaction rolled back; {remaining} retained"),
                );
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
                ..
            } => {
                self.upsert_summary(session.clone());
                if let Some(view) = self.sessions.get_mut(&envelope.session_id) {
                    view.activity = None;
                    view.live.active_tool = None;
                    view.live.awaiting_approval.clear();
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
                    RunOutcome::Failed { failure } => {
                        self.set_error_for(Some(envelope.session_id), failure.message.clone());
                    }
                    RunOutcome::BudgetExhausted { exhaustion } => {
                        self.set_error_for(Some(envelope.session_id), exhaustion.message.clone());
                    }
                    RunOutcome::Completed | RunOutcome::Cancelled | RunOutcome::Interrupted => {}
                }
            }
        }
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
    }

    /// Drops a deleted session from every client map, mirroring the server's
    /// cascade: its children become roots, its per-call display state and
    /// optimistic prompts are discarded, and a deleted focus moves to the
    /// nearest remaining session (or clears).
    pub(super) fn remove_session(&mut self, session_id: SessionId) {
        if !self.sessions.contains_key(&session_id) {
            return;
        }
        let refocus = if self.focused == Some(session_id) {
            let order = self.thread_order();
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
            return;
        };
        for call in removed.tool_calls.iter().flatten() {
            self.live_tool_output.remove(&call.id);
            self.edit_previews.remove(&call.id);
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
        if self.focused == Some(session_id) {
            self.focused = refocus;
            if let (Some(next), Some(workspace_id)) = (refocus, self.workspace_id) {
                self.queued_requests
                    .push(ClientRequest::Snapshot(SnapshotRequest {
                        workspace_id,
                        focused_session_id: Some(next),
                        include_sessions: Vec::new(),
                        session_limit: SNAPSHOT_SESSION_LIMIT,
                        message_limit: SNAPSHOT_MESSAGE_LIMIT,
                    }));
            }
        }
        if let Some(Overlay::Sessions {
            selected, confirm, ..
        }) = &mut self.overlay
        {
            if matches!(confirm, Some(SessionConfirm::Delete(pending)) if *pending == session_id) {
                *confirm = None;
            }
            if *selected == Some(session_id) {
                *selected = None;
                self.reset_session_picker_selection();
            }
        }
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

    /// Appends one live output chunk to a call's tail buffer, dropping the
    /// oldest bytes past the bound. Trimming lands on a character boundary so
    /// a chunk split mid-UTF-8 sequence still renders sanely.
    fn append_live_tool_output(&mut self, tool_call_id: qq_protocol::ToolCallId, chunk: &str) {
        let buffer = self.live_tool_output.entry(tool_call_id).or_default();
        buffer.push_str(chunk);
        if buffer.len() > MAX_LIVE_TOOL_OUTPUT_BYTES {
            let mut start = buffer.len() - MAX_LIVE_TOOL_OUTPUT_BYTES;
            while !buffer.is_char_boundary(start) {
                start += 1;
            }
            buffer.drain(..start);
        }
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
