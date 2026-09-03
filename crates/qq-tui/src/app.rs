use std::collections::{HashMap, VecDeque};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use qq_protocol::{
    ApprovalDecision, ApprovalGrant, ApprovalMode, ApprovalResolution, CommandId, CommandOutcome,
    CommandRequest, EditPreview, MessageSnapshot, ModelDescriptor, ModelSelection, RunActivity,
    RunId, SessionCommand, SessionEventEnvelope, SessionId, SessionSnapshot, SessionStatus,
    SessionSummary, SnapshotRequest, ToolCallSnapshot, ToolCallState, WorkspaceId,
    WorkspaceSnapshot,
};
use thiserror::Error;

use crate::{
    Action, ClientFailure, ClientPort, ClientRequest, ClientUpdate, ConnectionState, Layout,
    Settings,
    commands::{self, Command, SlashEntry},
    composer::Composer,
    input::{Mode, Overlay, SessionConfirm},
    panes::{Axis, Direction, PaneId, Panes, Viewport},
    picker::Picker,
    terminal,
    theme::Theme,
};
use reduce::{retain_recent_messages, retain_recent_tool_calls};

mod reduce;

const MAX_INPUT_BYTES: usize = 64 * 1024;
const MAX_RECENT_EVENTS: usize = 1024;
const SNAPSHOT_SESSION_LIMIT: u16 = 512;
const SNAPSHOT_MESSAGE_LIMIT: u16 = 256;
const MAX_RECENT_TOOL_CALLS: usize = 64;
/// Per-call cap on buffered live tool output. The buffer is a display tail,
/// not a record: the head drops first, and the persisted bounded result
/// replaces the buffer when the call finishes.
const MAX_LIVE_TOOL_OUTPUT_BYTES: usize = 4 * 1024;
const MAX_PROMPT_HISTORY: usize = 100;
const MOUSE_SCROLL_ROWS: usize = 3;
/// Notices are deliberately ephemeral. At the 125 ms UI tick this keeps each
/// notice visible for five seconds without making it permanent UI.
const NOTICE_TICKS: u16 = 40;
/// Locally queued drafts per session.
const MAX_QUEUED_DRAFTS: usize = 8;
/// Animation ticks (125 ms) within which a second Esc cancels the active run.
const ESC_CANCEL_TICKS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoticeLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Default)]
pub struct TuiOptions {
    pub settings: Settings,
    pub model: ModelSelection,
    pub models: Vec<ModelOption>,
    /// Every selectable theme; the first is active at startup. An empty list
    /// means the compiled `qq` theme.
    pub themes: Vec<Theme>,
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

/// Warm transcript bodies kept loaded at once. The focused session is always
/// warm; the rest are the most recently focused sessions so switching back
/// costs no round trip.
const WARM_BODY_LIMIT: usize = 8;
/// Bytes of assistant text retained per session for the live status tail.
const LIVE_TAIL_BYTES: usize = 256;

/// Bytes of reasoning text retained per run.
const MAX_REASONING_BYTES: usize = 16 * 1024;

/// One run's displayable reasoning: text accumulated from `ReasoningDelta`
/// events plus whether the block is still streaming.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Reasoning {
    pub text: String,
    pub streaming: bool,
    /// Animation ticks observed while streaming, for an elapsed label.
    pub ticks: usize,
}

impl Reasoning {
    fn append(&mut self, text: &str) {
        self.text.push_str(text);
        if self.text.len() > MAX_REASONING_BYTES {
            let mut start = self.text.len() - MAX_REASONING_BYTES;
            while !self.text.is_char_boundary(start) {
                start += 1;
            }
            self.text.drain(..start);
        }
    }
}

/// Whether reasoning blocks render as a collapsed one-liner or in full.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ReasoningDetail {
    #[default]
    Collapsed,
    Expanded,
}

/// Cheap per-session liveness reduced from every event, whether or not the
/// session's transcript body is loaded. This is what a sidebar or session
/// list shows for the sessions the user is not looking at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LiveStatus {
    /// Last bytes of the newest assistant message, whitespace-collapsed.
    pub tail: String,
    /// The previous append ended in whitespace that has not yet been emitted
    /// as a separator.
    tail_space_pending: bool,
    /// Name of the tool call currently running or awaiting approval.
    pub active_tool: Option<String>,
    /// Tool calls awaiting an approval answer. A set rather than a count so
    /// replayed or repeated events cannot drift it.
    pub awaiting_approval: std::collections::BTreeSet<qq_protocol::ToolCallId>,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionView {
    pub summary: SessionSummary,
    /// `Some` only while the body is warm; `None` means summary-only.
    pub messages: Option<Vec<MessageSnapshot>>,
    pub tool_calls: Option<Vec<ToolCallSnapshot>>,
    pub context_window: Option<u32>,
    /// Latest replaceable liveness state for the active run, seeded from the
    /// summary on load and replaced by `RunActivityChanged` events.
    pub activity: Option<(RunId, RunActivity)>,
    pub live: LiveStatus,
    /// Provider-exposed reasoning per run, bounded, kept only for runs whose
    /// messages are loaded. Display-only: never fed back to the model.
    pub reasoning: HashMap<RunId, Reasoning>,
    /// Focus clock at the last time this session was focused; orders warm
    /// body eviction. Zero for never-focused sessions.
    pub(crate) last_focused: u64,
    pub(crate) loaded_through: u64,
}

impl SessionView {
    pub(super) fn summary_only(
        summary: SessionSummary,
        context_window: Option<u32>,
        loaded_through: u64,
    ) -> Self {
        let activity = summary.active_run_id.zip(summary.activity);
        Self {
            summary,
            messages: None,
            tool_calls: None,
            context_window,
            activity,
            live: LiveStatus::default(),
            reasoning: HashMap::new(),
            last_focused: 0,
            loaded_through,
        }
    }

    /// Refresh the summary in place. Activity follows the summary when the
    /// summary carries it or the run changed; a live event already applied
    /// for the same run is kept when the summary is silent.
    pub(super) fn set_summary(&mut self, summary: SessionSummary, context_window: Option<u32>) {
        match (summary.active_run_id, summary.activity) {
            (Some(run_id), Some(activity)) => self.activity = Some((run_id, activity)),
            (Some(run_id), None) => {
                if self.activity.is_some_and(|(active, _)| active != run_id) {
                    self.activity = None;
                }
            }
            (None, _) => self.activity = None,
        }
        self.summary = summary;
        self.context_window = context_window;
    }

    pub(crate) fn is_warm(&self) -> bool {
        self.messages.is_some()
    }
}

impl LiveStatus {
    /// Derive status from a loaded body, as after a snapshot.
    fn from_body(messages: &[MessageSnapshot], tool_calls: &[ToolCallSnapshot]) -> Self {
        let mut live = Self::default();
        if let Some(message) = messages
            .iter()
            .rev()
            .find(|message| message.role == qq_protocol::MessageRole::Assistant)
        {
            live.set_tail(&message.output);
        }
        for call in tool_calls {
            live.note_tool_call(call);
        }
        live
    }

    /// Replace the tail with the last [`LIVE_TAIL_BYTES`] of `text`, with
    /// whitespace collapsed to single spaces so it fits one row.
    pub(super) fn set_tail(&mut self, text: &str) {
        let mut start = text.len().saturating_sub(LIVE_TAIL_BYTES);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        self.tail.clear();
        self.tail_space_pending = false;
        self.push_collapsed(&text[start..]);
    }

    fn push_collapsed(&mut self, text: &str) {
        for character in text.chars() {
            if character.is_whitespace() {
                self.tail_space_pending = !self.tail.is_empty();
            } else if let Some(character) = terminal_safe_character(character) {
                if self.tail_space_pending {
                    self.tail.push(' ');
                    self.tail_space_pending = false;
                }
                self.tail.push(character);
            }
        }
    }

    /// Append streamed text and trim the front back to the byte bound.
    pub(super) fn append_tail(&mut self, text: &str) {
        if text.len() >= LIVE_TAIL_BYTES {
            self.set_tail(text);
            return;
        }
        self.push_collapsed(text);
        if self.tail.len() > LIVE_TAIL_BYTES {
            let mut start = self.tail.len() - LIVE_TAIL_BYTES;
            while !self.tail.is_char_boundary(start) {
                start += 1;
            }
            self.tail.drain(..start);
        }
    }

    pub(super) fn note_tool_call(&mut self, call: &ToolCallSnapshot) {
        match call.state {
            ToolCallState::AwaitingApproval => {
                self.awaiting_approval.insert(call.id);
                self.active_tool = Some(call.name.clone());
            }
            ToolCallState::Running | ToolCallState::Requested => {
                self.awaiting_approval.remove(&call.id);
                self.active_tool = Some(call.name.clone());
            }
            ToolCallState::Completed
            | ToolCallState::Failed
            | ToolCallState::Denied
            | ToolCallState::Interrupted => {
                self.awaiting_approval.remove(&call.id);
                if self.active_tool.as_deref() == Some(call.name.as_str()) {
                    self.active_tool = None;
                }
            }
        }
    }
}

/// Whether the live session tree renders beside the transcript.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Sidebar {
    /// Visible when the terminal is at least [`SIDEBAR_AUTO_WIDTH`] columns.
    #[default]
    Auto,
    Shown,
    Hidden,
}

/// Terminal width at which `Sidebar::Auto` shows the sidebar.
pub(crate) const SIDEBAR_AUTO_WIDTH: usize = 120;

impl Sidebar {
    #[must_use]
    pub(crate) const fn next(self) -> Self {
        match self {
            Self::Auto | Self::Shown => Self::Hidden,
            Self::Hidden => Self::Shown,
        }
    }

    #[must_use]
    pub(crate) const fn visible(self, width: usize) -> bool {
        match self {
            Self::Auto => width >= SIDEBAR_AUTO_WIDTH,
            Self::Shown => true,
            Self::Hidden => false,
        }
    }
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

/// An event worth interrupting the user for while the terminal is unfocused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Attention {
    /// A tool call is waiting for the user to approve it.
    ApprovalRequested { session_title: String },
    /// A run finished in a session; the user may want to read the result.
    RunFinished { session_title: String },
}

impl Attention {
    /// One-line text for a desktop notification.
    pub(crate) fn summary(&self) -> String {
        match self {
            Self::ApprovalRequested { session_title } => {
                format!("qq: {session_title} needs approval")
            }
            Self::RunFinished { session_title } => format!("qq: {session_title} finished"),
        }
    }
}

#[derive(Debug, Clone)]
enum PendingIntent {
    Create,
    Prompt {
        session_id: SessionId,
        text: String,
    },
    Cancel {
        session_id: SessionId,
    },
    Compact {
        session_id: SessionId,
    },
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
    /// Tiled panes; the focused pane decides which session the composer,
    /// approvals, footers, and tree navigation act on.
    pub(crate) panes: Panes,
    /// Monotonic counter bumped on every focus change; stamps `last_focused`.
    focus_clock: u64,
    /// The open overlay, if any. At most one overlay owns input at a time.
    pub overlay: Option<Overlay>,
    pub composer: Composer,
    prompt_history: HashMap<SessionId, VecDeque<String>>,
    history_position: Option<usize>,
    history_draft: Option<String>,
    /// Cursor into the slash autocomplete list. The query is the composer
    /// text itself, so only the cursor lives here.
    slash: Picker,
    /// Drafts held locally (Alt-Enter) while the focused session runs; they
    /// submit in order when it goes idle. Bounded by [`MAX_QUEUED_DRAFTS`].
    drafts: HashMap<SessionId, VecDeque<String>>,
    /// Tick at which Esc was last pressed with nothing to dismiss; a second
    /// press within [`ESC_CANCEL_TICKS`] cancels the active run.
    esc_armed_at: Option<usize>,
    /// Set when the user asked to edit the draft externally. The loop takes
    /// it, suspends the terminal, runs the editor, and hands the text back.
    editor_requested: bool,
    /// Server capability gate for `Command::SteerRun`; false until the
    /// capability document exists.
    steering_available: bool,
    pub connection: ConnectionState,
    pub status: Option<String>,
    /// Session owning the current transient notice. A notice never follows
    /// the user into another session.
    status_session_id: Option<SessionId>,
    pub(crate) status_level: NoticeLevel,
    status_ticks_left: u16,
    pub animation_tick: usize,
    pub quit: bool,
    pub tool_detail: ToolDetail,
    pub reasoning_detail: ReasoningDetail,
    /// Session sidebar visibility. `Auto` shows it when the terminal is wide
    /// enough; the toggle command cycles through explicit on and off.
    pub sidebar: Sidebar,
    /// Selectable themes and the index of the active one. Changing the
    /// index bumps `theme_generation` so the renderer repaints everything.
    pub(crate) themes: Vec<Theme>,
    pub(crate) theme: usize,
    pub(crate) theme_generation: u64,
    /// Whether the terminal window has keyboard focus, from the terminal's
    /// focus events. Assumed focused until told otherwise.
    terminal_focused: bool,
    /// Something happened that deserves the user's attention while the
    /// terminal was unfocused; the loop takes it and rings the terminal.
    attention: Option<Attention>,
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
            panes: Panes::default(),
            focus_clock: 0,
            overlay: None,
            composer: Composer::default(),
            prompt_history: HashMap::new(),
            history_position: None,
            history_draft: None,
            slash: Picker::new(),
            drafts: HashMap::new(),
            esc_armed_at: None,
            editor_requested: false,
            steering_available: false,
            connection: ConnectionState::Connecting,
            status: None,
            status_session_id: None,
            status_level: NoticeLevel::Info,
            status_ticks_left: 0,
            animation_tick: 0,
            quit: false,
            tool_detail: ToolDetail::default(),
            reasoning_detail: ReasoningDetail::default(),
            sidebar: Sidebar::default(),
            themes: if options.themes.is_empty() {
                vec![Theme::default()]
            } else {
                options.themes
            },
            theme: 0,
            theme_generation: 0,
            terminal_focused: true,
            attention: None,
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
    /// Who owns keyboard input right now. Overlays win over the approval
    /// prompt, which wins over the composer.
    pub(crate) fn mode(&self) -> Mode {
        match &self.overlay {
            Some(overlay) => overlay.mode(),
            None if self.pending_approval().is_some() => Mode::Approval,
            None => Mode::Compose,
        }
    }

    pub fn take_requests(&mut self) -> Vec<ClientRequest> {
        std::mem::take(&mut self.queued_requests)
    }

    /// Whether the user asked for an external editor since the last call.
    /// Returns the current expanded draft to seed the editor with.
    pub fn take_editor_request(&mut self) -> Option<String> {
        if !std::mem::take(&mut self.editor_requested) {
            return None;
        }
        Some(self.composer.expanded())
    }

    /// The external editor could not deliver text; the draft stays as it was.
    pub fn note_editor_failure(&mut self, reason: &str) {
        self.set_warning(reason.to_owned());
    }

