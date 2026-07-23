//! Agent runtime, session behavior, tools, and persistence.

#![forbid(unsafe_code)]

use std::{pin::Pin, sync::Arc};

use async_stream::stream;
use futures_core::Stream;
use futures_util::StreamExt;
use qq_protocol::{RunCommand, RunEvent, RunFailureKind, TokenUsage};
use qq_provider::{Message, ModelRequest, Provider, ProviderErrorKind, ProviderEvent};
use thiserror::Error;

mod sessions;

pub use sessions::{
    LoadedRuntime, RuntimeLoadError, RuntimeLoadFuture, RuntimeLoadRequest, RuntimeLoader,
    SessionEventStream, SessionRuntime, SessionRuntimeError, SessionRuntimeOptions,
};

pub type RunStream = Pin<Box<dyn Stream<Item = RunEvent> + Send + 'static>>;

/// Runs protocol commands against a configured model provider.
#[derive(Clone)]
pub struct Runtime {
    provider: Arc<dyn Provider>,
    model: Arc<str>,
    max_output_tokens: u32,
}

impl Runtime {
    pub fn new(
        provider: impl Provider + 'static,
        model: impl Into<Arc<str>>,
        max_output_tokens: u32,
    ) -> Result<Self, RuntimeConfigError> {
        Self::with_provider(Arc::new(provider), model, max_output_tokens)
    }

    /// Creates a runtime without reboxing an already shared provider.
    pub fn with_provider(
        provider: Arc<dyn Provider>,
        model: impl Into<Arc<str>>,
        max_output_tokens: u32,
    ) -> Result<Self, RuntimeConfigError> {
        let model = model.into();
        if model.trim().is_empty() {
            return Err(RuntimeConfigError::EmptyModel);
        }
        if max_output_tokens == 0 {
            return Err(RuntimeConfigError::ZeroMaxOutputTokens);
        }

        Ok(Self {
            provider,
            model,
            max_output_tokens,
        })
    }

    /// Runs one command and returns events as they become available.
    pub fn run(&self, command: RunCommand) -> RunStream {
        self.run_messages(vec![Message::user(command.into_prompt())])
    }

    /// Runs one model turn with explicit prior conversation context.
    pub fn run_messages(&self, messages: Vec<Message>) -> RunStream {
        let provider = Arc::clone(&self.provider);
        let model = Arc::clone(&self.model);
        let max_output_tokens = self.max_output_tokens;
        Box::pin(stream! {
            yield RunEvent::Started;

            if messages.is_empty()
                || messages
                    .iter()
                    .any(|message| message.content().trim().is_empty())
            {
                yield RunEvent::Failed {
                    kind: RunFailureKind::InvalidCommand,
                    message: "conversation messages must not be empty".to_owned(),
                };
                return;
            }

            let request = ModelRequest::new(model, messages, max_output_tokens);
            let mut provider_events = provider.stream(request);

            while let Some(event) = provider_events.next().await {
                match event {
                    Ok(ProviderEvent::OutputTextDelta { text }) => {
                        yield RunEvent::OutputTextDelta { text };
                    }
                    Ok(ProviderEvent::RefusalDelta { text }) => {
                        yield RunEvent::RefusalDelta { text };
                    }
                    Ok(ProviderEvent::Completed { usage }) => {
                        if let Some(usage) = usage {
                            yield RunEvent::Usage {
                                usage: TokenUsage {
                                    input_tokens: usage.input_tokens,
                                    cache_read_input_tokens: usage.cache_read_input_tokens,
                                    cache_write_input_tokens: usage.cache_write_input_tokens,
                                    output_tokens: usage.output_tokens,
                                },
                            };
                        }
                        yield RunEvent::Completed;
                        return;
                    }
                    Err(error) => {
                        yield RunEvent::Failed {
                            kind: run_failure_kind(error.kind()),
                            message: error.to_string(),
                        };
                        return;
                    }
                }
            }

            yield RunEvent::Failed {
                kind: RunFailureKind::ProviderProtocol,
                message: "provider stream ended without a terminal event".to_owned(),
            };
        })
    }
}

