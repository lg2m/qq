//! Commands, events, identifiers, and versioned wire types.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

mod ids;
mod sessions;

pub use ids::{CommandId, IdError, MessageId, RunId, SessionId, StoreId, WorkspaceId};
pub use sessions::{
    CommandOutcome, CommandReceipt, CommandRequest, CursorError, EventCursor, MessageRole,
    MessageSnapshot, MessageState, ModelCatalogRequest, ModelDescriptor, ModelPricing,
    ModelPricingTier, ModelSelection, RunFailure, RunOutcome, RunSnapshot, RunStatus,
    SessionCommand, SessionEvent, SessionEventEnvelope, SessionSnapshot, SessionStatus,
    SessionSummary, SnapshotRequest, SubscribeRequest, TextChannel, TokenUsage, WorkspaceSnapshot,
    WorkspaceSummary,
};

pub const PROTOCOL_VERSION: u16 = 4;

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
    ProviderUnavailable,
    ProviderTransport,
    ProviderApi,
    ProviderResponse,
    ProviderProtocol,
}

/// One prompt submitted to a QQ server, optionally within a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskRequest {
    pub prompt: String,
    pub workspace: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
}

impl AskRequest {
    #[must_use]
    pub fn new(prompt: impl Into<String>, workspace: PathBuf) -> Self {
        Self {
            prompt: prompt.into(),
            workspace,
            session_id: None,
            model: None,
            max_output_tokens: None,
            organization: None,
        }
    }
}

/// Version information returned by the server health endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub protocol_version: u16,
    pub version: String,
    pub pid: u32,
}
