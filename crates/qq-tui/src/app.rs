use std::collections::{HashMap, VecDeque};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use qq_protocol::{
    ApprovalDecision, ApprovalGrant, ApprovalMode, ApprovalResolution, CommandId, CommandOutcome,
    CommandRequest, EditPreview, MessageSnapshot, MessageState, ModelDescriptor, ModelSelection,
    RunOutcome, SessionCommand, SessionEvent, SessionEventEnvelope, SessionId, SessionSnapshot,
    SessionStatus, SessionSummary, SnapshotRequest, TokenUsage, ToolCallSnapshot, ToolCallState,
    WorkspaceId, WorkspaceSnapshot,
};
use thiserror::Error;

use crate::{
    Action, ClientFailure, ClientPort, ClientRequest, ClientUpdate, ConnectionState, Layout,
    Settings, terminal,
};

const MAX_INPUT_BYTES: usize = 64 * 1024;
const MAX_PICKER_SEARCH_BYTES: usize = 256;
const MAX_RECENT_EVENTS: usize = 1024;
const SNAPSHOT_SESSION_LIMIT: u16 = 512;
const SNAPSHOT_MESSAGE_LIMIT: u16 = 256;
const MAX_RECENT_TOOL_CALLS: usize = 64;
/// Per-call cap on buffered live tool output. The buffer is a display tail,
/// not a record: the head drops first, and the persisted bounded result
/// replaces the buffer when the call finishes.
const MAX_LIVE_TOOL_OUTPUT_BYTES: usize = 4 * 1024;
const MOUSE_SCROLL_ROWS: usize = 3;

pub(crate) struct SlashCommand {
    pub name: &'static str,
    pub description: &'static str,
    action: SlashAction,
}

#[derive(Clone, Copy)]
enum SlashAction {
    Models,
    New,
    Sessions,
    Compact,
    Quit,
}

const SLASH_COMMANDS: [SlashCommand; 7] = [
    SlashCommand {
        name: "/models",
        description: "choose a model",
        action: SlashAction::Models,
    },
    SlashCommand {
        name: "/sessions",
        description: "open sessions",
        action: SlashAction::Sessions,
    },
    SlashCommand {
        name: "/resume",
        description: "open sessions",
        action: SlashAction::Sessions,
    },
    SlashCommand {
        name: "/new",
        description: "create a session",
        action: SlashAction::New,
    },
    SlashCommand {
        name: "/compact",
        description: "compact session context",
        action: SlashAction::Compact,
    },
    SlashCommand {
        name: "/quit",
        description: "exit QQ",
        action: SlashAction::Quit,
    },
    SlashCommand {
        name: "/exit",
        description: "exit QQ",
        action: SlashAction::Quit,
    },
];

