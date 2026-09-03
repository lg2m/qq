//! Commands, events, identifiers, and versioned wire types.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

mod ids;
mod limits;
mod local;
mod sessions;

pub use ids::{CommandId, IdError, MessageId, RunId, SessionId, StoreId, ToolCallId, WorkspaceId};
pub use limits::{
    MAX_EVENT_BYTES, MAX_MODEL_BYTES, MAX_ORGANIZATION_BYTES, MAX_REQUEST_BYTES,
    MAX_WORKSPACE_BYTES,
};
pub use local::{LocalConnectionError, LocalServerConnection};
pub use qq_reasoning::{ReasoningEvent, ReasoningKind};
pub use sessions::{
    AccountingTotal, ApprovalDecision, ApprovalGrant, ApprovalMode, ApprovalResolution,
    BudgetExhaustion, BudgetLimitKind, CapabilitySupport, CommandOutcome, CommandReceipt,
    CommandRequest, ContentHash, ContentHashError, CursorError, EditPreview, EventCursor,
    GenerationCapabilities, GuidanceIdentity, GuidanceKind, InstructionHash, InstructionHashError,
    MAX_INCLUDED_SESSIONS, MessageRole, MessageSnapshot, MessageState, ModelCatalogRequest,
    ModelDescriptor, ModelPricing, ModelPricingTier, ModelSelection, PromptCacheCapabilities,
    PromptVersion, ProviderRequestShapeIdentity, ProviderRequestShapeVersion, ResolvedModel,
    ResolvedModelVersion, RunActivity, RunFailure, RunLimits, RunOutcome, RunPromptIdentity,
    RunSnapshot, RunStatus, SessionAccounting, SessionCommand, SessionEvent, SessionEventEnvelope,
    SessionSnapshot, SessionStatus, SessionSummary, ShellCommandPreview, SnapshotRequest,
    SpawnOrigin, SubscribeRequest, TextChannel, TokenUsage, ToolCallDisplay, ToolCallSnapshot,
    ToolCallState, WorkspaceGrantOutcome, WorkspaceSnapshot, WorkspaceSummary,
};

pub const PROTOCOL_VERSION: u16 = 12;

/// Slash commands owned by interactive clients rather than the shared
/// runtime. Keeping this vocabulary in the transport-neutral protocol avoids
/// a client/runtime drift where one side forwards a name the other reserves.
pub const RESERVED_CLIENT_SLASH_COMMANDS: [&str; 8] = [
    "/models",
    "/sessions",
    "/resume",
    "/agents",
    "/new",
    "/compact",
    "/quit",
    "/exit",
];

/// Starts one model run from a user prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCommand {
    prompt: String,
}

impl RunCommand {
    #[must_use]
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
        }
    }

    #[must_use]
    pub fn into_prompt(self) -> String {
        self.prompt
    }
}

/// Provider-independent events produced by one model run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunEvent {
    Started,
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
    Usage {
        usage: TokenUsage,
    },
    Completed,
    Failed {
        kind: RunFailureKind,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunFailureKind {
    InvalidCommand,
    Configuration,
    Authentication,
    Policy,
    Server,
    ProviderConfiguration,
    ProviderAuthentication,
    ProviderRateLimited,
    ProviderInvalidRequest,
    /// The assembled request exceeded the model's context window. The
    /// session layer treats this as recoverable: it compacts and retries
    /// before ever surfacing this kind as terminal.
    ProviderContextExceeded,
    ProviderUnavailable,
    ProviderTransport,
    ProviderApi,
    ProviderResponse,
    ProviderProtocol,
}

/// Version information returned by the server health endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub protocol_version: u16,
    pub version: String,
    pub pid: u32,
}
