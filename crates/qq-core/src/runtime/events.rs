use qq_protocol::{
    ReasoningKind, RunActivity, RunFailureKind, RunPromptIdentity, TokenUsage, ToolCallDisplay,
    ToolCallId,
};
use qq_provider::Message;

use crate::workspace::FileStateUpdate;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RuntimeEvent {
    Started,
    Prepared {
        identity: RunPromptIdentity,
    },
    ActivityChanged {
        activity: RunActivity,
    },
    ReasoningStarted {
        kind: ReasoningKind,
    },
    ReasoningDelta {
        kind: ReasoningKind,
        text: String,
    },
    ReasoningCompleted {
        kind: ReasoningKind,
    },
    OutputTextDelta {
        text: String,
    },
    RefusalDelta {
        text: String,
    },
    AssistantTurnCompleted {
        turn_ordinal: u16,
        message: Message,
        usage: Option<TokenUsage>,
        /// Tool calls requested by this turn, in request order. Carried on the
        /// same event as the completed turn so the store can persist the turn
        /// and its calls in one transaction; a crash must never leave a
        /// persisted ToolCall block without its tool_calls rows.
        calls: Vec<RuntimeToolCall>,
    },
    ToolCallStarted {
        id: ToolCallId,
    },
    ToolCallDenied {
        id: ToolCallId,
        message: String,
    },
    /// A chunk of live output from a running tool (shell commands stream their
    /// combined stdout+stderr). Display-only: the bounded result on
    /// `ToolCallFinished` remains authoritative.
    ToolCallOutputDelta {
        id: ToolCallId,
        chunk: String,
    },
    ToolCallFinished {
        id: ToolCallId,
        result: String,
        is_error: bool,
        /// A file-state map entry recorded by this execution, persisted with
        /// the result so the map can be rebuilt for later runs.
        file_state: Option<FileStateUpdate>,
        /// A UI-facing payload persisted with the result (the applied diff of
        /// a successful edit). Never enters model context.
        display: Option<ToolCallDisplay>,
    },
    Completed,
    Failed {
        kind: RunFailureKind,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeToolCall {
    pub(crate) id: ToolCallId,
    pub(crate) turn_ordinal: u16,
    pub(crate) call_ordinal: u16,
    pub(crate) provider_call_id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
    /// Set when the provider streamed arguments that were not valid JSON. The
    /// call is never executed; this message is returned to the model as a
    /// retryable tool error instead of failing the run.
    pub(crate) argument_error: Option<String>,
}

pub(crate) struct PendingToolCall {
    pub(crate) provider_call_id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
    pub(crate) parsed_arguments: Option<serde_json::Value>,
    pub(crate) argument_error: Option<String>,
    pub(crate) completed: bool,
}

pub(crate) enum TurnBlock {
    Text(String),
    ToolCall(usize),
}
