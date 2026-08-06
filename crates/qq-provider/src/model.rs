//! Provider-neutral request, event, usage, and error vocabulary.

use std::sync::Arc;

use qq_reasoning::ReasoningKind;
use thiserror::Error;

/// A provider-neutral model generation request.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRequest {
    model: Arc<str>,
    messages: Vec<Message>,
    tools: Vec<ToolSpec>,
    system: Option<Arc<str>>,
    max_output_tokens: u32,
}

impl ModelRequest {
    #[must_use]
    pub fn new(model: impl Into<Arc<str>>, messages: Vec<Message>, max_output_tokens: u32) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
            system: None,
            max_output_tokens,
        }
    }

    /// Declares the tools the model may call during this request.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
        self
    }

    /// Sets the system prompt; each codec maps it to its native
    /// system/instructions slot. Requests without one keep their previous
    /// wire shape.
    #[must_use]
    pub fn with_system(mut self, system: impl Into<Arc<str>>) -> Self {
        self.system = Some(system.into());
        self
    }

    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    #[must_use]
    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    #[must_use]
    pub fn system(&self) -> Option<&str> {
        self.system.as_deref()
    }

    #[must_use]
    pub const fn max_output_tokens(&self) -> u32 {
        self.max_output_tokens
    }
}

/// A tool the model may call, described provider-neutrally.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

impl ToolSpec {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    #[must_use]
    pub const fn input_schema(&self) -> &serde_json::Value {
        &self.input_schema
    }
}

/// A message in model context, holding ordered content blocks.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    role: Role,
    content: Vec<ContentBlock>,
}

impl Message {
    #[must_use]
    pub fn new(role: Role, content: Vec<ContentBlock>) -> Self {
        Self { role, content }
    }

    /// A user message with one text block.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: content.into(),
            }],
        }
    }

    /// An assistant message with one text block.
    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: content.into(),
            }],
        }
    }

    /// A user-role message carrying tool results back to the model.
    #[must_use]
    pub fn tool_results(results: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::User,
            content: results,
        }
    }

    #[must_use]
    pub const fn role(&self) -> Role {
        self.role
    }

    #[must_use]
    pub fn content(&self) -> &[ContentBlock] {
        &self.content
    }

    /// Whether any block carries usable content.
    #[must_use]
    pub fn has_content(&self) -> bool {
        self.content.iter().any(|block| match block {
            ContentBlock::Text { text } => !text.trim().is_empty(),
            ContentBlock::ToolCall { .. } | ContentBlock::ToolResult { .. } => true,
        })
    }
}

/// One ordered unit of message content.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentBlock {
    Text {
        text: String,
    },
    /// A model-requested tool invocation, valid in assistant messages.
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    /// The result of one tool invocation, valid in user messages.
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

/// Events common to provider streaming implementations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderEvent {
    OutputTextDelta { text: String },
    RefusalDelta { text: String },
    ReasoningStarted { kind: ReasoningKind },
    ReasoningDelta { kind: ReasoningKind, text: String },
    ReasoningCompleted { kind: ReasoningKind },
    ToolCallStarted { id: String, name: String },
    ToolCallArgumentsDelta { id: String, json: String },
    ToolCallCompleted { id: String },
    Completed { usage: Option<ProviderUsage> },
}

/// Provider-neutral token counts for one completed model response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderUsage {
    /// Input tokens that were neither read from nor written to a cache.
    pub input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    /// All generated tokens, including hidden reasoning tokens when reported.
    pub output_tokens: u64,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider configuration is invalid: {0}")]
    Configuration(String),
    #[error("provider credentials are unavailable: {0}")]
    CredentialsUnavailable(String),
    #[error("provider request failed: {0}")]
    Transport(String),
    #[error("provider returned HTTP {status}: {message}")]
    Api { status: u16, message: String },
    #[error("provider response failed: {message}")]
    ResponseFailed {
        kind: ProviderErrorKind,
        message: String,
    },
    #[error("provider response was incomplete: {0}")]
    ResponseIncomplete(String),
    #[error("provider stream was invalid: {0}")]
    Protocol(String),
}

impl ProviderError {
    #[must_use]
    pub const fn kind(&self) -> ProviderErrorKind {
        match self {
            Self::Configuration(_) => ProviderErrorKind::Configuration,
            Self::CredentialsUnavailable(_) => ProviderErrorKind::Authentication,
            Self::Transport(_) => ProviderErrorKind::Transport,
            Self::Api { status, .. } => match *status {
                400 | 404 | 409 | 422 => ProviderErrorKind::InvalidRequest,
                401 | 403 => ProviderErrorKind::Authentication,
                429 => ProviderErrorKind::RateLimited,
                500..=599 => ProviderErrorKind::Unavailable,
                _ => ProviderErrorKind::Api,
            },
            Self::ResponseFailed { kind, .. } => *kind,
            Self::ResponseIncomplete(_) => ProviderErrorKind::Response,
            Self::Protocol(_) => ProviderErrorKind::Protocol,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    Configuration,
    Authentication,
    RateLimited,
    InvalidRequest,
    /// The request exceeded the model's context window (or the provider's
    /// request-size cap). Distinguished from other invalid requests because
    /// the session layer can recover by compacting and retrying.
    ContextExceeded,
    Unavailable,
    Transport,
    Api,
    Response,
    Protocol,
}
