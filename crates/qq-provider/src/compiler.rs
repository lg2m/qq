//! Provider recipe compilation.

use std::{fmt, sync::Arc};

use crate::{
    Provider, ProviderError, SharedRequestCredentialProvider,
    bedrock::{Bedrock, BedrockAuth},
    construction::{
        HttpConstructionAuth, HttpConstructionSpec, HttpEndpointKind, construct_http_provider,
    },
    http::{build_client, build_direct_client, validate_endpoint},
    mantle::Mantle,
};

/// Compiles provider recipes while sharing expensive transport state.
#[derive(Clone)]
pub struct ProviderCompiler {
    http: reqwest::Client,
    direct_http: reqwest::Client,
}

impl ProviderCompiler {
    /// Creates a compiler with one reusable HTTP connection pool.
    pub fn new() -> Result<Self, ProviderError> {
        Ok(Self {
            http: build_client()?,
            direct_http: build_direct_client()?,
        })
    }

    /// Validates and compiles one immutable provider recipe.
    pub fn compile(&self, recipe: ProviderRecipe) -> Result<Arc<dyn Provider>, ProviderError> {
        match recipe {
            ProviderRecipe::Http(recipe) => self.compile_http(recipe),
            ProviderRecipe::AmazonBedrock { region, auth } => {
                Ok(Arc::new(Bedrock::new(auth, region)?))
            }
            ProviderRecipe::AmazonBedrockMantle {
                region,
                protocol,
                auth,
            } => Ok(Arc::new(Mantle::new(
                self.direct_http.clone(),
                region,
                protocol,
                auth,
            )?)),
        }
    }

    fn compile_http(&self, recipe: HttpProviderRecipe) -> Result<Arc<dyn Provider>, ProviderError> {
        let endpoint_kind = match recipe.endpoint.kind {
            EndpointKind::Base => HttpEndpointKind::Base,
            EndpointKind::Exact => HttpEndpointKind::Exact,
        };
        let endpoint = recipe.endpoint.resolve(recipe.protocol)?;
        let client = if endpoint.scheme() == "http" {
            self.direct_http.clone()
        } else {
            self.http.clone()
        };
        let spec = HttpConstructionSpec {
            protocol: recipe.protocol,
            endpoint,
            endpoint_kind,
            auth: recipe.auth.into(),
            headers: recipe.headers,
        };

        Ok(Arc::new(construct_http_provider(client, spec)?))
    }
}

/// An uncompiled provider deployment recipe.
pub enum ProviderRecipe {
    Http(HttpProviderRecipe),
    AmazonBedrock {
        region: Option<String>,
        auth: BedrockAuth,
    },
    AmazonBedrockMantle {
        region: Option<String>,
        protocol: HttpProtocol,
        auth: BedrockAuth,
    },
}

impl ProviderRecipe {
    #[must_use]
    pub fn http(recipe: HttpProviderRecipe) -> Self {
        Self::Http(recipe)
    }

    #[must_use]
    pub fn amazon_bedrock(region: Option<String>, auth: BedrockAuth) -> Self {
        Self::AmazonBedrock { region, auth }
    }

    #[must_use]
    pub fn amazon_bedrock_mantle(
        region: Option<String>,
        protocol: HttpProtocol,
        auth: BedrockAuth,
    ) -> Self {
        Self::AmazonBedrockMantle {
            region,
            protocol,
            auth,
        }
    }
}

/// A protocol-bound HTTP provider recipe.
pub struct HttpProviderRecipe {
    endpoint: EndpointSpec,
    protocol: HttpProtocol,
    auth: HttpAuth,
    headers: Vec<(String, String)>,
}

impl HttpProviderRecipe {
    #[must_use]
    pub fn new(endpoint: EndpointSpec, protocol: HttpProtocol, auth: HttpAuth) -> Self {
        Self {
            endpoint,
            protocol,
            auth,
            headers: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = headers;
        self
    }
}

/// Whether a configured URL is a protocol-independent base or an exact endpoint.
pub struct EndpointSpec {
    url: String,
    kind: EndpointKind,
    allow_http: bool,
}

impl EndpointSpec {
    #[must_use]
    pub fn base(url: impl Into<String>, allow_http: bool) -> Self {
        Self {
            url: url.into(),
            kind: EndpointKind::Base,
            allow_http,
        }
    }

    #[must_use]
    pub fn exact(url: impl Into<String>, allow_http: bool) -> Self {
        Self {
            url: url.into(),
            kind: EndpointKind::Exact,
            allow_http,
        }
    }

