//! Client-side session state reduced from server events: the per-session
//! view with its warm body, live status, and reasoning, and the store that
//! indexes the tree they form.

use std::{
    cell::OnceCell,
    collections::{HashMap, VecDeque, hash_map},
};

use qq_protocol::{
    EditPreview, MessageSnapshot, RunActivity, RunId, SessionId, SessionSummary, ToolCallId,
    ToolCallSnapshot, ToolCallState,
};

use crate::app::terminal_safe_character;

/// Warm transcript bodies kept loaded at once. The focused session is always
/// warm; the rest are the most recently focused sessions so switching back
/// costs no round trip.
pub(super) const WARM_BODY_LIMIT: usize = 8;
/// Bytes of assistant text retained per session for the live status tail.
pub(super) const LIVE_TAIL_BYTES: usize = 256;

/// Bytes of reasoning text retained per run.
pub(super) const MAX_REASONING_BYTES: usize = 16 * 1024;
/// Bytes of live tool output retained per running call.
pub(super) const MAX_LIVE_TOOL_OUTPUT_BYTES: usize = 4 * 1024;
/// Prompts remembered per session for Up/Down history browsing.
pub(super) const MAX_PROMPT_HISTORY: usize = 100;
/// Drafts that may wait per session while it runs.
pub(crate) const MAX_QUEUED_DRAFTS: usize = 8;

/// Every session the client knows about, with the tree indexes the sidebar,
/// pickers, and navigation read every frame.
///
/// Reads go through `get`; writes go through `get_mut`, `insert`, `remove`,
/// `entry`, and `values_mut`, each of which drops the cached tree index so
/// the next read rebuilds it once. A frame therefore pays for the index at
/// most once per batch of mutations instead of once per call site.
#[derive(Debug, Default)]
pub(crate) struct SessionStore {
    sessions: HashMap<SessionId, SessionView>,
    index: OnceCell<TreeIndex>,
}

/// Derived tree shape, valid until the next mutation.
#[derive(Debug, Default)]
struct TreeIndex {
    /// Depth-first order: roots newest-first, children oldest-first.
    order: Vec<SessionId>,
    depth: HashMap<SessionId, usize>,
    /// Children oldest-first, keyed by parent; `None` holds the roots.
    children: HashMap<Option<SessionId>, Vec<SessionId>>,
    spawned_by_call: HashMap<ToolCallId, SessionId>,
}

impl SessionStore {
    pub(crate) fn get(&self, id: &SessionId) -> Option<&SessionView> {
        self.sessions.get(id)
    }

    pub(crate) fn get_mut(&mut self, id: &SessionId) -> Option<&mut SessionView> {
        self.index.take();
        self.sessions.get_mut(id)
    }

    pub(crate) fn contains_key(&self, id: &SessionId) -> bool {
        self.sessions.contains_key(id)
    }

    pub(crate) fn insert(&mut self, id: SessionId, view: SessionView) -> Option<SessionView> {
        self.index.take();
        self.sessions.insert(id, view)
    }

    pub(crate) fn remove(&mut self, id: &SessionId) -> Option<SessionView> {
        self.index.take();
        self.sessions.remove(id)
    }

    pub(crate) fn entry(&mut self, id: SessionId) -> hash_map::Entry<'_, SessionId, SessionView> {
        self.index.take();
        self.sessions.entry(id)
    }

    pub(crate) fn clear(&mut self) {
        self.index.take();
        self.sessions.clear();
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &SessionView> {
        self.sessions.values()
    }

    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut SessionView> {
        self.index.take();
        self.sessions.values_mut()
    }

    /// Every session in tree order: roots newest-first, each followed by
    /// its descendants oldest-first.
    pub(crate) fn thread_order(&self) -> &[SessionId] {
        &self.index().order
    }

    /// Distance from the root; zero for roots and unknown sessions.
    pub(crate) fn depth(&self, id: SessionId) -> usize {
        self.index().depth.get(&id).copied().unwrap_or(0)
    }

    /// Direct children of `parent`, oldest-first.
    pub(crate) fn children_of(&self, parent: SessionId) -> &[SessionId] {
        self.index()
            .children
            .get(&Some(parent))
            .map_or(&[], Vec::as_slice)
    }

    /// Root sessions, oldest-first.
    pub(crate) fn roots(&self) -> &[SessionId] {
        self.index().children.get(&None).map_or(&[], Vec::as_slice)
    }

    /// The child session a `spawn_agent` call created, if any.
    pub(crate) fn child_spawned_by(&self, tool_call_id: ToolCallId) -> Option<SessionId> {
        self.index().spawned_by_call.get(&tool_call_id).copied()
    }

    fn index(&self) -> &TreeIndex {
        self.index.get_or_init(|| {
            let mut index = TreeIndex::default();
            for session in self.sessions.values() {
                index
                    .children
                    .entry(session.summary.parent_id)
                    .or_default()
                    .push(session.summary.id);
                if let Some(origin) = session.summary.spawned_by
                    && let Some(call) = origin.tool_call_id
                {
                    index.spawned_by_call.insert(call, session.summary.id);
                }
            }
            for siblings in index.children.values_mut() {
                siblings.sort_by_key(|id| self.sessions[id].summary.updated_at_ms);
            }
            // Roots are newest-first; popping from the back yields the newest.
            let mut stack: Vec<(SessionId, usize)> = index
                .children
                .get(&None)
                .into_iter()
                .flatten()
                .map(|id| (*id, 0))
                .collect();
            index.order.reserve(self.sessions.len());
            while let Some((session_id, depth)) = stack.pop() {
                index.order.push(session_id);
                index.depth.insert(session_id, depth);
                if let Some(kids) = index.children.get(&Some(session_id)) {
                    // Children render oldest-first, so push newest first to
                    // be popped last.
                    stack.extend(kids.iter().rev().map(|id| (*id, depth + 1)));
                }
            }
            index
        })
    }
}

