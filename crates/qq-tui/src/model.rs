//! Client-side session state reduced from server events: the per-session
//! view with its warm body, live status, and reasoning.

use std::collections::{HashMap, VecDeque};

use qq_protocol::{
    EditPreview, MessageSnapshot, RunActivity, RunId, SessionSummary, ToolCallId, ToolCallSnapshot,
    ToolCallState,
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