#[derive(Debug, Clone, Default)]
pub struct TuiOptions {
    pub settings: Settings,
    pub model: ModelSelection,
    pub models: Vec<ModelOption>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOption {
    pub provider: String,
    pub model: String,
    pub name: Option<String>,
    pub context_window: Option<u32>,
    pub selection: ModelSelection,
}

impl From<ModelDescriptor> for ModelOption {
    fn from(descriptor: ModelDescriptor) -> Self {
        Self {
            provider: descriptor.provider,
            model: descriptor.model,
            name: descriptor.name,
            context_window: descriptor.context_window,
            selection: descriptor.selection,
        }
    }
}

pub async fn run<P>(client: P, options: TuiOptions) -> Result<(), TuiError>
where
    P: ClientPort,
{
    terminal::run(client, App::new(options)).await
}

#[derive(Debug, Error)]
pub enum TuiError {
    #[error("terminal I/O failed")]
    Terminal(#[from] std::io::Error),
    #[error("TUI client stopped")]
    ClientStopped,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionView {
    pub summary: SessionSummary,
    pub messages: Option<Vec<MessageSnapshot>>,
    pub tool_calls: Option<Vec<ToolCallSnapshot>>,
    pub latest_input_tokens: Option<u64>,
    pub context_window: Option<u32>,
    loaded_through: u64,
}

pub(crate) struct ModelPicker {
    pub query: String,
    pub selected: usize,
}

pub(crate) struct SessionPicker {
    pub query: String,
    pub selected: Option<SessionId>,
    /// A destructive action awaiting its inline y/n answer.
    pub confirm: Option<SessionPickerConfirm>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionPickerConfirm {
    Delete(SessionId),
    Prune,
}

/// How much of each tool call the transcript shows. Session-local because the
/// persisted settings surface only carries keybindings and layout today.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ToolDetail {
    #[default]
    Collapsed,
    Expanded,
}

impl ToolDetail {
    #[must_use]
    pub(crate) const fn next(self) -> Self {
        match self {
            Self::Collapsed => Self::Expanded,
            Self::Expanded => Self::Collapsed,
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Collapsed => "collapsed",
            Self::Expanded => "expanded",
        }
    }
}

#[derive(Debug, Default)]
struct TranscriptViewport {
    context: Option<(Option<SessionId>, Layout)>,
    body_rows: usize,
    height: usize,
    offset: usize,
}

#[derive(Debug, Clone)]
enum PendingIntent {
    Create,
    Prompt {
        session_id: SessionId,
        text: String,
    },
    Cancel,
    Compact,
    Approval {
        tool_call_id: qq_protocol::ToolCallId,
    },
}

pub(crate) struct App {
    pub settings: Settings,
    pub layout: Layout,
    pub model: ModelSelection,
    pub models: Vec<ModelOption>,
    pub workspace_id: Option<WorkspaceId>,
    pub workspace_path: String,
    pub sessions: HashMap<SessionId, SessionView>,
    pub focused: Option<SessionId>,
    pub session_picker: Option<SessionPicker>,
    pub model_picker: Option<ModelPicker>,
    pub input: String,
    slash_selected: usize,
    pub connection: ConnectionState,
    pub status: Option<String>,
    pub animation_tick: usize,
    pub quit: bool,
    pub tool_detail: ToolDetail,
    transcript_viewport: TranscriptViewport,
    last_sequence: u64,
    recent_events: VecDeque<SessionEventEnvelope>,
    pending: HashMap<CommandId, PendingIntent>,
    answered_approvals: std::collections::HashSet<qq_protocol::ToolCallId>,
    /// Diff previews carried by approval requests, kept only while the call
    /// awaits an answer so the modal can show what an edit would change.
    edit_previews: HashMap<qq_protocol::ToolCallId, EditPreview>,
    /// Bounded tails of live streamed output per running tool call, dropped
    /// when the call reaches a terminal state or the session state reloads.
    pub live_tool_output: HashMap<qq_protocol::ToolCallId, String>,
    /// Requests produced while applying client updates (for example the
    /// snapshot fetch after a remote deletion refocuses), drained by the
    /// terminal loop after each update.
    queued_requests: Vec<ClientRequest>,
}

impl App {
    pub(crate) fn new(options: TuiOptions) -> Self {
        Self {
            layout: options.settings.initial_layout(),
            settings: options.settings,
            model: options.model,
            models: options.models,
            workspace_id: None,
            workspace_path: String::new(),
            sessions: HashMap::new(),
            focused: None,
            session_picker: None,
            model_picker: None,
            input: String::new(),
            slash_selected: 0,
            connection: ConnectionState::Connecting,
            status: None,
            animation_tick: 0,
            quit: false,
            tool_detail: ToolDetail::default(),
            transcript_viewport: TranscriptViewport::default(),
            last_sequence: 0,
            recent_events: VecDeque::new(),
            pending: HashMap::new(),
            answered_approvals: std::collections::HashSet::new(),
            edit_previews: HashMap::new(),
            live_tool_output: HashMap::new(),
            queued_requests: Vec::new(),
        }
    }

    /// Requests queued by [`Self::apply_client_update`]; the terminal loop
    /// drains and sends them after each update.
    pub fn take_requests(&mut self) -> Vec<ClientRequest> {
        std::mem::take(&mut self.queued_requests)
    }

    pub fn apply_client_update(&mut self, update: ClientUpdate) -> bool {
        match update {
            ClientUpdate::Connection(connection) => {
                self.connection = connection;
                true
            }
            ClientUpdate::Snapshot(snapshot) => self.apply_snapshot(snapshot),
            ClientUpdate::ResetSnapshot(snapshot) => {
                self.workspace_id = None;
                self.workspace_path.clear();
                self.sessions.clear();
                self.focused = None;
                self.session_picker = None;
                self.model_picker = None;
                self.last_sequence = 0;
                self.recent_events.clear();
                self.edit_previews.clear();
                self.live_tool_output.clear();
                self.status = Some("session state reset after reconnecting".to_owned());
                self.apply_snapshot(snapshot)
            }
            ClientUpdate::Models { models, selected } => {
                self.apply_models(models, selected);
                true
            }
            ClientUpdate::Event(event) => self.apply_live_event(event),
            ClientUpdate::CommandResult { command_id, result } => {
                match result {
                    Ok(receipt) => {
                        let intent = self.pending.remove(&command_id);
                        if let CommandOutcome::SessionCreated { session_id } = receipt.outcome
                            && intent
                                .as_ref()
                                .is_some_and(|intent| matches!(intent, PendingIntent::Create))
                        {
                            self.focused = Some(session_id);
                        }
                        if matches!(intent, Some(PendingIntent::Cancel)) {
                            self.status = Some("cancellation requested".to_owned());
                        }
                        if let CommandOutcome::ToolApprovalResolved { resolution, .. } =
                            receipt.outcome
                        {
                            self.status = Some(
                                match resolution {
                                    ApprovalResolution::ApprovedOnce => "tool call approved",
                                    ApprovalResolution::ApprovedForSession => {
                                        "tool call approved for this session"
                                    }
                                    ApprovalResolution::Denied => "tool call denied",
                                    ApprovalResolution::DeniedTimeout => {
                                        "tool call already denied by timeout"
                                    }
                                }
                                .to_owned(),
                            );
                        }
                        if matches!(receipt.outcome, CommandOutcome::RunAlreadyFinished { .. }) {
                            self.status = Some("run already finished".to_owned());
                        }
                        match &receipt.outcome {
                            CommandOutcome::CompactionQueued { .. } => {
                                self.status = Some("compacting session...".to_owned());
                            }
                            CommandOutcome::SessionModelSet { model, .. } => {
                                self.status = Some(format!(
                                    "session model set to {}",
                                    model.model.as_deref().unwrap_or("default")
                                ));
                            }
                            CommandOutcome::SessionDeleted { .. } => {
                                self.status = Some("session deleted".to_owned());
                            }
                            CommandOutcome::SessionsPruned { deleted: 0, .. } => {
                                self.status = Some("no empty sessions to delete".to_owned());
                            }
                            CommandOutcome::SessionsPruned { deleted: 1, .. } => {
                                self.status = Some("deleted 1 empty session".to_owned());
                            }
                            CommandOutcome::SessionsPruned { deleted, .. } => {
                                self.status = Some(format!("deleted {deleted} empty sessions"));
                            }
                            _ => {}
                        }
                    }
                    Err(error) => self.reject_pending(command_id, error),
                }
                true
            }
            ClientUpdate::SnapshotFailed(error) => {
                self.status = Some(error.message().to_owned());
                true
            }
        }
    }

    fn apply_models(
        &mut self,
        models: Vec<ModelDescriptor>,
        selected_model: Option<ModelSelection>,
    ) {
        let selected = self.model_picker.as_ref().and_then(|picker| {
            self.filtered_models()
                .get(picker.selected)
                .and_then(|index| self.models.get(*index))
                .map(|model| (model.provider.clone(), model.model.clone()))
        });
        self.models = models.into_iter().map(Into::into).collect();
        self.models.sort_by(|left, right| {
            (&left.provider, &left.name, &left.model).cmp(&(
                &right.provider,
                &right.name,
                &right.model,
            ))
        });
        if let Some(selected_model) = selected_model {
            self.model = selected_model;
        }
        for session in self.sessions.values_mut() {
            session.context_window =
                model_context_window(&self.models, session.summary.model.as_deref());
        }
        if self.model_picker.is_some() {
            let filtered = self.filtered_models();
            let selected = selected.and_then(|selected| {
                filtered.iter().position(|index| {
                    self.models.get(*index).is_some_and(|model| {
                        (model.provider.as_str(), model.model.as_str())
                            == (selected.0.as_str(), selected.1.as_str())
                    })
                })
            });
            if let Some(picker) = &mut self.model_picker {
                picker.selected = selected.unwrap_or(0).min(filtered.len().saturating_sub(1));
            }
        }
    }

    fn apply_snapshot(&mut self, snapshot: WorkspaceSnapshot) -> bool {
        let initial = self.workspace_id.is_none();
        if self
            .workspace_id
            .is_some_and(|workspace| workspace != snapshot.workspace.id)
        {
            self.status = Some("server returned a snapshot for another workspace".to_owned());
            return true;
        }
        let snapshot_focus = snapshot.focused.as_ref().map(|focused| focused.summary.id);
        if !initial
            && self.focused.is_some()
            && snapshot_focus.is_some()
            && snapshot_focus != self.focused
        {
            return false;
        }
        if snapshot.cursor.sequence < self.last_sequence
            && self
                .recent_events
                .front()
                .is_none_or(|event| event.cursor.sequence > snapshot.cursor.sequence + 1)
        {
            self.status = Some("snapshot was too stale; reconnecting is required".to_owned());
            return true;
        }

        let snapshot_sequence = snapshot.cursor.sequence;
        if initial {
            self.workspace_id = Some(snapshot.workspace.id);
            self.workspace_path = snapshot.workspace.path;
        }
        if initial || snapshot_sequence >= self.last_sequence {
            for summary in snapshot.sessions {
                let context_window = model_context_window(&self.models, summary.model.as_deref());
                self.sessions
                    .entry(summary.id)
                    .and_modify(|session| {
                        session.summary = summary.clone();
                        session.context_window = context_window;
                    })
                    .or_insert(SessionView {
                        summary,
                        messages: None,
                        tool_calls: None,
                        latest_input_tokens: None,
                        context_window,
                        loaded_through: snapshot_sequence,
                    });
            }
        }
        if let Some(focused) = snapshot.focused {
            let focused_id = focused.summary.id;
            self.install_session_snapshot(focused, snapshot_sequence);
            self.focused = Some(focused_id);
        } else if self.focused.is_none() {
            self.focused = self.root_sessions().first().copied();
        }
        if initial {
            self.last_sequence = snapshot_sequence;
        }
        let replay = self
            .recent_events
            .iter()
            .filter(|event| {
                event.cursor.sequence > snapshot_sequence
                    && snapshot_focus.is_some_and(|focused| event.session_id == focused)
            })
            .cloned()
            .collect::<Vec<_>>();
        for event in replay {
            self.reduce_event(&event);
        }
        true
    }

    fn install_session_snapshot(&mut self, snapshot: SessionSnapshot, loaded_through: u64) {
        for session in self.sessions.values_mut() {
            session.messages = None;
            session.tool_calls = None;
        }
        // A snapshot replaces live per-call state wholesale; buffered output
        // for calls it no longer reports as running would render forever.
        self.live_tool_output.clear();
        let mut messages = snapshot.messages;
        retain_recent_messages(&mut messages);
        let mut tool_calls = snapshot.tool_calls;
        retain_recent_tool_calls(&mut tool_calls);
        let latest_input_tokens = snapshot
            .runs
            .iter()
            .rev()
            .find_map(|run| run.usage.map(total_input_tokens));
        let context_window = model_context_window(&self.models, snapshot.summary.model.as_deref());
        self.sessions.insert(
            snapshot.summary.id,
            SessionView {
                summary: snapshot.summary,
                messages: Some(messages),
                tool_calls: Some(tool_calls),
                latest_input_tokens,
                context_window,
                loaded_through,
            },
        );
    }

    fn apply_live_event(&mut self, event: SessionEventEnvelope) -> bool {
        if self
            .workspace_id
            .is_some_and(|workspace| workspace != event.cursor.workspace_id)
        {
            self.status = Some("server sent an event for another workspace".to_owned());
            return true;
        }
        if event.cursor.sequence <= self.last_sequence {
            return false;
        }
        if self.last_sequence != 0 && event.cursor.sequence != self.last_sequence + 1 {
            self.connection = ConnectionState::Replaying;
            self.status = Some("session event gap detected".to_owned());
            return true;
        }
        self.workspace_id.get_or_insert(event.cursor.workspace_id);
        self.last_sequence = event.cursor.sequence;
        let already_loaded = self
            .sessions
            .get(&event.session_id)
            .is_some_and(|session| event.cursor.sequence <= session.loaded_through);
        if !already_loaded {
            self.reduce_event(&event);
        }
        if let Some(command_id) = event.caused_by {
            self.pending.remove(&command_id);
        }
        self.recent_events.push_back(event);
        while self.recent_events.len() > MAX_RECENT_EVENTS {
            self.recent_events.pop_front();
        }
        true
    }

    fn reduce_event(&mut self, envelope: &SessionEventEnvelope) {
        match &envelope.event {
            SessionEvent::SessionCreated { session } => {
                self.upsert_summary(session.clone());
                if envelope
                    .caused_by
                    .and_then(|id| self.pending.get(&id))
                    .is_some_and(|intent| matches!(intent, PendingIntent::Create))
                {
                    self.focused = Some(session.id);
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
            SessionEvent::AssistantMessageStarted { message } => {
                // A new turn's message means every earlier turn of the run
                // has committed; the server finalized those messages inside
                // the turn persist without a dedicated event.
                self.complete_streamed_turns(
                    envelope.session_id,
                    message.run_id,
                    message.turn_ordinal.saturating_sub(1),
                );
                self.push_message(message.clone());
            }
            SessionEvent::TextAppended {
                message_id,
                channel,
                text,
            } => {
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
                self.upsert_tool_call(tool_call.clone());
            }
            SessionEvent::SessionCompacted {
                session,
                before_bytes,
                after_bytes,
                ..
            } => {
                self.upsert_summary(session.clone());
                self.status = Some(format!(
                    "compacted: {} -> {}",
                    format_bytes(*before_bytes),
                    format_bytes(*after_bytes)
                ));
            }
            SessionEvent::RunFinished {
                session,
                run_id,
                outcome,
                usage,
            } => {
                self.upsert_summary(session.clone());
                if let Some(usage) = usage
                    && let Some(session) = self.sessions.get_mut(&envelope.session_id)
                {
                    session.latest_input_tokens = Some(total_input_tokens(*usage));
                }
                if let Some(messages) = self
                    .sessions
                    .get_mut(&envelope.session_id)
                    .and_then(|session| session.messages.as_mut())
                {
                    let state = match outcome {
                        RunOutcome::Completed => MessageState::Complete,
                        RunOutcome::Cancelled => MessageState::Cancelled,
                        RunOutcome::Interrupted => MessageState::Interrupted,
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
                if let RunOutcome::Failed { failure } = outcome {
                    self.status = Some(failure.message.clone());
                }
            }
        }
    }

    fn upsert_summary(&mut self, summary: SessionSummary) {
        let context_window = model_context_window(&self.models, summary.model.as_deref());
        self.sessions
            .entry(summary.id)
            .and_modify(|session| {
                session.summary = summary.clone();
                session.context_window = context_window;
            })
            .or_insert(SessionView {
                summary,
                messages: None,
                tool_calls: None,
                latest_input_tokens: None,
                context_window,
                loaded_through: 0,
            });
    }

    /// Drops a deleted session from every client map, mirroring the server's
    /// cascade: its children become roots, its per-call display state and
    /// optimistic prompts are discarded, and a deleted focus moves to the
    /// nearest remaining session (or clears).
    fn remove_session(&mut self, session_id: SessionId) {
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
                        session_limit: SNAPSHOT_SESSION_LIMIT,
                        message_limit: SNAPSHOT_MESSAGE_LIMIT,
                    }));
            }
        }
        if let Some(picker) = &mut self.session_picker {
            if matches!(picker.confirm, Some(SessionPickerConfirm::Delete(pending)) if pending == session_id)
            {
                picker.confirm = None;
            }
            if picker.selected == Some(session_id) {
                picker.selected = None;
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

    fn push_message(&mut self, message: MessageSnapshot) {
        let Some(messages) = self
            .sessions
            .get_mut(&message.session_id)
            .and_then(|session| session.messages.as_mut())
        else {
            return;
        };
        if messages.iter().any(|candidate| candidate.id == message.id) {
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

    fn upsert_tool_call(&mut self, tool_call: ToolCallSnapshot) {
        let Some(tool_calls) = self
            .sessions
            .get_mut(&tool_call.session_id)
            .and_then(|session| session.tool_calls.as_mut())
        else {
            return;
        };
        if let Some(existing) = tool_calls
            .iter_mut()
            .find(|existing| existing.id == tool_call.id)
        {
            *existing = tool_call;
        } else {
            tool_calls.push(tool_call);
            retain_recent_tool_calls(tool_calls);
        }
    }

    fn reject_pending(&mut self, command_id: CommandId, error: ClientFailure) {
        match self.pending.remove(&command_id) {
            Some(PendingIntent::Prompt { session_id, text })
                if self.focused == Some(session_id) && self.input.is_empty() =>
            {
                self.input = text;
            }
            Some(PendingIntent::Approval { tool_call_id }) => {
                // Re-open the prompt so the user can answer again.
                self.answered_approvals.remove(&tool_call_id);
            }
            _ => {}
        }
        self.status = Some(error.message().to_owned());
    }

    pub fn handle_terminal_event(&mut self, event: Event) -> (bool, Vec<ClientRequest>) {
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                self.handle_key(key)
            }
            Event::Paste(text) if self.session_picker.is_some() => {
                let changed = self.push_session_search(&text);
                (changed, Vec::new())
            }
            Event::Paste(text) if self.model_picker.is_some() => {
                let changed = self.push_model_search(&text);
                (changed, Vec::new())
            }
            Event::Paste(text) => {
                let changed = self.push_composer_text(&text);
                (changed, Vec::new())
            }
            Event::Mouse(mouse) if self.model_picker.is_none() && self.session_picker.is_none() => {
                let changed = match mouse.kind {
                    MouseEventKind::ScrollUp => self.scroll_transcript_up(MOUSE_SCROLL_ROWS),
                    MouseEventKind::ScrollDown => self.scroll_transcript_down(MOUSE_SCROLL_ROWS),
                    _ => false,
                };
                (changed, Vec::new())
            }
            Event::Resize(_, _) | Event::FocusGained | Event::FocusLost => (true, Vec::new()),
            Event::Key(_) | Event::Mouse(_) => (false, Vec::new()),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> (bool, Vec<ClientRequest>) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return (true, Vec::new());
        }
        if self.session_picker.is_some() {
            return self.handle_session_picker_key(key);
        }
        if self.model_picker.is_some() {
            return self.handle_model_picker_key(key);
        }
        if self.pending_approval().is_some() {
            return self.handle_approval_key(key);
        }
        // Shift-Enter inserts a newline in the composer. Handle it before slash
        // completion and configured bindings so it never submits.
        if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::SHIFT) {
            let changed = self.push_input('\n');
            return (changed, Vec::new());
        }
        if let Some(result) = self.handle_slash_key(key.code) {
            return result;
        }
        if let Some(action) = self.settings.action_for(key) {
            return self.handle_action(action);
        }
        // Ctrl-O cycles tool call detail. Checked after configured bindings so
        // a user rebinding Ctrl-O keeps winning.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('o' | 'O'))
        {
            self.tool_detail = self.tool_detail.next();
            return (true, Vec::new());
        }
        match key.code {
            KeyCode::Esc => {
                if let Some(parent) = self
                    .focused
                    .and_then(|focused| self.sessions.get(&focused)?.summary.parent_id)
                {
                    return self.focus_session(parent);
                }
                (false, Vec::new())
            }
            KeyCode::Enter => self.submit_prompt(),
            KeyCode::PageUp => {
                let changed = self.scroll_transcript_up(self.transcript_viewport.height);
                (changed, Vec::new())
            }
            KeyCode::PageDown => {
                let changed = self.scroll_transcript_down(self.transcript_viewport.height);
                (changed, Vec::new())
            }
            KeyCode::Backspace => {
                let changed = self.input.pop().is_some();
                if changed {
                    self.slash_selected = 0;
                }
                (changed, Vec::new())
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let changed = self.push_input(character);
                (changed, Vec::new())
            }
            _ => (false, Vec::new()),
        }
    }

    pub(crate) fn update_transcript_viewport(&mut self, body_rows: usize, height: usize) {
        let context = (self.focused, self.layout);
        if self.transcript_viewport.context != Some(context) {
            self.transcript_viewport = TranscriptViewport {
                context: Some(context),
                body_rows,
                height,
                offset: 0,
            };
            return;
        }
        if self.transcript_viewport.offset > 0 && self.transcript_viewport.height > 0 {
            let top = self
                .transcript_viewport
                .body_rows
                .saturating_sub(self.transcript_viewport.offset)
                .saturating_sub(self.transcript_viewport.height);
            self.transcript_viewport.offset = body_rows.saturating_sub(top.saturating_add(height));
        }
        self.transcript_viewport.body_rows = body_rows;
        self.transcript_viewport.height = height;
        self.transcript_viewport.offset = self
            .transcript_viewport
            .offset
            .min(body_rows.saturating_sub(height));
    }

    pub(crate) const fn transcript_scroll_offset(&self) -> usize {
        self.transcript_viewport.offset
    }

    fn scroll_transcript_up(&mut self, rows: usize) -> bool {
        let before = self.transcript_viewport.offset;
        let maximum = self
            .transcript_viewport
            .body_rows
            .saturating_sub(self.transcript_viewport.height);
        self.transcript_viewport.offset = before.saturating_add(rows).min(maximum);
        self.transcript_viewport.offset != before
    }

    fn scroll_transcript_down(&mut self, rows: usize) -> bool {
        let before = self.transcript_viewport.offset;
        self.transcript_viewport.offset = before.saturating_sub(rows);
        self.transcript_viewport.offset != before
    }

    fn handle_action(&mut self, action: Action) -> (bool, Vec<ClientRequest>) {
        match action {
            Action::SelectThreadline => self.layout = Layout::Threadline,
            Action::SelectFoldFocus => self.layout = Layout::FoldFocus,
            Action::NextLayout => self.layout = self.layout.next(),
            Action::PreviousLayout => self.layout = self.layout.previous(),
            Action::ToggleNavigator => {
                if self.session_picker.is_some() {
                    self.session_picker = None;
                } else {
                    return self.open_sessions();
                }
            }
            Action::CreateRootSession => return self.create_session(None),
            Action::CreateChildSession => return self.create_session(self.focused),
            Action::CancelRun => return self.cancel_run(),
        }
        (true, Vec::new())
    }

    fn open_models(&mut self) -> (bool, Vec<ClientRequest>) {
        if self.models.is_empty() {
            self.status = Some("no authenticated providers have selectable models".to_owned());
            return (true, Vec::new());
        }
        self.session_picker = None;
        self.model_picker = Some(ModelPicker {
            query: String::new(),
            selected: 0,
        });
        (true, Vec::new())
    }

    pub(crate) fn filtered_models(&self) -> Vec<usize> {
        let Some(picker) = &self.model_picker else {
            return Vec::new();
        };
        let query = picker.query.to_ascii_lowercase();
        self.models
            .iter()
            .enumerate()
            .filter(|(_, option)| {
                query.is_empty()
                    || option.provider.to_ascii_lowercase().contains(&query)
                    || option.model.to_ascii_lowercase().contains(&query)
                    || option
                        .name
                        .as_deref()
                        .is_some_and(|name| name.to_ascii_lowercase().contains(&query))
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn handle_model_picker_key(&mut self, key: KeyEvent) -> (bool, Vec<ClientRequest>) {
        let filtered = self.filtered_models();
        match key.code {
            KeyCode::Esc => {
                self.model_picker = None;
                (true, Vec::new())
            }
            KeyCode::Up => {
                if let Some(picker) = &mut self.model_picker {
                    picker.selected = picker.selected.saturating_sub(1);
                }
                (true, Vec::new())
            }
            KeyCode::Down => {
                if let Some(picker) = &mut self.model_picker {
                    picker.selected = (picker.selected + 1).min(filtered.len().saturating_sub(1));
                }
                (true, Vec::new())
            }
            // Enter applies the model to the focused session; without a
            // focus it keeps the historical create behavior. Ctrl-N always
            // creates a session with the selected model.
            KeyCode::Enter => {
                let Some(model) = self.selected_picker_model(&filtered) else {
                    return (false, Vec::new());
                };
                let focused = self
                    .focused
                    .filter(|session_id| self.sessions.contains_key(session_id));
                let result = match focused {
                    Some(session_id) => self.set_session_model(session_id, model),
                    None => self.create_session_with_model(None, model),
                };
                if !result.1.is_empty() {
                    self.model_picker = None;
                }
                result
            }
            KeyCode::Char('n' | 'N') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let Some(model) = self.selected_picker_model(&filtered) else {
                    return (false, Vec::new());
                };
                let result = self.create_session_with_model(None, model);
                if !result.1.is_empty() {
                    self.model_picker = None;
                }
                result
            }
            KeyCode::Backspace => {
                let changed = self
                    .model_picker
                    .as_mut()
                    .is_some_and(|picker| picker.query.pop().is_some());
                if let Some(picker) = &mut self.model_picker {
                    picker.selected = 0;
                }
                (changed, Vec::new())
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let mut encoded = [0; 4];
                (
                    self.push_model_search(character.encode_utf8(&mut encoded)),
                    Vec::new(),
                )
            }
            _ => (false, Vec::new()),
        }
    }

    fn selected_picker_model(&self, filtered: &[usize]) -> Option<ModelSelection> {
        self.model_picker
            .as_ref()
            .and_then(|picker| filtered.get(picker.selected))
            .and_then(|index| self.models.get(*index))
            .map(|option| option.selection.clone())
    }

    fn set_session_model(
        &mut self,
        session_id: SessionId,
        model: ModelSelection,
    ) -> (bool, Vec<ClientRequest>) {
        let Ok(command_id) = CommandId::generate() else {
            self.status = Some("secure randomness is unavailable".to_owned());
            return (true, Vec::new());
        };
        (
            true,
            vec![ClientRequest::Command(CommandRequest {
                command_id,
                command: SessionCommand::SetSessionModel { session_id, model },
            })],
        )
    }

    fn push_model_search(&mut self, text: &str) -> bool {
        let Some(picker) = &mut self.model_picker else {
            return false;
        };
        let before = picker.query.len();
        for character in text.chars() {
            if picker.query.len() + character.len_utf8() > MAX_PICKER_SEARCH_BYTES {
                break;
            }
            if let Some(character) = terminal_safe_character(character) {
                picker.query.push(character);
            }
        }
        picker.selected = 0;
        picker.query.len() != before
    }

    fn open_sessions(&mut self) -> (bool, Vec<ClientRequest>) {
        self.model_picker = None;
        self.session_picker = Some(SessionPicker {
            query: String::new(),
            selected: self
                .focused
                .filter(|session_id| self.sessions.contains_key(session_id))
                .or_else(|| self.thread_order().first().copied()),
            confirm: None,
        });
        (true, Vec::new())
    }

    pub(crate) fn filtered_sessions(&self) -> Vec<SessionId> {
        let Some(picker) = &self.session_picker else {
            return Vec::new();
        };
        let query = picker.query.to_ascii_lowercase();
        self.thread_order()
            .into_iter()
            .filter(|session_id| {
                query.is_empty()
                    || self.sessions[session_id]
                        .summary
                        .title
                        .to_ascii_lowercase()
                        .contains(&query)
            })
            .collect()
    }

    fn handle_session_picker_key(&mut self, key: KeyEvent) -> (bool, Vec<ClientRequest>) {
        let filtered = self.filtered_sessions();
        let selected = self
            .session_picker
            .as_ref()
            .and_then(|picker| picker.selected);
        if let Some(confirm) = self
            .session_picker
            .as_ref()
            .and_then(|picker| picker.confirm)
        {
            return self.handle_session_picker_confirm_key(key, confirm);
        }
        let position =
            selected.and_then(|selected| filtered.iter().position(|session| *session == selected));
        match key.code {
            KeyCode::Esc => {
                self.session_picker = None;
                (true, Vec::new())
            }
            KeyCode::Up => {
                if let Some(picker) = &mut self.session_picker {
                    picker.selected = filtered
                        .get(position.unwrap_or_default().saturating_sub(1))
                        .copied();
                }
                (true, Vec::new())
            }
            KeyCode::Down => {
                if let Some(picker) = &mut self.session_picker {
                    picker.selected = filtered
                        .get(
                            position
                                .map(|position| position + 1)
                                .unwrap_or_default()
                                .min(filtered.len().saturating_sub(1)),
                        )
                        .copied();
                }
                (true, Vec::new())
            }
            KeyCode::Enter => {
                let Some(selected) = selected.filter(|selected| filtered.contains(selected)) else {
                    return (false, Vec::new());
                };
                self.session_picker = None;
                self.focus_session(selected)
            }
            KeyCode::Backspace => {
                let changed = self
                    .session_picker
                    .as_mut()
                    .is_some_and(|picker| picker.query.pop().is_some());
                self.reset_session_picker_selection();
                (changed, Vec::new())
            }
            // Ctrl-modified so plain letters keep feeding the search query.
            KeyCode::Delete => self.request_delete_confirmation(selected, &filtered),
            KeyCode::Char('d' | 'D') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.request_delete_confirmation(selected, &filtered)
            }
            KeyCode::Char('p' | 'P') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(picker) = &mut self.session_picker {
                    picker.confirm = Some(SessionPickerConfirm::Prune);
                }
                (true, Vec::new())
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let mut encoded = [0; 4];
                (
                    self.push_session_search(character.encode_utf8(&mut encoded)),
                    Vec::new(),
                )
            }
            _ => (false, Vec::new()),
        }
    }

    fn request_delete_confirmation(
        &mut self,
        selected: Option<SessionId>,
        filtered: &[SessionId],
    ) -> (bool, Vec<ClientRequest>) {
        let Some(selected) = selected.filter(|selected| filtered.contains(selected)) else {
            return (false, Vec::new());
        };
        if self
            .sessions
            .get(&selected)
            .is_some_and(|session| session.summary.active_run_id.is_some())
        {
            self.status = Some("cancel the active run before deleting".to_owned());
            return (true, Vec::new());
        }
        if let Some(picker) = &mut self.session_picker {
            picker.confirm = Some(SessionPickerConfirm::Delete(selected));
        }
        (true, Vec::new())
    }

    fn handle_session_picker_confirm_key(
        &mut self,
        key: KeyEvent,
        confirm: SessionPickerConfirm,
    ) -> (bool, Vec<ClientRequest>) {
        match key.code {
            KeyCode::Char('y' | 'Y') => {
                if let Some(picker) = &mut self.session_picker {
                    picker.confirm = None;
                }
                match confirm {
                    SessionPickerConfirm::Delete(session_id) => self.delete_session(session_id),
                    SessionPickerConfirm::Prune => self.prune_sessions(),
                }
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                if let Some(picker) = &mut self.session_picker {
                    picker.confirm = None;
                }
                (true, Vec::new())
            }
            _ => (false, Vec::new()),
        }
    }

    fn delete_session(&mut self, session_id: SessionId) -> (bool, Vec<ClientRequest>) {
        let Ok(command_id) = CommandId::generate() else {
            self.status = Some("secure randomness is unavailable".to_owned());
            return (true, Vec::new());
        };
        (
            true,
            vec![ClientRequest::Command(CommandRequest {
                command_id,
                command: SessionCommand::DeleteSession { session_id },
            })],
        )
    }

    fn prune_sessions(&mut self) -> (bool, Vec<ClientRequest>) {
        let Some(workspace_id) = self.workspace_id else {
            self.status = Some("workspace is still connecting".to_owned());
            return (true, Vec::new());
        };
        let Ok(command_id) = CommandId::generate() else {
            self.status = Some("secure randomness is unavailable".to_owned());
            return (true, Vec::new());
        };
        (
            true,
            vec![ClientRequest::Command(CommandRequest {
                command_id,
                command: SessionCommand::PruneSessions { workspace_id },
            })],
        )
    }

    fn push_session_search(&mut self, text: &str) -> bool {
        let Some(picker) = &mut self.session_picker else {
            return false;
        };
        let before = picker.query.len();
        for character in text.chars() {
            if picker.query.len() + character.len_utf8() > MAX_PICKER_SEARCH_BYTES {
                break;
            }
            if let Some(character) = terminal_safe_character(character) {
                picker.query.push(character);
            }
        }
        let changed = picker.query.len() != before;
        self.reset_session_picker_selection();
        changed
    }

    fn reset_session_picker_selection(&mut self) {
        let selected = self.filtered_sessions().first().copied();
        if let Some(picker) = &mut self.session_picker {
            picker.selected = selected;
        }
    }

    fn focus_session(&mut self, session_id: SessionId) -> (bool, Vec<ClientRequest>) {
        self.focused = Some(session_id);
        let Some(workspace_id) = self.workspace_id else {
            return (true, Vec::new());
        };
        (
            true,
            vec![ClientRequest::Snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                session_limit: SNAPSHOT_SESSION_LIMIT,
                message_limit: SNAPSHOT_MESSAGE_LIMIT,
            })],
        )
    }

    fn create_session(&mut self, parent_id: Option<SessionId>) -> (bool, Vec<ClientRequest>) {
        self.create_session_with_model(parent_id, self.model.clone())
    }

    fn create_session_with_model(
        &mut self,
        parent_id: Option<SessionId>,
        model: ModelSelection,
    ) -> (bool, Vec<ClientRequest>) {
        if !model.model.as_ref().is_some_and(|route| {
            route
                .split_once('/')
                .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
        }) {
            self.status = Some("choose a model with /models before creating a session".to_owned());
            return (true, Vec::new());
        }
        let Some(workspace_id) = self.workspace_id else {
            self.status = Some("workspace is still connecting".to_owned());
            return (true, Vec::new());
        };
        let Ok(command_id) = CommandId::generate() else {
            self.status = Some("secure randomness is unavailable".to_owned());
            return (true, Vec::new());
        };
        self.pending.insert(command_id, PendingIntent::Create);
        (
            true,
            vec![ClientRequest::Command(CommandRequest {
                command_id,
                command: SessionCommand::CreateSession {
                    workspace_id,
                    parent_id,
                    model,
                    approval_mode: ApprovalMode::default(),
                },
            })],
        )
    }

    fn submit_prompt(&mut self) -> (bool, Vec<ClientRequest>) {
        let prompt = self.input.trim().to_owned();
        if prompt.is_empty() {
            return (false, Vec::new());
        }
        // Composer commands: input starting with '/' is never sent to the
        // model. The first token names the command; an unknown name shows a
        // hint instead of becoming a prompt.
        if prompt.starts_with('/') {
            let name = prompt.split_whitespace().next().unwrap_or(&prompt);
            let Some(action) = SLASH_COMMANDS
                .iter()
                .find(|command| command.name == name)
                .map(|command| command.action)
            else {
                self.status = Some(format!(
                    "unknown command {name}; try {}",
                    SLASH_COMMANDS
                        .iter()
                        .map(|command| command.name)
                        .collect::<Vec<_>>()
                        .join(" ")
                ));
                self.input.clear();
                self.slash_selected = 0;
                return (true, Vec::new());
            };
            return self.execute_slash_action(action);
        }
        let Some(session_id) = self.focused else {
            self.status = Some("create a session before sending a prompt".to_owned());
            return (true, Vec::new());
        };
        let Ok(command_id) = CommandId::generate() else {
            self.status = Some("secure randomness is unavailable".to_owned());
            return (true, Vec::new());
        };
        self.input.clear();
        self.pending.insert(
            command_id,
            PendingIntent::Prompt {
                session_id,
                text: prompt.clone(),
            },
        );
        (
            true,
            vec![ClientRequest::Command(CommandRequest {
                command_id,
                command: SessionCommand::SubmitPrompt { session_id, prompt },
            })],
        )
    }

    fn compact_session(&mut self) -> (bool, Vec<ClientRequest>) {
        let Some(session_id) = self.focused else {
            self.status = Some("create a session before compacting".to_owned());
            return (true, Vec::new());
        };
        if self
            .sessions
            .get(&session_id)
            .is_some_and(|session| session.summary.status != SessionStatus::Idle)
        {
            self.status = Some("compaction needs an idle session; wait or cancel first".to_owned());
            return (true, Vec::new());
        }
        let Ok(command_id) = CommandId::generate() else {
            self.status = Some("secure randomness is unavailable".to_owned());
            return (true, Vec::new());
        };
        self.pending.insert(command_id, PendingIntent::Compact);
        self.status = Some("compacting session...".to_owned());
        (
            true,
            vec![ClientRequest::Command(CommandRequest {
                command_id,
                command: SessionCommand::CompactSession { session_id },
            })],
        )
    }

    fn cancel_run(&mut self) -> (bool, Vec<ClientRequest>) {
        let Some(run_id) = self
            .focused
            .and_then(|session_id| self.sessions.get(&session_id))
            .and_then(|session| session.summary.active_run_id)
        else {
            self.status = Some("focused session has no active run".to_owned());
            return (true, Vec::new());
        };
        let Ok(command_id) = CommandId::generate() else {
            self.status = Some("secure randomness is unavailable".to_owned());
            return (true, Vec::new());
        };
        self.pending.insert(command_id, PendingIntent::Cancel);
        (
            true,
            vec![ClientRequest::Command(CommandRequest {
                command_id,
                command: SessionCommand::CancelRun { run_id },
            })],
        )
    }

    /// The focused session's oldest unanswered tool approval, if any.
    pub(crate) fn pending_approval(&self) -> Option<&ToolCallSnapshot> {
        let session = self.sessions.get(&self.focused?)?;
        session.tool_calls.as_ref()?.iter().find(|tool_call| {
            tool_call.state == ToolCallState::AwaitingApproval
                && !self.answered_approvals.contains(&tool_call.id)
        })
    }

    /// The diff preview carried by the pending approval's request, if any.
    pub(crate) fn pending_approval_edit(&self) -> Option<&EditPreview> {
        self.edit_previews.get(&self.pending_approval()?.id)
    }

    fn handle_approval_key(&mut self, key: KeyEvent) -> (bool, Vec<ClientRequest>) {
        if matches!(self.settings.action_for(key), Some(Action::CancelRun)) {
            return self.cancel_run();
        }
        match key.code {
            KeyCode::Char('y' | 'Y') => self.respond_to_approval(ApprovalChoice::Once),
            KeyCode::Char('a' | 'A') => self.respond_to_approval(ApprovalChoice::Session),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                self.respond_to_approval(ApprovalChoice::Deny)
            }
            _ => (false, Vec::new()),
        }
    }

