//! Commands, events, identifiers, and versioned wire types.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

mod capabilities;
mod ids;
mod input;
mod limits;
mod local;
mod plan;
mod sessions;

pub use capabilities::{
    AgentProfileSummary, CAPABILITIES_VERSION, CapabilitiesRequest, LimitCapabilities,
    ServerCapabilities, SteeringCapabilities,
};
pub use ids::{CommandId, IdError, MessageId, RunId, SessionId, StoreId, ToolCallId, WorkspaceId};
pub use input::{
    Correlation, CorrelationError, InputError, InputPart, InputPartKind, MAX_CORRELATION_BYTES,
    MAX_CORRELATION_ENTRIES, MAX_CORRELATION_KEY_BYTES, MAX_CORRELATION_VALUE_BYTES,
    MAX_INPUT_FILE_BYTES, MAX_INPUT_FILE_PARTS, MAX_INPUT_PARTS, MAX_INPUT_PATH_BYTES,
    MAX_INPUT_TEXT_BYTES, MAX_RESOLVED_INPUT_BYTES, validate_input,
};
pub use limits::{
    MAX_EVENT_BYTES, MAX_MODEL_BYTES, MAX_ORGANIZATION_BYTES, MAX_REQUEST_BYTES,
    MAX_WORKSPACE_BYTES,
};
pub use local::{LocalConnectionError, LocalServerConnection};
pub use plan::{
    AgentPlanDigest, AgentProfileId, AgentProfileIdError, CredentialEpoch, MAX_PROFILE_ID_BYTES,
    RunPlanIdentity,
};
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
    RunSnapshot, RunStatus, SessionAccounting, SessionCommand, SessionCommandKind, SessionEvent,
    SessionEventEnvelope, SessionSnapshot, SessionStatus, SessionSummary, ShellCommandPreview,
    SnapshotRequest, SpawnOrigin, SubscribeRequest, TextChannel, TokenUsage, ToolCallDisplay,
    ToolCallSnapshot, ToolCallState, WorkspaceGrantOutcome, WorkspaceSnapshot, WorkspaceSummary,
};

pub const PROTOCOL_VERSION: u16 = 13;

/// Slash commands owned by interactive clients rather than the shared
/// runtime. Keeping this vocabulary in the transport-neutral protocol avoids
/// a client/runtime drift where one side forwards a name the other reserves.
pub const RESERVED_CLIENT_SLASH_COMMANDS: [&str; 16] = [
    "/help",
    "/commands",
    "/models",
    "/sessions",
    "/resume",
    "/agents",
    "/theme",
    "/editor",
    "/new",
    "/compact",
    "/prune",
    "/mouse",
    "/attention",
    "/changes",
    "/quit",
    "/exit",
];

/// Starts one model run from user input. The in-process form of a prompt:
/// `new` wraps one text part, `from_parts` carries validated structured input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCommand {
    input: Vec<InputPart>,
}

impl RunCommand {
    #[must_use]
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            input: vec![InputPart::text(prompt)],
        }
    }

    /// Wraps structured input after checking the shared bounds.
    pub fn from_parts(input: Vec<InputPart>) -> Result<Self, InputError> {
        validate_input(&input)?;
        Ok(Self { input })
    }

    #[must_use]
    pub fn input(&self) -> &[InputPart] {
        &self.input
    }

    #[must_use]
    pub fn into_input(self) -> Vec<InputPart> {
        self.input
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

/// Version information returned by the server health endpoint. Tolerates
/// unknown fields so an older client can read a newer server's answer and
/// report the version skew instead of a decode failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub protocol_version: u16,
    pub version: String,
    pub pid: u32,
}
