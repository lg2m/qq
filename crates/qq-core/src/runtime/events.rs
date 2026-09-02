use std::sync::Arc;

use qq_protocol::{
    BudgetExhaustion, ContentHash, ReasoningKind, RunActivity, RunFailureKind, RunPromptIdentity,
    TokenUsage, ToolCallDisplay, ToolCallId,
};
use qq_provider::Message;
use sha2::{Digest, Sha256};

use crate::workspace::FileStateUpdate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PreparedRequestWeight {
    pub(crate) max_output_tokens: u32,
    pub(crate) system_bytes: u64,
    pub(crate) tool_schema_bytes: u64,
    pub(crate) reducible_message_bytes: u64,
    pub(crate) irreducible_message_bytes: u64,
    /// Provider-measured occupancy of the compatible preceding request plus
    /// the conservative byte weight appended since that request.
    pub(crate) compatible_input_tokens: Option<u64>,
}

impl PreparedRequestWeight {
    pub(crate) const fn input_bytes(self) -> u64 {
        self.system_bytes
            .saturating_add(self.tool_schema_bytes)
            .saturating_add(self.reducible_message_bytes)
            .saturating_add(self.irreducible_message_bytes)
    }
}

/// Identity of the exact immutable prefix placed before the conversation for
/// one provider turn. Tool-free checkpoint and compaction turns intentionally
/// differ from ordinary turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct PreparedStaticPrefix(ContentHash);

impl PreparedStaticPrefix {
    pub(crate) fn new(system: ContentHash, tools: Option<ContentHash>) -> Self {
        let mut digest = Sha256::new();
        digest.update(b"qq-prepared-static-prefix-v1");
        digest.update(system.as_bytes());
        match tools {
            Some(tools) => {
                digest.update([1]);
                digest.update(tools.as_bytes());
            }
            None => digest.update([0]),
        }
        Self(ContentHash::from_bytes(digest.finalize().into()))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RuntimeEvent {
    Started,
    Prepared {
        turn_ordinal: u16,
        identity: Option<Arc<RunPromptIdentity>>,
        static_prefix: PreparedStaticPrefix,
        weight: PreparedRequestWeight,
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
    /// A caller-imposed limit settled the run. Emitted after the reserved
    /// final response turn (if any) has been persisted via
    /// `AssistantTurnCompleted`.
    BudgetExhausted {
        exhaustion: BudgetExhaustion,
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