    fn respond_to_approval(&mut self, choice: ApprovalChoice) -> (bool, Vec<ClientRequest>) {
        let Some(tool_call) = self.pending_approval() else {
            return (false, Vec::new());
        };
        let tool_call_id = tool_call.id;
        let run_id = tool_call.run_id;
        let decision = match choice {
            ApprovalChoice::Once => ApprovalDecision::ApproveOnce,
            ApprovalChoice::Session => ApprovalDecision::ApproveForSession {
                grant: approval_grant(tool_call),
            },
            ApprovalChoice::Deny => ApprovalDecision::Deny,
        };
        let Ok(command_id) = CommandId::generate() else {
            self.status = Some("secure randomness is unavailable".to_owned());
            return (true, Vec::new());
        };
        self.answered_approvals.insert(tool_call_id);
        self.pending
            .insert(command_id, PendingIntent::Approval { tool_call_id });
        (
            true,
            vec![ClientRequest::Command(CommandRequest {
                command_id,
                command: SessionCommand::RespondToolApproval {
                    run_id,
                    tool_call_id,
                    decision,
                },
            })],
        )
    }

    fn push_input(&mut self, character: char) -> bool {
        let Some(character) = composer_character(character) else {
            return false;
        };
        if self.input.len() + character.len_utf8() > MAX_INPUT_BYTES {
            return false;
        }
        self.input.push(character);
        self.slash_selected = 0;
        true
    }