    fn resolve(self, protocol: HttpProtocol) -> Result<reqwest::Url, ProviderError> {
        let mut url = validate_endpoint(&self.url, self.allow_http)?;
        if self.kind == EndpointKind::Exact {
            return Ok(url);
        }
        if url.query().is_some() {
            return Err(ProviderError::Configuration(
                "base endpoint URL must not contain a query".to_owned(),
            ));
        }

        url.path_segments_mut()
            .map_err(|()| {
                ProviderError::Configuration(
                    "base endpoint URL cannot contain protocol paths".to_owned(),
                )
            })?
            .pop_if_empty()
            .extend(protocol.path_segments());
        Ok(url)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EndpointKind {
    Base,
    Exact,
}

/// HTTP wire protocols supported by the compiler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpProtocol {
    OpenAiResponses,
    OpenAiChatCompletions,
    AnthropicMessages,
    GoogleGenerateContent,
}

impl HttpProtocol {
    fn path_segments(self) -> &'static [&'static str] {
        match self {
            Self::OpenAiResponses => &["responses"],
            Self::OpenAiChatCompletions => &["chat", "completions"],
            Self::AnthropicMessages => &["messages"],
            Self::GoogleGenerateContent => &[],
        }
    }
}

/// Protocol-independent HTTP authentication intent.
#[derive(Clone, PartialEq, Eq)]
pub enum HttpAuth {
    NoAuth,
    ApiKey(String),
    Bearer(String),
    Header(String, String),
    Codex {
        access_token: String,
        account_id: String,
        is_fedramp: bool,
    },
    /// Bearer credentials resolved on each request.
    RequestTimeBearer(SharedRequestCredentialProvider),
    /// Codex credentials resolved on each request.
    ///
    /// This intent is valid only for OpenAI Responses and selects the Codex
    /// request body shape automatically.
    RequestTimeCodex(SharedRequestCredentialProvider),
}

impl fmt::Debug for HttpAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAuth => formatter.write_str("NoAuth"),
            Self::ApiKey(_) => formatter
                .debug_tuple("ApiKey")
                .field(&"<redacted>")
                .finish(),
            Self::Bearer(_) => formatter
                .debug_tuple("Bearer")
                .field(&"<redacted>")
                .finish(),
            Self::Header(name, _) => formatter
                .debug_tuple("Header")
                .field(name)
                .field(&"<redacted>")
                .finish(),
            Self::Codex { .. } => formatter
                .debug_struct("Codex")
                .field("access_token", &"<redacted>")
                .field("account_id", &"<redacted>")
                .finish_non_exhaustive(),
            Self::RequestTimeBearer(_) => formatter
                .debug_tuple("RequestTimeBearer")
                .field(&"[REDACTED]")
                .finish(),
            Self::RequestTimeCodex(_) => formatter
                .debug_tuple("RequestTimeCodex")
                .field(&"[REDACTED]")
                .finish(),
        }
    }
}

