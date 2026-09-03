use std::collections::{HashMap, VecDeque};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use qq_protocol::{
    ApprovalDecision, ApprovalGrant, ApprovalMode, ApprovalResolution, CommandId, CommandOutcome,
    CommandRequest, EditPreview, ModelDescriptor, ModelSelection, SessionCommand,
    SessionEventEnvelope, SessionId, SessionSnapshot, SessionStatus, SnapshotRequest,
    SteeringCapabilities, ToolCallSnapshot, ToolCallState, WorkspaceId, WorkspaceSnapshot,
};
use thiserror::Error;

pub(crate) use crate::model::{LiveStatus, ReasoningDetail, SessionView};
use crate::model::{MAX_PROMPT_HISTORY, MAX_QUEUED_DRAFTS, WARM_BODY_LIMIT};
use crate::{
    Action, ClientFailure, ClientPort, ClientRequest, ClientUpdate, ConnectionState, Layout,
    Settings,
    commands::{self, Command, SlashEntry},
    composer::Composer,
    effect::{Effect, Effects, Redraw},
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
const MOUSE_SCROLL_ROWS: usize = 3;
/// Notices are deliberately ephemeral. At the 125 ms UI tick this keeps each
/// notice visible for five seconds without making it permanent UI.
const NOTICE_TICKS: u16 = 40;
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
    /// A steering message in flight; `text` returns to the composer if the
    /// server refuses it.
    Steer {
        session_id: SessionId,
        text: String,
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
    history_position: Option<usize>,
    history_draft: Option<String>,
    /// Cursor into the slash autocomplete list. The query is the composer
    /// text itself, so only the cursor lives here.
    slash: Picker,
    /// Tick at which Esc was last pressed with nothing to dismiss; a second
    /// press within [`ESC_CANCEL_TICKS`] cancels the active run.
    esc_armed_at: Option<usize>,
    /// The server's advertised steering support. `None` until the capability
    /// document arrives, which reads as "unavailable": `Submit` queues and
    /// the steering commands say why.
    steering: Option<SteeringCapabilities>,
    pub connection: ConnectionState,
    pub status: Option<String>,
    /// Session owning the current transient notice. A notice never follows
    /// the user into another session.
    status_session_id: Option<SessionId>,
    pub(crate) status_level: NoticeLevel,
    status_ticks_left: u16,
    pub animation_tick: usize,
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
    last_sequence: u64,
    recent_events: VecDeque<SessionEventEnvelope>,
    pending: HashMap<CommandId, PendingIntent>,
    answered_approvals: std::collections::HashSet<qq_protocol::ToolCallId>,
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
            history_position: None,
            history_draft: None,
            slash: Picker::new(),
            esc_armed_at: None,
            steering: None,
            connection: ConnectionState::Connecting,
            status: None,
            status_session_id: None,
            status_level: NoticeLevel::Info,
            status_ticks_left: 0,
            animation_tick: 0,
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
            last_sequence: 0,
            recent_events: VecDeque::new(),
            pending: HashMap::new(),
            answered_approvals: std::collections::HashSet::new(),
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

    pub fn apply_client_update(&mut self, update: ClientUpdate) -> Effects {
        match update {
            ClientUpdate::Connection(connection) => {
                self.connection = connection;
                Effects::redraw(Redraw::Scheduled)
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
                self.set_warning("session state reset after reconnecting".to_owned());
                self.apply_snapshot(snapshot)
            }
            ClientUpdate::Models { models, selected } => {
                self.apply_models(models, selected);
                Effects::redraw(Redraw::Scheduled)
            }
            ClientUpdate::Steering(capabilities) => {
                self.steering = Some(capabilities);
                Effects::none()
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
                            // A steer that lost the race to the finishing run was never
                            // applied; hand the text back rather than losing it.
                            if let Some(PendingIntent::Steer { session_id, text }) = intent
                                && self.focused() == Some(session_id)
                                && self.composer.text.is_empty()
                            {
                                self.composer.replace(text);
                                self.set_warning(
                                    "run finished before it could be steered; draft restored"
                                        .to_owned(),
                                );
                            } else {
                                self.set_warning("run already finished".to_owned());
                            }
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
                Effects::redraw(Redraw::Scheduled)
            }
            ClientUpdate::SnapshotFailed(error) => {
                self.set_warning(error.message().to_owned());
                Effects::redraw(Redraw::Scheduled)
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

    fn apply_snapshot(&mut self, snapshot: WorkspaceSnapshot) -> Effects {
        let initial = self.workspace_id.is_none();
        if self
            .workspace_id
            .is_some_and(|workspace| workspace != snapshot.workspace.id)
        {
            self.set_warning("server returned a snapshot for another workspace".to_owned());
            return Effects::redraw(Redraw::Scheduled);
        }
        let snapshot_focus = snapshot.focused.as_ref().map(|focused| focused.summary.id);
        // A late snapshot for a session no pane shows any more is stale
        // navigation output; installing it would yank focus back.
        if !initial
            && self.focused().is_some()
            && snapshot_focus.is_some_and(|id| !self.panes.sessions().any(|shown| shown == id))
        {
            return Effects::none();
        }
        if snapshot.cursor.sequence < self.last_sequence
            && self
                .recent_events
                .front()
                .is_none_or(|event| event.cursor.sequence > snapshot.cursor.sequence + 1)
        {
            self.set_warning("snapshot was too stale; reconnecting is required".to_owned());
            return Effects::redraw(Redraw::Scheduled);
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
        let mut effects = Effects::redraw(Redraw::Scheduled);
        for event in replay {
            effects.extend(self.reduce_event(&event));
        }
        effects
    }

    /// Load one session's transcript body. Other warm bodies are untouched;
    /// `evict_cold_bodies` enforces the warm limit afterwards.
    fn install_session_snapshot(&mut self, snapshot: SessionSnapshot, loaded_through: u64) {
        let session_id = snapshot.summary.id;
        let mut messages = snapshot.messages;
        retain_recent_messages(&mut messages);
        let history = messages
            .iter()
            .filter(|message| message.role == qq_protocol::MessageRole::User)
            .map(|message| message.output.clone())
            .filter(|prompt| !prompt.trim().is_empty())
            .collect::<VecDeque<_>>();
        let mut tool_calls = snapshot.tool_calls;
        retain_recent_tool_calls(&mut tool_calls);
        let context_window = model_context_window(&self.models, snapshot.summary.model.as_deref());
        let previous = self.sessions.remove(&session_id);
        let mut view = SessionView::summary_only(snapshot.summary, context_window, loaded_through);
        view.live = LiveStatus::from_body(&messages, &tool_calls);
        view.prompt_history = history
            .into_iter()
            .rev()
            .take(MAX_PROMPT_HISTORY)
            .rev()
            .collect();
        if let Some(previous) = previous {
            view.last_focused = previous.last_focused;
            view.drafts = previous.drafts;
            // Live tool output for calls this body no longer reports as
            // running would render forever; keep only the running ones.
            view.live_tool_output = previous.live_tool_output;
            view.live_tool_output.retain(|id, _| {
                tool_calls
                    .iter()
                    .any(|call| call.id == *id && call.state == ToolCallState::Running)
            });
            view.edit_previews = previous.edit_previews;
        }
        view.messages = Some(messages);
        view.tool_calls = Some(tool_calls);
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
                session.evict_body();
            }
        }
    }

    fn apply_live_event(&mut self, event: SessionEventEnvelope) -> Effects {
        if self
            .workspace_id
            .is_some_and(|workspace| workspace != event.cursor.workspace_id)
        {
            self.set_warning("server sent an event for another workspace".to_owned());
            return Effects::redraw(Redraw::Scheduled);
        }
        if event.cursor.sequence <= self.last_sequence {
            return Effects::none();
        }
        if self.last_sequence != 0 && event.cursor.sequence != self.last_sequence + 1 {
            self.connection = ConnectionState::Replaying;
            self.set_warning("session event gap detected".to_owned());
            return Effects::redraw(Redraw::Scheduled);
        }
        self.workspace_id.get_or_insert(event.cursor.workspace_id);
        self.last_sequence = event.cursor.sequence;
        let already_loaded = self
            .sessions
            .get(&event.session_id)
            .is_some_and(|session| event.cursor.sequence <= session.loaded_through);
        let mut effects = Effects::redraw(Redraw::Scheduled);
        if !already_loaded {
            effects.extend(self.reduce_event(&event));
        }
        if let Some(command_id) = event.caused_by {
            self.pending.remove(&command_id);
        }
        self.recent_events.push_back(event);
        while self.recent_events.len() > MAX_RECENT_EVENTS {
            self.recent_events.pop_front();
        }
        effects
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
            | Some(PendingIntent::Steer { session_id, .. })
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
            | Some(PendingIntent::Steer { session_id, text })
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

    pub fn handle_terminal_event(&mut self, event: Event) -> Effects {
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
                Effects::changed_now(changed)
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
                Effects::changed_now(changed)
            }
            Event::FocusGained => {
                self.terminal_focused = true;
                Effects::redraw(Redraw::Immediate)
            }
            Event::FocusLost => {
                self.terminal_focused = false;
                Effects::redraw(Redraw::Immediate)
            }
            Event::Resize(_, _) => Effects::redraw(Redraw::Immediate),
            Event::Key(_) | Event::Mouse(_) => Effects::none(),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Effects {
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

    fn handle_compose_key(&mut self, key: KeyEvent) -> Effects {
        // Newline chords insert into the composer. Handle them before slash
        // completion and configured bindings so they never submit.
        if is_composer_newline_key(key) {
            let changed = self.push_input('\n');
            return Effects::changed_now(changed);
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
                    return Effects::redraw(Redraw::Immediate);
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
                    return Effects::redraw(Redraw::Immediate);
                }
                if let Some(parent) = self
                    .focused()
                    .and_then(|focused| self.sessions.get(&focused)?.summary.parent_id)
                {
                    return self.focus_session(parent);
                }
                Effects::none()
            }
            KeyCode::Enter => self.submit_prompt(),
            KeyCode::PageUp => Effects::changed_now(self.scroll_focused_page(true)),
            KeyCode::PageDown => Effects::changed_now(self.scroll_focused_page(false)),
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
                Effects::changed_now(changed)
            }
            KeyCode::Backspace => {
                let changed = self.composer.backspace();
                if changed {
                    self.reset_history_browse();
                    self.slash.select(0);
                }
                Effects::changed_now(changed)
            }
            KeyCode::Delete => {
                let changed = self.composer.delete();
                if changed {
                    self.reset_history_browse();
                }
                Effects::changed_now(changed)
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Effects::changed_now(self.composer.move_word_left())
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Effects::changed_now(self.composer.move_word_right())
            }
            KeyCode::Left => Effects::changed_now(self.composer.move_left()),
            KeyCode::Right => Effects::changed_now(self.composer.move_right()),
            KeyCode::Home => Effects::changed_now(self.composer.move_line_start()),
            KeyCode::End => Effects::changed_now(self.composer.move_line_end()),
            KeyCode::Char(character) if key.modifiers == KeyModifiers::CONTROL => {
                let changed = match character.to_ascii_lowercase() {
                    'a' => self.composer.move_line_start(),
                    'e' => self.composer.move_line_end(),
                    'w' => self.composer.kill_word_back(),
                    'k' => self.composer.kill_to_line_end(),
                    'u' => self.composer.kill_to_line_start(),
                    'y' => self.composer.yank(),
                    'z' | '_' => self.composer.undo(),
                    _ => return Effects::none(),
                };
                if changed {
                    self.reset_history_browse();
                    self.slash.select(0);
                }
                Effects::changed_now(changed)
            }
            KeyCode::Up => {
                let changed = self.composer.move_up() || self.browse_prompt_history(false);
                Effects::changed_now(changed)
            }
            KeyCode::Down => {
                let changed = self.composer.move_down() || self.browse_prompt_history(true);
                Effects::changed_now(changed)
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let changed = self.push_input(character);
                Effects::changed_now(changed)
            }
            _ => Effects::none(),
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

    fn open_themes(&mut self) -> Effects {
        if self.themes.len() < 2 {
            self.set_info(
                "only the compiled `qq` theme is available; add themes/<name>.ron to choose"
                    .to_owned(),
            );
            return Effects::redraw(Redraw::Immediate);
        }
        let mut picker = Picker::new();
        picker.select(self.theme);
        self.overlay = Some(Overlay::Themes {
            picker,
            restore: self.theme,
        });
        Effects::redraw(Redraw::Immediate)
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
    fn handle_theme_picker_key(&mut self, key: KeyEvent) -> Effects {
        let filtered = self.filtered_themes();
        let Some(Overlay::Themes { picker, restore }) = &mut self.overlay else {
            return Effects::none();
        };
        let restore = *restore;
        let changed = match key.code {
            KeyCode::Esc => {
                self.overlay = None;
                self.set_theme(restore);
                return Effects::redraw(Redraw::Immediate);
            }
            KeyCode::Enter => {
                let name = self.theme().name.clone();
                self.overlay = None;
                self.set_info(format!(
                    "theme `{name}`; set `theme: \"{name}\"` in tui.ron to keep it"
                ));
                return Effects::redraw(Redraw::Immediate);
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
        Effects::changed_now(changed)
    }

    /// An effect asking for the user's attention, or nothing while the
    /// terminal is focused: a focused user is already looking.
    pub(super) fn attention(&self, attention: Attention) -> Effects {
        let mut effects = Effects::none();
        if !self.terminal_focused {
            effects.push(Effect::Attention(attention));
        }
        effects
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
    fn split_pane(&mut self, axis: Axis) -> Effects {
        match self.panes.split(axis) {
            Some(_) => Effects::redraw(Redraw::Immediate),
            None => {
                self.set_warning(format!(
                    "at most {} panes can be open",
                    crate::panes::MAX_PANES
                ));
                Effects::redraw(Redraw::Immediate)
            }
        }
    }

    fn close_pane(&mut self) -> Effects {
        let closed = self.panes.close();
        if closed.is_some() {
            self.reset_history_browse();
            self.evict_cold_bodies();
        } else {
            self.set_info("the last pane stays open".to_owned());
        }
        Effects::redraw(Redraw::Immediate)
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

    fn focus_pane_direction(&mut self, direction: Direction) -> Effects {
        match self.panes.neighbour(direction) {
            Some(id) => Effects::changed_now(self.focus_pane(id)),
            None => Effects::none(),
        }
    }

    /// Run one command from the registry. Every command surface — keybinding,
    /// slash entry, and later the palette — ends here so behavior cannot drift
    /// between them.
    pub(crate) fn execute(&mut self, command: Command) -> Effects {
        match command {
            Command::OpenModels => self.open_models(),
            Command::OpenThemes => self.open_themes(),
            Command::OpenSessions => self.open_sessions(),
            Command::OpenAgents => self.open_agents(),
            Command::ToggleSessions => {
                if matches!(self.overlay, Some(Overlay::Sessions { .. })) {
                    self.overlay = None;
                    Effects::redraw(Redraw::Immediate)
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
                Effects::redraw(Redraw::Immediate)
            }
            Command::SelectFoldFocus => {
                self.layout = Layout::FoldFocus;
                Effects::redraw(Redraw::Immediate)
            }
            Command::NextLayout => {
                self.layout = self.layout.next();
                Effects::redraw(Redraw::Immediate)
            }
            Command::PreviousLayout => {
                self.layout = self.layout.next();
                Effects::redraw(Redraw::Immediate)
            }
            Command::ToggleToolDetail => {
                self.tool_detail = self.tool_detail.next();
                Effects::redraw(Redraw::Immediate)
            }
            Command::ToggleReasoning => {
                self.reasoning_detail = match self.reasoning_detail {
                    ReasoningDetail::Collapsed => ReasoningDetail::Expanded,
                    ReasoningDetail::Expanded => ReasoningDetail::Collapsed,
                };
                Effects::redraw(Redraw::Immediate)
            }
            Command::ToggleSidebar => {
                self.sidebar = self.sidebar.next();
                Effects::redraw(Redraw::Immediate)
            }
            Command::FocusParent => match self
                .focused()
                .and_then(|focused| self.sessions.get(&focused)?.summary.parent_id)
            {
                Some(parent) => self.focus_session(parent),
                None => Effects::none(),
            },
            Command::FocusFirstChild => match self
                .focused()
                .and_then(|focused| self.children_of(focused).first().copied())
            {
                Some(child) => self.focus_session(child),
                None => Effects::none(),
            },
            Command::FocusNextSibling => match self.sibling(1) {
                Some(sibling) => self.focus_session(sibling),
                None => Effects::none(),
            },
            Command::FocusPreviousSibling => match self.sibling(-1) {
                Some(sibling) => self.focus_session(sibling),
                None => Effects::none(),
            },
            Command::OpenEditor => {
                let mut effects = Effects::redraw(Redraw::Immediate);
                effects.push(Effect::Editor(self.composer.expanded()));
                effects
            }
            Command::SplitBeside => self.split_pane(Axis::Columns),
            Command::SplitBelow => self.split_pane(Axis::Rows),
            Command::ClosePane => self.close_pane(),
            Command::ZoomPane => Effects::changed_now(self.panes.toggle_zoom()),
            Command::FocusPaneLeft => self.focus_pane_direction(Direction::Left),
            Command::FocusPaneRight => self.focus_pane_direction(Direction::Right),
            Command::FocusPaneUp => self.focus_pane_direction(Direction::Up),
            Command::FocusPaneDown => self.focus_pane_direction(Direction::Down),
            Command::ResizePaneLeft => Effects::changed_now(self.panes.resize(Direction::Left)),
            Command::ResizePaneRight => Effects::changed_now(self.panes.resize(Direction::Right)),
            Command::ResizePaneUp => Effects::changed_now(self.panes.resize(Direction::Up)),
            Command::ResizePaneDown => Effects::changed_now(self.panes.resize(Direction::Down)),
            Command::QueueDraft => self.queue_draft(),
            Command::DequeueDraft => self.dequeue_draft(),
            Command::SteerRun => {
                if self.steering.is_some_and(|steering| steering.boundary) {
                    return self.steer_run(false);
                }
                self.set_warning(
                    "this server does not support steering; the draft was queued instead"
                        .to_owned(),
                );
                self.queue_draft()
            }
            Command::InterruptRun => {
                if self.steering.is_some_and(|steering| steering.interrupt) {
                    return self.steer_run(true);
                }
                self.set_warning(
                    "this server does not support interrupting a run; the draft was queued instead"
                        .to_owned(),
                );
                self.queue_draft()
            }
            Command::FocusNextApproval => match self.next_session_awaiting_approval() {
                Some(session_id) => self.focus_session(session_id),
                None => {
                    self.set_info("no session is waiting for approval".to_owned());
                    Effects::redraw(Redraw::Immediate)
                }
            },
            Command::Quit => {
                let mut effects = Effects::redraw(Redraw::Immediate);
                effects.push(Effect::Quit);
                effects
            }
        }
    }

    fn open_models(&mut self) -> Effects {
        if self.models.is_empty() {
            self.set_warning("no authenticated providers have selectable models".to_owned());
            return Effects::redraw(Redraw::Immediate);
        }
        self.overlay = Some(Overlay::Models(Picker::new()));
        Effects::redraw(Redraw::Immediate)
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

    fn handle_model_picker_key(&mut self, key: KeyEvent) -> Effects {
        let filtered = self.filtered_models();
        let Some(Overlay::Models(picker)) = &mut self.overlay else {
            return Effects::none();
        };
        match key.code {
            KeyCode::Esc => {
                self.overlay = None;
                Effects::redraw(Redraw::Immediate)
            }
            KeyCode::Up => {
                picker.move_up();
                Effects::redraw(Redraw::Immediate)
            }
            KeyCode::Down => {
                picker.move_down(filtered.len());
                Effects::redraw(Redraw::Immediate)
            }
            // Enter applies the model to the focused session; without a
            // focus it keeps the historical create behavior. Ctrl-N always
            // creates a session with the selected model.
            KeyCode::Enter => {
                let Some(model) = self.selected_picker_model(&filtered) else {
                    return Effects::none();
                };
                let focused = self
                    .focused()
                    .filter(|session_id| self.sessions.contains_key(session_id));
                let result = match focused {
                    Some(session_id) => self.set_session_model(session_id, model),
                    None => self.create_session_with_model(None, model),
                };
                if result
                    .iter()
                    .any(|effect| matches!(effect, Effect::Send(_)))
                {
                    self.overlay = None;
                }
                result
            }
            KeyCode::Char('n' | 'N') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let Some(model) = self.selected_picker_model(&filtered) else {
                    return Effects::none();
                };
                let result = self.create_session_with_model(None, model);
                if result
                    .iter()
                    .any(|effect| matches!(effect, Effect::Send(_)))
                {
                    self.overlay = None;
                }
                result
            }
            KeyCode::Backspace => Effects::changed_now(picker.pop_query()),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let mut encoded = [0; 4];
                Effects::changed_now(picker.push_query(character.encode_utf8(&mut encoded)))
            }
            _ => Effects::none(),
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

    fn set_session_model(&mut self, session_id: SessionId, model: ModelSelection) -> Effects {
        let Ok(command_id) = CommandId::generate() else {
            self.set_warning("secure randomness is unavailable".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        // Remember the pick as the client default so /new and later creates
        // keep using it until the user chooses another model.
        self.model = model.clone();
        Effects::send_now(ClientRequest::Command(CommandRequest {
            command_id,
            command: SessionCommand::SetSessionModel { session_id, model },
        }))
    }

    fn open_sessions(&mut self) -> Effects {
        self.open_session_picker(None)
    }

    /// `/agents`: the focused session's root and every descendant, so the
    /// user can see and jump between the agents one task fanned out into.
    fn open_agents(&mut self) -> Effects {
        let Some(focused) = self.focused() else {
            self.set_warning("focus a session to view its agents".to_owned());
            return Effects::redraw(Redraw::Immediate);
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

    fn open_session_picker(&mut self, scope: Option<SessionId>) -> Effects {
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
        Effects::redraw(Redraw::Immediate)
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

    fn handle_session_picker_key(&mut self, key: KeyEvent) -> Effects {
        if let Some(confirm) = self.session_picker_confirm() {
            return self.handle_session_picker_confirm_key(key, confirm);
        }
        let filtered = self.filtered_sessions();
        let current = self.session_picker_selected();
        let position = current.and_then(|current| filtered.iter().position(|id| *id == current));
        match key.code {
            KeyCode::Esc => {
                self.overlay = None;
                Effects::redraw(Redraw::Immediate)
            }
            KeyCode::Up => {
                let next = filtered
                    .get(position.unwrap_or_default().saturating_sub(1))
                    .copied();
                self.set_session_picker_selection(next);
                Effects::redraw(Redraw::Immediate)
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
                Effects::redraw(Redraw::Immediate)
            }
            KeyCode::Enter => {
                let Some(current) = current else {
                    return Effects::none();
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
                Effects::changed_now(changed)
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
                Effects::redraw(Redraw::Immediate)
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
                Effects::changed_now(changed)
            }
            _ => Effects::none(),
        }
    }

    fn set_session_picker_selection(&mut self, next: Option<SessionId>) {
        if let Some(Overlay::Sessions { selected, .. }) = &mut self.overlay {
            *selected = next;
        }
    }

    fn request_delete_confirmation(&mut self, selected: Option<SessionId>) -> Effects {
        let Some(selected) = selected else {
            return Effects::none();
        };
        if self
            .sessions
            .get(&selected)
            .is_some_and(|session| session.summary.active_run_id.is_some())
        {
            self.set_warning("cancel the active run before deleting".to_owned());
            return Effects::redraw(Redraw::Immediate);
        }
        if let Some(Overlay::Sessions { confirm, .. }) = &mut self.overlay {
            *confirm = Some(SessionConfirm::Delete(selected));
        }
        Effects::redraw(Redraw::Immediate)
    }

    fn handle_session_picker_confirm_key(
        &mut self,
        key: KeyEvent,
        confirm: SessionConfirm,
    ) -> Effects {
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
                Effects::redraw(Redraw::Immediate)
            }
            _ => Effects::none(),
        }
    }

    fn clear_session_picker_confirm(&mut self) {
        if let Some(Overlay::Sessions { confirm, .. }) = &mut self.overlay {
            *confirm = None;
        }
    }

    fn delete_session(&mut self, session_id: SessionId) -> Effects {
        let Ok(command_id) = CommandId::generate() else {
            self.set_warning("secure randomness is unavailable".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        Effects::send_now(ClientRequest::Command(CommandRequest {
            command_id,
            command: SessionCommand::DeleteSession { session_id },
        }))
    }

    fn prune_sessions(&mut self) -> Effects {
        let Some(workspace_id) = self.workspace_id else {
            self.set_warning("workspace is still connecting".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        let Ok(command_id) = CommandId::generate() else {
            self.set_warning("secure randomness is unavailable".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        Effects::send_now(ClientRequest::Command(CommandRequest {
            command_id,
            command: SessionCommand::PruneSessions { workspace_id },
        }))
    }

    fn reset_session_picker_selection(&mut self) {
        let first = self.filtered_sessions().first().copied();
        self.set_session_picker_selection(first);
    }

    /// Focus a session. A warm body renders immediately with no request; a
    /// cold one shows its summary and live tail while its body is fetched.
    pub(crate) fn focus_session(&mut self, session_id: SessionId) -> Effects {
        self.set_focus(session_id);
        self.reset_history_browse();
        self.evict_cold_bodies();
        if self
            .sessions
            .get(&session_id)
            .is_some_and(SessionView::is_warm)
        {
            return Effects::redraw(Redraw::Immediate);
        }
        let Some(workspace_id) = self.workspace_id else {
            return Effects::redraw(Redraw::Immediate);
        };
        Effects::send_now(ClientRequest::Snapshot(SnapshotRequest {
            workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: SNAPSHOT_SESSION_LIMIT,
            message_limit: SNAPSHOT_MESSAGE_LIMIT,
        }))
    }

    fn create_session(&mut self, parent_id: Option<SessionId>) -> Effects {
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
    ) -> Effects {
        if !model.model.as_ref().is_some_and(|route| {
            route
                .split_once('/')
                .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
        }) {
            self.set_warning("choose a model with /models before creating a session".to_owned());
            return Effects::redraw(Redraw::Immediate);
        }
        let Some(workspace_id) = self.workspace_id else {
            self.set_warning("workspace is still connecting".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        let Ok(command_id) = CommandId::generate() else {
            self.set_warning("secure randomness is unavailable".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        // Keep the chosen model as the client default for the rest of this TUI
        // process until /models picks something else.
        self.model = model.clone();
        self.pending.insert(command_id, PendingIntent::Create);
        Effects::send_now(ClientRequest::Command(CommandRequest {
            command_id,
            command: SessionCommand::CreateSession {
                workspace_id,
                parent_id,
                model,
                approval_mode: ApprovalMode::default(),
                profile: qq_protocol::AgentProfileId::default(),
                correlation: qq_protocol::Correlation::default(),
            },
        }))
    }

    fn submit_prompt(&mut self) -> Effects {
        let prompt = self.composer.expanded().trim().to_owned();
        if prompt.is_empty() {
            return Effects::none();
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
            return Effects::redraw(Redraw::Immediate);
        };
        // Enter during an active run steers when the server supports it and
        // otherwise holds the draft locally until the run finishes. Sending
        // it to the server queue now would lose the ability to edit it.
        let running = self
            .sessions
            .get(&session_id)
            .is_some_and(|session| session.summary.active_run_id.is_some());
        if running {
            if self.steering.is_some_and(|steering| steering.boundary) {
                return self.steer_run(false);
            }
            return self.queue_draft();
        }
        self.submit_text(session_id, prompt)
    }

    /// Send the draft to the focused session's active run as steering. The
    /// caller has checked the capability; this only checks there is a run.
    /// With `interrupt`, the run's in-flight turn is aborted first.
    fn steer_run(&mut self, interrupt: bool) -> Effects {
        let text = self.composer.expanded().trim().to_owned();
        if text.is_empty() {
            return Effects::none();
        }
        let Some(session_id) = self.focused() else {
            self.set_warning("create a session before steering a run".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        let Some(run_id) = self
            .sessions
            .get(&session_id)
            .and_then(|session| session.summary.active_run_id)
        else {
            self.set_warning("focused session has no active run to steer".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        let Ok(command_id) = CommandId::generate() else {
            self.set_warning("secure randomness is unavailable".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        self.record_prompt(session_id, &text);
        self.composer.clear();
        self.reset_history_browse();
        self.slash.select(0);
        self.esc_armed_at = None;
        self.pending.insert(
            command_id,
            PendingIntent::Steer {
                session_id,
                text: text.clone(),
            },
        );
        Effects::send_now(ClientRequest::Command(CommandRequest {
            command_id,
            command: SessionCommand::SteerRun {
                run_id,
                input: vec![qq_protocol::InputPart::text(text)],
                interrupt,
            },
        }))
    }

    /// Send `prompt` to `session_id` as a new run.
    fn submit_text(&mut self, session_id: SessionId, prompt: String) -> Effects {
        let Ok(command_id) = CommandId::generate() else {
            self.set_warning("secure randomness is unavailable".to_owned());
            return Effects::redraw(Redraw::Immediate);
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
        Effects::send_now(ClientRequest::Command(CommandRequest {
            command_id,
            command: SessionCommand::SubmitPrompt {
                session_id,
                input: vec![qq_protocol::InputPart::text(prompt)],
                limits: qq_protocol::RunLimits::default(),
                correlation: qq_protocol::Correlation::default(),
            },
        }))
    }

    /// Hold the composer text for the focused session until its run ends.
    fn queue_draft(&mut self) -> Effects {
        let prompt = self.composer.expanded().trim().to_owned();
        if prompt.is_empty() {
            return Effects::none();
        }
        let Some(session_id) = self.focused() else {
            self.set_warning("create a session before queueing a prompt".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return Effects::none();
        };
        if session.drafts.len() >= MAX_QUEUED_DRAFTS {
            self.set_warning(format!(
                "at most {MAX_QUEUED_DRAFTS} drafts can wait per session"
            ));
            return Effects::redraw(Redraw::Immediate);
        }
        session.drafts.push_back(prompt);
        self.composer.clear();
        self.reset_history_browse();
        self.slash.select(0);
        Effects::redraw(Redraw::Immediate)
    }

    /// Pull the newest queued draft back into the composer for editing. A
    /// non-empty composer is queued first so nothing is lost.
    fn dequeue_draft(&mut self) -> Effects {
        let Some(session_id) = self.focused() else {
            return Effects::none();
        };
        if !self.composer.text.is_empty() {
            if self.queue_draft().is_empty() {
                return Effects::none();
            }
            // The draft just queued is newest; rotate so the previously
            // newest one comes back.
            if let Some(session) = self.sessions.get_mut(&session_id)
                && session.drafts.len() > 1
                && let Some(just_queued) = session.drafts.pop_back()
            {
                session.drafts.push_front(just_queued);
            }
        }
        let Some(draft) = self
            .sessions
            .get_mut(&session_id)
            .and_then(|session| session.drafts.pop_back())
        else {
            return Effects::none();
        };
        self.composer.replace(draft);
        Effects::redraw(Redraw::Immediate)
    }

    /// Drafts waiting for `session_id` in submission order.
    pub(crate) fn queued_drafts(&self, session_id: SessionId) -> impl Iterator<Item = &str> {
        self.sessions
            .get(&session_id)
            .into_iter()
            .flat_map(|session| &session.drafts)
            .map(String::as_str)
    }

    /// Submit the oldest waiting draft once the session goes idle. Called by
    /// the reducer on `RunFinished`; one draft per run so each becomes its
    /// own run in order.
    pub(super) fn flush_draft(&mut self, session_id: SessionId) -> Effects {
        let Some(draft) = self
            .sessions
            .get_mut(&session_id)
            .and_then(|session| session.drafts.pop_front())
        else {
            return Effects::none();
        };
        self.submit_text(session_id, draft)
    }

    fn record_prompt(&mut self, session_id: SessionId, prompt: &str) {
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.record_prompt(prompt);
        }
    }

    fn browse_prompt_history(&mut self, forward: bool) -> bool {
        let Some(session_id) = self.focused() else {
            return false;
        };
        let Some(history) = self
            .sessions
            .get(&session_id)
            .map(|session| &session.prompt_history)
        else {
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

    fn compact_session(&mut self) -> Effects {
        let Some(session_id) = self.focused() else {
            self.set_warning("create a session before compacting".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        if self
            .sessions
            .get(&session_id)
            .is_some_and(|session| session.summary.status != SessionStatus::Idle)
        {
            self.set_warning("compaction needs an idle session; wait or cancel first".to_owned());
            return Effects::redraw(Redraw::Immediate);
        }
        let Ok(command_id) = CommandId::generate() else {
            self.set_warning("secure randomness is unavailable".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        self.pending
            .insert(command_id, PendingIntent::Compact { session_id });
        self.set_info("compacting session...".to_owned());
        Effects::send_now(ClientRequest::Command(CommandRequest {
            command_id,
            command: SessionCommand::CompactSession { session_id },
        }))
    }

    fn cancel_run(&mut self) -> Effects {
        let Some(session_id) = self.focused() else {
            self.set_warning("focused session has no active run".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        let Some(run_id) = self
            .sessions
            .get(&session_id)
            .and_then(|session| session.summary.active_run_id)
        else {
            self.set_warning("focused session has no active run".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        let Ok(command_id) = CommandId::generate() else {
            self.set_warning("secure randomness is unavailable".to_owned());
            return Effects::redraw(Redraw::Immediate);
        };
        self.pending
            .insert(command_id, PendingIntent::Cancel { session_id });
        Effects::send_now(ClientRequest::Command(CommandRequest {
            command_id,
            command: SessionCommand::CancelRun { run_id },
        }))
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
        let tool_call = self.pending_approval()?;
        self.sessions
            .get(&tool_call.session_id)?
            .edit_previews
            .get(&tool_call.id)
    }

    fn handle_approval_key(&mut self, key: KeyEvent) -> Effects {
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
            _ => Effects::none(),
        }
    }

    fn respond_to_approval(&mut self, choice: ApprovalChoice) -> Effects {
        let Some(tool_call) = self.pending_approval() else {
            return Effects::none();
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
            return Effects::redraw(Redraw::Immediate);
        };
        self.answered_approvals.insert(tool_call_id);
        self.pending
            .insert(command_id, PendingIntent::Approval { tool_call_id });
        Effects::send_now(ClientRequest::Command(CommandRequest {
            command_id,
            command: SessionCommand::RespondToolApproval {
                run_id,
                tool_call_id,
                decision,
            },
        }))
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

    fn handle_slash_key(&mut self, code: KeyCode) -> Option<Effects> {
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
                Some(Effects::redraw(Redraw::Immediate))
            }
            KeyCode::Down => {
                self.slash.move_down(entries.len());
                Some(Effects::redraw(Redraw::Immediate))
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

    /// Prompts and steering sent to `session_id` whose receipt has not
    /// arrived; shown optimistically until the server's row replaces them.
    pub fn pending_prompts(&self, session_id: SessionId) -> impl Iterator<Item = &str> {
        self.pending
            .values()
            .filter_map(move |intent| match intent {
                PendingIntent::Prompt {
                    session_id: candidate,
                    text,
                }
                | PendingIntent::Steer {
                    session_id: candidate,
                    text,
                } if *candidate == session_id => Some(text.as_str()),
                PendingIntent::Create
                | PendingIntent::Prompt { .. }
                | PendingIntent::Steer { .. }
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
mod tests;