    fn push_composer_text(&mut self, text: &str) -> bool {
        let before = self.input.len();
        for character in text.chars() {
            if self.input.len() + character.len_utf8() > MAX_INPUT_BYTES {
                break;
            }
            if let Some(character) = composer_character(character) {
                self.input.push(character);
            }
        }
        let changed = self.input.len() != before;
        if changed {
            self.slash_selected = 0;
        }
        changed
    }

    fn handle_slash_key(&mut self, code: KeyCode) -> Option<(bool, Vec<ClientRequest>)> {
        let commands = self.filtered_slash_commands();
        let command_count = commands.len();
        if command_count == 0 {
            return None;
        }
        match code {
            KeyCode::Up => {
                self.slash_selected = self.slash_selected.saturating_sub(1);
                Some((true, Vec::new()))
            }
            KeyCode::Down => {
                self.slash_selected = (self.slash_selected + 1).min(command_count - 1);
                Some((true, Vec::new()))
            }
            KeyCode::Enter | KeyCode::Tab => {
                Some(self.execute_slash_action(
                    commands[self.slash_selected.min(command_count - 1)].action,
                ))
            }
            _ => None,
        }
    }

    fn execute_slash_action(&mut self, action: SlashAction) -> (bool, Vec<ClientRequest>) {
        self.input.clear();
        self.slash_selected = 0;
        match action {
            SlashAction::Quit => {
                self.quit = true;
                (true, Vec::new())
            }
            SlashAction::Models => self.open_models(),
            SlashAction::New => self.create_session(None),
            SlashAction::Sessions => self.open_sessions(),
            SlashAction::Compact => self.compact_session(),
        }
    }

    pub(crate) fn filtered_slash_commands(&self) -> Vec<&'static SlashCommand> {
        if !self.input.starts_with('/') || self.input.chars().any(char::is_whitespace) {
            return Vec::new();
        }
        SLASH_COMMANDS
            .iter()
            .filter(|command| command.name.starts_with(&self.input))
            .collect()
    }

    pub(crate) fn slash_selected(&self) -> usize {
        self.slash_selected
    }

    pub fn advance_animation(&mut self) -> bool {
        self.animation_tick = self.animation_tick.wrapping_add(1);
        self.sessions
            .values()
            .any(|session| matches!(session.summary.status, qq_protocol::SessionStatus::Running))
    }

    pub fn has_activity(&self) -> bool {
        self.sessions
            .values()
            .any(|session| matches!(session.summary.status, qq_protocol::SessionStatus::Running))
    }

    pub fn pending_prompts(&self, session_id: SessionId) -> impl Iterator<Item = &str> {
        self.pending
            .values()
            .filter_map(move |intent| match intent {
                PendingIntent::Prompt {
                    session_id: candidate,
                    text,
                } if *candidate == session_id => Some(text.as_str()),
                PendingIntent::Create
                | PendingIntent::Prompt { .. }
                | PendingIntent::Cancel
                | PendingIntent::Compact
                | PendingIntent::Approval { .. } => None,
            })
    }

    pub(crate) fn focused_context_usage(&self) -> Option<(u64, u32)> {
        let session = self.focused.and_then(|id| self.sessions.get(&id))?;
        Some((session.latest_input_tokens?, session.context_window?))
    }

    pub fn thread_order(&self) -> Vec<SessionId> {
        let mut roots = self.root_sessions();
        roots.sort_by_key(|id| self.sessions[id].summary.updated_at_ms);
        roots.reverse();
        let mut stack = roots.into_iter().rev().collect::<Vec<_>>();
        let mut output = Vec::with_capacity(self.sessions.len());
        while let Some(session_id) = stack.pop() {
            output.push(session_id);
            let mut children = self
                .sessions
                .values()
                .filter(|session| session.summary.parent_id == Some(session_id))
                .map(|session| session.summary.id)
                .collect::<Vec<_>>();
            children.sort_by_key(|id| self.sessions[id].summary.updated_at_ms);
            stack.extend(children);
        }
        output
    }