    /// Install text returned by the external editor. `None` means it exited
    /// without changing the draft.
    pub fn apply_editor_result(&mut self, text: Option<String>) -> bool {
        let Some(text) = text else {
            self.set_info("external editor made no changes".to_owned());
            return true;
        };
        let mut sanitized = String::with_capacity(text.len().min(MAX_INPUT_BYTES));
        for character in text.chars() {
            if sanitized.len() + character.len_utf8() > MAX_INPUT_BYTES {
                break;
            }
            if let Some(character) = composer_character(character) {
                sanitized.push(character);
            }
        }
        let trimmed = sanitized.trim_end().to_owned();
        self.composer.replace(trimmed);
        self.reset_history_browse();
        self.slash.select(0);
        true
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
                self.panes.clear_sessions();
                self.overlay = None;
                self.last_sequence = 0;
                self.recent_events.clear();
                self.edit_previews.clear();
                self.live_tool_output.clear();
                self.set_warning("session state reset after reconnecting".to_owned());
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
                            self.adopt_created_session(session_id);
                        }
                        if let Some(PendingIntent::Cancel { session_id }) = intent.as_ref() {
                            self.set_info_for(
                                Some(*session_id),
                                "cancellation requested".to_owned(),
                            );
                        }
                        if let CommandOutcome::ToolApprovalResolved { resolution, .. } =
                            receipt.outcome
                        {
                            self.set_info(
                                match resolution {
                                    ApprovalResolution::ApprovedOnce => "tool call approved",
                                    ApprovalResolution::ApprovedForSession => {
                                        "tool call approved for this session"
                                    }
                                    ApprovalResolution::ApprovedForWorkspace => {
                                        "tool call approved for this workspace"
                                    }
                                    ApprovalResolution::ApprovedByReviewer => {
                                        "tool call already approved by the reviewer"
                                    }
                                    ApprovalResolution::Denied => "tool call denied",
                                    ApprovalResolution::DeniedTimeout => {
                                        "tool call already denied by timeout"
                                    }
                                    ApprovalResolution::DeniedByReviewer => {
                                        "tool call already denied by the reviewer"
                                    }
                                }
                                .to_owned(),
                            );
                        }
                        if matches!(receipt.outcome, CommandOutcome::RunAlreadyFinished { .. }) {
                            self.set_warning("run already finished".to_owned());
                        }
                        match &receipt.outcome {
                            CommandOutcome::CompactionQueued { session_id, .. } => {
                                self.set_info_for(
                                    Some(*session_id),
                                    "compacting session...".to_owned(),
                                );
                            }
                            CommandOutcome::SessionModelSet { session_id, model } => {
                                self.set_info_for(
                                    Some(*session_id),
                                    format!(
                                        "session model set to {}",
                                        model.model.as_deref().unwrap_or("default")
                                    ),
                                );
                            }
                            CommandOutcome::SessionDeleted { .. } => {
                                self.set_warning("session deleted".to_owned());
                            }
                            CommandOutcome::SessionsPruned { deleted: 0, .. } => {
                                self.set_warning("no empty sessions to delete".to_owned());
                            }
                            CommandOutcome::SessionsPruned { deleted: 1, .. } => {
                                self.set_warning("deleted 1 empty session".to_owned());
                            }
                            CommandOutcome::SessionsPruned { deleted, .. } => {
                                self.set_warning(format!("deleted {deleted} empty sessions"));
                            }
                            _ => {}
                        }
                    }
                    Err(error) => self.reject_pending(command_id, error),
                }
                true
            }
            ClientUpdate::SnapshotFailed(error) => {
                self.set_warning(error.message().to_owned());
                true
            }
        }
    }

    fn apply_models(
        &mut self,
        models: Vec<ModelDescriptor>,
        selected_model: Option<ModelSelection>,
    ) {
        // Remember what the open model picker points at so the refreshed
        // catalog keeps the cursor on the same model.
        let selected = match &self.overlay {
            Some(Overlay::Models(picker)) => {
                let filtered = self.filtered_models();
                filtered
                    .get(picker.selected(filtered.len()))
                    .and_then(|index| self.models.get(*index))
                    .map(|model| (model.provider.clone(), model.model.clone()))
            }
            Some(Overlay::Sessions { .. } | Overlay::Themes { .. }) | None => None,
        };
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
        if matches!(self.overlay, Some(Overlay::Models(_))) {
            let identities: Vec<(String, String)> = self
                .filtered_models()
                .into_iter()
                .map(|index| {
                    let model = &self.models[index];
                    (model.provider.clone(), model.model.clone())
                })
                .collect();
            if let Some(Overlay::Models(picker)) = &mut self.overlay {
                picker.preserve(selected, identities);
            }
        }
    }

    fn apply_snapshot(&mut self, snapshot: WorkspaceSnapshot) -> bool {
        let initial = self.workspace_id.is_none();
        if self
            .workspace_id
            .is_some_and(|workspace| workspace != snapshot.workspace.id)
        {
            self.set_warning("server returned a snapshot for another workspace".to_owned());
            return true;
        }
        let snapshot_focus = snapshot.focused.as_ref().map(|focused| focused.summary.id);
        // A late snapshot for a session no pane shows any more is stale
        // navigation output; installing it would yank focus back.
        if !initial
            && self.focused().is_some()
            && snapshot_focus.is_some_and(|id| !self.panes.sessions().any(|shown| shown == id))
        {
            return false;
        }
        if snapshot.cursor.sequence < self.last_sequence
            && self
                .recent_events
                .front()
                .is_none_or(|event| event.cursor.sequence > snapshot.cursor.sequence + 1)
        {
            self.set_warning("snapshot was too stale; reconnecting is required".to_owned());
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
                match self.sessions.entry(summary.id) {
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        entry.get_mut().set_summary(summary, context_window);
                    }
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(SessionView::summary_only(
                            summary,
                            context_window,
                            snapshot_sequence,
                        ));
                    }
                }
            }
        }
        for body in snapshot.included {
            self.install_session_snapshot(body, snapshot_sequence);
        }
        if let Some(focused) = snapshot.focused {
            let focused_id = focused.summary.id;
            self.install_session_snapshot(focused, snapshot_sequence);
            // The body may have been fetched for a non-focused pane (the
            // user moved on before it arrived); only the initial snapshot
            // and a still-focused pane move focus.
            if initial || self.focused().is_none() || self.focused() == Some(focused_id) {
                self.set_focus(focused_id);
            } else {
                self.set_focus_clock(focused_id);
            }
        } else if self.focused().is_none()
            && let Some(first) = self.root_sessions().first().copied()
        {
            self.set_focus(first);
        }
        self.evict_cold_bodies();
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

    /// Load one session's transcript body. Other warm bodies are untouched;
    /// `evict_cold_bodies` enforces the warm limit afterwards.
    fn install_session_snapshot(&mut self, snapshot: SessionSnapshot, loaded_through: u64) {
        let session_id = snapshot.summary.id;
        // Live tool output for calls this body no longer reports as running
        // would render forever; drop this session's buffers.
        let running: std::collections::HashSet<_> = snapshot
            .tool_calls
            .iter()
            .filter(|call| call.state == ToolCallState::Running)
            .map(|call| call.id)
            .collect();
        self.live_tool_output.retain(|id, _| {
            running.contains(id)
                || self
                    .sessions
                    .get(&session_id)
                    .and_then(|session| session.tool_calls.as_ref())
                    .is_none_or(|calls| !calls.iter().any(|call| call.id == *id))
        });
        let mut messages = snapshot.messages;
        retain_recent_messages(&mut messages);
        let history = messages
            .iter()
            .filter(|message| message.role == qq_protocol::MessageRole::User)
            .map(|message| message.output.clone())
            .filter(|prompt| !prompt.trim().is_empty())
            .collect::<VecDeque<_>>();
        self.prompt_history.insert(
            session_id,
            history
                .into_iter()
                .rev()
                .take(MAX_PROMPT_HISTORY)
                .rev()
                .collect(),
        );
        let mut tool_calls = snapshot.tool_calls;
        retain_recent_tool_calls(&mut tool_calls);
        let context_window = model_context_window(&self.models, snapshot.summary.model.as_deref());
        let last_focused = self
            .sessions
            .get(&session_id)
            .map_or(0, |session| session.last_focused);
        let mut view = SessionView::summary_only(snapshot.summary, context_window, loaded_through);
        view.live = LiveStatus::from_body(&messages, &tool_calls);
        view.messages = Some(messages);
        view.tool_calls = Some(tool_calls);
        view.last_focused = last_focused;
        self.sessions.insert(session_id, view);
    }

    /// A session this client just created has an empty transcript by
    /// construction, so it is warm immediately: focus moves in this frame and
    /// no snapshot round trip is needed before the user can type.
    pub(super) fn adopt_created_session(&mut self, session_id: SessionId) {
        if let Some(session) = self.sessions.get_mut(&session_id)
            && !session.is_warm()
        {
            session.messages = Some(Vec::new());
            session.tool_calls = Some(Vec::new());
        }
        self.set_focus(session_id);
        self.reset_history_browse();
        self.evict_cold_bodies();
    }

    /// Show `session_id` in the focused pane and stamp it so warm-body
    /// eviction keeps the most recently viewed sessions. Does not request
    /// anything.
    fn set_focus(&mut self, session_id: SessionId) {
        self.panes.focused_mut().session = Some(session_id);
        self.set_focus_clock(session_id);
    }

    fn set_focus_clock(&mut self, session_id: SessionId) {
        self.focus_clock += 1;
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.last_focused = self.focus_clock;
        }
    }

    /// Drop transcript bodies beyond the warm limit, least recently focused
    /// first. Sessions shown in any pane are pinned and never evicted.
    /// Summaries and live status stay, so the sidebar and pickers keep
    /// working for cold sessions.
    fn evict_cold_bodies(&mut self) {
        let pinned: std::collections::HashSet<SessionId> = self.panes.sessions().collect();
        let mut warm: Vec<(u64, SessionId)> = self
            .sessions
            .values()
            .filter(|session| session.is_warm() && !pinned.contains(&session.summary.id))
            .map(|session| (session.last_focused, session.summary.id))
            .collect();
        let keep = WARM_BODY_LIMIT.saturating_sub(pinned.len());
        if warm.len() <= keep {
            return;
        }
        warm.sort_unstable();
        let evict = warm.len() - keep;
        for (_, session_id) in warm.into_iter().take(evict) {
            if let Some(session) = self.sessions.get_mut(&session_id) {
                if let Some(calls) = session.tool_calls.take() {
                    for call in calls {
                        self.live_tool_output.remove(&call.id);
                    }
                }
                session.messages = None;
            }
        }
    }

    fn apply_live_event(&mut self, event: SessionEventEnvelope) -> bool {
        if self
            .workspace_id
            .is_some_and(|workspace| workspace != event.cursor.workspace_id)
        {
            self.set_warning("server sent an event for another workspace".to_owned());
            return true;
        }
        if event.cursor.sequence <= self.last_sequence {
            return false;
        }
        if self.last_sequence != 0 && event.cursor.sequence != self.last_sequence + 1 {
            self.connection = ConnectionState::Replaying;
            self.set_warning("session event gap detected".to_owned());
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

    fn set_notice_for(&mut self, session_id: Option<SessionId>, text: String, level: NoticeLevel) {
        self.status = Some(text);
        self.status_session_id = session_id;
        self.status_level = level;
        // Errors are sticky: a failure must stay visible until the user acts
        // (a new prompt, an interrupt, or another notice replaces it).
        // Informational notices expire on their own.
        self.status_ticks_left = match level {
            NoticeLevel::Error => 0,
            NoticeLevel::Info | NoticeLevel::Warning => NOTICE_TICKS,
        };
    }

    fn set_notice(&mut self, text: String, level: NoticeLevel) {
        self.set_notice_for(self.focused(), text, level);
    }

    fn set_info_for(&mut self, session_id: Option<SessionId>, text: String) {
        self.set_notice_for(session_id, text, NoticeLevel::Info);
    }

    fn set_info(&mut self, text: String) {
        self.set_notice(text, NoticeLevel::Info);
    }

    fn set_warning(&mut self, text: String) {
        self.set_notice(text, NoticeLevel::Warning);
    }

    fn set_error_for(&mut self, session_id: Option<SessionId>, text: String) {
        self.set_notice_for(session_id, text, NoticeLevel::Error);
    }

    pub(crate) fn visible_status(&self) -> Option<(&str, NoticeLevel)> {
        if self.status_session_id != self.focused() {
            return None;
        }
        self.status.as_deref().map(|text| (text, self.status_level))
    }

    fn expire_status(&mut self) -> bool {
        if self.status.is_none() || self.status_ticks_left == 0 {
            return false;
        }
        self.status_ticks_left -= 1;
        if self.status_ticks_left == 0 {
            self.status = None;
            return true;
        }
        false
    }

    fn reject_pending(&mut self, command_id: CommandId, error: ClientFailure) {
        let intent = self.pending.remove(&command_id);
        let status_session_id = match &intent {
            Some(PendingIntent::Prompt { session_id, .. })
            | Some(PendingIntent::Cancel { session_id })
            | Some(PendingIntent::Compact { session_id }) => Some(*session_id),
            Some(PendingIntent::Approval { tool_call_id }) => self
                .sessions
                .values()
                .flat_map(|session| session.tool_calls.iter().flatten())
                .find(|tool_call| tool_call.id == *tool_call_id)
                .map(|tool_call| tool_call.session_id),
            Some(PendingIntent::Create) | None => self.focused(),
        };
        match intent {
            Some(PendingIntent::Prompt { session_id, text })
                if self.focused() == Some(session_id) && self.composer.text.is_empty() =>
            {
                self.composer.replace(text);
            }
            Some(PendingIntent::Approval { tool_call_id }) => {
                // Re-open the prompt so the user can answer again.
                self.answered_approvals.remove(&tool_call_id);
            }
            _ => {}
        }
        self.set_error_for(status_session_id, error.message().to_owned());
    }

    pub fn handle_terminal_event(&mut self, event: Event) -> (bool, Vec<ClientRequest>) {
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                self.handle_key(key)
            }
            Event::Paste(text) => {
                let changed = match &mut self.overlay {
                    Some(Overlay::Sessions { picker, .. }) => {
                        let changed = picker.push_query(&text);
                        self.reset_session_picker_selection();
                        changed
                    }
                    Some(Overlay::Models(picker) | Overlay::Themes { picker, .. }) => {
                        picker.push_query(&text)
                    }
                    None => self.push_composer_text(&text),
                };
                (changed, Vec::new())
            }
            Event::Mouse(mouse) if self.overlay.is_none() => {
                // The wheel scrolls the pane under the cursor; a click focuses
                // it. Coordinates outside every pane fall back to the focused
                // pane so a wheel over the chrome still does something useful.
                let under = self
                    .panes
                    .hit(usize::from(mouse.column), usize::from(mouse.row))
                    .unwrap_or(self.panes.focused_id());
                let rows = isize::try_from(MOUSE_SCROLL_ROWS).unwrap_or(isize::MAX);
                let changed = match mouse.kind {
                    MouseEventKind::ScrollUp => self.scroll_pane(under, rows),
                    MouseEventKind::ScrollDown => self.scroll_pane(under, -rows),
                    MouseEventKind::Down(_) => self.focus_pane(under),
                    _ => false,
                };
                (changed, Vec::new())
            }
            Event::FocusGained => {
                self.terminal_focused = true;
                self.attention = None;
                (true, Vec::new())
            }
            Event::FocusLost => {
                self.terminal_focused = false;
                (true, Vec::new())
            }
            Event::Resize(_, _) => (true, Vec::new()),
            Event::Key(_) | Event::Mouse(_) => (false, Vec::new()),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> (bool, Vec<ClientRequest>) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return self.execute(Command::Quit);
        }
        match self.mode() {
            Mode::Sessions => self.handle_session_picker_key(key),
            Mode::Models => self.handle_model_picker_key(key),
            Mode::Themes => self.handle_theme_picker_key(key),
            Mode::Approval => self.handle_approval_key(key),
            Mode::Compose => self.handle_compose_key(key),
        }
    }

    fn handle_compose_key(&mut self, key: KeyEvent) -> (bool, Vec<ClientRequest>) {
        // Newline chords insert into the composer. Handle them before slash
        // completion and configured bindings so they never submit.
        if is_composer_newline_key(key) {
            let changed = self.push_input('\n');
            return (changed, Vec::new());
        }
        // Ctrl-Enter (or Ctrl-Q where the terminal cannot report it) queues
        // the draft explicitly instead of sending it.
        if (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL))
            || (matches!(key.code, KeyCode::Char('q' | 'Q'))
                && key.modifiers == KeyModifiers::CONTROL)
        {
            return self.execute(Command::QueueDraft);
        }
        if key.code == KeyCode::Up && key.modifiers == KeyModifiers::ALT {
            return self.execute(Command::DequeueDraft);
        }
        if matches!(key.code, KeyCode::Char('e' | 'E')) && key.modifiers == KeyModifiers::ALT {
            return self.execute(Command::OpenEditor);
        }
        if key.code != KeyCode::Esc {
            self.esc_armed_at = None;
        }
        if let Some(result) = self.handle_slash_key(key.code) {
            return result;
        }
        if let Some(action) = self.settings.action_for(key) {
            return self.execute(commands::command_for_action(action));
        }
        // Ctrl-O cycles tool call detail. Checked after configured bindings so
        // a user rebinding Ctrl-O keeps winning.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('o' | 'O'))
        {
            return self.execute(Command::ToggleToolDetail);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('b' | 'B'))
        {
            return self.execute(Command::ToggleSidebar);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('r' | 'R'))
        {
            return self.execute(Command::ToggleReasoning);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('g' | 'G'))
        {
            return self.execute(Command::FocusNextApproval);
        }
        // Alt-arrows walk the session tree: down to the first child, left and
        // right across siblings in spawn order. Alt-Up dequeues a draft (see
        // above); Esc reaches the parent.
        if key.modifiers == KeyModifiers::ALT {
            let command = match key.code {
                KeyCode::Down => Some(Command::FocusFirstChild),
                KeyCode::Left => Some(Command::FocusPreviousSibling),
                KeyCode::Right => Some(Command::FocusNextSibling),
                // Panes: Alt-\ and Alt-- draw the divider they create;
                // Alt-H/J/K/L move focus like a tiling window manager.
                KeyCode::Char('\\') => Some(Command::SplitBeside),
                KeyCode::Char('-') => Some(Command::SplitBelow),
                KeyCode::Char('w' | 'W') => Some(Command::ClosePane),
                KeyCode::Char('z' | 'Z') => Some(Command::ZoomPane),
                KeyCode::Char('h') => Some(Command::FocusPaneLeft),
                KeyCode::Char('j') => Some(Command::FocusPaneDown),
                KeyCode::Char('k') => Some(Command::FocusPaneUp),
                KeyCode::Char('l') => Some(Command::FocusPaneRight),
                _ => None,
            };
            if let Some(command) = command {
                return self.execute(command);
            }
        }
        // Alt-Shift-H/J/K/L move the divider enclosing the focused pane.
        if key.modifiers == KeyModifiers::ALT | KeyModifiers::SHIFT {
            let command = match key.code {
                KeyCode::Char('H') => Some(Command::ResizePaneLeft),
                KeyCode::Char('J') => Some(Command::ResizePaneDown),
                KeyCode::Char('K') => Some(Command::ResizePaneUp),
                KeyCode::Char('L') => Some(Command::ResizePaneRight),
                _ => None,
            };
            if let Some(command) = command {
                return self.execute(command);
            }
        }
        match key.code {
            KeyCode::Esc => {
                // A sticky error notice dismisses first: acknowledging the
                // failure is the most immediate intent Esc can carry.
                if self.status.is_some()
                    && self.status_level == NoticeLevel::Error
                    && self.status_session_id == self.focused()
                {
                    self.status = None;
                    return (true, Vec::new());
                }
                // While a run is active, Esc twice within a short window cancels
                // it; the first press only arms and shows the hint.
                let running = self
                    .focused()
                    .and_then(|id| self.sessions.get(&id))
                    .is_some_and(|session| session.summary.active_run_id.is_some());
                if running {
                    let now = self.animation_tick;
                    if self
                        .esc_armed_at
                        .is_some_and(|armed| now.wrapping_sub(armed) <= ESC_CANCEL_TICKS)
                    {
                        self.esc_armed_at = None;
                        return self.cancel_run();
                    }
                    self.esc_armed_at = Some(now);
                    self.set_info("press Esc again to cancel the run".to_owned());
                    return (true, Vec::new());
                }
                if let Some(parent) = self
                    .focused()
                    .and_then(|focused| self.sessions.get(&focused)?.summary.parent_id)
                {
                    return self.focus_session(parent);
                }
                (false, Vec::new())
            }
            KeyCode::Enter => self.submit_prompt(),
            KeyCode::PageUp => (self.scroll_focused_page(true), Vec::new()),
            KeyCode::PageDown => (self.scroll_focused_page(false), Vec::new()),
            KeyCode::Backspace
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
            {
                let changed = self.composer.kill_word_back();
                if changed {
                    self.reset_history_browse();
                    self.slash.select(0);
                }
                (changed, Vec::new())
            }
            KeyCode::Backspace => {
                let changed = self.composer.backspace();
                if changed {
                    self.reset_history_browse();
                    self.slash.select(0);
                }
                (changed, Vec::new())
            }
            KeyCode::Delete => {
                let changed = self.composer.delete();
                if changed {
                    self.reset_history_browse();
                }
                (changed, Vec::new())
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                (self.composer.move_word_left(), Vec::new())
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                (self.composer.move_word_right(), Vec::new())
            }
            KeyCode::Left => (self.composer.move_left(), Vec::new()),
            KeyCode::Right => (self.composer.move_right(), Vec::new()),
            KeyCode::Home => (self.composer.move_line_start(), Vec::new()),
            KeyCode::End => (self.composer.move_line_end(), Vec::new()),
            KeyCode::Char(character) if key.modifiers == KeyModifiers::CONTROL => {
                let changed = match character.to_ascii_lowercase() {
                    'a' => self.composer.move_line_start(),
                    'e' => self.composer.move_line_end(),
                    'w' => self.composer.kill_word_back(),
                    'k' => self.composer.kill_to_line_end(),
                    'u' => self.composer.kill_to_line_start(),
                    'y' => self.composer.yank(),
                    'z' | '_' => self.composer.undo(),
                    _ => return (false, Vec::new()),
                };
                if changed {
                    self.reset_history_browse();
                    self.slash.select(0);
                }
                (changed, Vec::new())
            }
            KeyCode::Up => {
                let changed = self.composer.move_up() || self.browse_prompt_history(false);
                (changed, Vec::new())
            }
            KeyCode::Down => {
                let changed = self.composer.move_down() || self.browse_prompt_history(true);
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

    /// The session shown in the focused pane; what every focus-dependent
    /// surface (composer, approvals, footers, tree navigation) acts on.
    pub(crate) fn focused(&self) -> Option<SessionId> {
        self.panes.focused().session
    }

    /// The active theme. The theme picker previews by moving `theme`, so
    /// this is always what the next frame should paint with.
    pub(crate) fn theme(&self) -> &Theme {
        &self.themes[self.theme.min(self.themes.len() - 1)]
    }

    fn open_themes(&mut self) -> (bool, Vec<ClientRequest>) {
        if self.themes.len() < 2 {
            self.set_info(
                "only the compiled `qq` theme is available; add themes/<name>.ron to choose"
                    .to_owned(),
            );
            return (true, Vec::new());
        }
        let mut picker = Picker::new();
        picker.select(self.theme);
        self.overlay = Some(Overlay::Themes {
            picker,
            restore: self.theme,
        });
        (true, Vec::new())
    }

    /// Indexes into `themes` matching the open theme picker's query.
    pub(crate) fn filtered_themes(&self) -> Vec<usize> {
        let Some(Overlay::Themes { picker, .. }) = &self.overlay else {
            return Vec::new();
        };
        self.themes
            .iter()
            .enumerate()
            .filter(|(_, theme)| picker.matches([theme.name.as_str()]))
            .map(|(index, _)| index)
            .collect()
    }

    fn set_theme(&mut self, index: usize) -> bool {
        if index >= self.themes.len() || index == self.theme {
            return false;
        }
        self.theme = index;
        self.theme_generation += 1;
        true
    }

    /// Theme picker keys. Up/Down and typing preview the highlighted theme
    /// immediately; Enter keeps it, Esc restores the theme that was active
    /// when the picker opened.
    fn handle_theme_picker_key(&mut self, key: KeyEvent) -> (bool, Vec<ClientRequest>) {
        let filtered = self.filtered_themes();
        let Some(Overlay::Themes { picker, restore }) = &mut self.overlay else {
            return (false, Vec::new());
        };
        let restore = *restore;
        let changed = match key.code {
            KeyCode::Esc => {
                self.overlay = None;
                self.set_theme(restore);
                return (true, Vec::new());
            }
            KeyCode::Enter => {
                let name = self.theme().name.clone();
                self.overlay = None;
                self.set_info(format!(
                    "theme `{name}`; set `theme: \"{name}\"` in tui.ron to keep it"
                ));
                return (true, Vec::new());
            }
            KeyCode::Up => {
                picker.move_up();
                true
            }
            KeyCode::Down => {
                picker.move_down(filtered.len());
                true
            }
            KeyCode::Backspace => picker.pop_query(),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let mut encoded = [0; 4];
                picker.push_query(character.encode_utf8(&mut encoded))
            }
            _ => false,
        };
        if changed {
            let filtered = self.filtered_themes();
            if let Some(Overlay::Themes { picker, .. }) = &self.overlay
                && let Some(index) = filtered.get(picker.selected(filtered.len()))
            {
                self.set_theme(*index);
            }
        }
        (changed, Vec::new())
    }

    /// Take the pending attention request, if any. The loop rings the
    /// terminal once per request.
    pub(crate) fn take_attention(&mut self) -> Option<Attention> {
        self.attention.take()
    }

    /// Record something the user should notice. Only fires while the
    /// terminal is unfocused: a focused user is already looking.
    pub(super) fn request_attention(&mut self, attention: Attention) {
        if !self.terminal_focused {
            self.attention = Some(attention);
        }
    }

    /// Reconcile pane `id`'s viewport with the body laid out this frame.
    pub(crate) fn update_viewport(
        &mut self,
        id: PaneId,
        body_rows: usize,
        height: usize,
        preserve_tail_anchor: bool,
    ) {
        let layout = self.layout;
        if let Some(pane) = self.panes.get_mut(id) {
            let context = (pane.session, layout);
            pane.viewport
                .update(context, body_rows, height, preserve_tail_anchor);
        }
    }

    /// Test view of the focused pane's viewport through the pre-pane API.
    #[cfg(test)]
    pub(crate) fn update_transcript_viewport(
        &mut self,
        body_rows: usize,
        height: usize,
        preserve_tail_anchor: bool,
    ) {
        let id = self.panes.focused_id();
        self.update_viewport(id, body_rows, height, preserve_tail_anchor);
    }

    #[cfg(test)]
    pub(crate) fn transcript_scroll_offset(&self) -> usize {
        self.panes.focused().viewport.offset()
    }

    pub(crate) fn viewport(&self, id: PaneId) -> Option<&Viewport> {
        self.panes.get(id).map(|pane| &pane.viewport)
    }

    fn scroll_pane(&mut self, id: PaneId, rows: isize) -> bool {
        let Some(pane) = self.panes.get_mut(id) else {
            return false;
        };
        match rows.cmp(&0) {
            std::cmp::Ordering::Greater => pane.viewport.scroll_up(rows.unsigned_abs()),
            std::cmp::Ordering::Less => pane.viewport.scroll_down(rows.unsigned_abs()),
            std::cmp::Ordering::Equal => false,
        }
    }

    fn scroll_focused_page(&mut self, up: bool) -> bool {
        let id = self.panes.focused_id();
        let page = isize::try_from(self.panes.focused().viewport.height()).unwrap_or(isize::MAX);
        self.scroll_pane(id, if up { page } else { -page })
    }

    /// Split the focused pane. The new pane inherits the session and takes
    /// focus, so nothing needs fetching.
    fn split_pane(&mut self, axis: Axis) -> (bool, Vec<ClientRequest>) {
        match self.panes.split(axis) {
            Some(_) => (true, Vec::new()),
            None => {
                self.set_warning(format!(
                    "at most {} panes can be open",
                    crate::panes::MAX_PANES
                ));
                (true, Vec::new())
            }
        }
    }

    fn close_pane(&mut self) -> (bool, Vec<ClientRequest>) {
        let closed = self.panes.close();
        if closed.is_some() {
            self.reset_history_browse();
            self.evict_cold_bodies();
        } else {
            self.set_info("the last pane stays open".to_owned());
        }
        (true, Vec::new())
    }

    /// Move focus to a neighbouring pane. Focus changes the session the
    /// composer targets, so the history browse resets like a session switch.
    fn focus_pane(&mut self, id: PaneId) -> bool {
        if !self.panes.focus(id) {
            return false;
        }
        if let Some(session_id) = self.focused() {
            self.set_focus_clock(session_id);
        }
        self.reset_history_browse();
        true
    }

    fn focus_pane_direction(&mut self, direction: Direction) -> (bool, Vec<ClientRequest>) {
        match self.panes.neighbour(direction) {
            Some(id) => (self.focus_pane(id), Vec::new()),
            None => (false, Vec::new()),
        }
    }

    /// Run one command from the registry. Every command surface — keybinding,
    /// slash entry, and later the palette — ends here so behavior cannot drift
    /// between them.
    pub(crate) fn execute(&mut self, command: Command) -> (bool, Vec<ClientRequest>) {
        match command {
            Command::OpenModels => self.open_models(),
            Command::OpenThemes => self.open_themes(),
            Command::OpenSessions => self.open_sessions(),
            Command::OpenAgents => self.open_agents(),
            Command::ToggleSessions => {
                if matches!(self.overlay, Some(Overlay::Sessions { .. })) {
                    self.overlay = None;
                    (true, Vec::new())
                } else {
                    self.open_sessions()
                }
            }
            Command::NewRootSession => self.create_session(None),
            Command::NewChildSession => self.create_session(self.focused()),
            Command::CompactSession => self.compact_session(),
            Command::CancelRun => self.cancel_run(),
            Command::SelectThreadline => {
                self.layout = Layout::Threadline;
                (true, Vec::new())
            }
            Command::SelectFoldFocus => {
                self.layout = Layout::FoldFocus;
                (true, Vec::new())
            }
            Command::NextLayout => {
                self.layout = self.layout.next();
                (true, Vec::new())
            }
            Command::PreviousLayout => {
                self.layout = self.layout.previous();
                (true, Vec::new())
            }
            Command::ToggleToolDetail => {
                self.tool_detail = self.tool_detail.next();
                (true, Vec::new())
            }
            Command::ToggleReasoning => {
                self.reasoning_detail = match self.reasoning_detail {
                    ReasoningDetail::Collapsed => ReasoningDetail::Expanded,
                    ReasoningDetail::Expanded => ReasoningDetail::Collapsed,
                };
                (true, Vec::new())
            }
            Command::ToggleSidebar => {
                self.sidebar = self.sidebar.next();
                (true, Vec::new())
            }
            Command::FocusParent => match self
                .focused()
                .and_then(|focused| self.sessions.get(&focused)?.summary.parent_id)
            {
                Some(parent) => self.focus_session(parent),
                None => (false, Vec::new()),
            },
            Command::FocusFirstChild => match self
                .focused()
                .and_then(|focused| self.children_of(focused).first().copied())
            {
                Some(child) => self.focus_session(child),
                None => (false, Vec::new()),
            },
            Command::FocusNextSibling => match self.sibling(1) {
                Some(sibling) => self.focus_session(sibling),
                None => (false, Vec::new()),
            },
            Command::FocusPreviousSibling => match self.sibling(-1) {
                Some(sibling) => self.focus_session(sibling),
                None => (false, Vec::new()),
            },
            Command::OpenEditor => {
                self.editor_requested = true;
                (true, Vec::new())
            }
            Command::SplitBeside => self.split_pane(Axis::Columns),
            Command::SplitBelow => self.split_pane(Axis::Rows),
            Command::ClosePane => self.close_pane(),
            Command::ZoomPane => (self.panes.toggle_zoom(), Vec::new()),
            Command::FocusPaneLeft => self.focus_pane_direction(Direction::Left),
            Command::FocusPaneRight => self.focus_pane_direction(Direction::Right),
            Command::FocusPaneUp => self.focus_pane_direction(Direction::Up),
            Command::FocusPaneDown => self.focus_pane_direction(Direction::Down),
            Command::ResizePaneLeft => (self.panes.resize(Direction::Left), Vec::new()),
            Command::ResizePaneRight => (self.panes.resize(Direction::Right), Vec::new()),
            Command::ResizePaneUp => (self.panes.resize(Direction::Up), Vec::new()),
            Command::ResizePaneDown => (self.panes.resize(Direction::Down), Vec::new()),
            Command::QueueDraft => self.queue_draft(),
            Command::DequeueDraft => self.dequeue_draft(),
            Command::SteerRun => {
                if self.steering_available {
                    // H3 supplies the steering command; until then the flag
                    // is never set, so this arm is unreachable in practice.
                    self.set_warning("steering is not wired to a protocol command yet".to_owned());
                } else {
                    self.set_warning(
                        "this server does not support steering; the draft was queued instead"
                            .to_owned(),
                    );
                    return self.queue_draft();
                }
                (true, Vec::new())
            }
            Command::FocusNextApproval => match self.next_session_awaiting_approval() {
                Some(session_id) => self.focus_session(session_id),
                None => {
                    self.set_info("no session is waiting for approval".to_owned());
                    (true, Vec::new())
                }
            },
            Command::Quit => {
                self.quit = true;
                (true, Vec::new())
            }
        }
    }

    fn open_models(&mut self) -> (bool, Vec<ClientRequest>) {
        if self.models.is_empty() {
            self.set_warning("no authenticated providers have selectable models".to_owned());
            return (true, Vec::new());
        }
        self.overlay = Some(Overlay::Models(Picker::new()));
        (true, Vec::new())
    }

    /// Indexes into `models` matching the open model picker's query, in
    /// catalog order. Empty when the model picker is closed.
    pub(crate) fn filtered_models(&self) -> Vec<usize> {
        let Some(Overlay::Models(picker)) = &self.overlay else {
            return Vec::new();
        };
        self.models
            .iter()
            .enumerate()
            .filter(|(_, option)| {
                picker.matches([
                    option.provider.as_str(),
                    option.model.as_str(),
                    option.name.as_deref().unwrap_or_default(),
                ])
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn handle_model_picker_key(&mut self, key: KeyEvent) -> (bool, Vec<ClientRequest>) {
        let filtered = self.filtered_models();
        let Some(Overlay::Models(picker)) = &mut self.overlay else {
            return (false, Vec::new());
        };
        match key.code {
            KeyCode::Esc => {
                self.overlay = None;
                (true, Vec::new())
            }
            KeyCode::Up => {
                picker.move_up();
                (true, Vec::new())
            }
            KeyCode::Down => {
                picker.move_down(filtered.len());
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
                    .focused()
                    .filter(|session_id| self.sessions.contains_key(session_id));
                let result = match focused {
                    Some(session_id) => self.set_session_model(session_id, model),
                    None => self.create_session_with_model(None, model),
                };
                if !result.1.is_empty() {
                    self.overlay = None;
                }
                result
            }
            KeyCode::Char('n' | 'N') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let Some(model) = self.selected_picker_model(&filtered) else {
                    return (false, Vec::new());
                };
                let result = self.create_session_with_model(None, model);
                if !result.1.is_empty() {
                    self.overlay = None;
                }
                result
            }
            KeyCode::Backspace => (picker.pop_query(), Vec::new()),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let mut encoded = [0; 4];
                (
                    picker.push_query(character.encode_utf8(&mut encoded)),
                    Vec::new(),
                )
            }
            _ => (false, Vec::new()),
        }
    }

    fn selected_picker_model(&self, filtered: &[usize]) -> Option<ModelSelection> {
        let Some(Overlay::Models(picker)) = &self.overlay else {
            return None;
        };
        filtered
            .get(picker.selected(filtered.len()))
            .and_then(|index| self.models.get(*index))
            .map(|option| option.selection.clone())
    }

    fn set_session_model(
        &mut self,
        session_id: SessionId,
        model: ModelSelection,
    ) -> (bool, Vec<ClientRequest>) {
        let Ok(command_id) = CommandId::generate() else {
            self.set_warning("secure randomness is unavailable".to_owned());
            return (true, Vec::new());
        };
        // Remember the pick as the client default so /new and later creates
        // keep using it until the user chooses another model.
        self.model = model.clone();
        (
            true,
            vec![ClientRequest::Command(CommandRequest {
                command_id,
                command: SessionCommand::SetSessionModel { session_id, model },
            })],
        )
    }

    fn open_sessions(&mut self) -> (bool, Vec<ClientRequest>) {
        self.open_session_picker(None)
    }

    /// `/agents`: the focused session's root and every descendant, so the
    /// user can see and jump between the agents one task fanned out into.
    fn open_agents(&mut self) -> (bool, Vec<ClientRequest>) {
        let Some(focused) = self.focused() else {
            self.set_warning("focus a session to view its agents".to_owned());
            return (true, Vec::new());
        };
        let mut root = focused;
        while let Some(parent) = self
            .sessions
            .get(&root)
            .and_then(|session| session.summary.parent_id)
        {
            root = parent;
        }
        self.open_session_picker(Some(root))
    }

    fn open_session_picker(&mut self, scope: Option<SessionId>) -> (bool, Vec<ClientRequest>) {
        let selected = self
            .focused()
            .filter(|session_id| self.sessions.contains_key(session_id))
            .or_else(|| self.thread_order().first().copied());
        self.overlay = Some(Overlay::Sessions {
            picker: Picker::new(),
            scope,
            selected,
            confirm: None,
        });
        (true, Vec::new())
    }

    /// Sessions matching the open session picker's query, in tree order.
    /// Empty when the session picker is closed.
    pub(crate) fn filtered_sessions(&self) -> Vec<SessionId> {
        let Some(Overlay::Sessions { picker, scope, .. }) = &self.overlay else {
            return Vec::new();
        };
        self.thread_order()
            .into_iter()
            .filter(|session_id| {
                scope.is_none_or(|root| self.is_descendant_or_self(*session_id, root))
            })
            .filter(|session_id| picker.matches([self.sessions[session_id].summary.title.as_str()]))
            .collect()
    }

    fn is_descendant_or_self(&self, session_id: SessionId, root: SessionId) -> bool {
        let mut cursor = Some(session_id);
        while let Some(current) = cursor {
            if current == root {
                return true;
            }
            cursor = self
                .sessions
                .get(&current)
                .and_then(|session| session.summary.parent_id);
        }
        false
    }

    /// The highlighted session in the picker, if it is still in the filtered
    /// list.
    pub(crate) fn session_picker_selected(&self) -> Option<SessionId> {
        let Some(Overlay::Sessions { selected, .. }) = &self.overlay else {
            return None;
        };
        (*selected).filter(|selected| self.filtered_sessions().contains(selected))
    }

    pub(crate) fn session_picker_confirm(&self) -> Option<SessionConfirm> {
        match &self.overlay {
            Some(Overlay::Sessions { confirm, .. }) => *confirm,
            Some(Overlay::Models(_) | Overlay::Themes { .. }) | None => None,
        }
    }

    fn handle_session_picker_key(&mut self, key: KeyEvent) -> (bool, Vec<ClientRequest>) {
        if let Some(confirm) = self.session_picker_confirm() {
            return self.handle_session_picker_confirm_key(key, confirm);
        }
        let filtered = self.filtered_sessions();
        let current = self.session_picker_selected();
        let position = current.and_then(|current| filtered.iter().position(|id| *id == current));
        match key.code {
            KeyCode::Esc => {
                self.overlay = None;
                (true, Vec::new())
            }
            KeyCode::Up => {
                let next = filtered
                    .get(position.unwrap_or_default().saturating_sub(1))
                    .copied();
                self.set_session_picker_selection(next);
                (true, Vec::new())
            }
            KeyCode::Down => {
                let next = filtered
                    .get(
                        position
                            .map(|position| position + 1)
                            .unwrap_or_default()
                            .min(filtered.len().saturating_sub(1)),
                    )
                    .copied();
                self.set_session_picker_selection(next);
                (true, Vec::new())
            }
            KeyCode::Enter => {
                let Some(current) = current else {
                    return (false, Vec::new());
                };
                self.overlay = None;
                self.focus_session(current)
            }
            KeyCode::Backspace => {
                let changed = self
                    .overlay
                    .as_mut()
                    .is_some_and(|overlay| overlay.picker_mut().pop_query());
                self.reset_session_picker_selection();
                (changed, Vec::new())
            }
            // Ctrl-modified so plain letters keep feeding the search query.
            KeyCode::Delete => self.request_delete_confirmation(current),
            KeyCode::Char('d' | 'D') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.request_delete_confirmation(current)
            }
            KeyCode::Char('p' | 'P') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(Overlay::Sessions { confirm, .. }) = &mut self.overlay {
                    *confirm = Some(SessionConfirm::Prune);
                }
                (true, Vec::new())
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let mut encoded = [0; 4];
                let text = character.encode_utf8(&mut encoded);
                let changed = self
                    .overlay
                    .as_mut()
                    .is_some_and(|overlay| overlay.picker_mut().push_query(text));
                self.reset_session_picker_selection();
                (changed, Vec::new())
            }
            _ => (false, Vec::new()),
        }
    }

    fn set_session_picker_selection(&mut self, next: Option<SessionId>) {
        if let Some(Overlay::Sessions { selected, .. }) = &mut self.overlay {
            *selected = next;
        }
    }

    fn request_delete_confirmation(
        &mut self,
        selected: Option<SessionId>,
    ) -> (bool, Vec<ClientRequest>) {
        let Some(selected) = selected else {
            return (false, Vec::new());
        };
        if self
            .sessions
            .get(&selected)
            .is_some_and(|session| session.summary.active_run_id.is_some())
        {
            self.set_warning("cancel the active run before deleting".to_owned());
            return (true, Vec::new());
        }
        if let Some(Overlay::Sessions { confirm, .. }) = &mut self.overlay {
            *confirm = Some(SessionConfirm::Delete(selected));
        }
        (true, Vec::new())
    }

    fn handle_session_picker_confirm_key(
        &mut self,
        key: KeyEvent,
        confirm: SessionConfirm,
    ) -> (bool, Vec<ClientRequest>) {
        match key.code {
            KeyCode::Char('y' | 'Y') => {
                self.clear_session_picker_confirm();
                match confirm {
                    SessionConfirm::Delete(session_id) => self.delete_session(session_id),
                    SessionConfirm::Prune => self.prune_sessions(),
                }
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                self.clear_session_picker_confirm();
                (true, Vec::new())
            }
            _ => (false, Vec::new()),
        }
    }

    fn clear_session_picker_confirm(&mut self) {
        if let Some(Overlay::Sessions { confirm, .. }) = &mut self.overlay {
            *confirm = None;
        }
    }

    fn delete_session(&mut self, session_id: SessionId) -> (bool, Vec<ClientRequest>) {
        let Ok(command_id) = CommandId::generate() else {
            self.set_warning("secure randomness is unavailable".to_owned());
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
            self.set_warning("workspace is still connecting".to_owned());
            return (true, Vec::new());
        };
        let Ok(command_id) = CommandId::generate() else {
            self.set_warning("secure randomness is unavailable".to_owned());
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

    fn reset_session_picker_selection(&mut self) {
        let first = self.filtered_sessions().first().copied();
        self.set_session_picker_selection(first);
    }

    /// Focus a session. A warm body renders immediately with no request; a
    /// cold one shows its summary and live tail while its body is fetched.
    pub(crate) fn focus_session(&mut self, session_id: SessionId) -> (bool, Vec<ClientRequest>) {
        self.set_focus(session_id);
        self.reset_history_browse();
        self.evict_cold_bodies();
        if self
            .sessions
            .get(&session_id)
            .is_some_and(SessionView::is_warm)
        {
            return (true, Vec::new());
        }
        let Some(workspace_id) = self.workspace_id else {
            return (true, Vec::new());
        };
        (
            true,
            vec![ClientRequest::Snapshot(SnapshotRequest {
                workspace_id,
                focused_session_id: Some(session_id),
                include_sessions: Vec::new(),
                session_limit: SNAPSHOT_SESSION_LIMIT,
                message_limit: SNAPSHOT_MESSAGE_LIMIT,
            })],
        )
    }

    fn create_session(&mut self, parent_id: Option<SessionId>) -> (bool, Vec<ClientRequest>) {
        let model = self.model_for_new_session();
        self.create_session_with_model(parent_id, model)
    }

    /// Choose the model for an implicit create (`/new` and create shortcuts).
    /// An explicit/configured client default wins. If none is available (for
    /// example while reattaching before model discovery completes), inherit
    /// the focused session's route.
    fn model_for_new_session(&self) -> ModelSelection {
        if self.model.model.as_deref().is_some_and(valid_model_route) {
            return self.model.clone();
        }

        let Some(route) = self
            .focused()
            .and_then(|session_id| self.sessions.get(&session_id))
            .and_then(|session| session.summary.model.as_deref())
            .filter(|route| valid_model_route(route))
        else {
            return self.model.clone();
        };

        self.models
            .iter()
            .find(|option| option.selection.model.as_deref() == Some(route))
            .map(|option| option.selection.clone())
            .unwrap_or_else(|| ModelSelection {
                model: Some(route.to_owned()),
                ..ModelSelection::default()
            })
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
            self.set_warning("choose a model with /models before creating a session".to_owned());
            return (true, Vec::new());
        }
        let Some(workspace_id) = self.workspace_id else {
            self.set_warning("workspace is still connecting".to_owned());
            return (true, Vec::new());
        };
        let Ok(command_id) = CommandId::generate() else {
            self.set_warning("secure randomness is unavailable".to_owned());
            return (true, Vec::new());
        };
        // Keep the chosen model as the client default for the rest of this TUI
        // process until /models picks something else.
        self.model = model.clone();
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
        let prompt = self.composer.expanded().trim().to_owned();
        if prompt.is_empty() {
            return (false, Vec::new());
        }
        // Reserved composer commands stay client-side. Every other leading
        // slash is submitted through the ordinary command path so the shared
        // runtime can resolve an explicit command or skill consistently for
        // direct, embedded, and remote clients.
        if prompt.starts_with('/') {
            let name = prompt.split_whitespace().next().unwrap_or(&prompt);
            if let Some(entry) = commands::slash_entries().find(|entry| entry.name == name) {
                self.composer.clear();
                self.slash.select(0);
                return self.execute(entry.command);
            }
        }
        let Some(session_id) = self.focused() else {
            self.set_warning("create a session before sending a prompt".to_owned());
            return (true, Vec::new());
        };
        // Enter during an active run steers when the server supports it and
        // otherwise holds the draft locally until the run finishes. Sending
        // it to the server queue now would lose the ability to edit it.
        let running = self
            .sessions
            .get(&session_id)
            .is_some_and(|session| session.summary.active_run_id.is_some());
        if running {
            if self.steering_available {
                return self.execute(Command::SteerRun);
            }
            return self.queue_draft();
        }
        self.submit_text(session_id, prompt)
    }

    /// Send `prompt` to `session_id` as a new run.
    fn submit_text(&mut self, session_id: SessionId, prompt: String) -> (bool, Vec<ClientRequest>) {
        let Ok(command_id) = CommandId::generate() else {
            self.set_warning("secure randomness is unavailable".to_owned());
            return (true, Vec::new());
        };
        self.record_prompt(session_id, &prompt);
        self.composer.clear();
        self.reset_history_browse();
        // Submitting a new prompt acknowledges any sticky failure notice.
        if self.status_level == NoticeLevel::Error && self.status_session_id == Some(session_id) {
            self.status = None;
        }
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
                command: SessionCommand::SubmitPrompt {
                    session_id,
                    prompt,
                    limits: qq_protocol::RunLimits::default(),
                },
            })],
        )
    }

    /// Hold the composer text for the focused session until its run ends.
    fn queue_draft(&mut self) -> (bool, Vec<ClientRequest>) {
        let prompt = self.composer.expanded().trim().to_owned();
        if prompt.is_empty() {
            return (false, Vec::new());
        }
        let Some(session_id) = self.focused() else {
            self.set_warning("create a session before queueing a prompt".to_owned());
            return (true, Vec::new());
        };
        let drafts = self.drafts.entry(session_id).or_default();
        if drafts.len() >= MAX_QUEUED_DRAFTS {
            self.set_warning(format!(
                "at most {MAX_QUEUED_DRAFTS} drafts can wait per session"
            ));
            return (true, Vec::new());
        }
        drafts.push_back(prompt);
        self.composer.clear();
        self.reset_history_browse();
        self.slash.select(0);
        (true, Vec::new())
    }

    /// Pull the newest queued draft back into the composer for editing. A
    /// non-empty composer is queued first so nothing is lost.
    fn dequeue_draft(&mut self) -> (bool, Vec<ClientRequest>) {
        let Some(session_id) = self.focused() else {
            return (false, Vec::new());
        };
        if !self.composer.text.is_empty() {
            let (changed, _) = self.queue_draft();
            if !changed {
                return (false, Vec::new());
            }
            // The draft just queued is newest; rotate so the previously
            // newest one comes back.
            if let Some(drafts) = self.drafts.get_mut(&session_id)
                && drafts.len() > 1
                && let Some(just_queued) = drafts.pop_back()
            {
                drafts.push_front(just_queued);
            }
        }
        let Some(draft) = self
            .drafts
            .get_mut(&session_id)
            .and_then(VecDeque::pop_back)
        else {
            return (false, Vec::new());
        };
        self.composer.replace(draft);
        (true, Vec::new())
    }

    /// Drafts waiting for `session_id` in submission order.
    pub(crate) fn queued_drafts(&self, session_id: SessionId) -> impl Iterator<Item = &str> {
        self.drafts
            .get(&session_id)
            .into_iter()
            .flatten()
            .map(String::as_str)
    }

    /// Submit the oldest waiting draft once the session goes idle. Called by
    /// the reducer on `RunFinished`; one draft per run so each becomes its
    /// own run in order.
    pub(super) fn flush_draft(&mut self, session_id: SessionId) {
        let Some(draft) = self
            .drafts
            .get_mut(&session_id)
            .and_then(VecDeque::pop_front)
        else {
            return;
        };
        let (_, requests) = self.submit_text(session_id, draft);
        self.queued_requests.extend(requests);
    }

    fn record_prompt(&mut self, session_id: SessionId, prompt: &str) {
        let history = self.prompt_history.entry(session_id).or_default();
        if history.back().is_some_and(|previous| previous == prompt) {
            return;
        }
        history.push_back(prompt.to_owned());
        while history.len() > MAX_PROMPT_HISTORY {
            history.pop_front();
        }
    }

    fn browse_prompt_history(&mut self, forward: bool) -> bool {
        let Some(session_id) = self.focused() else {
            return false;
        };
        let Some(history) = self.prompt_history.get(&session_id) else {
            return false;
        };
        if history.is_empty() {
            return false;
        }

        if forward {
            let Some(position) = self.history_position else {
                return false;
            };
            if position + 1 < history.len() {
                self.history_position = Some(position + 1);
                self.composer.replace(history[position + 1].clone());
            } else {
                self.history_position = None;
                self.composer
                    .replace(self.history_draft.take().unwrap_or_default());
            }
            return true;
        }

        let position = match self.history_position {
            Some(0) => return false,
            Some(position) => position - 1,
            None => {
                self.history_draft = Some(self.composer.text.clone());
                history.len() - 1
            }
        };
        self.history_position = Some(position);
        self.composer.replace(history[position].clone());
        true
    }

    fn reset_history_browse(&mut self) {
        self.history_position = None;
        self.history_draft = None;
    }

    fn compact_session(&mut self) -> (bool, Vec<ClientRequest>) {
        let Some(session_id) = self.focused() else {
            self.set_warning("create a session before compacting".to_owned());
            return (true, Vec::new());
        };
        if self
            .sessions
            .get(&session_id)
            .is_some_and(|session| session.summary.status != SessionStatus::Idle)
        {
            self.set_warning("compaction needs an idle session; wait or cancel first".to_owned());
            return (true, Vec::new());
        }
        let Ok(command_id) = CommandId::generate() else {
            self.set_warning("secure randomness is unavailable".to_owned());
            return (true, Vec::new());
        };
        self.pending
            .insert(command_id, PendingIntent::Compact { session_id });
        self.set_info("compacting session...".to_owned());
        (
            true,
            vec![ClientRequest::Command(CommandRequest {
                command_id,
                command: SessionCommand::CompactSession { session_id },
            })],
        )
    }

    fn cancel_run(&mut self) -> (bool, Vec<ClientRequest>) {
        let Some(session_id) = self.focused() else {
            self.set_warning("focused session has no active run".to_owned());
            return (true, Vec::new());
        };
        let Some(run_id) = self
            .sessions
            .get(&session_id)
            .and_then(|session| session.summary.active_run_id)
        else {
            self.set_warning("focused session has no active run".to_owned());
            return (true, Vec::new());
        };
        let Ok(command_id) = CommandId::generate() else {
            self.set_warning("secure randomness is unavailable".to_owned());
            return (true, Vec::new());
        };
        self.pending
            .insert(command_id, PendingIntent::Cancel { session_id });
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
        let session = self.sessions.get(&self.focused()?)?;
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
            KeyCode::Char('w' | 'W') => self.respond_to_approval(ApprovalChoice::Workspace),
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
            ApprovalChoice::Workspace => ApprovalDecision::ApproveForWorkspace {
                grant: approval_grant(tool_call),
            },
            ApprovalChoice::Deny => ApprovalDecision::Deny,
        };
        let Ok(command_id) = CommandId::generate() else {
            self.set_warning("secure randomness is unavailable".to_owned());
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
        if self.composer.text.len() + character.len_utf8() > MAX_INPUT_BYTES {
            return false;
        }
        self.composer.insert(character);
        self.reset_history_browse();
        self.slash.select(0);
        true
    }

    /// Insert pasted text. The sanitized content goes through the composer's
    /// paste path, which collapses large pastes to a placeholder; the byte
    /// bound applies to the expanded content so a placeholder cannot hide an
    /// oversized prompt.
    fn push_composer_text(&mut self, text: &str) -> bool {
        let mut sanitized = String::with_capacity(text.len().min(MAX_INPUT_BYTES));
        let budget = MAX_INPUT_BYTES.saturating_sub(self.composer.expanded().len());
        for character in text.chars() {
            if sanitized.len() + character.len_utf8() > budget {
                break;
            }
            if let Some(character) = composer_character(character) {
                sanitized.push(character);
            }
        }
        let changed = self.composer.paste(&sanitized);
        if changed {
            self.reset_history_browse();
            self.slash.select(0);
        }
        changed
    }

    fn handle_slash_key(&mut self, code: KeyCode) -> Option<(bool, Vec<ClientRequest>)> {
        // Only navigation and acceptance keys consult the list; ordinary typing
        // must not pay for building it.
        if !matches!(
            code,
            KeyCode::Up | KeyCode::Down | KeyCode::Enter | KeyCode::Tab
        ) || !self.composer.text.starts_with('/')
        {
            return None;
        }
        let entries = self.filtered_slash_commands();
        if entries.is_empty() {
            return None;
        }
        match code {
            KeyCode::Up => {
                self.slash.move_up();
                Some((true, Vec::new()))
            }
            KeyCode::Down => {
                self.slash.move_down(entries.len());
                Some((true, Vec::new()))
            }
            KeyCode::Enter | KeyCode::Tab => {
                let command = entries[self.slash.selected(entries.len())].command;
                self.composer.clear();
                self.slash.select(0);
                Some(self.execute(command))
            }
            _ => None,
        }
    }

    /// Slash entries matching the composer text, in registry order. Empty
    /// unless the composer holds a bare `/token`.
    pub(crate) fn filtered_slash_commands(&self) -> Vec<SlashEntry> {
        commands::matching_slash_entries(&self.composer.text)
    }

    /// Highlighted row in the slash autocomplete list, clamped to `len`.
    pub(crate) fn slash_selected(&self, len: usize) -> usize {
        self.slash.selected(len)
    }

    pub fn advance_animation(&mut self) -> bool {
        self.animation_tick = self.animation_tick.wrapping_add(1);
        for session in self.sessions.values_mut() {
            for reasoning in session.reasoning.values_mut() {
                if reasoning.streaming {
                    reasoning.ticks += 1;
                }
            }
        }
        let active = self
            .sessions
            .values()
            .any(|session| matches!(session.summary.status, qq_protocol::SessionStatus::Running));
        self.expire_status() || active
    }

    pub fn has_activity(&self) -> bool {
        self.status.is_some()
            || self.sessions.values().any(|session| {
                matches!(session.summary.status, qq_protocol::SessionStatus::Running)
            })
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
                | PendingIntent::Cancel { .. }
                | PendingIntent::Compact { .. }
                | PendingIntent::Approval { .. } => None,
            })
    }

    pub(crate) fn focused_context_usage(&self) -> Option<(u64, u32)> {
        let session = self.focused().and_then(|id| self.sessions.get(&id))?;
        Some((session.summary.context_tokens?, session.context_window?))
    }

    pub(crate) fn focused_context_window(&self) -> Option<u32> {
        let session = self.focused().and_then(|id| self.sessions.get(&id))?;
        session.context_window
    }

    /// Every session in tree order: roots newest-first, each followed by its
    /// descendants depth-first with siblings oldest-first. One pass groups
    /// children by parent so the walk is linear in the number of sessions
    /// plus sorting, not quadratic.
    pub fn thread_order(&self) -> Vec<SessionId> {
        let mut children: HashMap<Option<SessionId>, Vec<SessionId>> = HashMap::new();
        for session in self.sessions.values() {
            children
                .entry(session.summary.parent_id)
                .or_default()
                .push(session.summary.id);
        }
        for siblings in children.values_mut() {
            siblings.sort_by_key(|id| self.sessions[id].summary.updated_at_ms);
        }
        let mut stack = children.remove(&None).unwrap_or_default();
        // Roots are newest-first; popping from the back yields the newest.
        let mut output = Vec::with_capacity(self.sessions.len());
        while let Some(session_id) = stack.pop() {
            output.push(session_id);
            if let Some(mut kids) = children.remove(&Some(session_id)) {
                // Children render oldest-first, so push newest first to be
                // popped last.
                kids.reverse();
                stack.extend(kids);
            }
        }
        output
    }

    /// The child session created by a `spawn_agent` call, if the server has
    /// recorded that linkage. Linear over sessions; the map is small and this
    /// runs only for `spawn_agent` rows on screen.
    pub fn child_spawned_by(&self, tool_call_id: qq_protocol::ToolCallId) -> Option<SessionId> {
        self.sessions
            .values()
            .find(|session| {
                session
                    .summary
                    .spawned_by
                    .is_some_and(|origin| origin.tool_call_id == Some(tool_call_id))
            })
            .map(|session| session.summary.id)
    }

    /// The focused session's sibling `offset` places away in spawn order
    /// (oldest-first), wrapping at either end. Roots are siblings of roots.
    fn sibling(&self, offset: isize) -> Option<SessionId> {
        let focused = self.focused()?;
        let parent = self.sessions.get(&focused)?.summary.parent_id;
        let mut siblings = match parent {
            Some(parent) => self.children_of(parent),
            None => self.root_sessions(),
        };
        siblings.sort_by_key(|id| self.sessions[id].summary.updated_at_ms);
        if siblings.len() < 2 {
            return None;
        }
        let position = siblings.iter().position(|id| *id == focused)?;
        let next = (position as isize + offset).rem_euclid(siblings.len() as isize) as usize;
        Some(siblings[next])
    }

    /// Sessions with a tool call awaiting approval, in tree order.
    pub(crate) fn sessions_awaiting_approval(&self) -> Vec<SessionId> {
        self.thread_order()
            .into_iter()
            .filter(|id| !self.sessions[id].live.awaiting_approval.is_empty())
            .collect()
    }

    /// The next session (after the focused one, wrapping) that is waiting
    /// for an approval answer, excluding the focused session itself.
    fn next_session_awaiting_approval(&self) -> Option<SessionId> {
        let waiting = self.sessions_awaiting_approval();
        let others: Vec<SessionId> = waiting
            .iter()
            .copied()
            .filter(|id| Some(*id) != self.focused())
            .collect();
        if others.is_empty() {
            return None;
        }
        let order = self.thread_order();
        let focus_position = self
            .focused()
            .and_then(|focused| order.iter().position(|id| *id == focused))
            .unwrap_or(0);
        others
            .iter()
            .copied()
            .find(|id| order.iter().position(|o| o == id).unwrap_or(0) > focus_position)
            .or_else(|| others.first().copied())
    }

    /// Direct children of `parent`, oldest-first.
    pub fn children_of(&self, parent: SessionId) -> Vec<SessionId> {
        let mut children = self
            .sessions
            .values()
            .filter(|session| session.summary.parent_id == Some(parent))
            .map(|session| session.summary.id)
            .collect::<Vec<_>>();
        children.sort_by_key(|id| self.sessions[id].summary.updated_at_ms);
        children
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
    Workspace,
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

fn valid_model_route(route: &str) -> bool {
    route
        .split_once('/')
        .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
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

/// Keys that insert a hard newline in the composer without submitting.
///
/// Shift-Enter is the primary chord. Without kitty keyboard enhancement many
/// terminals cannot report Shift on Enter, so Alt-Enter and Ctrl-J (the raw
/// line-feed / historical newline) are accepted as fallbacks.
fn is_composer_newline_key(key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Enter => key
            .modifiers
            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT),
        // In raw mode a bare LF arrives as Ctrl-J rather than Enter.
        KeyCode::Char('j' | 'J') if key.modifiers == KeyModifiers::CONTROL => true,
        KeyCode::Char('\n') => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::MouseEvent;
    use qq_protocol::{
        EventCursor, MessageId, MessageRole, MessageState, RunId, RunOutcome, RunSnapshot,
        RunStatus, SessionEvent, SessionStatus, StoreId, TextChannel, TokenUsage, ToolCallId,
        ToolCallState, WorkspaceGrantOutcome, WorkspaceSummary,
    };

    use super::*;

    fn id<T>(byte: u8, constructor: impl FnOnce([u8; 16]) -> T) -> T {
        constructor([byte; 16])
    }

    fn snapshot() -> WorkspaceSnapshot {
        let workspace_id = id(1, WorkspaceId::from_bytes);
        let session_id = id(2, SessionId::from_bytes);
        WorkspaceSnapshot {
            included: Vec::new(),
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
                activity: None,
                spawned_by: None,
                id: session_id,
                workspace_id,
                parent_id: None,
                title: "Session".to_owned(),
                status: SessionStatus::Idle,
                active_run_id: None,
                queued_prompts: 0,
                model: Some("openai/gpt-test".to_owned()),
                context_tokens: None,
                accounting: None,
                estimated_cost_usd_nanos: Some(0),
                updated_at_ms: 1,
                last_outcome: None,
            }],
            focused: Some(SessionSnapshot {
                summary: SessionSummary {
                    activity: None,
                    spawned_by: None,
                    id: session_id,
                    workspace_id,
                    parent_id: None,
                    title: "Session".to_owned(),
                    status: SessionStatus::Idle,
                    active_run_id: None,
                    queued_prompts: 0,
                    model: Some("openai/gpt-test".to_owned()),
                    context_tokens: None,
                    accounting: None,
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
        app.composer.text = "hello".to_owned();
        let (changed, requests) =
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        assert!(changed);
        assert!(requests.is_empty());
        assert_eq!(app.composer.text, "hello\n");
    }

    #[test]
    fn alt_enter_and_ctrl_j_insert_newlines_without_submitting() {
        let mut app = App::new(TuiOptions::default());
        app.composer.text = "hello".to_owned();

        let (changed, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        assert!(changed);
        assert!(requests.is_empty());
        assert_eq!(app.composer.text, "hello\n");

        let (changed, requests) =
            app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert!(changed);
        assert!(requests.is_empty());
        assert_eq!(app.composer.text, "hello\n\n");
    }

    #[test]
    fn paste_preserves_newlines_in_the_composer() {
        let mut app = App::new(TuiOptions::default());
        let (changed, requests) =
            app.handle_terminal_event(Event::Paste("alpha\r\nbeta".to_owned()));
        assert!(changed);
        assert!(requests.is_empty());
        assert_eq!(app.composer.text, "alpha\nbeta");

        // Three or more lines collapse to a placeholder; the submitted prompt
        // carries the real content with CRLF normalized.
        app.composer.clear();
        app.handle_terminal_event(Event::Paste("alpha\r\nbeta\ngamma".to_owned()));
        assert_eq!(app.composer.text, "[Pasted #1 3 lines]");
        assert_eq!(app.composer.expanded(), "alpha\nbeta\ngamma");
    }

    #[test]
    fn submit_is_optimistic_but_restores_a_rejected_prompt() {
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(snapshot());
        app.composer.text = "hello".to_owned();

        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let ClientRequest::Command(request) = requests.into_iter().next().unwrap() else {
            panic!("expected command")
        };
        assert!(app.composer.text.is_empty());
        app.apply_client_update(ClientUpdate::CommandResult {
            command_id: request.command_id,
            result: Err(ClientFailure::new("offline")),
        });

        assert_eq!(app.composer.text, "hello");
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
        assert!(app.composer.text.is_empty());

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
    fn approve_for_workspace_sends_the_decision_and_surfaces_the_promotion() {
        let mut app = App::new(TuiOptions::default());
        let initial = snapshot();
        let session_id = initial.focused.as_ref().unwrap().summary.id;
        let workspace_id = initial.workspace.id;
        let store_id = initial.cursor.store_id;
        app.apply_snapshot(initial);
        app.upsert_tool_call(ToolCallSnapshot {
            id: id(8, ToolCallId::from_bytes),
            session_id,
            run_id: id(4, RunId::from_bytes),
            turn_ordinal: 1,
            call_ordinal: 1,
            provider_call_id: "call_0".to_owned(),
            name: "shell".to_owned(),
            arguments: r#"{"command":"cargo test --workspace"}"#.to_owned(),
            state: ToolCallState::AwaitingApproval,
            result: None,
            is_error: false,
            display: None,
        });

        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        let ClientRequest::Command(request) = requests.into_iter().next().unwrap() else {
            panic!("expected a command")
        };
        assert!(matches!(
            request.command,
            SessionCommand::RespondToolApproval {
                decision: ApprovalDecision::ApproveForWorkspace {
                    grant: ApprovalGrant::ShellPrefix { prefix },
                },
                ..
            } if prefix == "cargo test --workspace"
        ));

        let envelope = |sequence, outcome| SessionEventEnvelope {
            cursor: EventCursor {
                store_id,
                workspace_id,
                sequence,
            },
            session_id,
            run_id: Some(id(4, RunId::from_bytes)),
            caused_by: Some(request.command_id),
            occurred_at_ms: sequence,
            event: SessionEvent::WorkspaceGrantPromoted {
                grant: ApprovalGrant::ShellPrefix {
                    prefix: "cargo test --workspace".to_owned(),
                },
                outcome,
            },
        };
        app.apply_live_event(envelope(
            2,
            WorkspaceGrantOutcome::Written {
                path: "/repo/.qq/config.ron".to_owned(),
            },
        ));
        assert_eq!(
            app.status.as_deref(),
            Some("grant written to /repo/.qq/config.ron")
        );
        app.apply_live_event(envelope(
            3,
            WorkspaceGrantOutcome::Failed {
                message: "denied by managed policy".to_owned(),
            },
        ));
        assert_eq!(
            app.status.as_deref(),
            Some("workspace grant not saved: denied by managed policy")
        );
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
            app.composer.text = command.to_owned();
            let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            assert!(requests.is_empty());
            assert!(matches!(app.overlay, Some(Overlay::Sessions { .. })));
            app.overlay = None;
        }

        for command in ["/quit", "/exit"] {
            let mut app = App::new(TuiOptions::default());
            app.composer.text = command.to_owned();
            let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            assert!(requests.is_empty());
            assert!(app.quit);
        }
    }

    #[test]
    fn compact_slash_command_sends_compact_session_for_the_focused_idle_session() {
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(snapshot());
        let session_id = app.focused().unwrap();
        app.composer.text = "/compact".to_owned();

        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let ClientRequest::Command(request) = requests.into_iter().next().unwrap() else {
            panic!("expected a command")
        };
        assert_eq!(
            request.command,
            SessionCommand::CompactSession { session_id }
        );
        assert!(app.composer.text.is_empty());
        assert_eq!(app.status.as_deref(), Some("compacting session..."));
        assert_eq!(
            app.visible_status(),
            Some(("compacting session...", NoticeLevel::Info))
        );
    }

    #[test]
    fn notices_only_render_for_the_session_that_owns_them() {
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(snapshot());
        let owner = app.focused().unwrap();
        let other = id(9, SessionId::from_bytes);

        app.set_error_for(Some(owner), "model request failed".to_owned());
        assert_eq!(
            app.visible_status(),
            Some(("model request failed", NoticeLevel::Error))
        );

        app.panes.focused_mut().session = Some(other);
        assert_eq!(app.visible_status(), None);

        app.panes.focused_mut().session = Some(owner);
        assert_eq!(
            app.visible_status(),
            Some(("model request failed", NoticeLevel::Error))
        );
    }

    #[test]
    fn warning_notices_expire_but_error_notices_stick_until_dismissed() {
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(snapshot());
        app.set_notice("temporary notice".to_owned(), NoticeLevel::Warning);
        for _ in 0..NOTICE_TICKS {
            app.advance_animation();
        }
        assert_eq!(app.visible_status(), None, "warning notice remained");
        assert!(!app.has_activity());

        // An error notice never expires on its own: a failure must stay
        // visible until the user acknowledges it (Esc) or replaces it.
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(snapshot());
        app.set_notice("model request failed".to_owned(), NoticeLevel::Error);
        for _ in 0..NOTICE_TICKS * 4 {
            app.advance_animation();
        }
        assert_eq!(
            app.visible_status(),
            Some(("model request failed", NoticeLevel::Error))
        );
        let (changed, requests) = app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(changed);
        assert!(requests.is_empty());
        assert_eq!(app.visible_status(), None);
    }

    #[test]
    fn compact_refuses_while_the_focused_session_is_not_idle() {
        let mut app = App::new(TuiOptions::default());
        let mut initial = snapshot();
        initial.sessions[0].status = SessionStatus::Running;
        initial.focused.as_mut().unwrap().summary.status = SessionStatus::Running;
        app.apply_snapshot(initial);
        app.composer.text = "/compact".to_owned();

        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(requests.is_empty());
        assert_eq!(
            app.status.as_deref(),
            Some("compaction needs an idle session; wait or cancel first")
        );
    }

    #[test]
    fn runtime_slash_invocations_are_submitted_as_prompts() {
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(snapshot());
        app.composer.text = "/frobnicate the context".to_owned();

        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(requests.len(), 1);
        let ClientRequest::Command(CommandRequest {
            command:
                SessionCommand::SubmitPrompt {
                    session_id: _,
                    prompt,
                    limits: _,
                },
            ..
        }) = &requests[0]
        else {
            panic!("runtime slash invocation must use the ordinary prompt command")
        };
        assert_eq!(prompt, "/frobnicate the context");
        assert!(app.composer.text.is_empty());
        assert_eq!(app.status, None);
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
            themes: Vec::new(),
        });
        app.apply_snapshot(snapshot());
        app.composer.text = "/new".to_owned();

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
        app.composer.text = "/".to_owned();

        assert_eq!(
            app.filtered_slash_commands()
                .iter()
                .map(|command| command.name)
                .collect::<Vec<_>>(),
            [
                "/models",
                "/sessions",
                "/resume",
                "/agents",
                "/theme",
                "/new",
                "/compact",
                "/editor",
                "/split",
                "/stack",
                "/close",
                "/zoom",
                "/quit",
                "/exit"
            ]
        );
        for _ in 0..20 {
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        assert_eq!(app.slash_selected(usize::MAX), 13);
        for _ in 0..20 {
            app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        }
        assert_eq!(app.slash_selected(usize::MAX), 0);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(app.composer.text.is_empty());
        assert!(matches!(app.overlay, Some(Overlay::Sessions { .. })));

        app.overlay = None;
        app.composer.text = "/qu".to_owned();
        app.slash.select(0);
        assert_eq!(
            app.filtered_slash_commands()[0].name,
            "/quit",
            "a command prefix should hide unrelated commands"
        );
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.composer.text.is_empty());
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
            activity: None,
            spawned_by: None,
            id: target,
            workspace_id,
            parent_id: None,
            title: "Fix Login Redirect".to_owned(),
            status: SessionStatus::Idle,
            active_run_id: None,
            queued_prompts: 0,
            model: Some("openai/gpt-test".to_owned()),
            context_tokens: None,
            accounting: None,
            estimated_cost_usd_nanos: Some(0),
            updated_at_ms: 2,
            last_outcome: None,
        });
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(initial);
        app.composer.text = "/sessions".to_owned();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let (changed, requests) = app.handle_terminal_event(Event::Paste("LOGIN".to_owned()));

        assert!(changed);
        assert!(requests.is_empty());
        assert_eq!(app.filtered_sessions(), [target]);
        assert_eq!(app.session_picker_selected(), Some(target));

        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            &requests[0],
            ClientRequest::Snapshot(SnapshotRequest {
                focused_session_id: Some(session_id),
                ..
            }) if *session_id == target
        ));
        assert!(app.overlay.is_none());
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
        assert!(matches!(app.overlay, Some(Overlay::Sessions { .. })));
    }

    #[test]
    fn session_picker_deletes_the_highlighted_session_after_a_confirm() {
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(snapshot());
        let session_id = app.focused().unwrap();
        app.open_sessions();

        // The confirm gate: Ctrl-D asks, n keeps, y deletes.
        let (changed, requests) =
            app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(changed);
        assert!(requests.is_empty());
        assert_eq!(
            app.session_picker_confirm(),
            Some(SessionConfirm::Delete(session_id))
        );
        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(requests.is_empty());
        assert_eq!(app.session_picker_confirm(), None);

        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(matches!(
            &requests[0],
            ClientRequest::Command(CommandRequest {
                command: SessionCommand::DeleteSession { session_id: target },
                ..
            }) if *target == session_id
        ));
        assert_eq!(app.session_picker_confirm(), None);
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
        assert_eq!(app.session_picker_confirm(), None);
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
        assert_eq!(app.session_picker_confirm(), Some(SessionConfirm::Prune));

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
            activity: None,
            spawned_by: None,
            id: neighbor,
            workspace_id,
            parent_id: None,
            title: "Neighbor".to_owned(),
            status: SessionStatus::Idle,
            active_run_id: None,
            queued_prompts: 0,
            model: Some("openai/gpt-test".to_owned()),
            context_tokens: None,
            accounting: None,
            estimated_cost_usd_nanos: Some(0),
            updated_at_ms: 0,
            last_outcome: None,
        });
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(initial);
        assert_eq!(app.focused(), Some(deleted));
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
        assert_eq!(app.focused(), Some(neighbor));
        assert_eq!(app.session_picker_selected(), Some(neighbor));
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
        assert_eq!(app.focused(), Some(deleted));

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
        assert_eq!(app.focused(), None);
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

    fn context_meter_app() -> App {
        let selection = ModelSelection {
            model: Some("openai/gpt-test".to_owned()),
            max_output_tokens: Some(4_096),
            organization: None,
        };
        App::new(TuiOptions {
            settings: Settings::default(),
            model: selection.clone(),
            models: vec![ModelOption {
                provider: "openai".to_owned(),
                model: "gpt-test".to_owned(),
                name: Some("GPT Test".to_owned()),
                context_window: Some(128_000),
                selection,
            }],
            themes: Vec::new(),
        })
    }

    #[test]
    fn context_usage_uses_last_turn_tokens_live_updates_and_the_model_limit() {
        let mut app = context_meter_app();
        let mut initial = snapshot();
        let session_id = initial.focused.as_ref().unwrap().summary.id;
        // The snapshot rehydrates the meter from session-owned state, not the
        // run's multi-turn billing sum (12_500 here).
        initial.focused.as_mut().unwrap().runs.push(RunSnapshot {
            id: id(7, RunId::from_bytes),
            session_id,
            status: RunStatus::Completed,
            outcome: Some(RunOutcome::Completed),
            prompt_identity: None,
            resolved_model: None,
            usage: Some(TokenUsage {
                input_tokens: 10_000,
                cache_read_input_tokens: 2_000,
                cache_write_input_tokens: 500,
                output_tokens: 1_000,
            }),
            context_tokens: Some(9_000),
            estimated_cost_usd_nanos: Some(1),
            limits: None,
        });
        initial.focused.as_mut().unwrap().summary.context_tokens = Some(9_000);
        let mut summary = initial.focused.as_ref().unwrap().summary.clone();
        let workspace_id = initial.workspace.id;
        let store_id = initial.cursor.store_id;
        app.apply_snapshot(initial);

        assert_eq!(app.focused_context_usage(), Some((9_000, 128_000)));

        // A committed model turn moves the meter while the run is still going.
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
            event: SessionEvent::SessionContextUpdated {
                run_id: id(8, RunId::from_bytes),
                context_tokens: Some(15_000),
            },
        });
        assert_eq!(app.focused_context_usage(), Some((15_000, 128_000)));

        // RunFinished settles the meter on the final turn's figure even
        // though the run's summed usage is larger (24_000 here).
        summary.context_tokens = Some(18_000);
        // A persisted pre-v5 run audit event must not transiently repopulate
        // the authoritative session meter during replay.
        app.apply_live_event(SessionEventEnvelope {
            cursor: EventCursor {
                store_id,
                workspace_id,
                sequence: 3,
            },
            session_id,
            run_id: Some(id(8, RunId::from_bytes)),
            caused_by: None,
            occurred_at_ms: 3,
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
                context_tokens: Some(18_000),
            },
        });

        assert_eq!(app.focused_context_usage(), Some((18_000, 128_000)));
    }

    #[test]
    fn prompt_start_and_streaming_do_not_recalculate_session_context() {
        let mut app = context_meter_app();
        app.models[0].context_window = Some(272_000);
        let mut initial = snapshot();
        let session_id = initial.focused.as_ref().unwrap().summary.id;
        initial.focused.as_mut().unwrap().summary.context_tokens = Some(54_400);
        let workspace_id = initial.workspace.id;
        let store_id = initial.cursor.store_id;
        let run_id = id(8, RunId::from_bytes);
        let user_message_id = id(9, MessageId::from_bytes);
        let assistant_message_id = id(10, MessageId::from_bytes);
        let mut summary = initial.focused.as_ref().unwrap().summary.clone();
        app.apply_snapshot(initial);
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

        summary.status = SessionStatus::Queued;
        summary.queued_prompts = 1;
        app.apply_live_event(envelope(
            2,
            SessionEvent::PromptQueued {
                session: summary.clone(),
                message: MessageSnapshot {
                    id: user_message_id,
                    session_id,
                    run_id,
                    turn_ordinal: 0,
                    role: MessageRole::User,
                    state: MessageState::Queued,
                    output: "question".to_owned(),
                    refusal: String::new(),
                    created_at_ms: 2,
                },
                run: RunSnapshot {
                    id: run_id,
                    session_id,
                    status: RunStatus::Queued,
                    outcome: None,
                    prompt_identity: None,
                    resolved_model: None,
                    usage: None,
                    context_tokens: None,
                    estimated_cost_usd_nanos: None,
                    limits: None,
                },
                queue_position: 1,
            },
        ));
        assert_eq!(app.focused_context_usage(), Some((54_400, 272_000)));

        summary.status = SessionStatus::Running;
        summary.active_run_id = Some(run_id);
        summary.queued_prompts = 0;
        app.apply_live_event(envelope(
            3,
            SessionEvent::RunStarted {
                session: summary,
                run_id,
            },
        ));
        app.apply_live_event(envelope(
            4,
            SessionEvent::AssistantMessageStarted {
                message: MessageSnapshot {
                    id: assistant_message_id,
                    session_id,
                    run_id,
                    turn_ordinal: 1,
                    role: MessageRole::Assistant,
                    state: MessageState::Streaming,
                    output: "a".to_owned(),
                    refusal: String::new(),
                    created_at_ms: 4,
                },
            },
        ));
        app.apply_live_event(envelope(
            5,
            SessionEvent::TextAppended {
                message_id: assistant_message_id,
                channel: qq_protocol::TextChannel::Output,
                text: "nswer".to_owned(),
            },
        ));
        assert_eq!(app.focused_context_usage(), Some((54_400, 272_000)));

        app.apply_live_event(envelope(
            6,
            SessionEvent::SessionContextUpdated {
                run_id,
                context_tokens: Some(13_600),
            },
        ));
        assert_eq!(app.focused_context_usage(), Some((13_600, 272_000)));
    }

    #[test]
    fn legacy_cumulative_usage_is_not_presented_as_context_occupancy() {
        let mut app = context_meter_app();
        app.models[0].context_window = Some(272_000);
        let mut initial = snapshot();
        let session_id = initial.focused.as_ref().unwrap().summary.id;
        // A run persisted before context_tokens existed reports only its
        // cumulative billing usage. Four model turns around tools can easily
        // total 20% of the window even when the last request occupied 5%.
        initial.focused.as_mut().unwrap().runs.push(RunSnapshot {
            id: id(7, RunId::from_bytes),
            session_id,
            status: RunStatus::Completed,
            outcome: Some(RunOutcome::Completed),
            prompt_identity: None,
            resolved_model: None,
            usage: Some(TokenUsage {
                input_tokens: 40_000,
                cache_read_input_tokens: 12_000,
                cache_write_input_tokens: 2_400,
                output_tokens: 4_000,
            }),
            context_tokens: None,
            estimated_cost_usd_nanos: Some(1),
            limits: None,
        });
        let workspace_id = initial.workspace.id;
        let store_id = initial.cursor.store_id;
        app.apply_snapshot(initial);

        assert_eq!(app.focused_context_usage(), None);

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
            event: SessionEvent::RunContextUpdated {
                run_id: id(8, RunId::from_bytes),
                context_tokens: 13_600,
            },
        });
        assert_eq!(app.focused_context_usage(), None);

        app.apply_live_event(SessionEventEnvelope {
            cursor: EventCursor {
                store_id,
                workspace_id,
                sequence: 3,
            },
            session_id,
            run_id: Some(id(8, RunId::from_bytes)),
            caused_by: None,
            occurred_at_ms: 3,
            event: SessionEvent::SessionContextUpdated {
                run_id: id(8, RunId::from_bytes),
                context_tokens: Some(13_600),
            },
        });
        assert_eq!(app.focused_context_usage(), Some((13_600, 272_000)));
    }

    #[test]
    fn compaction_run_usage_does_not_become_session_context() {
        let mut app = context_meter_app();
        app.models[0].context_window = Some(272_000);
        let mut initial = snapshot();
        let session_id = initial.focused.as_ref().unwrap().summary.id;
        initial.focused.as_mut().unwrap().summary.context_tokens = Some(54_400);
        let workspace_id = initial.workspace.id;
        let store_id = initial.cursor.store_id;
        app.apply_snapshot(initial);
        assert_eq!(app.focused_context_usage(), Some((54_400, 272_000)));

        let mut compacted = app.sessions[&session_id].summary.clone();
        compacted.context_tokens = None;
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
                session: compacted.clone(),
                run_id: id(8, RunId::from_bytes),
                outcome: RunOutcome::Completed,
                usage: Some(TokenUsage {
                    input_tokens: 54_000,
                    cache_read_input_tokens: 6_000,
                    cache_write_input_tokens: 0,
                    output_tokens: 2_000,
                }),
                // This is the compaction request's pre-summary input, not
                // the session occupancy after the summary replaced it.
                context_tokens: Some(60_000),
            },
        });
        assert_eq!(app.focused_context_usage(), None);

        app.apply_live_event(SessionEventEnvelope {
            cursor: EventCursor {
                store_id,
                workspace_id,
                sequence: 3,
            },
            session_id,
            run_id: Some(id(8, RunId::from_bytes)),
            caused_by: None,
            occurred_at_ms: 3,
            event: SessionEvent::SessionCompacted {
                session: compacted,
                summary: Some("short summary".to_owned()),
                before_bytes: 200_000,
                after_bytes: 1_000,
            },
        });
        assert_eq!(app.focused_context_usage(), None);
        assert_eq!(app.focused_context_window(), Some(272_000));
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

        let focused = app.focused().unwrap();
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
            themes: Vec::new(),
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
        let focused = app.focused().unwrap();
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

        assert!(app.focused().is_some());
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
            themes: Vec::new(),
        });
        app.apply_snapshot(snapshot());
        app.composer.text = "/models".to_owned();

        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(requests.is_empty());
        assert!(matches!(app.overlay, Some(Overlay::Models(_))));
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.filtered_models(), vec![0]);

        // Enter with a focused session repoints that session's model and
        // remembers it as the client default for later /new creates.
        let focused = app.focused().unwrap();
        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let ClientRequest::Command(request) = &requests[0] else {
            panic!("expected set-session-model command")
        };
        assert!(matches!(
            &request.command,
            SessionCommand::SetSessionModel { session_id, model }
                if model == &selection && *session_id == focused
        ));
        assert_eq!(app.model, selection);
        assert!(app.overlay.is_none());

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
        assert_eq!(app.model, selection);
        assert!(app.overlay.is_none());
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
            themes: Vec::new(),
        });
        let mut empty = snapshot();
        empty.sessions.clear();
        empty.focused = None;
        app.apply_snapshot(empty);
        assert!(app.focused().is_none());
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
        assert_eq!(app.model, selection);
        assert!(app.overlay.is_none());
    }

    #[test]
    fn model_picker_selection_becomes_the_default_for_new_sessions() {
        let initial = ModelSelection {
            model: Some("openai/gpt-test".to_owned()),
            max_output_tokens: Some(4_096),
            organization: None,
        };
        let switched = ModelSelection {
            model: Some("anthropic/claude-sonnet-5".to_owned()),
            max_output_tokens: Some(8_192),
            organization: None,
        };
        let mut app = App::new(TuiOptions {
            settings: Settings::default(),
            model: initial,
            models: vec![ModelOption {
                provider: "anthropic".to_owned(),
                model: "claude-sonnet-5".to_owned(),
                name: Some("Claude Sonnet 5".to_owned()),
                context_window: Some(200_000),
                selection: switched.clone(),
            }],
            themes: Vec::new(),
        });
        app.apply_snapshot(snapshot());
        app.open_models();

        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            &requests[0],
            ClientRequest::Command(CommandRequest {
                command: SessionCommand::SetSessionModel { model, .. },
                ..
            }) if model == &switched
        ));
        assert_eq!(app.model, switched);

        let (_, requests) = app.execute(Command::NewRootSession);
        assert!(matches!(
            &requests[0],
            ClientRequest::Command(CommandRequest {
                command: SessionCommand::CreateSession { model, .. },
                ..
            }) if model == &switched
        ));
    }

    #[test]
    fn new_inherits_the_focused_session_model_when_no_default_is_loaded() {
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(snapshot());
        app.model = ModelSelection::default();
        app.composer.text = "/new".to_owned();

        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(
            &requests[0],
            ClientRequest::Command(CommandRequest {
                command: SessionCommand::CreateSession { model, .. },
                ..
            }) if model.model.as_deref() == Some("openai/gpt-test")
        ));
    }

    #[test]
    fn create_without_a_default_or_focused_session_still_requires_a_model() {
        let mut initial = snapshot();
        initial.sessions.clear();
        initial.focused = None;
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(initial);

        let (_, requests) = app.execute(Command::NewRootSession);

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
        app.composer.text = "keep me".to_owned();
        let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let ClientRequest::Command(request) = requests.into_iter().next().unwrap() else {
            panic!("expected command")
        };

        app.apply_client_update(ClientUpdate::ResetSnapshot(snapshot));
        app.apply_client_update(ClientUpdate::CommandResult {
            command_id: request.command_id,
            result: Err(ClientFailure::new("server restarted")),
        });

        assert_eq!(app.composer.text, "keep me");
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
            activity: None,
            spawned_by: None,
            id: new_focus,
            workspace_id: initial.workspace.id,
            parent_id: None,
            title: "New focus".to_owned(),
            status: SessionStatus::Idle,
            active_run_id: None,
            queued_prompts: 0,
            model: Some("openai/gpt-test".to_owned()),
            context_tokens: None,
            accounting: None,
            estimated_cost_usd_nanos: Some(0),
            updated_at_ms: 2,
            last_outcome: None,
        });
        app.apply_snapshot(initial.clone());
        app.focus_session(new_focus);

        assert!(!app.apply_snapshot(initial));
        assert_eq!(app.focused(), Some(new_focus));
        assert_ne!(app.focused(), Some(old_focus));
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
        app.overlay = Some(Overlay::sessions("", None, None));
        app.handle_key(ctrl_o);
        assert_eq!(app.tool_detail, ToolDetail::Collapsed);
    }

    #[test]
    fn page_keys_scroll_the_transcript_by_one_visible_page() {
        let mut app = App::new(TuiOptions::default());
        app.update_transcript_viewport(100, 12, false);

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
        app.update_transcript_viewport(100, 12, false);

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
        app.update_transcript_viewport(40, 10, false);
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )));

        app.update_transcript_viewport(45, 10, false);

        assert_eq!(app.transcript_scroll_offset(), 15);
    }

    #[test]
    fn session_and_layout_changes_return_the_transcript_to_the_live_tail() {
        let mut app = App::new(TuiOptions::default());
        app.panes.focused_mut().session = Some(SessionId::from_bytes([1; 16]));
        app.update_transcript_viewport(100, 10, false);
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )));

        app.panes.focused_mut().session = Some(SessionId::from_bytes([2; 16]));
        app.update_transcript_viewport(100, 10, false);

        assert_eq!(app.transcript_scroll_offset(), 0);

        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )));
        app.layout = app.layout.next();
        app.update_transcript_viewport(100, 10, false);

        assert_eq!(app.transcript_scroll_offset(), 0);
    }

    #[test]
    fn scrolling_clamps_at_the_oldest_row_and_the_live_tail() {
        let mut app = App::new(TuiOptions::default());
        app.update_transcript_viewport(25, 10, false);
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
        app.update_transcript_viewport(100, 10, false);
        app.overlay = Some(Overlay::models());
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

        app.overlay = None;
        app.overlay = Some(Overlay::sessions("", app.focused(), None));
        assert!(!app.handle_terminal_event(wheel).0);
        assert!(!app.handle_terminal_event(page).0);
        assert_eq!(app.transcript_scroll_offset(), 0);
    }

    fn summary_named(byte: u8, workspace_id: WorkspaceId, title: &str) -> SessionSummary {
        SessionSummary {
            id: id(byte, SessionId::from_bytes),
            workspace_id,
            parent_id: None,
            spawned_by: None,
            title: title.to_owned(),
            status: SessionStatus::Idle,
            active_run_id: None,
            activity: None,
            queued_prompts: 0,
            model: Some("openai/gpt-test".to_owned()),
            context_tokens: None,
            accounting: None,
            estimated_cost_usd_nanos: Some(0),
            updated_at_ms: u64::from(byte),
            last_outcome: None,
        }
    }

    fn body_for(summary: &SessionSummary, output: &str) -> SessionSnapshot {
        SessionSnapshot {
            summary: summary.clone(),
            messages: vec![MessageSnapshot {
                id: id(summary.id.as_bytes()[0], MessageId::from_bytes),
                session_id: summary.id,
                run_id: id(0xaa, RunId::from_bytes),
                turn_ordinal: 1,
                role: MessageRole::Assistant,
                state: MessageState::Complete,
                output: output.to_owned(),
                refusal: String::new(),
                created_at_ms: 1,
            }],
            runs: Vec::new(),
            tool_calls: Vec::new(),
            has_older_tool_calls: false,
            has_older_messages: false,
        }
    }

    #[test]
    fn creating_a_session_adopts_it_without_a_snapshot_round_trip() {
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(snapshot());
        let workspace_id = app.workspace_id.unwrap();
        let (_, requests) = app.execute(Command::NewRootSession);
        let [ClientRequest::Command(request)] = requests.as_slice() else {
            panic!("expected one create command, got {requests:?}");
        };
        let created = id(0x42, SessionId::from_bytes);
        let mut summary = summary_named(0x42, workspace_id, "New session");
        summary.updated_at_ms = 99;

        // The durable event arrives first (the SSE stream is usually ahead of
        // the HTTP receipt); focus moves and the body is already warm.
        let changed = app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
            cursor: EventCursor {
                store_id: id(3, StoreId::from_bytes),
                workspace_id,
                sequence: 2,
            },
            session_id: created,
            run_id: None,
            caused_by: Some(request.command_id),
            occurred_at_ms: 1,
            event: SessionEvent::SessionCreated {
                session: summary.clone(),
            },
        }));
        assert!(changed);
        assert_eq!(app.focused(), Some(created));
        assert!(app.sessions[&created].is_warm());
        assert!(app.take_requests().is_empty(), "no snapshot after create");

        // The receipt confirms without changing anything or requesting more.
        app.apply_client_update(ClientUpdate::CommandResult {
            command_id: request.command_id,
            result: Ok(qq_protocol::CommandReceipt {
                command_id: request.command_id,
                outcome: CommandOutcome::SessionCreated {
                    session_id: created,
                },
                committed_through: EventCursor {
                    store_id: id(3, StoreId::from_bytes),
                    workspace_id,
                    sequence: 2,
                },
            }),
        });
        assert_eq!(app.focused(), Some(created));
        assert!(app.take_requests().is_empty());
        // The previously focused session keeps its body warm.
        let previous = snapshot().sessions[0].id;
        assert!(app.sessions[&previous].is_warm());
    }

    #[test]
    fn switching_to_a_warm_session_needs_no_request_and_a_cold_one_does() {
        let mut app = App::new(TuiOptions::default());
        let mut initial = snapshot();
        let workspace_id = initial.workspace.id;
        let warm = summary_named(0x51, workspace_id, "warm");
        let cold = summary_named(0x52, workspace_id, "cold");
        initial.sessions.push(warm.clone());
        initial.sessions.push(cold.clone());
        initial.included.push(body_for(&warm, "warm body"));
        app.apply_snapshot(initial);
        let first = snapshot().sessions[0].id;
        assert_eq!(app.focused(), Some(first));
        assert!(app.sessions[&warm.id].is_warm());
        assert!(!app.sessions[&cold.id].is_warm());

        let (changed, requests) = app.focus_session(warm.id);
        assert!(changed);
        assert!(requests.is_empty(), "warm switch must not request");
        assert_eq!(app.focused(), Some(warm.id));
        assert!(app.sessions[&first].is_warm(), "leaving does not evict");

        let (_, requests) = app.focus_session(cold.id);
        assert!(matches!(
            requests.as_slice(),
            [ClientRequest::Snapshot(SnapshotRequest {
                focused_session_id: Some(id),
                ..
            })] if *id == cold.id
        ));
        assert_eq!(app.focused(), Some(cold.id));
    }

    #[test]
    fn warm_bodies_are_bounded_and_evict_least_recently_focused() {
        let mut app = App::new(TuiOptions::default());
        let mut initial = snapshot();
        let workspace_id = initial.workspace.id;
        let ids: Vec<SessionId> = (0x60..0x60 + (WARM_BODY_LIMIT as u8) + 2)
            .map(|byte| {
                let summary = summary_named(byte, workspace_id, "s");
                initial.sessions.push(summary.clone());
                summary.id
            })
            .collect();
        app.apply_snapshot(initial);
        // Focus each in turn, loading a body every time.
        for session_id in &ids {
            app.focus_session(*session_id);
            let summary = app.sessions[session_id].summary.clone();
            app.apply_snapshot(WorkspaceSnapshot {
                focused: Some(body_for(&summary, "body")),
                ..snapshot()
            });
        }
        let warm: Vec<_> = app.sessions.values().filter(|s| s.is_warm()).collect();
        assert_eq!(warm.len(), WARM_BODY_LIMIT);
        // The most recent WARM_BODY_LIMIT are warm; the earliest two are not.
        assert!(!app.sessions[&ids[0]].is_warm());
        assert!(!app.sessions[&ids[1]].is_warm());
        assert!(app.sessions[ids.last().unwrap()].is_warm());
        // Cold sessions keep their summary and status.
        assert_eq!(app.sessions[&ids[0]].summary.title, "s");
    }

    #[test]
    fn live_status_tracks_cold_sessions_and_activity_seeds_from_snapshots() {
        let mut app = App::new(TuiOptions::default());
        let mut initial = snapshot();
        let workspace_id = initial.workspace.id;
        let mut child = summary_named(0x71, workspace_id, "child");
        child.parent_id = Some(initial.sessions[0].id);
        child.status = SessionStatus::Running;
        child.active_run_id = Some(id(0x72, RunId::from_bytes));
        child.activity = Some(RunActivity::Reasoning);
        initial.sessions.push(child.clone());
        app.apply_snapshot(initial);
        assert!(!app.sessions[&child.id].is_warm());
        assert_eq!(
            app.sessions[&child.id].activity,
            Some((id(0x72, RunId::from_bytes), RunActivity::Reasoning))
        );

        let mut sequence = 1;
        let mut event = |event: SessionEvent| {
            sequence += 1;
            ClientUpdate::Event(SessionEventEnvelope {
                cursor: EventCursor {
                    store_id: id(3, StoreId::from_bytes),
                    workspace_id,
                    sequence,
                },
                session_id: child.id,
                run_id: child.active_run_id,
                caused_by: None,
                occurred_at_ms: sequence,
                event,
            })
        };
        let message = MessageSnapshot {
            id: id(0x73, MessageId::from_bytes),
            session_id: child.id,
            run_id: child.active_run_id.unwrap(),
            turn_ordinal: 1,
            role: MessageRole::Assistant,
            state: MessageState::Streaming,
            output: String::new(),
            refusal: String::new(),
            created_at_ms: 1,
        };
        app.apply_client_update(event(SessionEvent::AssistantMessageStarted { message }));
        app.apply_client_update(event(SessionEvent::TextAppended {
            message_id: id(0x73, MessageId::from_bytes),
            channel: TextChannel::Output,
            text: "Reading   the\nrepository ".to_owned(),
        }));
        app.apply_client_update(event(SessionEvent::TextAppended {
            message_id: id(0x73, MessageId::from_bytes),
            channel: TextChannel::Output,
            text: "layout".to_owned(),
        }));
        let call = ToolCallSnapshot {
            id: id(0x74, ToolCallId::from_bytes),
            session_id: child.id,
            run_id: child.active_run_id.unwrap(),
            turn_ordinal: 1,
            call_ordinal: 0,
            provider_call_id: "c".to_owned(),
            name: "search".to_owned(),
            arguments: "{}".to_owned(),
            state: ToolCallState::AwaitingApproval,
            result: None,
            is_error: false,
            display: None,
        };
        app.apply_client_update(event(SessionEvent::ToolApprovalRequested {
            tool_call: call.clone(),
            shell: None,
            edit: None,
        }));

        let live = &app.sessions[&child.id].live;
        assert_eq!(live.tail, "Reading the repository layout");
        assert_eq!(live.active_tool.as_deref(), Some("search"));
        assert_eq!(live.awaiting_approval.len(), 1);
        // Still cold: deltas did not create a body.
        assert!(!app.sessions[&child.id].is_warm());

        let finished = ToolCallSnapshot {
            state: ToolCallState::Completed,
            ..call
        };
        app.apply_client_update(event(SessionEvent::ToolCallFinished {
            tool_call: finished,
        }));
        let live = &app.sessions[&child.id].live;
        assert_eq!(live.active_tool, None);
        assert!(live.awaiting_approval.is_empty());

        // A long stream keeps the tail bounded.
        app.apply_client_update(event(SessionEvent::TextAppended {
            message_id: id(0x73, MessageId::from_bytes),
            channel: TextChannel::Output,
            text: "x".repeat(LIVE_TAIL_BYTES * 3),
        }));
        assert!(app.sessions[&child.id].live.tail.len() <= LIVE_TAIL_BYTES);
    }

    #[test]
    fn agents_picker_lists_only_the_focused_roots_subtree() {
        let mut app = App::new(TuiOptions::default());
        let mut initial = snapshot();
        let workspace_id = initial.workspace.id;
        let root = initial.sessions[0].id;
        let mut child = summary_named(0x81, workspace_id, "child");
        child.parent_id = Some(root);
        let mut grandchild = summary_named(0x82, workspace_id, "grandchild");
        grandchild.parent_id = Some(child.id);
        let other_root = summary_named(0x83, workspace_id, "other root");
        initial
            .sessions
            .extend([child.clone(), grandchild.clone(), other_root.clone()]);
        app.apply_snapshot(initial);

        // From deep in the tree, /agents scopes to the whole root's subtree.
        app.focus_session(grandchild.id);
        app.execute(Command::OpenAgents);
        let mut listed = app.filtered_sessions();
        listed.sort();
        let mut expected = vec![root, child.id, grandchild.id];
        expected.sort();
        assert_eq!(listed, expected);
        assert!(!app.filtered_sessions().contains(&other_root.id));

        // /sessions lists everything.
        app.execute(Command::OpenSessions);
        assert_eq!(app.filtered_sessions().len(), 4);
    }

    /// A snapshot whose one session is mid-run, plus the envelope builder for
    /// follow-up events on it.
    fn running_app() -> (
        App,
        SessionId,
        RunId,
        impl FnMut(SessionEvent) -> ClientUpdate,
    ) {
        let mut app = App::new(TuiOptions::default());
        let mut initial = snapshot();
        let workspace_id = initial.workspace.id;
        let session_id = initial.sessions[0].id;
        let run_id = id(0x90, RunId::from_bytes);
        for summary in initial
            .sessions
            .iter_mut()
            .chain(initial.focused.iter_mut().map(|body| &mut body.summary))
        {
            summary.status = SessionStatus::Running;
            summary.active_run_id = Some(run_id);
        }
        app.apply_snapshot(initial);
        let mut sequence = 1;
        let event = move |event: SessionEvent| {
            sequence += 1;
            ClientUpdate::Event(SessionEventEnvelope {
                cursor: EventCursor {
                    store_id: id(3, StoreId::from_bytes),
                    workspace_id,
                    sequence,
                },
                session_id,
                run_id: Some(run_id),
                caused_by: None,
                occurred_at_ms: sequence,
                event,
            })
        };
        (app, session_id, run_id, event)
    }

    #[test]
    fn enter_during_a_run_queues_the_draft_and_it_submits_when_the_run_ends() {
        let (mut app, session_id, run_id, mut event) = running_app();
        app.composer.text = "follow up".to_owned();
        let (changed, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(changed);
        assert!(
            requests.is_empty(),
            "nothing is sent while the run is active"
        );
        assert!(app.composer.text.is_empty());
        assert_eq!(
            app.queued_drafts(session_id).collect::<Vec<_>>(),
            ["follow up"]
        );

        // Ctrl-Enter queues explicitly as well; drafts keep order.
        app.composer.text = "second".to_owned();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
        assert_eq!(
            app.queued_drafts(session_id).collect::<Vec<_>>(),
            ["follow up", "second"]
        );

        // Alt-Up brings the newest back for editing.
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
        assert_eq!(app.composer.text, "second");
        assert_eq!(
            app.queued_drafts(session_id).collect::<Vec<_>>(),
            ["follow up"]
        );
        app.composer.clear();

        // The run finishes idle: the oldest draft becomes the next run.
        let mut summary = app.sessions[&session_id].summary.clone();
        summary.status = SessionStatus::Idle;
        summary.active_run_id = None;
        app.apply_client_update(event(SessionEvent::RunFinished {
            session: summary,
            run_id,
            outcome: RunOutcome::Completed,
            usage: None,
            context_tokens: None,
        }));
        let requests = app.take_requests();
        assert!(matches!(
            requests.as_slice(),
            [ClientRequest::Command(CommandRequest {
                command: SessionCommand::SubmitPrompt { prompt, .. },
                ..
            })] if prompt == "follow up"
        ));
        assert!(app.queued_drafts(session_id).next().is_none());
    }

    #[test]
    fn esc_twice_cancels_the_active_run_but_once_only_arms() {
        let (mut app, session_id, run_id, _) = running_app();
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let (changed, requests) = app.handle_key(esc);
        assert!(changed);
        assert!(requests.is_empty());
        assert!(app.status.as_deref().unwrap().contains("Esc again"));

        // Typing disarms.
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        let (_, requests) = app.handle_key(esc);
        assert!(requests.is_empty(), "disarmed by intervening input");

        let (_, requests) = app.handle_key(esc);
        assert!(matches!(
            requests.as_slice(),
            [ClientRequest::Command(CommandRequest {
                command: SessionCommand::CancelRun { run_id: cancelled },
                ..
            })] if *cancelled == run_id
        ));
        assert!(app.sessions[&session_id].summary.active_run_id.is_some());

        // Too slow: the arm expires.
        let (_, _) = app.handle_key(esc);
        for _ in 0..=ESC_CANCEL_TICKS {
            app.advance_animation();
        }
        let (_, requests) = app.handle_key(esc);
        assert!(requests.is_empty());
    }

    #[test]
    fn steer_falls_back_to_queueing_until_the_server_advertises_it() {
        let (mut app, session_id, _, _) = running_app();
        app.composer.text = "go left".to_owned();
        let (_, requests) = app.execute(Command::SteerRun);
        assert!(requests.is_empty());
        assert_eq!(
            app.queued_drafts(session_id).collect::<Vec<_>>(),
            ["go left"]
        );
        assert!(
            app.status
                .as_deref()
                .unwrap()
                .contains("does not support steering")
        );
    }

    fn alt(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::ALT)
    }

    /// Two warm root sessions; the first is focused. Returns their ids.
    fn two_session_app() -> (App, SessionId, SessionId) {
        let mut app = App::new(TuiOptions::default());
        let mut initial = snapshot();
        let workspace_id = initial.workspace.id;
        let other = summary_named(0x42, workspace_id, "other");
        initial.sessions.push(other.clone());
        initial.included.push(body_for(&other, "other body"));
        let first = initial.sessions[0].id;
        app.apply_snapshot(initial);
        assert!(app.sessions[&other.id].is_warm());
        (app, first, other.id)
    }

    #[test]
    fn splitting_inherits_the_session_and_pane_commands_route_through_keys() {
        let (mut app, first, other) = two_session_app();
        assert_eq!(app.panes.len(), 1);

        // Alt-\ splits beside; the new pane shows the same session, focused.
        let (changed, requests) = app.handle_key(alt('\\'));
        assert!(changed);
        assert!(requests.is_empty(), "a split never fetches");
        assert_eq!(app.panes.len(), 2);
        assert_eq!(app.focused(), Some(first));

        // Switching the session in the new pane leaves the other pane alone.
        app.focus_session(other);
        assert_eq!(app.focused(), Some(other));
        let shown: Vec<_> = app.panes.sessions().collect();
        assert!(shown.contains(&first) && shown.contains(&other));

        // Alt-- stacks below the focused pane; Alt-K goes back up after a
        // layout has produced geometry.
        app.handle_key(alt('-'));
        assert_eq!(app.panes.len(), 3);
        app.panes.layout(crate::panes::Rect::new(0, 2, 160, 40));
        let before = app.panes.focused_id();
        let (changed, _) = app.handle_key(alt('k'));
        assert!(changed);
        assert_ne!(app.panes.focused_id(), before);
        assert_eq!(app.focused(), Some(other));

        // Alt-W closes; Alt-Z zooms.
        app.handle_key(alt('w'));
        assert_eq!(app.panes.len(), 2);
        let (changed, _) = app.handle_key(alt('z'));
        assert!(changed && app.panes.is_zoomed());
        app.handle_key(alt('z'));
        assert!(!app.panes.is_zoomed());
        app.handle_key(alt('w'));
        assert_eq!(app.panes.len(), 1);
        let (_, _) = app.handle_key(alt('w'));
        assert_eq!(app.panes.len(), 1, "the last pane stays");
        assert!(app.status.as_deref().unwrap().contains("last pane"));
    }

    #[test]
    fn slash_pane_commands_match_their_keys() {
        let (mut app, _, _) = two_session_app();
        for (slash, expected) in [("/split", 2), ("/stack", 3), ("/close", 2), ("/zoom", 2)] {
            app.composer.text = slash.to_owned();
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            assert_eq!(app.panes.len(), expected, "{slash}");
        }
        assert!(app.panes.is_zoomed());
    }

    #[test]
    fn sessions_shown_in_any_pane_are_pinned_warm() {
        let mut app = App::new(TuiOptions::default());
        let mut initial = snapshot();
        let workspace_id = initial.workspace.id;
        let ids: Vec<SessionId> = (0x60..0x60 + (WARM_BODY_LIMIT as u8) + 2)
            .map(|byte| {
                let summary = summary_named(byte, workspace_id, "s");
                initial.sessions.push(summary.clone());
                summary.id
            })
            .collect();
        app.apply_snapshot(initial);
        // Pin the first id in a second pane, then cycle every other session
        // through the focused pane.
        app.focus_session(ids[0]);
        let summary = app.sessions[&ids[0]].summary.clone();
        app.apply_snapshot(WorkspaceSnapshot {
            focused: Some(body_for(&summary, "pinned")),
            ..snapshot()
        });
        app.execute(Command::SplitBeside);
        for session_id in &ids[1..] {
            app.focus_session(*session_id);
            let summary = app.sessions[session_id].summary.clone();
            app.apply_snapshot(WorkspaceSnapshot {
                focused: Some(body_for(&summary, "body")),
                ..snapshot()
            });
        }
        assert!(app.sessions[&ids[0]].is_warm(), "shown in a pane: pinned");
        assert!(!app.sessions[&ids[1]].is_warm(), "oldest unpinned evicts");
        let warm = app.sessions.values().filter(|s| s.is_warm()).count();
        assert_eq!(warm, WARM_BODY_LIMIT);
    }

    #[test]
    fn a_body_fetched_for_a_pane_that_lost_focus_still_installs_without_moving_focus() {
        let (mut app, first, _) = two_session_app();
        let workspace_id = app.workspace_id.unwrap();
        let cold = summary_named(0x77, workspace_id, "cold");
        app.apply_snapshot(WorkspaceSnapshot {
            sessions: vec![cold.clone()],
            focused: None,
            ..snapshot()
        });
        app.execute(Command::SplitBeside);
        let (_, requests) = app.focus_session(cold.id);
        assert_eq!(requests.len(), 1, "cold body is requested");
        // The user moves back to the first pane before the body arrives.
        app.execute(Command::FocusPaneLeft);
        app.panes.layout(crate::panes::Rect::new(0, 2, 160, 40));
        let (moved, _) = app.execute(Command::FocusPaneLeft);
        assert!(moved);
        assert_eq!(app.focused(), Some(first));

        let installed = app.apply_snapshot(WorkspaceSnapshot {
            focused: Some(body_for(&cold, "arrived")),
            ..snapshot()
        });
        assert!(installed);
        assert!(app.sessions[&cold.id].is_warm());
        assert_eq!(
            app.focused(),
            Some(first),
            "focus stays where the user put it"
        );

        // A body for a session no pane shows any more is dropped.
        let gone = summary_named(0x78, workspace_id, "gone");
        app.apply_snapshot(WorkspaceSnapshot {
            sessions: vec![gone.clone()],
            focused: None,
            ..snapshot()
        });
        assert!(!app.apply_snapshot(WorkspaceSnapshot {
            focused: Some(body_for(&gone, "late")),
            ..snapshot()
        }));
    }

    #[test]
    fn deleting_a_session_repoints_every_pane_showing_it() {
        let (mut app, first, other) = two_session_app();
        let workspace_id = app.workspace_id.unwrap();
        app.execute(Command::SplitBeside);
        app.execute(Command::SplitBelow);
        // Panes: [first] [first / first]; point the focused one at `other`.
        app.focus_session(other);
        app.apply_client_update(ClientUpdate::Event(SessionEventEnvelope {
            cursor: EventCursor {
                store_id: id(3, StoreId::from_bytes),
                workspace_id,
                sequence: 2,
            },
            session_id: first,
            run_id: None,
            caused_by: None,
            occurred_at_ms: 2,
            event: SessionEvent::SessionDeleted { session_id: first },
        }));
        assert!(!app.sessions.contains_key(&first));
        let shown: Vec<_> = app.panes.sessions().collect();
        assert_eq!(shown.len(), 3);
        assert!(shown.iter().all(|id| *id == other));
        // The replacement is warm, so nothing is fetched.
        assert!(app.take_requests().is_empty());
    }

    #[test]
    fn the_mouse_scrolls_the_pane_under_the_cursor_and_clicks_focus_it() {
        use crossterm::event::{MouseButton, MouseEvent};
        let (mut app, _, other) = two_session_app();
        app.execute(Command::SplitBeside);
        app.focus_session(other);
        let (tiles, _) = app.panes.layout(crate::panes::Rect::new(0, 2, 161, 40));
        let left = tiles[0];
        let right = tiles[1];
        assert_eq!(app.panes.focused_id(), right.pane);
        // Give both panes a scrollable body.
        for tile in [left, right] {
            app.update_viewport(tile.pane, 200, 40, false);
        }
        let mouse = |kind, column: usize, row: usize| {
            Event::Mouse(MouseEvent {
                kind,
                column: u16::try_from(column).unwrap(),
                row: u16::try_from(row).unwrap(),
                modifiers: KeyModifiers::NONE,
            })
        };
        app.handle_terminal_event(mouse(MouseEventKind::ScrollUp, left.rect.x + 2, 5));
        assert_eq!(app.viewport(left.pane).unwrap().offset(), MOUSE_SCROLL_ROWS);
        assert_eq!(app.viewport(right.pane).unwrap().offset(), 0);
        assert_eq!(
            app.panes.focused_id(),
            right.pane,
            "scrolling does not focus"
        );
        let (changed, _) = app.handle_terminal_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            left.rect.x + 2,
            5,
        ));
        assert!(changed);
        assert_eq!(app.panes.focused_id(), left.pane);
    }

    #[test]
    fn resize_keys_move_the_divider_of_the_enclosing_split() {
        let (mut app, _, _) = two_session_app();
        app.execute(Command::SplitBeside);
        let area = crate::panes::Rect::new(0, 2, 201, 40);
        let (before, _) = app.panes.layout(area);
        let (changed, _) = app.handle_key(KeyEvent::new(
            KeyCode::Char('H'),
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        ));
        assert!(changed);
        let (after, _) = app.panes.layout(area);
        assert!(after[0].rect.width < before[0].rect.width);
        // No row split encloses the focused pane, so Alt-Shift-K does nothing.
        let (changed, _) = app.handle_key(KeyEvent::new(
            KeyCode::Char('K'),
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        ));
        assert!(!changed);
    }

    fn themed_app() -> App {
        let mut app = App::new(TuiOptions {
            settings: Settings::default(),
            model: ModelSelection::default(),
            models: Vec::new(),
            themes: vec![
                crate::Theme::qq(),
                crate::Theme::from_roles(
                    "rose-pine",
                    [crate::ThemeColor::Rgb(0xe0, 0xde, 0xf4); 8],
                ),
                crate::Theme::from_roles("mono", [crate::ThemeColor::White; 8]),
            ],
        });
        app.apply_snapshot(snapshot());
        app
    }

    #[test]
    fn the_theme_picker_previews_live_and_esc_restores() {
        let mut app = themed_app();
        assert_eq!(app.theme().name, "qq");
        app.composer.text = "/theme".to_owned();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.mode(), Mode::Themes);
        let generation = app.theme_generation;

        // Down previews the next theme immediately.
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.theme().name, "rose-pine");
        assert_eq!(app.theme_generation, generation + 1);
        // Typing filters and the highlighted theme follows the filter.
        app.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        assert_eq!(app.filtered_themes().len(), 1);
        assert_eq!(app.theme().name, "mono");
        // Esc puts the original back.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode(), Mode::Compose);
        assert_eq!(app.theme().name, "qq");

        // Enter keeps the preview and tells the user how to persist it.
        app.execute(Command::OpenThemes);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.theme().name, "rose-pine");
        let (status, _) = app.visible_status().expect("info notice");
        assert!(status.contains("theme: \"rose-pine\""), "{status}");
    }

    #[test]
    fn a_single_theme_makes_the_picker_a_notice_instead() {
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(snapshot());
        assert_eq!(app.themes.len(), 1, "the compiled theme is always present");
        app.execute(Command::OpenThemes);
        assert_eq!(app.mode(), Mode::Compose);
        assert!(
            app.visible_status()
                .unwrap()
                .0
                .contains("themes/<name>.ron")
        );
    }

    #[test]
    fn attention_is_requested_only_while_the_terminal_is_unfocused() {
        let (mut app, session_id, run_id, mut event) = running_app();
        let finish = |run_id| SessionEvent::RunFinished {
            session: SessionSummary {
                status: SessionStatus::Idle,
                active_run_id: None,
                ..summary_named(2, id(1, WorkspaceId::from_bytes), "Deploy")
            },
            run_id,
            outcome: RunOutcome::Completed,
            usage: None,
            context_tokens: None,
        };
        // Focused: nothing to report.
        app.apply_client_update(event(finish(run_id)));
        assert_eq!(app.take_attention(), None);

        // Unfocused: a finished run asks for attention with the title.
        app.handle_terminal_event(Event::FocusLost);
        app.apply_client_update(event(finish(run_id)));
        assert_eq!(
            app.take_attention(),
            Some(Attention::RunFinished {
                session_title: "Deploy".to_owned()
            })
        );
        assert_eq!(app.take_attention(), None, "taken once");

        // An approval request while unfocused also asks; regaining focus
        // clears anything not yet delivered.
        app.apply_client_update(event(SessionEvent::ToolApprovalRequested {
            tool_call: ToolCallSnapshot {
                id: id(0x51, qq_protocol::ToolCallId::from_bytes),
                session_id,
                run_id,
                turn_ordinal: 1,
                call_ordinal: 0,
                provider_call_id: "call".to_owned(),
                name: "shell".to_owned(),
                arguments: "{}".to_owned(),
                state: ToolCallState::AwaitingApproval,
                result: None,
                is_error: false,
                display: None,
            },
            shell: None,
            edit: None,
        }));
        assert!(matches!(
            app.attention,
            Some(Attention::ApprovalRequested { .. })
        ));
        app.handle_terminal_event(Event::FocusGained);
        assert_eq!(app.take_attention(), None);
        assert_eq!(
            Attention::ApprovalRequested {
                session_title: "Deploy".to_owned()
            }
            .summary(),
            "qq: Deploy needs approval"
        );
    }
}