const fn run_failure_kind(kind: ProviderErrorKind) -> RunFailureKind {
    match kind {
        ProviderErrorKind::Configuration => RunFailureKind::ProviderConfiguration,
        ProviderErrorKind::Authentication => RunFailureKind::ProviderAuthentication,
        ProviderErrorKind::RateLimited => RunFailureKind::ProviderRateLimited,
        ProviderErrorKind::InvalidRequest => RunFailureKind::ProviderInvalidRequest,
        ProviderErrorKind::Unavailable => RunFailureKind::ProviderUnavailable,
        ProviderErrorKind::Transport => RunFailureKind::ProviderTransport,
        ProviderErrorKind::Api => RunFailureKind::ProviderApi,
        ProviderErrorKind::Response => RunFailureKind::ProviderResponse,
        ProviderErrorKind::Protocol => RunFailureKind::ProviderProtocol,
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RuntimeConfigError {
    #[error("model must not be empty")]
    EmptyModel,
    #[error("maximum output tokens must be greater than zero")]
    ZeroMaxOutputTokens,
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures_util::{StreamExt, stream};
    use qq_provider::{ProviderError, ProviderStream};

    use super::*;

    struct ScriptedProvider {
        request: Arc<Mutex<Option<ModelRequest>>>,
        fails: bool,
    }

    impl Provider for ScriptedProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            *self.request.lock().unwrap() = Some(request);

            if self.fails {
                return Box::pin(stream::once(async {
                    Err(ProviderError::Transport("offline".to_owned()))
                }));
            }

            Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "hel".to_owned(),
                }),
                Ok(ProviderEvent::OutputTextDelta {
                    text: "lo".to_owned(),
                }),
                Ok(ProviderEvent::RefusalDelta {
                    text: " cannot continue".to_owned(),
                }),
                Ok(ProviderEvent::Completed {
                    usage: Some(qq_provider::ProviderUsage {
                        input_tokens: 12,
                        cache_read_input_tokens: 3,
                        cache_write_input_tokens: 2,
                        output_tokens: 5,
                    }),
                }),
            ]))
        }
    }

    #[tokio::test]
    async fn maps_provider_events_to_protocol_events() {
        let captured = Arc::new(Mutex::new(None));
        let runtime = Runtime::new(
            ScriptedProvider {
                request: Arc::clone(&captured),
                fails: false,
            },
            "gpt-test",
            256,
        )
        .unwrap();

        let events = runtime
            .run(RunCommand::new("say hello"))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(
            events,
            vec![
                RunEvent::Started,
                RunEvent::OutputTextDelta {
                    text: "hel".to_owned()
                },
                RunEvent::OutputTextDelta {
                    text: "lo".to_owned()
                },
                RunEvent::RefusalDelta {
                    text: " cannot continue".to_owned()
                },
                RunEvent::Usage {
                    usage: TokenUsage {
                        input_tokens: 12,
                        cache_read_input_tokens: 3,
                        cache_write_input_tokens: 2,
                        output_tokens: 5,
                    }
                },
                RunEvent::Completed,
            ]
        );

        let request = captured.lock().unwrap().clone().unwrap();
        assert_eq!(request.model(), "gpt-test");
        assert_eq!(request.max_output_tokens(), 256);
        assert_eq!(request.messages(), [Message::user("say hello")]);
    }

    #[tokio::test]
    async fn passes_multi_turn_context_to_the_provider() {
        let captured = Arc::new(Mutex::new(None));
        let runtime = Runtime::new(
            ScriptedProvider {
                request: Arc::clone(&captured),
                fails: false,
            },
            "gpt-test",
            256,
        )
        .unwrap();

        runtime
            .run_messages(vec![
                Message::user("hey"),
                Message::assistant("Hello!"),
                Message::user("what was my first message?"),
            ])
            .collect::<Vec<_>>()
            .await;

        let request = captured.lock().unwrap().clone().unwrap();
        assert_eq!(
            request.messages(),
            [
                Message::user("hey"),
                Message::assistant("Hello!"),
                Message::user("what was my first message?"),
            ]
        );
    }

    #[tokio::test]
    async fn turns_provider_errors_into_failed_events() {
        let runtime = Runtime::new(
            ScriptedProvider {
                request: Arc::new(Mutex::new(None)),
                fails: true,
            },
            "gpt-test",
            256,
        )
        .unwrap();

        let events = runtime
            .run(RunCommand::new("hello"))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(events[0], RunEvent::Started);
        assert!(matches!(
            &events[1],
            RunEvent::Failed {
                kind: RunFailureKind::ProviderTransport,
                message,
            } if message.contains("offline")
        ));
    }
}