    fn root_sessions(&self) -> Vec<SessionId> {
        self.sessions
            .values()
            .filter(|session| session.summary.parent_id.is_none())
            .map(|session| session.summary.id)
            .collect()
    }

    pub fn depth(&self, session_id: SessionId) -> usize {
        let mut depth = 0;
        let mut cursor = self
            .sessions
            .get(&session_id)
            .and_then(|session| session.summary.parent_id);
        while let Some(parent) = cursor {
            depth += 1;
            cursor = self
                .sessions
                .get(&parent)
                .and_then(|session| session.summary.parent_id);
        }
        depth
    }
}

#[derive(Clone, Copy)]
enum ApprovalChoice {
    Once,
    Session,
    Deny,
}

/// Derives the approve-for-session grant from the pending call: shell calls
/// allowlist their exact command as a prefix, everything else grants the tool.
fn approval_grant(tool_call: &ToolCallSnapshot) -> ApprovalGrant {
    if tool_call.name == "shell"
        && let Ok(arguments) = serde_json::from_str::<serde_json::Value>(&tool_call.arguments)
        && let Some(command) = arguments.get("command").and_then(|value| value.as_str())
        && !command.trim().is_empty()
    {
        return ApprovalGrant::ShellPrefix {
            prefix: command.to_owned(),
        };
    }
    ApprovalGrant::Tool {
        name: tool_call.name.clone(),
    }
}

fn retain_recent_messages(messages: &mut Vec<MessageSnapshot>) {
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

fn retain_recent_tool_calls(tool_calls: &mut Vec<ToolCallSnapshot>) {
    let excess = tool_calls.len().saturating_sub(MAX_RECENT_TOOL_CALLS);
    if excess > 0 {
        tool_calls.drain(..excess);
    }
}

/// Renders a byte count for the status line: whole bytes below 1 KiB, one
/// decimal of KiB/MiB/GiB above it.
fn format_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u64); 3] = [
        ("GiB", 1024 * 1024 * 1024),
        ("MiB", 1024 * 1024),
        ("KiB", 1024),
    ];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            #[expect(clippy::cast_precision_loss, reason = "display rounding only")]
            let value = bytes as f64 / scale as f64;
            return format!("{value:.1} {unit}");
        }
    }
    format!("{bytes} B")
}

const fn total_input_tokens(usage: TokenUsage) -> u64 {
    usage
        .input_tokens
        .saturating_add(usage.cache_read_input_tokens)
        .saturating_add(usage.cache_write_input_tokens)
}

fn model_context_window(models: &[ModelOption], model: Option<&str>) -> Option<u32> {
    models
        .iter()
        .find(|option| option.selection.model.as_deref() == model)?
        .context_window
}

pub(crate) fn terminal_safe_character(character: char) -> Option<char> {
    if character.is_control() {
        return character.is_whitespace().then_some(' ');
    }
    if matches!(
        character,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    ) {
        return None;
    }
    Some(character)
}

