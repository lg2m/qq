//! Model-provider interfaces and adapters.

#![forbid(unsafe_code)]

use std::{pin::Pin, sync::Arc};

use futures_core::Stream;
use thiserror::Error;

pub mod anthropic;
pub mod bedrock;
pub mod compiler;
pub mod google;
mod http;
mod limits;
mod mantle;
pub mod openai;
pub mod openai_chat;
mod request_auth;
mod sanitize;

pub use compiler::{
    EndpointSpec, HttpAuth, HttpProtocol, HttpProviderRecipe, ProviderCompiler, ProviderRecipe,
};

/// A stream of semantic model events from a configured provider.
pub type ProviderStream =
    Pin<Box<dyn Stream<Item = Result<ProviderEvent, ProviderError>> + Send + 'static>>;

/// The provider seam consumed by the agent runtime.
pub trait Provider: Send + Sync {
    fn stream(&self, request: ModelRequest) -> ProviderStream;
}

impl<T> Provider for Arc<T>
where
    T: Provider + ?Sized,
{
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        (**self).stream(request)
    }
}

impl<T> Provider for Box<T>
where
    T: Provider + ?Sized,
{
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        (**self).stream(request)
    }
}

/// A provider-neutral model generation request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRequest {
    model: Arc<str>,
    messages: Vec<Message>,
    max_output_tokens: u32,
}

impl ModelRequest {
    #[must_use]
    pub fn new(model: impl Into<Arc<str>>, messages: Vec<Message>, max_output_tokens: u32) -> Self {
        Self {
            model: model.into(),
            messages,
            max_output_tokens,
        }
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
    pub const fn max_output_tokens(&self) -> u32 {
        self.max_output_tokens
    }
}

/// A text message in model context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    role: Role,
    content: String,
}

impl Message {
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }

    #[must_use]
    pub const fn role(&self) -> Role {
        self.role
    }

    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }
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
    Unavailable,
    Transport,
    Api,
    Response,
    Protocol,
}
