//! Provider recipe compilation.

use std::{fmt, sync::Arc};

use crate::{
    Provider, ProviderError,
    anthropic::{AnthropicAuth, AnthropicMessages},
    bedrock::{Bedrock, BedrockAuth},
    google::{GoogleAuth, GoogleEndpoint, GoogleGenerateContent},
    http::{build_client, build_direct_client, validate_endpoint},
    mantle::Mantle,
    openai::{OpenAi, ResponsesAuth},
    openai_chat::{ChatCompletionsAuth, OpenAiChatCompletions},
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
        let endpoint_kind = recipe.endpoint.kind;
        let endpoint = recipe.endpoint.resolve(recipe.protocol)?;
        let client = if endpoint.scheme() == "http" {
            self.direct_http.clone()
        } else {
            self.http.clone()
        };

        match recipe.protocol {
            HttpProtocol::OpenAiResponses => {
                let auth = responses_auth(recipe.auth)?;
                Ok(Arc::new(OpenAi::with_client(
                    client,
                    endpoint,
                    auth,
                    recipe.headers,
                )?))
            }
            HttpProtocol::OpenAiChatCompletions => {
                let auth = chat_completions_auth(recipe.auth)?;
                Ok(Arc::new(OpenAiChatCompletions::with_client(
                    client,
                    endpoint,
                    auth,
                    recipe.headers,
                )?))
            }
            HttpProtocol::AnthropicMessages => {
                let auth = anthropic_auth(recipe.auth)?;
                Ok(Arc::new(AnthropicMessages::with_client(
                    client,
                    endpoint,
                    auth,
                    recipe.headers,
                )?))
            }
            HttpProtocol::GoogleGenerateContent => {
                let auth = google_auth(recipe.auth)?;
                let endpoint_kind = match endpoint_kind {
                    EndpointKind::Base => GoogleEndpoint::Base,
                    EndpointKind::Exact => GoogleEndpoint::Exact,
                };
                Ok(Arc::new(GoogleGenerateContent::with_client(
                    client,
                    endpoint,
                    endpoint_kind,
                    auth,
                    recipe.headers,
                )?))
            }
        }
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
        }
    }
}

fn responses_auth(auth: HttpAuth) -> Result<ResponsesAuth, ProviderError> {
    match auth {
        HttpAuth::NoAuth => Ok(ResponsesAuth::NoAuth),
        HttpAuth::ApiKey(secret) | HttpAuth::Bearer(secret) => Ok(ResponsesAuth::Bearer(secret)),
        HttpAuth::Header(name, secret) => Ok(ResponsesAuth::Header(name, secret)),
        HttpAuth::Codex {
            access_token,
            account_id,
            is_fedramp,
        } => Ok(ResponsesAuth::Codex {
            access_token,
            account_id,
            is_fedramp,
        }),
    }
}

fn chat_completions_auth(auth: HttpAuth) -> Result<ChatCompletionsAuth, ProviderError> {
    match auth {
        HttpAuth::NoAuth => Ok(ChatCompletionsAuth::NoAuth),
        HttpAuth::ApiKey(secret) | HttpAuth::Bearer(secret) => {
            Ok(ChatCompletionsAuth::Bearer(secret))
        }
        HttpAuth::Header(name, secret) => Ok(ChatCompletionsAuth::Header(name, secret)),
        HttpAuth::Codex { .. } => Err(ProviderError::Configuration(
            "Codex authentication requires the OpenAI Responses protocol".to_owned(),
        )),
    }
}

fn anthropic_auth(auth: HttpAuth) -> Result<AnthropicAuth, ProviderError> {
    match auth {
        HttpAuth::NoAuth => Ok(AnthropicAuth::NoAuth),
        HttpAuth::ApiKey(secret) => Ok(AnthropicAuth::XApiKey(secret)),
        HttpAuth::Bearer(secret) => Ok(AnthropicAuth::Bearer(secret)),
        HttpAuth::Header(name, secret) => Ok(AnthropicAuth::Header(name, secret)),
        HttpAuth::Codex { .. } => Err(ProviderError::Configuration(
            "Codex authentication requires the OpenAI Responses protocol".to_owned(),
        )),
    }
}

fn google_auth(auth: HttpAuth) -> Result<GoogleAuth, ProviderError> {
    match auth {
        HttpAuth::NoAuth => Ok(GoogleAuth::NoAuth),
        HttpAuth::ApiKey(secret) => Ok(GoogleAuth::XGoogApiKey(secret)),
        HttpAuth::Bearer(secret) => Ok(GoogleAuth::Bearer(secret)),
        HttpAuth::Header(name, secret) => Ok(GoogleAuth::Header(name, secret)),
        HttpAuth::Codex { .. } => Err(ProviderError::Configuration(
            "Codex authentication requires the OpenAI Responses protocol".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        thread::{self, JoinHandle},
        time::Duration,
    };

    use futures_util::StreamExt;

    use crate::{Message, ModelRequest, ProviderEvent};

    use super::*;

    #[tokio::test]
    async fn compiles_a_base_endpoint_into_a_streaming_provider() {
        let (base_url, server) = serve_once(concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"fast\"}\n\n",
            "data: {\"type\":\"response.completed\"}\n\n",
        ));
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
        let request = server.join().unwrap();
        let head = request.split_once("\r\n\r\n").unwrap().0;
        assert_eq!(head.lines().next(), Some("POST /v1/responses HTTP/1.1"));
        assert_eq!(
            request_header(head, "authorization"),
            Some("Bearer test-secret")
        );
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

    fn serve_once(body: &str) -> (String, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let body = body.to_owned();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let request = read_request(&mut stream);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8(request).unwrap()
        });
        (base_url, server)
    }

    fn read_request(stream: &mut TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut buffer = [0; 4_096];
        loop {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                continue;
            };
            let body_start = header_end + 4;
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or_default();
            if request.len() >= body_start + content_length {
                break;
            }
        }
        request
    }

    fn request_header<'a>(headers: &'a str, expected_name: &str) -> Option<&'a str> {
        headers
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case(expected_name))
            .map(|(_, value)| value.trim())
    }
}