impl From<HttpAuth> for HttpConstructionAuth {
    fn from(auth: HttpAuth) -> Self {
        match auth {
            HttpAuth::NoAuth => Self::NoAuth,
            HttpAuth::ApiKey(secret) => Self::ApiKey(secret),
            HttpAuth::Bearer(secret) => Self::Bearer(secret),
            HttpAuth::Header(name, secret) => Self::Header(name, secret),
            HttpAuth::Codex {
                access_token,
                account_id,
                is_fedramp,
            } => Self::Codex {
                access_token,
                account_id,
                is_fedramp,
            },
            HttpAuth::RequestTimeBearer(credentials) => Self::RequestTimeBearer(credentials),
            HttpAuth::RequestTimeCodex(credentials) => Self::RequestTimeCodex(credentials),
        }
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;

    use crate::{test_support::LoopbackServer, Message, ModelRequest, ProviderEvent};

    use super::*;

    #[tokio::test]
    async fn compiles_a_base_endpoint_into_a_streaming_provider() {
        let server = LoopbackServer::sse(concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"fast\"}\n\n",
            "data: {\"type\":\"response.completed\"}\n\n",
        ));
        let base_url = server.base_url.clone();
        let compiler = ProviderCompiler::new().unwrap();
        let provider = compiler
            .compile(ProviderRecipe::http(HttpProviderRecipe::new(
                EndpointSpec::base(format!("{base_url}/v1"), true),
                HttpProtocol::OpenAiResponses,
                HttpAuth::ApiKey("test-secret".to_owned()),
            )))
            .unwrap();

        let events = provider
            .stream(ModelRequest::new(
                "test-model",
                vec![Message::user("hello")],
                64,
            ))
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(
            &events[..],
            [
                Ok(ProviderEvent::OutputTextDelta { text }),
                Ok(ProviderEvent::Completed { usage: None })
            ] if text == "fast"
        ));
        let request = server.capture();
        assert_eq!(request.request_line(), Some("POST /v1/responses HTTP/1.1"));
        assert_eq!(request.header("authorization"), Some("Bearer test-secret"));
        assert_eq!(request.json_body()["max_output_tokens"], 64);
    }

    #[tokio::test]
    async fn request_time_codex_auth_omits_max_output_tokens() {
        let server = LoopbackServer::sse("data: {\"type\":\"response.completed\"}\n\n");
        let base_url = server.base_url.clone();
        let compiler = ProviderCompiler::new().unwrap();
        let provider = compiler
            .compile(ProviderRecipe::http(HttpProviderRecipe::new(
                EndpointSpec::exact(format!("{base_url}/backend-api/codex/responses"), true),
                HttpProtocol::OpenAiResponses,
                HttpAuth::RequestTimeCodex(SharedRequestCredentialProvider::new(
                    StaticCodexCredentials,
                )),
            )))
            .unwrap();

        let events = provider
            .stream(ModelRequest::new(
                "gpt-test",
                vec![Message::user("ping")],
                128,
            ))
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(
            &events[0],
            Ok(ProviderEvent::Completed { usage: None })
        ));
        let request = server.capture();
        assert_eq!(
            request.header("authorization"),
            Some("Bearer codex-test-access-token")
        );
        assert_eq!(
            request.header("chatgpt-account-id"),
            Some("workspace-test-id")
        );
        assert_eq!(request.header("originator"), Some("qq"));

        let request_body = request.json_body();
        assert_eq!(request_body["model"], "gpt-test");
        assert!(!request_body
            .as_object()
            .unwrap()
            .contains_key("max_output_tokens"));
    }

    struct MismatchedBearerCredentials;

    impl crate::RequestCredentialProvider for MismatchedBearerCredentials {
        fn credential(&self) -> crate::RequestCredentialFuture<'_> {
            Box::pin(async {
                crate::RequestCredential::codex("wrong-codex-token", "wrong-account", false)
            })
        }
    }

    struct MismatchedCodexCredentials;

    impl crate::RequestCredentialProvider for MismatchedCodexCredentials {
        fn credential(&self) -> crate::RequestCredentialFuture<'_> {
            Box::pin(async { crate::RequestCredential::bearer("wrong-bearer-token") })
        }
    }

    #[tokio::test]
    async fn rejects_request_time_credential_kind_mismatch_before_network_io() {
        let cases = [
            HttpAuth::RequestTimeBearer(SharedRequestCredentialProvider::new(
                MismatchedBearerCredentials,
            )),
            HttpAuth::RequestTimeCodex(SharedRequestCredentialProvider::new(
                MismatchedCodexCredentials,
            )),
        ];

        for auth in cases {
            let provider = ProviderCompiler::new()
                .unwrap()
                .compile(ProviderRecipe::http(HttpProviderRecipe::new(
                    EndpointSpec::exact("http://127.0.0.1:1/v1/responses", true),
                    HttpProtocol::OpenAiResponses,
                    auth,
                )))
                .unwrap();
            let events = provider
                .stream(ModelRequest::new(
                    "gpt-test",
                    vec![Message::user("ping")],
                    64,
                ))
                .collect::<Vec<_>>()
                .await;

            assert!(matches!(
                events.as_slice(),
                [Err(ProviderError::Configuration(message))]
                    if message == "request credential kind did not match configured authorization intent"
            ));
        }
    }

    struct StaticBearerCredentials;

    impl crate::RequestCredentialProvider for StaticBearerCredentials {
        fn credential(&self) -> crate::RequestCredentialFuture<'_> {
            Box::pin(async { crate::RequestCredential::bearer("request-time-bearer-token") })
        }
    }

    #[tokio::test]
    async fn request_time_bearer_keeps_standard_request_shape() {
        let server = LoopbackServer::sse("data: {\"type\":\"response.completed\"}\n\n");
        let provider = ProviderCompiler::new()
            .unwrap()
            .compile(ProviderRecipe::http(HttpProviderRecipe::new(
                EndpointSpec::base(format!("{}/v1", server.base_url), true),
                HttpProtocol::OpenAiResponses,
                HttpAuth::RequestTimeBearer(SharedRequestCredentialProvider::new(
                    StaticBearerCredentials,
                )),
            )))
            .unwrap();

        let events = provider
            .stream(ModelRequest::new(
                "gpt-test",
                vec![Message::user("ping")],
                128,
            ))
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(
            &events[0],
            Ok(ProviderEvent::Completed { usage: None })
        ));
        let request = server.capture();
        assert_eq!(
            request.header("authorization"),
            Some("Bearer request-time-bearer-token")
        );
        assert_eq!(request.json_body()["max_output_tokens"], 128);
    }

    struct StaticCodexCredentials;

    impl crate::RequestCredentialProvider for StaticCodexCredentials {
        fn credential(&self) -> crate::RequestCredentialFuture<'_> {
            Box::pin(async {
                crate::RequestCredential::codex(
                    "codex-test-access-token",
                    "workspace-test-id",
                    false,
                )
            })
        }
    }

    #[tokio::test]
    async fn compiles_chat_anthropic_and_google_through_the_provider_interface() {
        let cases = [
            (
                HttpProtocol::OpenAiChatCompletions,
                "/v1",
                "data: {\"choices\":[]}\n\ndata: [DONE]\n\n",
                "POST /v1/chat/completions HTTP/1.1",
                "authorization",
                "chat-secret",
                "Bearer chat-secret",
            ),
            (
                HttpProtocol::AnthropicMessages,
                "/v1",
                concat!(
                    "event: message_start\n",
                    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
                    "event: message_stop\n",
                    "data: {\"type\":\"message_stop\"}\n\n",
                ),
                "POST /v1/messages HTTP/1.1",
                "x-api-key",
                "anthropic-secret",
                "anthropic-secret",
            ),
            (
                HttpProtocol::GoogleGenerateContent,
                "",
                "data: {\"candidates\":[{\"finishReason\":\"STOP\",\"index\":0}]}\n\n",
                "POST /models/test-model:streamGenerateContent?alt=sse HTTP/1.1",
                "x-goog-api-key",
                "google-secret",
                "google-secret",
            ),
        ];

        for (protocol, base_path, response, request_line, auth_header, api_key, expected_auth) in
            cases
        {
            let server = LoopbackServer::sse(response);
            let endpoint = format!("{}{base_path}", server.base_url);
            let provider = ProviderCompiler::new()
                .unwrap()
                .compile(ProviderRecipe::http(HttpProviderRecipe::new(
                    EndpointSpec::base(endpoint, true),
                    protocol,
                    HttpAuth::ApiKey(api_key.to_owned()),
                )))
                .unwrap();

            let events = provider
                .stream(ModelRequest::new(
                    "test-model",
                    vec![Message::user("hello")],
                    64,
                ))
                .collect::<Vec<_>>()
                .await;

            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, Ok(ProviderEvent::Completed { .. }))),
                "{protocol:?} did not complete: {events:?}"
            );
            let request = server.capture();
            assert_eq!(request.request_line(), Some(request_line));
            assert_eq!(request.header(auth_header), Some(expected_auth));
        }
    }

    #[test]
    fn rejects_request_time_codex_for_non_responses_protocols() {
        for protocol in [
            HttpProtocol::OpenAiChatCompletions,
            HttpProtocol::AnthropicMessages,
            HttpProtocol::GoogleGenerateContent,
        ] {
            let error = ProviderCompiler::new()
                .unwrap()
                .compile(ProviderRecipe::http(HttpProviderRecipe::new(
                    EndpointSpec::exact("https://example.test/provider", false),
                    protocol,
                    HttpAuth::RequestTimeCodex(SharedRequestCredentialProvider::new(
                        StaticCodexCredentials,
                    )),
                )))
                .err()
                .expect("request-time Codex must be restricted to Responses");

            assert!(matches!(error, ProviderError::Configuration(_)));
        }
    }

    #[test]
    fn endpoint_modes_are_explicit_and_segment_aware() {
        let base = EndpointSpec::base("https://example.test/myresponses", false)
            .resolve(HttpProtocol::OpenAiResponses)
            .unwrap();
        let exact = EndpointSpec::exact("https://example.test/myresponses", false)
            .resolve(HttpProtocol::OpenAiResponses)
            .unwrap();

        assert_eq!(base.as_str(), "https://example.test/myresponses/responses");
        assert_eq!(exact.as_str(), "https://example.test/myresponses");
    }

    #[test]
    fn rejects_invalid_recipe_combinations_before_network_io() {
        let compiler = ProviderCompiler::new().unwrap();
        let error = compiler
            .compile(ProviderRecipe::http(HttpProviderRecipe::new(
                EndpointSpec::exact("https://example.test/v1/chat/completions", false),
                HttpProtocol::OpenAiChatCompletions,
                HttpAuth::Codex {
                    access_token: "test-access-token".to_owned(),
                    account_id: "test-account".to_owned(),
                    is_fedramp: false,
                },
            )))
            .err()
            .expect("incompatible recipe must fail");

        assert!(matches!(error, ProviderError::Configuration(_)));
    }
}