/// Sanitizes characters for the prompt composer.
///
/// Unlike [`terminal_safe_character`], hard newlines are preserved so Shift-Enter
/// and multiline paste can build multi-line prompts. Carriage returns are dropped
/// so CRLF paste collapses to a single newline.
fn composer_character(character: char) -> Option<char> {
    match character {
        '\n' => Some('\n'),
        '\r' => None,
        character => terminal_safe_character(character),
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::MouseEvent;
    use qq_protocol::{
        EventCursor, MessageId, MessageRole, RunId, RunSnapshot, RunStatus, SessionStatus, StoreId,
        TokenUsage, ToolCallId, ToolCallState, WorkspaceSummary,
    };

    use super::*;

    fn id<T>(byte: u8, constructor: impl FnOnce([u8; 16]) -> T) -> T {
        constructor([byte; 16])
    }

    fn snapshot() -> WorkspaceSnapshot {
        let workspace_id = id(1, WorkspaceId::from_bytes);
        let session_id = id(2, SessionId::from_bytes);
        WorkspaceSnapshot {
            cursor: EventCursor {
                store_id: id(3, StoreId::from_bytes),
                workspace_id,
                sequence: 1,
            },
            workspace: WorkspaceSummary {
                id: workspace_id,
                path: "/workspace".to_owned(),
            },
            sessions: vec![SessionSummary {
                id: session_id,
                workspace_id,
                parent_id: None,
                title: "Session".to_owned(),
                status: SessionStatus::Idle,
                active_run_id: None,
                queued_prompts: 0,
                model: Some("openai/gpt-test".to_owned()),
                estimated_cost_usd_nanos: Some(0),
                updated_at_ms: 1,
                last_outcome: None,
            }],
            focused: Some(SessionSnapshot {
                summary: SessionSummary {
                    id: session_id,
                    workspace_id,
                    parent_id: None,
                    title: "Session".to_owned(),
                    status: SessionStatus::Idle,
                    active_run_id: None,
                    queued_prompts: 0,
                    model: Some("openai/gpt-test".to_owned()),
                    estimated_cost_usd_nanos: Some(0),
                    updated_at_ms: 1,
                    last_outcome: None,
                },
                messages: Vec::new(),
                runs: Vec::new(),
                tool_calls: Vec::new(),
                has_older_tool_calls: false,
                has_older_messages: false,
            }),
            has_older_sessions: false,
        }
    }

    #[test]
    fn shift_enter_inserts_a_newline_without_submitting() {
        let mut app = App::new(TuiOptions::default());
        app.input = "hello".to_owned();
        let (changed, requests) =
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        assert!(changed);
        assert!(requests.is_empty());
        assert_eq!(app.input, "hello\n");
    }

    #[test]
    fn paste_preserves_newlines_in_the_composer() {
        let mut app = App::new(TuiOptions::default());
        let (changed, requests) =
            app.handle_terminal_event(Event::Paste("alpha\r\nbeta\ngamma".to_owned()));
        assert!(changed);
        assert!(requests.is_empty());
        assert_eq!(app.input, "alpha\nbeta\ngamma");
    }

    #[test]
    fn submit_is_optimistic_but_restores_a_rejected_prompt() {
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(snapshot());
        app.input = "hello".to_owned();

        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let ClientRequest::Command(request) = requests.into_iter().next().unwrap() else {
            panic!("expected command")
        };
        assert!(app.input.is_empty());
        app.apply_client_update(ClientUpdate::CommandResult {
            command_id: request.command_id,
            result: Err(ClientFailure::new("offline")),
        });

        assert_eq!(app.input, "hello");
        assert_eq!(app.status.as_deref(), Some("offline"));
    }

    #[test]
    fn approval_prompt_captures_keys_and_sends_the_decision() {
        let mut app = App::new(TuiOptions::default());
        let initial = snapshot();
        let session_id = initial.focused.as_ref().unwrap().summary.id;
        app.apply_snapshot(initial);
        let tool_call = ToolCallSnapshot {
            id: id(7, ToolCallId::from_bytes),
            session_id,
            run_id: id(4, RunId::from_bytes),
            turn_ordinal: 1,
            call_ordinal: 1,
            provider_call_id: "call_0".to_owned(),
            name: "write_file".to_owned(),
            arguments: r#"{"path":"note.txt"}"#.to_owned(),
            state: ToolCallState::AwaitingApproval,
            result: None,
            is_error: false,
            display: None,
        };
        app.upsert_tool_call(tool_call.clone());
        assert_eq!(
            app.pending_approval().map(|call| call.id),
            Some(tool_call.id)
        );

        // The prompt captures ordinary typing instead of the composer.
        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(requests.is_empty());
        assert!(app.input.is_empty());

        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        let ClientRequest::Command(request) = requests.into_iter().next().unwrap() else {
            panic!("expected a command")
        };
        assert_eq!(
            request.command,
            SessionCommand::RespondToolApproval {
                run_id: tool_call.run_id,
                tool_call_id: tool_call.id,
                decision: ApprovalDecision::ApproveOnce,
            }
        );
        // Answered approvals stop prompting until the server responds.
        assert!(app.pending_approval().is_none());
        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(requests.is_empty());

        // A failed command re-opens the prompt so the user can answer again.
        app.apply_client_update(ClientUpdate::CommandResult {
            command_id: request.command_id,
            result: Err(ClientFailure::new("offline")),
        });
        assert!(app.pending_approval().is_some());

        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let ClientRequest::Command(request) = requests.into_iter().next().unwrap() else {
            panic!("expected a command")
        };
        assert!(matches!(
            request.command,
            SessionCommand::RespondToolApproval {
                decision: ApprovalDecision::Deny,
                ..
            }
        ));
    }

    #[test]
    fn approve_for_session_grants_shell_commands_as_prefixes() {
        let mut app = App::new(TuiOptions::default());
        let initial = snapshot();
        let session_id = initial.focused.as_ref().unwrap().summary.id;
        app.apply_snapshot(initial);
        app.upsert_tool_call(ToolCallSnapshot {
            id: id(8, ToolCallId::from_bytes),
            session_id,
            run_id: id(4, RunId::from_bytes),
            turn_ordinal: 1,
            call_ordinal: 1,
            provider_call_id: "call_0".to_owned(),
            name: "shell".to_owned(),
            arguments: r#"{"command":"cargo test --workspace","cwd":"crates"}"#.to_owned(),
            state: ToolCallState::AwaitingApproval,
            result: None,
            is_error: false,
            display: None,
        });

        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        let ClientRequest::Command(request) = requests.into_iter().next().unwrap() else {
            panic!("expected a command")
        };
        assert!(matches!(
            request.command,
            SessionCommand::RespondToolApproval {
                decision: ApprovalDecision::ApproveForSession {
                    grant: ApprovalGrant::ShellPrefix { prefix },
                },
                ..
            } if prefix == "cargo test --workspace"
        ));
    }

    #[test]
    fn edit_previews_are_kept_only_while_the_approval_is_pending() {
        let mut app = App::new(TuiOptions::default());
        let initial = snapshot();
        let session_id = initial.focused.as_ref().unwrap().summary.id;
        let workspace_id = initial.workspace.id;
        let store_id = initial.cursor.store_id;
        app.apply_snapshot(initial);
        let run_id = id(4, RunId::from_bytes);
        let envelope = |sequence, event| SessionEventEnvelope {
            cursor: EventCursor {
                store_id,
                workspace_id,
                sequence,
            },
            session_id,
            run_id: Some(run_id),
            caused_by: None,
            occurred_at_ms: sequence,
            event,
        };
        let mut tool_call = ToolCallSnapshot {
            id: id(7, ToolCallId::from_bytes),
            session_id,
            run_id,
            turn_ordinal: 1,
            call_ordinal: 1,
            provider_call_id: "call_0".to_owned(),
            name: "edit_file".to_owned(),
            arguments: r#"{"path":"note.txt"}"#.to_owned(),
            state: ToolCallState::AwaitingApproval,
            result: None,
            is_error: false,
            display: None,
        };

        app.apply_live_event(envelope(
            2,
            SessionEvent::ToolApprovalRequested {
                tool_call: tool_call.clone(),
                shell: None,
                edit: Some(EditPreview {
                    path: "note.txt".to_owned(),
                    diff: "-old\n+new".to_owned(),
                }),
            },
        ));
        assert_eq!(
            app.pending_approval_edit().map(|edit| edit.diff.as_str()),
            Some("-old\n+new")
        );

        tool_call.state = ToolCallState::Running;
        app.apply_live_event(envelope(3, SessionEvent::ToolCallStarted { tool_call }));
        assert!(app.pending_approval_edit().is_none());
        assert!(app.edit_previews.is_empty());
    }

    #[test]
    fn slash_command_aliases_quit_and_open_sessions_without_submitting_prompts() {
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(snapshot());

        for command in ["/sessions", "/resume"] {
            app.input = command.to_owned();
            let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            assert!(requests.is_empty());
            assert!(app.session_picker.is_some());
            app.session_picker = None;
        }

        for command in ["/quit", "/exit"] {
            let mut app = App::new(TuiOptions::default());
            app.input = command.to_owned();
            let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            assert!(requests.is_empty());
            assert!(app.quit);
        }
    }

    #[test]
    fn compact_slash_command_sends_compact_session_for_the_focused_idle_session() {
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(snapshot());
        let session_id = app.focused.unwrap();
        app.input = "/compact".to_owned();

        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let ClientRequest::Command(request) = requests.into_iter().next().unwrap() else {
            panic!("expected a command")
        };
        assert_eq!(
            request.command,
            SessionCommand::CompactSession { session_id }
        );
        assert!(app.input.is_empty());
        assert_eq!(app.status.as_deref(), Some("compacting session..."));
    }

    #[test]
    fn compact_refuses_while_the_focused_session_is_not_idle() {
        let mut app = App::new(TuiOptions::default());
        let mut initial = snapshot();
        initial.sessions[0].status = SessionStatus::Running;
        initial.focused.as_mut().unwrap().summary.status = SessionStatus::Running;
        app.apply_snapshot(initial);
        app.input = "/compact".to_owned();

        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(requests.is_empty());
        assert_eq!(
            app.status.as_deref(),
            Some("compaction needs an idle session; wait or cancel first")
        );
    }

    #[test]
    fn unknown_slash_commands_hint_instead_of_prompting() {
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(snapshot());
        app.input = "/frobnicate the context".to_owned();

        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(
            requests.is_empty(),
            "slash input must never become a prompt"
        );
        assert!(app.input.is_empty());
        let status = app.status.as_deref().unwrap();
        assert!(
            status.starts_with("unknown command /frobnicate"),
            "{status}"
        );
        assert!(status.contains("/compact"), "{status}");
    }

    #[test]
    fn session_compacted_events_surface_the_shrink_in_the_status_line() {
        let mut app = App::new(TuiOptions::default());
        let initial = snapshot();
        let session = initial.focused.as_ref().unwrap().summary.clone();
        let (store_id, workspace_id) = (initial.cursor.store_id, initial.workspace.id);
        app.apply_snapshot(initial);

        app.apply_live_event(SessionEventEnvelope {
            cursor: EventCursor {
                store_id,
                workspace_id,
                sequence: 2,
            },
            session_id: session.id,
            run_id: None,
            caused_by: None,
            occurred_at_ms: 2,
            event: SessionEvent::SessionCompacted {
                session,
                summary: Some("intent: keep going".to_owned()),
                before_bytes: 3_250_586,
                after_bytes: 245_760,
            },
        });

        assert_eq!(
            app.status.as_deref(),
            Some("compacted: 3.1 MiB -> 240.0 KiB")
        );
    }

    #[test]
    fn new_slash_command_creates_a_root_session_with_the_selected_model() {
        let model = ModelSelection {
            model: Some("openai/gpt-test".to_owned()),
            max_output_tokens: Some(4_096),
            organization: None,
        };
        let mut app = App::new(TuiOptions {
            settings: Settings::default(),
            model: model.clone(),
            models: Vec::new(),
        });
        app.apply_snapshot(snapshot());
        app.input = "/new".to_owned();

        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(
            &requests[0],
            ClientRequest::Command(CommandRequest {
                command: SessionCommand::CreateSession {
                    parent_id: None,
                    model: selected,
                    ..
                },
                ..
            }) if selected == &model
        ));
    }

    #[test]
    fn slash_autocomplete_filters_selects_and_executes_commands() {
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(snapshot());
        app.input = "/".to_owned();

        assert_eq!(
            app.filtered_slash_commands()
                .iter()
                .map(|command| command.name)
                .collect::<Vec<_>>(),
            [
                "/models",
                "/sessions",
                "/resume",
                "/new",
                "/compact",
                "/quit",
                "/exit"
            ]
        );
        for _ in 0..10 {
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        assert_eq!(app.slash_selected, 6);
        for _ in 0..10 {
            app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        }
        assert_eq!(app.slash_selected, 0);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(app.input.is_empty());
        assert!(app.session_picker.is_some());

        app.session_picker = None;
        app.input = "/qu".to_owned();
        app.slash_selected = 0;
        assert_eq!(
            app.filtered_slash_commands()[0].name,
            "/quit",
            "a command prefix should hide unrelated commands"
        );
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.input.is_empty());
        assert!(app.quit);
    }

    #[test]
    fn session_picker_searches_titles_and_focuses_the_match() {
        let mut initial = snapshot();
        let workspace_id = initial.workspace.id;
        let target = id(9, SessionId::from_bytes);
        initial.sessions[0].title = "Deploy API".to_owned();
        initial.focused.as_mut().unwrap().summary.title = "Deploy API".to_owned();
        initial.sessions.push(SessionSummary {
            id: target,
            workspace_id,
            parent_id: None,
            title: "Fix Login Redirect".to_owned(),
            status: SessionStatus::Idle,
            active_run_id: None,
            queued_prompts: 0,
            model: Some("openai/gpt-test".to_owned()),
            estimated_cost_usd_nanos: Some(0),
            updated_at_ms: 2,
            last_outcome: None,
        });
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(initial);
        app.input = "/sessions".to_owned();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let (changed, requests) = app.handle_terminal_event(Event::Paste("LOGIN".to_owned()));

        assert!(changed);
        assert!(requests.is_empty());
        assert_eq!(app.filtered_sessions(), [target]);
        assert_eq!(app.session_picker.as_ref().unwrap().selected, Some(target));

        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            &requests[0],
            ClientRequest::Snapshot(SnapshotRequest {
                focused_session_id: Some(session_id),
                ..
            }) if *session_id == target
        ));
        assert!(app.session_picker.is_none());
    }

    #[test]
    fn session_picker_keeps_open_when_search_has_no_matches() {
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(snapshot());
        app.open_sessions();
        app.handle_terminal_event(Event::Paste("missing".to_owned()));

        assert!(app.filtered_sessions().is_empty());
        let (changed, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!changed);
        assert!(requests.is_empty());
        assert!(app.session_picker.is_some());
    }

    #[test]
    fn session_picker_deletes_the_highlighted_session_after_a_confirm() {
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(snapshot());
        let session_id = app.focused.unwrap();
        app.open_sessions();

        // The confirm gate: Ctrl-D asks, n keeps, y deletes.
        let (changed, requests) =
            app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(changed);
        assert!(requests.is_empty());
        assert_eq!(
            app.session_picker.as_ref().unwrap().confirm,
            Some(SessionPickerConfirm::Delete(session_id))
        );
        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(requests.is_empty());
        assert_eq!(app.session_picker.as_ref().unwrap().confirm, None);

        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(matches!(
            &requests[0],
            ClientRequest::Command(CommandRequest {
                command: SessionCommand::DeleteSession { session_id: target },
                ..
            }) if *target == session_id
        ));
        assert_eq!(app.session_picker.as_ref().unwrap().confirm, None);
    }

    #[test]
    fn session_picker_refuses_to_delete_a_session_with_an_active_run() {
        let mut initial = snapshot();
        let run_id = id(8, RunId::from_bytes);
        initial.sessions[0].active_run_id = Some(run_id);
        initial.focused.as_mut().unwrap().summary.active_run_id = Some(run_id);
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(initial);
        app.open_sessions();

        let (changed, requests) =
            app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));

        assert!(changed);
        assert!(requests.is_empty());
        assert_eq!(app.session_picker.as_ref().unwrap().confirm, None);
        assert_eq!(
            app.status.as_deref(),
            Some("cancel the active run before deleting")
        );
    }

    #[test]
    fn session_picker_prunes_empty_sessions_after_a_confirm() {
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(snapshot());
        let workspace_id = app.workspace_id.unwrap();
        app.open_sessions();

        let (changed, requests) =
            app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(changed);
        assert!(requests.is_empty());
        assert_eq!(
            app.session_picker.as_ref().unwrap().confirm,
            Some(SessionPickerConfirm::Prune)
        );

        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(matches!(
            &requests[0],
            ClientRequest::Command(CommandRequest {
                command: SessionCommand::PruneSessions { workspace_id: target },
                ..
            }) if *target == workspace_id
        ));
    }

    #[test]
    fn session_deleted_event_drops_state_and_refocuses_a_neighbor() {
        let mut initial = snapshot();
        let workspace_id = initial.workspace.id;
        let deleted = initial.sessions[0].id;
        let neighbor = id(9, SessionId::from_bytes);
        initial.sessions.push(SessionSummary {
            id: neighbor,
            workspace_id,
            parent_id: None,
            title: "Neighbor".to_owned(),
            status: SessionStatus::Idle,
            active_run_id: None,
            queued_prompts: 0,
            model: Some("openai/gpt-test".to_owned()),
            estimated_cost_usd_nanos: Some(0),
            updated_at_ms: 0,
            last_outcome: None,
        });
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(initial);
        assert_eq!(app.focused, Some(deleted));
        let tool_call_id = id(7, qq_protocol::ToolCallId::from_bytes);
        app.sessions
            .get_mut(&deleted)
            .unwrap()
            .tool_calls
            .as_mut()
            .unwrap()
            .push(ToolCallSnapshot {
                id: tool_call_id,
                session_id: deleted,
                run_id: id(8, RunId::from_bytes),
                turn_ordinal: 1,
                call_ordinal: 0,
                provider_call_id: "call_0".to_owned(),
                name: "shell".to_owned(),
                arguments: "{}".to_owned(),
                state: ToolCallState::Running,
                result: None,
                is_error: false,
                display: None,
            });
        app.live_tool_output
            .insert(tool_call_id, "output tail".to_owned());
        app.open_sessions();

        let changed = app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
            cursor: EventCursor {
                store_id: id(3, StoreId::from_bytes),
                workspace_id,
                sequence: 2,
            },
            session_id: deleted,
            run_id: None,
            caused_by: None,
            occurred_at_ms: 2,
            event: SessionEvent::SessionDeleted {
                session_id: deleted,
            },
        }));

        assert!(changed);
        assert!(!app.sessions.contains_key(&deleted));
        assert!(!app.live_tool_output.contains_key(&tool_call_id));
        assert_eq!(app.focused, Some(neighbor));
        assert_eq!(
            app.session_picker.as_ref().unwrap().selected,
            Some(neighbor)
        );
        // The refocus fetches the neighbor's transcript.
        let requests = app.take_requests();
        assert!(matches!(
            &requests[0],
            ClientRequest::Snapshot(SnapshotRequest {
                focused_session_id: Some(session_id),
                ..
            }) if *session_id == neighbor
        ));
        assert!(app.take_requests().is_empty());
    }

    #[test]
    fn session_deleted_event_clears_focus_when_no_session_remains() {
        let mut app = App::new(TuiOptions::default());
        let initial = snapshot();
        let workspace_id = initial.workspace.id;
        let deleted = initial.sessions[0].id;
        app.apply_snapshot(initial);
        assert_eq!(app.focused, Some(deleted));

        app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
            cursor: EventCursor {
                store_id: id(3, StoreId::from_bytes),
                workspace_id,
                sequence: 2,
            },
            session_id: deleted,
            run_id: None,
            caused_by: None,
            occurred_at_ms: 2,
            event: SessionEvent::SessionDeleted {
                session_id: deleted,
            },
        }));

        assert!(app.sessions.is_empty());
        assert_eq!(app.focused, None);
        assert!(app.take_requests().is_empty());
    }

    #[test]
    fn session_updated_event_repoints_the_session_model() {
        let mut app = App::new(TuiOptions::default());
        let initial = snapshot();
        let workspace_id = initial.workspace.id;
        let session_id = initial.sessions[0].id;
        let mut updated = initial.sessions[0].clone();
        updated.model = Some("anthropic/claude-sonnet-5".to_owned());
        app.apply_snapshot(initial);

        app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
            cursor: EventCursor {
                store_id: id(3, StoreId::from_bytes),
                workspace_id,
                sequence: 2,
            },
            session_id,
            run_id: None,
            caused_by: None,
            occurred_at_ms: 2,
            event: SessionEvent::SessionUpdated { session: updated },
        }));

        assert_eq!(
            app.sessions[&session_id].summary.model.as_deref(),
            Some("anthropic/claude-sonnet-5")
        );
    }

    #[test]
    fn context_usage_uses_latest_reported_input_and_model_limit() {
        let selection = ModelSelection {
            model: Some("openai/gpt-test".to_owned()),
            max_output_tokens: Some(4_096),
            organization: None,
        };
        let mut app = App::new(TuiOptions {
            settings: Settings::default(),
            model: selection.clone(),
            models: vec![ModelOption {
                provider: "openai".to_owned(),
                model: "gpt-test".to_owned(),
                name: Some("GPT Test".to_owned()),
                context_window: Some(128_000),
                selection,
            }],
        });
        let mut initial = snapshot();
        let session_id = initial.focused.as_ref().unwrap().summary.id;
        initial.focused.as_mut().unwrap().runs.push(RunSnapshot {
            id: id(7, RunId::from_bytes),
            session_id,
            status: RunStatus::Completed,
            outcome: Some(RunOutcome::Completed),
            usage: Some(TokenUsage {
                input_tokens: 10_000,
                cache_read_input_tokens: 2_000,
                cache_write_input_tokens: 500,
                output_tokens: 1_000,
            }),
            estimated_cost_usd_nanos: Some(1),
        });
        let summary = initial.focused.as_ref().unwrap().summary.clone();
        let workspace_id = initial.workspace.id;
        let store_id = initial.cursor.store_id;
        app.apply_snapshot(initial);

        assert_eq!(app.focused_context_usage(), Some((12_500, 128_000)));

        app.apply_live_event(SessionEventEnvelope {
            cursor: EventCursor {
                store_id,
                workspace_id,
                sequence: 2,
            },
            session_id,
            run_id: Some(id(8, RunId::from_bytes)),
            caused_by: None,
            occurred_at_ms: 2,
            event: SessionEvent::RunFinished {
                session: summary,
                run_id: id(8, RunId::from_bytes),
                outcome: RunOutcome::Completed,
                usage: Some(TokenUsage {
                    input_tokens: 20_000,
                    cache_read_input_tokens: 3_000,
                    cache_write_input_tokens: 1_000,
                    output_tokens: 2_000,
                }),
            },
        });

        assert_eq!(app.focused_context_usage(), Some((24_000, 128_000)));
    }

    #[test]
    fn discovered_models_refresh_existing_session_metadata() {
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(snapshot());
        assert_eq!(app.focused_context_usage(), None);

        app.apply_client_update(ClientUpdate::Models {
            models: vec![ModelDescriptor {
                provider: "openai".to_owned(),
                model: "gpt-test".to_owned(),
                name: Some("GPT Test".to_owned()),
                context_window: Some(128_000),
                selection: ModelSelection {
                    model: Some("openai/gpt-test".to_owned()),
                    max_output_tokens: Some(4_096),
                    organization: None,
                },
            }],
            selected: None,
        });

        let focused = app.focused.unwrap();
        assert_eq!(app.models.len(), 1);
        assert_eq!(app.sessions[&focused].context_window, Some(128_000));
    }

    #[test]
    fn model_refresh_preserves_the_open_picker_selection_by_identity() {
        let selection = ModelSelection {
            model: Some("zeta/model-z".to_owned()),
            max_output_tokens: Some(4_096),
            organization: None,
        };
        let mut app = App::new(TuiOptions {
            settings: Settings::default(),
            model: selection.clone(),
            models: vec![ModelOption {
                provider: "zeta".to_owned(),
                model: "model-z".to_owned(),
                name: Some("Zeta".to_owned()),
                context_window: None,
                selection: selection.clone(),
            }],
        });
        app.apply_snapshot(snapshot());
        app.open_models();

        app.apply_client_update(ClientUpdate::Models {
            models: vec![
                ModelDescriptor {
                    provider: "alpha".to_owned(),
                    model: "model-a".to_owned(),
                    name: Some("Alpha".to_owned()),
                    context_window: Some(64_000),
                    selection: ModelSelection {
                        model: Some("alpha/model-a".to_owned()),
                        max_output_tokens: Some(4_096),
                        organization: None,
                    },
                },
                ModelDescriptor {
                    provider: "zeta".to_owned(),
                    model: "model-z".to_owned(),
                    name: Some("Zeta".to_owned()),
                    context_window: Some(128_000),
                    selection: selection.clone(),
                },
            ],
            selected: Some(selection.clone()),
        });
        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.model, selection);
        // A session is focused, so Enter applies the preserved selection to
        // it rather than creating a new session.
        let focused = app.focused.unwrap();
        assert!(matches!(
            &requests[0],
            ClientRequest::Command(CommandRequest {
                command: SessionCommand::SetSessionModel { session_id, model },
                ..
            }) if model == &selection && *session_id == focused
        ));
    }

    #[test]
    fn first_focused_snapshot_can_arrive_after_the_workspace_snapshot() {
        let mut empty = snapshot();
        empty.sessions.clear();
        empty.focused = None;
        let mut app = App::new(TuiOptions::default());

        app.apply_snapshot(empty);
        app.apply_snapshot(snapshot());

        assert!(app.focused.is_some());
    }

    #[test]
    fn model_picker_applies_to_the_focused_session_and_ctrl_n_creates() {
        let selection = ModelSelection {
            model: Some("anthropic/claude-sonnet-5".to_owned()),
            max_output_tokens: Some(8_192),
            organization: None,
        };
        let mut app = App::new(TuiOptions {
            settings: Settings::default(),
            model: ModelSelection::default(),
            models: vec![ModelOption {
                provider: "anthropic".to_owned(),
                model: "claude-sonnet-5".to_owned(),
                name: Some("Claude Sonnet 5".to_owned()),
                context_window: Some(200_000),
                selection: selection.clone(),
            }],
        });
        app.apply_snapshot(snapshot());
        app.input = "/models".to_owned();

        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(requests.is_empty());
        assert!(app.model_picker.is_some());
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.filtered_models(), vec![0]);

        // Enter with a focused session repoints that session's model.
        let focused = app.focused.unwrap();
        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let ClientRequest::Command(request) = &requests[0] else {
            panic!("expected set-session-model command")
        };
        assert!(matches!(
            &request.command,
            SessionCommand::SetSessionModel { session_id, model }
                if model == &selection && *session_id == focused
        ));
        assert!(app.model_picker.is_none());

        // Ctrl-N creates a fresh session with the selected model instead.
        app.open_models();
        let (_, requests) =
            app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        let ClientRequest::Command(request) = &requests[0] else {
            panic!("expected create-session command")
        };
        assert!(matches!(
            &request.command,
            SessionCommand::CreateSession {
                parent_id: None,
                model,
                ..
            } if model == &selection
        ));
        assert!(app.model_picker.is_none());
    }

    #[test]
    fn model_picker_enter_without_a_focused_session_creates_one() {
        let selection = ModelSelection {
            model: Some("anthropic/claude-sonnet-5".to_owned()),
            max_output_tokens: Some(8_192),
            organization: None,
        };
        let mut app = App::new(TuiOptions {
            settings: Settings::default(),
            model: ModelSelection::default(),
            models: vec![ModelOption {
                provider: "anthropic".to_owned(),
                model: "claude-sonnet-5".to_owned(),
                name: Some("Claude Sonnet 5".to_owned()),
                context_window: Some(200_000),
                selection: selection.clone(),
            }],
        });
        let mut empty = snapshot();
        empty.sessions.clear();
        empty.focused = None;
        app.apply_snapshot(empty);
        assert!(app.focused.is_none());
        app.open_models();

        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let ClientRequest::Command(request) = &requests[0] else {
            panic!("expected create-session command")
        };
        assert!(matches!(
            &request.command,
            SessionCommand::CreateSession {
                parent_id: None,
                model,
                ..
            } if model == &selection
        ));
        assert!(app.model_picker.is_none());
    }

    #[test]
    fn session_shortcuts_require_a_selected_model() {
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(snapshot());
        app.model = ModelSelection::default();

        let (_, requests) = app.handle_action(Action::CreateRootSession);

        assert!(requests.is_empty());
        assert_eq!(
            app.status.as_deref(),
            Some("choose a model with /models before creating a session")
        );
    }

    #[test]
    fn reset_preserves_an_in_flight_prompt_until_its_result() {
        let mut app = App::new(TuiOptions::default());
        let snapshot = snapshot();
        app.apply_snapshot(snapshot.clone());
        app.input = "keep me".to_owned();
        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let ClientRequest::Command(request) = requests.into_iter().next().unwrap() else {
            panic!("expected command")
        };

        app.apply_client_update(ClientUpdate::ResetSnapshot(snapshot));
        app.apply_client_update(ClientUpdate::CommandResult {
            command_id: request.command_id,
            result: Err(ClientFailure::new("server restarted")),
        });

        assert_eq!(app.input, "keep me");
    }

    #[test]
    fn durable_events_update_the_focused_transcript() {
        let mut app = App::new(TuiOptions::default());
        let snapshot = snapshot();
        let session_id = snapshot.focused.as_ref().unwrap().summary.id;
        let workspace_id = snapshot.workspace.id;
        let store_id = snapshot.cursor.store_id;
        app.apply_snapshot(snapshot);
        let run_id = id(4, RunId::from_bytes);
        let message_id = id(5, MessageId::from_bytes);
        let event = |sequence, event| SessionEventEnvelope {
            cursor: EventCursor {
                store_id,
                workspace_id,
                sequence,
            },
            session_id,
            run_id: Some(run_id),
            caused_by: None,
            occurred_at_ms: sequence,
            event,
        };
        let message = MessageSnapshot {
            id: message_id,
            session_id,
            run_id,
            turn_ordinal: 1,
            role: MessageRole::Assistant,
            state: MessageState::Streaming,
            output: String::new(),
            refusal: String::new(),
            created_at_ms: 2,
        };

        app.apply_live_event(event(2, SessionEvent::AssistantMessageStarted { message }));
        app.apply_live_event(event(
            3,
            SessionEvent::TextAppended {
                message_id,
                channel: qq_protocol::TextChannel::Output,
                text: "hello".to_owned(),
            },
        ));
        let tool_call_id = id(6, ToolCallId::from_bytes);
        let mut tool_call = ToolCallSnapshot {
            id: tool_call_id,
            session_id,
            run_id,
            turn_ordinal: 1,
            call_ordinal: 1,
            provider_call_id: "call-1".to_owned(),
            name: "read_file".to_owned(),
            arguments: r#"{"path":"note.txt"}"#.to_owned(),
            state: ToolCallState::Requested,
            result: None,
            is_error: false,
            display: None,
        };
        app.apply_live_event(event(
            4,
            SessionEvent::ToolCallRequested {
                tool_call: tool_call.clone(),
            },
        ));
        tool_call.state = ToolCallState::Completed;
        tool_call.result = Some("contents".to_owned());
        app.apply_live_event(event(
            5,
            SessionEvent::ToolCallFinished {
                tool_call: tool_call.clone(),
            },
        ));

        assert_eq!(
            app.sessions[&session_id].messages.as_ref().unwrap()[0].output,
            "hello"
        );
        assert_eq!(
            app.sessions[&session_id].tool_calls.as_deref(),
            Some([tool_call].as_slice())
        );
    }

    #[test]
    fn live_tool_output_keeps_a_bounded_tail_and_drops_on_terminal_states() {
        let mut app = App::new(TuiOptions::default());
        let initial = snapshot();
        let session_id = initial.focused.as_ref().unwrap().summary.id;
        let workspace_id = initial.workspace.id;
        let store_id = initial.cursor.store_id;
        app.apply_snapshot(initial.clone());
        let run_id = id(4, RunId::from_bytes);
        let tool_call_id = id(6, ToolCallId::from_bytes);
        let event = |sequence, event| SessionEventEnvelope {
            cursor: EventCursor {
                store_id,
                workspace_id,
                sequence,
            },
            session_id,
            run_id: Some(run_id),
            caused_by: None,
            occurred_at_ms: sequence,
            event,
        };
        let delta = |sequence, chunk: &str| {
            event(
                sequence,
                SessionEvent::ToolCallOutputDelta {
                    tool_call_id,
                    chunk: chunk.to_owned(),
                },
            )
        };

        app.apply_live_event(delta(2, "hello "));
        app.apply_live_event(delta(3, "world\n"));
        assert_eq!(
            app.live_tool_output.get(&tool_call_id).map(String::as_str),
            Some("hello world\n")
        );

        // Overflow drops the head — the tail is a live view, not a record —
        // and trimming lands on a character boundary even when the bound
        // falls inside a multi-byte character.
        app.apply_live_event(delta(4, &"€".repeat(2 * MAX_LIVE_TOOL_OUTPUT_BYTES / 3)));
        let buffer = app.live_tool_output.get(&tool_call_id).unwrap();
        assert!(buffer.len() <= MAX_LIVE_TOOL_OUTPUT_BYTES);
        assert!(buffer.len() > MAX_LIVE_TOOL_OUTPUT_BYTES - 4);
        assert!(buffer.chars().all(|character| character == '€'));

        // A terminal state hands display over to the persisted result.
        app.apply_live_event(event(
            5,
            SessionEvent::ToolCallFinished {
                tool_call: ToolCallSnapshot {
                    id: tool_call_id,
                    session_id,
                    run_id,
                    turn_ordinal: 1,
                    call_ordinal: 1,
                    provider_call_id: "call-1".to_owned(),
                    name: "shell".to_owned(),
                    arguments: r#"{"command":"cargo build"}"#.to_owned(),
                    state: ToolCallState::Completed,
                    result: Some("ok\n".to_owned()),
                    is_error: false,
                    display: None,
                },
            },
        ));
        assert!(app.live_tool_output.is_empty());

        // A session snapshot reload replaces live per-call state wholesale.
        app.apply_live_event(delta(6, "restarted\n"));
        assert!(!app.live_tool_output.is_empty());
        let mut reloaded = initial;
        reloaded.cursor.sequence = 7;
        app.apply_snapshot(reloaded);
        assert!(app.live_tool_output.is_empty());
    }

    #[test]
    fn focused_snapshot_is_a_session_baseline_not_a_workspace_cursor() {
        let mut app = App::new(TuiOptions::default());
        let mut initial = snapshot();
        let session_id = initial.focused.as_ref().unwrap().summary.id;
        let workspace_id = initial.workspace.id;
        let store_id = initial.cursor.store_id;
        let run_id = id(4, RunId::from_bytes);
        let message_id = id(5, MessageId::from_bytes);
        initial
            .focused
            .as_mut()
            .unwrap()
            .messages
            .push(MessageSnapshot {
                id: message_id,
                session_id,
                run_id,
                turn_ordinal: 1,
                role: MessageRole::Assistant,
                state: MessageState::Streaming,
                output: String::new(),
                refusal: String::new(),
                created_at_ms: 2,
            });
        app.apply_snapshot(initial.clone());

        let mut ahead = initial;
        ahead.cursor.sequence = 3;
        ahead.focused.as_mut().unwrap().messages[0].output = "ab".to_owned();
        app.apply_snapshot(ahead);
        let event = |sequence, text: &str| SessionEventEnvelope {
            cursor: EventCursor {
                store_id,
                workspace_id,
                sequence,
            },
            session_id,
            run_id: Some(run_id),
            caused_by: None,
            occurred_at_ms: sequence,
            event: SessionEvent::TextAppended {
                message_id,
                channel: qq_protocol::TextChannel::Output,
                text: text.to_owned(),
            },
        };

        app.apply_live_event(event(2, "a"));
        app.apply_live_event(event(3, "b"));
        app.apply_live_event(event(4, "c"));

        assert_eq!(app.last_sequence, 4);
        assert_eq!(
            app.sessions[&session_id].messages.as_ref().unwrap()[0].output,
            "abc"
        );
    }

    #[test]
    fn stale_snapshot_cannot_change_the_selected_session() {
        let mut app = App::new(TuiOptions::default());
        let mut initial = snapshot();
        let old_focus = initial.focused.as_ref().unwrap().summary.id;
        let new_focus = id(9, SessionId::from_bytes);
        initial.sessions.push(SessionSummary {
            id: new_focus,
            workspace_id: initial.workspace.id,
            parent_id: None,
            title: "New focus".to_owned(),
            status: SessionStatus::Idle,
            active_run_id: None,
            queued_prompts: 0,
            model: Some("openai/gpt-test".to_owned()),
            estimated_cost_usd_nanos: Some(0),
            updated_at_ms: 2,
            last_outcome: None,
        });
        app.apply_snapshot(initial.clone());
        app.focus_session(new_focus);

        assert!(!app.apply_snapshot(initial));
        assert_eq!(app.focused, Some(new_focus));
        assert_ne!(app.focused, Some(old_focus));
    }

    #[test]
    fn focused_transcript_retains_only_the_snapshot_window() {
        let mut app = App::new(TuiOptions::default());
        let mut initial = snapshot();
        let session_id = initial.focused.as_ref().unwrap().summary.id;
        let run_id = id(4, RunId::from_bytes);
        let messages = &mut initial.focused.as_mut().unwrap().messages;
        for index in 0..usize::from(SNAPSHOT_MESSAGE_LIMIT) + 4 {
            messages.push(MessageSnapshot {
                id: MessageId::from_bytes((index as u128 + 1).to_be_bytes()),
                session_id,
                run_id,
                turn_ordinal: 0,
                role: MessageRole::Assistant,
                state: MessageState::Complete,
                output: index.to_string(),
                refusal: String::new(),
                created_at_ms: index as u64,
            });
        }

        app.apply_snapshot(initial);
        let retained = app.sessions[&session_id].messages.as_ref().unwrap();
        assert_eq!(retained.len(), usize::from(SNAPSHOT_MESSAGE_LIMIT));
        assert_eq!(retained.first().unwrap().output, "4");

        app.push_message(MessageSnapshot {
            id: MessageId::from_bytes(u128::MAX.to_be_bytes()),
            session_id,
            run_id,
            turn_ordinal: 0,
            role: MessageRole::Assistant,
            state: MessageState::Complete,
            output: "newest".to_owned(),
            refusal: String::new(),
            created_at_ms: u64::MAX,
        });
        let retained = app.sessions[&session_id].messages.as_ref().unwrap();
        assert_eq!(retained.len(), usize::from(SNAPSHOT_MESSAGE_LIMIT));
        assert_eq!(retained.last().unwrap().output, "newest");
    }

    #[test]
    fn mid_run_queued_prompts_stay_after_the_streaming_runs_turn_messages() {
        let mut app = App::new(TuiOptions::default());
        let initial = snapshot();
        let session_id = initial.focused.as_ref().unwrap().summary.id;
        app.apply_snapshot(initial);
        let streaming_run = id(4, RunId::from_bytes);
        let queued_run = id(5, RunId::from_bytes);
        let message = |byte, run_id, turn_ordinal, role, state, output: &str| MessageSnapshot {
            id: id(byte, MessageId::from_bytes),
            session_id,
            run_id,
            turn_ordinal,
            role,
            state,
            output: output.to_owned(),
            refusal: String::new(),
            created_at_ms: u64::from(byte),
        };

        app.push_message(message(
            6,
            streaming_run,
            0,
            MessageRole::User,
            MessageState::Complete,
            "prompt one",
        ));
        app.push_message(message(
            7,
            streaming_run,
            1,
            MessageRole::Assistant,
            MessageState::Complete,
            "turn one",
        ));
        // A prompt queued mid-run arrives before the run's later per-turn
        // messages...
        app.push_message(message(
            8,
            queued_run,
            0,
            MessageRole::User,
            MessageState::Queued,
            "queued prompt",
        ));
        app.push_message(message(
            9,
            streaming_run,
            2,
            MessageRole::Assistant,
            MessageState::Streaming,
            "turn two",
        ));

        // ...yet the live list keeps the snapshot's run-first order: the
        // whole streaming run, then the queued prompt's run.
        let outputs = app.sessions[&session_id]
            .messages
            .as_ref()
            .unwrap()
            .iter()
            .map(|message| message.output.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            outputs,
            ["prompt one", "turn one", "turn two", "queued prompt"]
        );
    }

    #[test]
    fn ctrl_o_cycles_tool_detail_and_yields_to_overlays() {
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(snapshot());
        assert_eq!(app.tool_detail, ToolDetail::Collapsed);
        let ctrl_o = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);

        let (changed, requests) = app.handle_key(ctrl_o);
        assert!(changed);
        assert!(requests.is_empty());
        assert_eq!(app.tool_detail, ToolDetail::Expanded);

        app.handle_key(ctrl_o);
        assert_eq!(app.tool_detail, ToolDetail::Collapsed);

        // Pickers own the keyboard; the toggle must not fire underneath them.
        app.session_picker = Some(SessionPicker {
            query: String::new(),
            selected: None,
            confirm: None,
        });
        app.handle_key(ctrl_o);
        assert_eq!(app.tool_detail, ToolDetail::Collapsed);
    }

    #[test]
    fn page_keys_scroll_the_transcript_by_one_visible_page() {
        let mut app = App::new(TuiOptions::default());
        app.update_transcript_viewport(100, 12);

        let (changed, requests) = app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )));

        assert!(changed);
        assert!(requests.is_empty());
        assert_eq!(app.transcript_scroll_offset(), 12);

        let (changed, requests) = app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::PageDown,
            KeyModifiers::NONE,
        )));

        assert!(changed);
        assert!(requests.is_empty());
        assert_eq!(app.transcript_scroll_offset(), 0);
    }

    #[test]
    fn mouse_wheel_scrolls_the_transcript_by_three_rows() {
        let mut app = App::new(TuiOptions::default());
        app.update_transcript_viewport(100, 12);

        let mouse = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            })
        };
        let (changed, requests) = app.handle_terminal_event(mouse(MouseEventKind::ScrollUp));

        assert!(changed);
        assert!(requests.is_empty());
        assert_eq!(app.transcript_scroll_offset(), 3);

        let (changed, requests) = app.handle_terminal_event(mouse(MouseEventKind::ScrollDown));

        assert!(changed);
        assert!(requests.is_empty());
        assert_eq!(app.transcript_scroll_offset(), 0);
    }

    #[test]
    fn streamed_rows_do_not_move_a_scrolled_transcript() {
        let mut app = App::new(TuiOptions::default());
        app.update_transcript_viewport(40, 10);
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )));

        app.update_transcript_viewport(45, 10);

        assert_eq!(app.transcript_scroll_offset(), 15);
    }

    #[test]
    fn session_and_layout_changes_return_the_transcript_to_the_live_tail() {
        let mut app = App::new(TuiOptions::default());
        app.focused = Some(SessionId::from_bytes([1; 16]));
        app.update_transcript_viewport(100, 10);
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )));

        app.focused = Some(SessionId::from_bytes([2; 16]));
        app.update_transcript_viewport(100, 10);

        assert_eq!(app.transcript_scroll_offset(), 0);

        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )));
        app.layout = app.layout.next();
        app.update_transcript_viewport(100, 10);

        assert_eq!(app.transcript_scroll_offset(), 0);
    }

    #[test]
    fn scrolling_clamps_at_the_oldest_row_and_the_live_tail() {
        let mut app = App::new(TuiOptions::default());
        app.update_transcript_viewport(25, 10);
        let page_up = Event::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        let page_down = Event::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));

        assert!(app.handle_terminal_event(page_up.clone()).0);
        assert!(app.handle_terminal_event(page_up.clone()).0);
        assert_eq!(app.transcript_scroll_offset(), 15);
        assert!(!app.handle_terminal_event(page_up).0);

        assert!(app.handle_terminal_event(page_down.clone()).0);
        assert!(app.handle_terminal_event(page_down.clone()).0);
        assert_eq!(app.transcript_scroll_offset(), 0);
        assert!(!app.handle_terminal_event(page_down).0);
    }

    #[test]
    fn transcript_scroll_controls_are_ignored_by_overlays() {
        let mut app = App::new(TuiOptions::default());
        app.update_transcript_viewport(100, 10);
        app.model_picker = Some(ModelPicker {
            query: String::new(),
            selected: 0,
        });
        let wheel = Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        let page = Event::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));

        assert!(!app.handle_terminal_event(wheel.clone()).0);
        assert!(!app.handle_terminal_event(page.clone()).0);
        assert_eq!(app.transcript_scroll_offset(), 0);

        app.model_picker = None;
        app.session_picker = Some(SessionPicker {
            query: String::new(),
            selected: app.focused,
            confirm: None,
        });
        assert!(!app.handle_terminal_event(wheel).0);
        assert!(!app.handle_terminal_event(page).0);
        assert_eq!(app.transcript_scroll_offset(), 0);
    }
}
