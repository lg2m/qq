use std::collections::{HashMap, VecDeque};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use qq_protocol::{
    CommandId, CommandOutcome, CommandRequest, MessageSnapshot, MessageState, ModelSelection,
    RunOutcome, SessionCommand, SessionEvent, SessionEventEnvelope, SessionId, SessionSnapshot,
    SessionSummary, SnapshotRequest, TokenUsage, WorkspaceId, WorkspaceSnapshot,
};
use thiserror::Error;

use crate::{
    Action, ClientFailure, ClientPort, ClientRequest, ClientUpdate, ConnectionState, Layout,
    Settings, terminal,
};

const MAX_INPUT_BYTES: usize = 64 * 1024;
const MAX_MODEL_SEARCH_BYTES: usize = 256;
const MAX_RECENT_EVENTS: usize = 1024;
const SNAPSHOT_SESSION_LIMIT: u16 = 512;
const SNAPSHOT_MESSAGE_LIMIT: u16 = 256;
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
    Quit,
}

const SLASH_COMMANDS: [SlashCommand; 6] = [
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
    pub latest_input_tokens: Option<u64>,
    pub context_window: Option<u32>,
    loaded_through: u64,
}

pub(crate) struct ModelPicker {
    pub query: String,
    pub selected: usize,
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
    Prompt { session_id: SessionId, text: String },
    Cancel,
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
    pub navigator: Option<SessionId>,
    pub navigator_open: bool,
    pub model_picker: Option<ModelPicker>,
    pub input: String,
    slash_selected: usize,
    pub connection: ConnectionState,
    pub status: Option<String>,
    pub animation_tick: usize,
    pub quit: bool,
    transcript_viewport: TranscriptViewport,
    last_sequence: u64,
    recent_events: VecDeque<SessionEventEnvelope>,
    pending: HashMap<CommandId, PendingIntent>,
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
            navigator: None,
            navigator_open: false,
            model_picker: None,
            input: String::new(),
            slash_selected: 0,
            connection: ConnectionState::Connecting,
            status: None,
            animation_tick: 0,
            quit: false,
            transcript_viewport: TranscriptViewport::default(),
            last_sequence: 0,
            recent_events: VecDeque::new(),
            pending: HashMap::new(),
        }
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
                self.navigator = None;
                self.navigator_open = false;
                self.model_picker = None;
                self.last_sequence = 0;
                self.recent_events.clear();
                self.status = Some("session state reset after reconnecting".to_owned());
                self.apply_snapshot(snapshot)
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
                        if matches!(receipt.outcome, CommandOutcome::RunAlreadyFinished { .. }) {
                            self.status = Some("run already finished".to_owned());
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
        if !initial && snapshot_focus.is_some() && snapshot_focus != self.focused {
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
        }
        let mut messages = snapshot.messages;
        retain_recent_messages(&mut messages);
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
                        if message.role == qq_protocol::MessageRole::Assistant
                            || message.state == MessageState::Queued
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
                latest_input_tokens: None,
                context_window,
                loaded_through: 0,
            });
    }

    fn push_message(&mut self, message: MessageSnapshot) {
        let Some(messages) = self
            .sessions
            .get_mut(&message.session_id)
            .and_then(|session| session.messages.as_mut())
        else {
            return;
        };
        if !messages.iter().any(|candidate| candidate.id == message.id) {
            messages.push(message);
            retain_recent_messages(messages);
        }
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

    fn reject_pending(&mut self, command_id: CommandId, error: ClientFailure) {
        if let Some(PendingIntent::Prompt { session_id, text }) = self.pending.remove(&command_id)
            && self.focused == Some(session_id)
            && self.input.is_empty()
        {
            self.input = text;
        }
        self.status = Some(error.message().to_owned());
    }

    pub fn handle_terminal_event(&mut self, event: Event) -> (bool, Vec<ClientRequest>) {
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                self.handle_key(key)
            }
            Event::Paste(text) if self.model_picker.is_some() => {
                let changed = self.push_model_search(&text);
                (changed, Vec::new())
            }
            Event::Paste(text) if !self.navigator_open => {
                let before = self.input.len();
                for character in text.chars() {
                    if self.input.len() + character.len_utf8() > MAX_INPUT_BYTES {
                        break;
                    }
                    if let Some(character) = terminal_safe_character(character) {
                        self.input.push(character);
                    }
                }
                let changed = self.input.len() != before;
                if changed {
                    self.slash_selected = 0;
                }
                (changed, Vec::new())
            }
            Event::Mouse(mouse) if self.model_picker.is_none() && !self.navigator_open => {
                let changed = match mouse.kind {
                    MouseEventKind::ScrollUp => self.scroll_transcript_up(MOUSE_SCROLL_ROWS),
                    MouseEventKind::ScrollDown => self.scroll_transcript_down(MOUSE_SCROLL_ROWS),
                    _ => false,
                };
                (changed, Vec::new())
            }
            Event::Resize(_, _) | Event::FocusGained | Event::FocusLost => (true, Vec::new()),
            Event::Key(_) | Event::Mouse(_) | Event::Paste(_) => (false, Vec::new()),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> (bool, Vec<ClientRequest>) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return (true, Vec::new());
        }
        if self.model_picker.is_some() {
            return self.handle_model_picker_key(key);
        }
        if self.navigator_open {
            return self.handle_navigator_key(key.code);
        }
        if let Some(result) = self.handle_slash_key(key.code) {
            return result;
        }
        if let Some(action) = self.settings.action_for(key) {
            return self.handle_action(action);
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
                self.model_picker = None;
                if self.navigator_open {
                    self.navigator = None;
                    self.navigator_open = false;
                } else {
                    self.navigator = self
                        .focused
                        .or_else(|| self.thread_order().first().copied());
                    self.navigator_open = true;
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
        self.navigator = None;
        self.navigator_open = false;
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
            KeyCode::Enter => {
                let selected = self
                    .model_picker
                    .as_ref()
                    .and_then(|picker| filtered.get(picker.selected))
                    .and_then(|index| self.models.get(*index))
                    .map(|option| option.selection.clone());
                let Some(model) = selected else {
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

    fn push_model_search(&mut self, text: &str) -> bool {
        let Some(picker) = &mut self.model_picker else {
            return false;
        };
        let before = picker.query.len();
        for character in text.chars() {
            if picker.query.len() + character.len_utf8() > MAX_MODEL_SEARCH_BYTES {
                break;
            }
            if let Some(character) = terminal_safe_character(character) {
                picker.query.push(character);
            }
        }
        picker.selected = 0;
        picker.query.len() != before
    }

    fn handle_navigator_key(&mut self, code: KeyCode) -> (bool, Vec<ClientRequest>) {
        let order = self.thread_order();
        if order.is_empty() {
            if code == KeyCode::Esc {
                self.navigator_open = false;
                return (true, Vec::new());
            }
            return (false, Vec::new());
        }
        let selected = self.navigator.unwrap_or(order[0]);
        let position = order
            .iter()
            .position(|session| *session == selected)
            .unwrap_or_default();
        match code {
            KeyCode::Esc => {
                self.navigator = None;
                self.navigator_open = false;
                (true, Vec::new())
            }
            KeyCode::Up => {
                self.navigator = Some(order[position.saturating_sub(1)]);
                (true, Vec::new())
            }
            KeyCode::Down => {
                self.navigator = Some(order[(position + 1).min(order.len() - 1)]);
                (true, Vec::new())
            }
            KeyCode::Enter => {
                self.navigator = None;
                self.navigator_open = false;
                self.focus_session(selected)
            }
            _ => (false, Vec::new()),
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
                },
            })],
        )
    }

    fn submit_prompt(&mut self) -> (bool, Vec<ClientRequest>) {
        let prompt = self.input.trim().to_owned();
        if prompt.is_empty() {
            return (false, Vec::new());
        }
        if let Some(action) = SLASH_COMMANDS
            .iter()
            .find(|command| command.name == prompt)
            .map(|command| command.action)
        {
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

    fn push_input(&mut self, character: char) -> bool {
        let Some(character) = terminal_safe_character(character) else {
            return false;
        };
        if self.input.len() + character.len_utf8() > MAX_INPUT_BYTES {
            return false;
        }
        self.input.push(character);
        self.slash_selected = 0;
        true
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
            SlashAction::Sessions => {
                self.model_picker = None;
                self.navigator = self
                    .focused
                    .or_else(|| self.thread_order().first().copied());
                self.navigator_open = true;
                (true, Vec::new())
            }
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
                PendingIntent::Create | PendingIntent::Prompt { .. } | PendingIntent::Cancel => {
                    None
                }
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

fn retain_recent_messages(messages: &mut Vec<MessageSnapshot>) {
    let excess = messages
        .len()
        .saturating_sub(usize::from(SNAPSHOT_MESSAGE_LIMIT));
    if excess > 0 {
        messages.drain(..excess);
    }
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

#[cfg(test)]
mod tests {
    use crossterm::event::MouseEvent;
    use qq_protocol::{
        EventCursor, MessageId, MessageRole, RunId, RunSnapshot, RunStatus, SessionStatus, StoreId,
        TokenUsage, WorkspaceSummary,
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
                has_older_messages: false,
            }),
            has_older_sessions: false,
        }
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
    fn slash_command_aliases_quit_and_open_sessions_without_submitting_prompts() {
        let mut app = App::new(TuiOptions::default());
        app.apply_snapshot(snapshot());

        for command in ["/sessions", "/resume"] {
            app.input = command.to_owned();
            let (_, requests) = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            assert!(requests.is_empty());
            assert!(app.navigator.is_some());
            assert!(app.navigator_open);
            app.navigator = None;
            app.navigator_open = false;
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
            ["/models", "/sessions", "/resume", "/new", "/quit", "/exit"]
        );
        for _ in 0..10 {
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        assert_eq!(app.slash_selected, 5);
        for _ in 0..10 {
            app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        }
        assert_eq!(app.slash_selected, 0);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(app.input.is_empty());
        assert!(app.navigator_open);

        app.navigator = None;
        app.navigator_open = false;
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
    fn model_picker_filters_and_creates_an_immutable_model_session() {
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

        assert_eq!(
            app.sessions[&session_id].messages.as_ref().unwrap()[0].output,
            "hello"
        );
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
        app.navigator_open = true;
        assert!(!app.handle_terminal_event(wheel).0);
        assert!(!app.handle_terminal_event(page).0);
        assert_eq!(app.transcript_scroll_offset(), 0);
    }
}