impl std::ops::Index<&SessionId> for SessionStore {
    type Output = SessionView;

    fn index(&self, id: &SessionId) -> &SessionView {
        &self.sessions[id]
    }
}
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
    pub(crate) fn append(&mut self, text: &str) {
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

/// What is known about one run's timing and outcome, for the completion
/// line under its last message. Timestamps are the server's `occurred_at_ms`
/// of the run events; historical runs loaded from a snapshot carry only the
/// outcome and usage until the protocol records run timing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RunStats {
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub outcome: Option<qq_protocol::RunOutcome>,
    pub usage: Option<qq_protocol::TokenUsage>,
    /// Tool calls the run made, counted as their finished events arrive or
    /// from the loaded body.
    pub tool_calls: u32,
    /// Estimated cost of the run, when the accounting delta was observable.
    pub cost_usd_nanos: Option<u64>,
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
    /// Timing and outcome per run, for completion lines. Bounded with
    /// `reasoning`: runs whose messages were trimmed are dropped together.
    pub runs: HashMap<RunId, RunStats>,
    /// Focus clock at the last time this session was focused; orders warm
    /// body eviction. Zero for never-focused sessions.
    pub(crate) last_focused: u64,
    pub(crate) loaded_through: u64,
    /// Prompts this session has submitted, oldest first, for history browsing.
    pub(crate) prompt_history: VecDeque<String>,
    /// Drafts held locally (Alt-Enter) while the session runs; they submit in
    /// order when it goes idle. Bounded by [`MAX_QUEUED_DRAFTS`].
    pub(crate) drafts: VecDeque<String>,
    /// Bounded tails of live streamed output per running tool call, dropped
    /// when the call reaches a terminal state or the body reloads.
    pub(crate) live_tool_output: HashMap<ToolCallId, String>,
    /// Diff previews carried by approval requests, kept only while the call
    /// awaits an answer so the modal can show what an edit would change.
    pub(crate) edit_previews: HashMap<ToolCallId, EditPreview>,
}

impl SessionView {
    pub(crate) fn summary_only(
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
            runs: HashMap::new(),
            last_focused: 0,
            loaded_through,
            prompt_history: VecDeque::new(),
            drafts: VecDeque::new(),
            live_tool_output: HashMap::new(),
            edit_previews: HashMap::new(),
        }
    }

    /// Refresh the summary in place. Activity follows the summary when the
    /// summary carries it or the run changed; a live event already applied
    /// for the same run is kept when the summary is silent.
    pub(crate) fn set_summary(&mut self, summary: SessionSummary, context_window: Option<u32>) {
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

    /// Append a live output chunk for `tool_call_id`, keeping the tail bounded.
    pub(crate) fn append_live_tool_output(&mut self, tool_call_id: ToolCallId, chunk: &str) {
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

    /// Remember a submitted prompt, skipping consecutive duplicates.
    pub(crate) fn record_prompt(&mut self, prompt: &str) {
        if self
            .prompt_history
            .back()
            .is_some_and(|previous| previous == prompt)
        {
            return;
        }
        self.prompt_history.push_back(prompt.to_owned());
        while self.prompt_history.len() > MAX_PROMPT_HISTORY {
            self.prompt_history.pop_front();
        }
    }

    /// Drop the warm body and everything keyed by its tool calls.
    pub(crate) fn evict_body(&mut self) {
        self.messages = None;
        self.tool_calls = None;
        self.live_tool_output.clear();
        self.edit_previews.clear();
    }
}

impl LiveStatus {
    /// Derive status from a loaded body, as after a snapshot.
    pub(crate) fn from_body(messages: &[MessageSnapshot], tool_calls: &[ToolCallSnapshot]) -> Self {
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
    pub(crate) fn set_tail(&mut self, text: &str) {
        let mut start = text.len().saturating_sub(LIVE_TAIL_BYTES);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        self.tail.clear();
        self.tail_space_pending = false;
        self.push_collapsed(&text[start..]);
    }

    pub(crate) fn push_collapsed(&mut self, text: &str) {
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
    pub(crate) fn append_tail(&mut self, text: &str) {
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

    pub(crate) fn note_tool_call(&mut self, call: &ToolCallSnapshot) {
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
