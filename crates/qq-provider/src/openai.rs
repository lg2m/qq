//! OpenAI Responses API adapter.

use std::{collections::HashMap, fmt, sync::Arc};

use async_stream::try_stream;
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ContentBlock, ModelRequest, Provider, ProviderError, ProviderErrorKind, ProviderEvent,
    ProviderStream, ProviderUsage, Role, ToolSpec,
    http::{
        ExchangeMessages, ExchangeOutcome, HttpExchange, HttpRejection, SafeHeaders, build_client,
        build_direct_client, is_request_controlled_header, transport_error, validate_endpoint,
    },
    limits::{ByteCounter, StreamLimits},
    request_auth::RequestAuthorizer,
    sanitize::sanitize_message,
    sse::{SseDecoder, Utf8ErrorMessage},
};

const RESPONSES_ENDPOINT: &str = "https://api.openai.com/v1/responses";

/// Authentication applied by an OpenAI-compatible Responses client.
#[derive(Clone, PartialEq, Eq)]
pub enum ResponsesAuth {
    NoAuth,
    Bearer(String),
    Header(String, String),
    Codex {
        access_token: String,
        account_id: String,
        is_fedramp: bool,
    },
}

impl fmt::Debug for ResponsesAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAuth => formatter.write_str("NoAuth"),
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

#[derive(Clone, Copy)]
enum ResponsesRequestKind {
    Standard,
    Codex,
}

/// A client for OpenAI-compatible Responses endpoints.
pub struct OpenAi {
    exchange: HttpExchange,
    endpoint: reqwest::Url,
    headers: HeaderMap,
    request_kind: ResponsesRequestKind,
}

impl OpenAi {
    /// Creates a client for OpenAI's standard Responses endpoint.
    pub fn new(api_key: &str) -> Result<Self, ProviderError> {
        Self::with_endpoint(
            RESPONSES_ENDPOINT,
            ResponsesAuth::Bearer(api_key.to_owned()),
            [],
            false,
        )
    }

    /// Creates a client for an exact OpenAI-compatible Responses endpoint URL.
    ///
    /// Plain HTTP is accepted only when `allow_http` is true and the URL host is
    /// loopback. Header names and values are validated while constructing the client.
    pub fn with_endpoint(
        exact_endpoint: &str,
        auth: ResponsesAuth,
        static_headers: impl IntoIterator<Item = (String, String)>,
        allow_http: bool,
    ) -> Result<Self, ProviderError> {
        let endpoint = validate_endpoint(exact_endpoint, allow_http)?;
        let client = if endpoint.scheme() == "http" {
            build_direct_client()?
        } else {
            build_client()?
        };
        Self::with_client(client, endpoint, auth, static_headers)
    }

    pub(crate) fn with_client(
        client: reqwest::Client,
        endpoint: reqwest::Url,
        auth: ResponsesAuth,
        static_headers: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, ProviderError> {
        let codex_request_shape = matches!(&auth, ResponsesAuth::Codex { .. });
        Self::with_client_and_authorizer(
            client,
            endpoint,
            auth,
            static_headers,
            RequestAuthorizer::default(),
            codex_request_shape,
        )
    }

    pub(crate) fn with_client_and_authorizer(
        client: reqwest::Client,
        endpoint: reqwest::Url,
        auth: ResponsesAuth,
        static_headers: impl IntoIterator<Item = (String, String)>,
        authorizer: RequestAuthorizer,
        codex_request_shape: bool,
    ) -> Result<Self, ProviderError> {
        // Codex rejects standard Responses fields such as max_output_tokens.
        // Request-time Codex auth keeps secrets out of ResponsesAuth, so the
        // compiler must pass the body shape explicitly.
        let request_kind = if codex_request_shape || matches!(&auth, ResponsesAuth::Codex { .. }) {
            ResponsesRequestKind::Codex
        } else {
            ResponsesRequestKind::Standard
        };
        let (headers, redactions) = build_headers(auth, static_headers)?;

        Ok(Self {
            exchange: HttpExchange::new(client, authorizer, Arc::from(redactions)),
            endpoint,
            headers,
            request_kind,
        })
    }
}

impl Provider for OpenAi {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let exchange = self.exchange.clone();
        let endpoint = self.endpoint.clone();
        let headers = self.headers.clone();
        let request_kind = self.request_kind;
        Box::pin(try_stream! {
            let limits = StreamLimits::new(request.max_output_tokens());
            let body = ResponsesRequest::new(&request, request_kind);
            let wire_request = exchange
                .request(reqwest::Method::POST, endpoint)
                .headers(headers)
                .header(ACCEPT, "text/event-stream")
                .json(&body)
                .build()
                .map_err(|error| transport_error(error, exchange.static_redactions()))?;
            let outcome = exchange
                .execute(
                    wire_request,
                    limits.wire,
                    ExchangeMessages {
                        wire_overflow: "OpenAI wire size overflowed",
                        wire_limit: "OpenAI stream exceeded the configured wire size limit",
                    },
                )
                .await?;
            let response = match outcome {
                ExchangeOutcome::Success(response) => response,
                ExchangeOutcome::Rejected(rejection) => Err(api_error(rejection))?,
            };

            // ChatGPT Codex streams valid SSE frames but often omits
            // Content-Type entirely. Standard OpenAI Responses still requires
            // text/event-stream so a JSON success body cannot be misread.
            if !accepts_responses_stream(response.headers(), request_kind) {
                Err(ProviderError::Protocol(
                    "OpenAI returned a non-SSE response".to_owned(),
                ))?;
            }

            let redactions = Arc::<[String]>::from(response.redactions());
            let mut chunks = response.into_body();
            let mut decoder = sse_decoder(limits.event);
            let mut output_bytes = ByteCounter::new(
                limits.output,
                "OpenAI output size overflowed",
                "OpenAI output exceeded the configured size limit",
            );
            // Maps streamed function-call item ids to call ids so argument
            // deltas and item completions can be attributed after the added
            // event; the call id is what round-trips into function_call_output.
            let mut tool_calls: HashMap<String, String> = HashMap::new();
            while let Some(chunk) = chunks.next().await {
                let chunk = chunk?;

                for event in decoder.push(&chunk)? {
                    let data = event.data;
                    if data == "[DONE]" || data.trim().is_empty() {
                        continue;
                    }

                    match decode_event(&data, redactions.as_ref())? {
                        DecodedEvent::OutputTextDelta(text) => {
                            output_bytes.add(text.len())?;
                            yield ProviderEvent::OutputTextDelta { text };
                        }
                        DecodedEvent::RefusalDelta(text) => {
                            output_bytes.add(text.len())?;
                            yield ProviderEvent::RefusalDelta { text };
                        }
                        DecodedEvent::ToolCallStarted { item_id, call_id, name } => {
                            if tool_calls.insert(item_id, call_id.clone()).is_some() {
                                Err(ProviderError::Protocol(
                                    "OpenAI-compatible stream reused a function-call item id"
                                        .to_owned(),
                                ))?;
                            }
                            yield ProviderEvent::ToolCallStarted { id: call_id, name };
                        }
                        DecodedEvent::ToolCallArguments { item_id, json } => {
                            match tool_calls.get(&item_id) {
                                Some(call_id) => {
                                    output_bytes.add(json.len())?;
                                    yield ProviderEvent::ToolCallArgumentsDelta {
                                        id: call_id.clone(),
                                        json,
                                    };
                                }
                                None => {
                                    Err(ProviderError::Protocol(
                                        "OpenAI-compatible stream sent arguments for an unknown function call"
                                            .to_owned(),
                                    ))?;
                                }
                            }
                        }
                        DecodedEvent::ToolCallDone { item_id } => {
                            if let Some(call_id) = tool_calls.remove(&item_id) {
                                yield ProviderEvent::ToolCallCompleted { id: call_id };
                            }
                        }
                        DecodedEvent::Completed(usage) => {
                            yield ProviderEvent::Completed { usage };
                            return;
                        }
                        DecodedEvent::Ignored => {}
                    }
                }
            }

            Err(ProviderError::Protocol(
                "OpenAI stream ended before response.completed".to_owned(),
            ))?;
        })
    }
}

fn build_headers(
    auth: ResponsesAuth,
    static_headers: impl IntoIterator<Item = (String, String)>,
) -> Result<(HeaderMap, Vec<String>), ProviderError> {
    let mut redactions = Vec::new();
    let codex_auth = matches!(&auth, ResponsesAuth::Codex { .. });
    let mut auth_headers = HeaderMap::new();
    match auth {
        ResponsesAuth::NoAuth => {}
        ResponsesAuth::Bearer(secret) => {
            if secret.is_empty() {
                return Err(ProviderError::Configuration(
                    "Bearer secret must not be empty".to_owned(),
                ));
            }
            let mut value = HeaderValue::from_str(&format!("Bearer {secret}")).map_err(|_| {
                ProviderError::Configuration(
                    "Bearer secret is not a valid HTTP header value".to_owned(),
                )
            })?;
            value.set_sensitive(true);
            redactions.push(secret);
            auth_headers.insert(AUTHORIZATION, value);
        }
        ResponsesAuth::Header(name, secret) => {
            if secret.is_empty() {
                return Err(ProviderError::Configuration(
                    "authentication header secret must not be empty".to_owned(),
                ));
            }
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                ProviderError::Configuration("authentication header name is invalid".to_owned())
            })?;
            if is_request_controlled_header(&name) {
                return Err(ProviderError::Configuration(
                    "authentication header is controlled by the HTTP client".to_owned(),
                ));
            }
            let mut value = HeaderValue::from_str(&secret).map_err(|_| {
                ProviderError::Configuration(
                    "authentication secret is not a valid HTTP header value".to_owned(),
                )
            })?;
            value.set_sensitive(true);
            redactions.push(secret);
            auth_headers.insert(name, value);
        }
        ResponsesAuth::Codex {
            access_token,
            account_id,
            is_fedramp,
        } => {
            if access_token.is_empty() {
                return Err(ProviderError::Configuration(
                    "Codex access token must not be empty".to_owned(),
                ));
            }
            if account_id.is_empty() {
                return Err(ProviderError::Configuration(
                    "Codex account ID must not be empty".to_owned(),
                ));
            }

            let mut authorization = HeaderValue::from_str(&format!("Bearer {access_token}"))
                .map_err(|_| {
                    ProviderError::Configuration(
                        "Codex access token is not a valid HTTP header value".to_owned(),
                    )
                })?;
            authorization.set_sensitive(true);
            let mut account_id_header = HeaderValue::from_str(&account_id).map_err(|_| {
                ProviderError::Configuration(
                    "Codex account ID is not a valid HTTP header value".to_owned(),
                )
            })?;
            account_id_header.set_sensitive(true);

            redactions.push(access_token);
            redactions.push(account_id);
            auth_headers.insert(AUTHORIZATION, authorization);
            auth_headers.insert(
                HeaderName::from_static("chatgpt-account-id"),
                account_id_header,
            );
            auth_headers.insert(
                HeaderName::from_static("originator"),
                HeaderValue::from_static("qq"),
            );
            if is_fedramp {
                auth_headers.insert(
                    HeaderName::from_static("x-openai-fedramp"),
                    HeaderValue::from_static("true"),
                );
            }
        }
    }

    let mut protocol_owned = vec![AUTHORIZATION];
    protocol_owned.extend(auth_headers.keys().cloned());
    if codex_auth {
        protocol_owned.extend([
            HeaderName::from_static("chatgpt-account-id"),
            HeaderName::from_static("originator"),
            HeaderName::from_static("x-openai-fedramp"),
        ]);
    }
    let mut headers = SafeHeaders::new(protocol_owned);
    headers.insert_configured(static_headers, true)?;
    headers.extend_owned(auth_headers);
    for redaction in redactions {
        headers.push_redaction(redaction);
    }
    Ok(headers.finish())
}

fn accepts_responses_stream(headers: &HeaderMap, request_kind: ResponsesRequestKind) -> bool {
    if crate::http::is_event_stream_headers(headers) {
        return true;
    }
    matches!(request_kind, ResponsesRequestKind::Codex)
        && headers.get(reqwest::header::CONTENT_TYPE).is_none()
}

fn sse_decoder(max_event_bytes: usize) -> SseDecoder {
    SseDecoder::data_only(
        max_event_bytes,
        "OpenAI event size overflowed",
        "OpenAI event exceeded the configured size limit",
        Utf8ErrorMessage::WithSource("OpenAI event was not UTF-8"),
    )
}

#[derive(Serialize)]
struct ResponsesRequest<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<&'a str>,
    input: Vec<InputItem<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ResponsesTool<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    stream: bool,
    store: bool,
}

impl<'a> ResponsesRequest<'a> {
    fn new(request: &'a ModelRequest, kind: ResponsesRequestKind) -> Self {
        // Each content block becomes its own Responses input item; a text
        // block keeps the plain message shape so tool-less requests stay
        // wire-identical.
        let mut input = Vec::new();
        for message in request.messages() {
            let role = match message.role() {
                Role::User => InputRole::User,
                Role::Assistant => InputRole::Assistant,
            };
            for block in message.content() {
                input.push(match block {
                    ContentBlock::Text { text } => InputItem::Message {
                        role,
                        content: text,
                    },
                    ContentBlock::ToolCall {
                        id,
                        name,
                        arguments,
                    } => InputItem::Function(FunctionItem::FunctionCall {
                        call_id: id,
                        name,
                        arguments: arguments.to_string(),
                    }),
                    ContentBlock::ToolResult {
                        call_id,
                        content,
                        is_error: _,
                    } => InputItem::Function(FunctionItem::FunctionCallOutput {
                        call_id,
                        output: content,
                    }),
                });
            }
        }

        Self {
            model: request.model(),
            instructions: request.system(),
            input,
            tools: request.tools().iter().map(ResponsesTool::from).collect(),
            max_output_tokens: matches!(kind, ResponsesRequestKind::Standard)
                .then(|| request.max_output_tokens()),
            stream: true,
            store: false,
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum InputItem<'a> {
    Message { role: InputRole, content: &'a str },
    Function(FunctionItem<'a>),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum FunctionItem<'a> {
    FunctionCall {
        call_id: &'a str,
        name: &'a str,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: &'a str,
        output: &'a str,
    },
}

#[derive(Serialize)]
struct ResponsesTool<'a> {
    #[serde(rename = "type")]
    tool_type: &'static str,
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
}

impl<'a> From<&'a ToolSpec> for ResponsesTool<'a> {
    fn from(tool: &'a ToolSpec) -> Self {
        Self {
            tool_type: "function",
            name: tool.name(),
            description: tool.description(),
            parameters: tool.input_schema(),
        }
    }
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum InputRole {
    User,
    Assistant,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum StreamingEvent {
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta { delta: String },
    #[serde(rename = "response.refusal.delta")]
    RefusalDelta { delta: String },
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded { item: OutputItem },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta { item_id: String, delta: String },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone { item: OutputItem },
    #[serde(rename = "response.completed")]
    Completed { response: Option<CompletedResponse> },
    #[serde(rename = "response.failed")]
    Failed { response: FailedResponse },
    #[serde(rename = "response.incomplete")]
    Incomplete { response: IncompleteResponse },
    #[serde(rename = "error")]
    Error {
        code: Option<String>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        error: Option<ApiError>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum OutputItem {
    #[serde(rename = "function_call")]
    FunctionCall {
        id: String,
        call_id: String,
        name: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct FailedResponse {
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct IncompleteResponse {
    incomplete_details: Option<IncompleteDetails>,
}

#[derive(Deserialize)]
struct IncompleteDetails {
    reason: Option<String>,
}

#[derive(Deserialize)]
struct CompletedResponse {
    usage: Option<ResponsesUsage>,
}

#[derive(Deserialize)]
struct ResponsesUsage {
    input_tokens: u64,
    output_tokens: u64,
    input_tokens_details: Option<ResponsesInputTokenDetails>,
}

#[derive(Deserialize)]
struct ResponsesInputTokenDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Deserialize)]
struct ApiErrorEnvelope {
    error: ApiError,
}

#[derive(Deserialize)]
struct ApiError {
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(rename = "type")]
    error_type: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum DecodedEvent {
    OutputTextDelta(String),
    RefusalDelta(String),
    ToolCallStarted {
        item_id: String,
        call_id: String,
        name: String,
    },
    ToolCallArguments {
        item_id: String,
        json: String,
    },
    ToolCallDone {
        item_id: String,
    },
    Completed(Option<ProviderUsage>),
    Ignored,
}

fn decode_event(data: &str, redactions: &[String]) -> Result<DecodedEvent, ProviderError> {
    let event: StreamingEvent = serde_json::from_str(data).map_err(|error| {
        // Include a short sanitized payload snippet so live Codex/OpenAI
        // decode failures remain diagnosable without logging the full stream.
        let snippet = truncate_chars(data.trim(), 240);
        ProviderError::Protocol(sanitize_message(
            &format!("could not decode OpenAI event: {error}; payload={snippet}"),
            redactions,
        ))
    })?;

    match event {
        StreamingEvent::OutputTextDelta { delta } => Ok(DecodedEvent::OutputTextDelta(delta)),
        StreamingEvent::RefusalDelta { delta } => Ok(DecodedEvent::RefusalDelta(delta)),
        StreamingEvent::OutputItemAdded {
            item: OutputItem::FunctionCall { id, call_id, name },
        } => Ok(DecodedEvent::ToolCallStarted {
            item_id: id,
            call_id,
            name,
        }),
        StreamingEvent::FunctionCallArgumentsDelta { item_id, delta } => {
            Ok(DecodedEvent::ToolCallArguments {
                item_id,
                json: delta,
            })
        }
        StreamingEvent::OutputItemDone {
            item: OutputItem::FunctionCall { id, .. },
        } => Ok(DecodedEvent::ToolCallDone { item_id: id }),
        StreamingEvent::OutputItemAdded {
            item: OutputItem::Other,
        }
        | StreamingEvent::OutputItemDone {
            item: OutputItem::Other,
        } => Ok(DecodedEvent::Ignored),
        StreamingEvent::Completed { response } => {
            let usage = response
                .and_then(|response| response.usage)
                .map(provider_usage)
                .transpose()?;
            Ok(DecodedEvent::Completed(usage))
        }
        StreamingEvent::Failed { response } => Err(response.error.map_or_else(
            || ProviderError::ResponseFailed {
                kind: ProviderErrorKind::Response,
                message: "OpenAI did not provide a reason".to_owned(),
            },
            |error| response_failed_from_api_error(error, redactions),
        )),
        StreamingEvent::Incomplete { response } => Err(ProviderError::ResponseIncomplete(
            response
                .incomplete_details
                .and_then(|details| details.reason)
                .map_or_else(
                    || "unknown reason".to_owned(),
                    |reason| sanitize_message(&reason, redactions),
                ),
        )),
        StreamingEvent::Error {
            code,
            message,
            error,
        } => Err(match error {
            Some(error) => response_failed_from_api_error(
                ApiError {
                    code: code.or(error.code),
                    message: message.or(error.message),
                    error_type: error.error_type,
                },
                redactions,
            ),
            None => ProviderError::ResponseFailed {
                kind: openai_error_kind(code.as_deref()),
                message: sanitize_message(
                    message
                        .as_deref()
                        .unwrap_or("OpenAI did not provide a reason"),
                    redactions,
                ),
            },
        }),
        StreamingEvent::Other => Ok(DecodedEvent::Ignored),
    }
}

fn response_failed_from_api_error(error: ApiError, redactions: &[String]) -> ProviderError {
    ProviderError::ResponseFailed {
        kind: openai_error_kind(error.code.as_deref().or(error.error_type.as_deref())),
        message: sanitize_message(
            error
                .message
                .as_deref()
                .unwrap_or("OpenAI did not provide a reason"),
            redactions,
        ),
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut truncated = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        truncated.push('…');
    }
    truncated
}

fn provider_usage(usage: ResponsesUsage) -> Result<ProviderUsage, ProviderError> {
    let cached = usage
        .input_tokens_details
        .map_or(0, |details| details.cached_tokens);
    let input_tokens = usage.input_tokens.checked_sub(cached).ok_or_else(|| {
        ProviderError::Protocol("OpenAI cached input tokens exceeded total input tokens".to_owned())
    })?;
    Ok(ProviderUsage {
        input_tokens,
        cache_read_input_tokens: cached,
        cache_write_input_tokens: 0,
        output_tokens: usage.output_tokens,
    })
}

fn openai_error_kind(code: Option<&str>) -> ProviderErrorKind {
    match code {
        Some("invalid_api_key" | "authentication_error") => ProviderErrorKind::Authentication,
        Some("rate_limit_exceeded" | "insufficient_quota") => ProviderErrorKind::RateLimited,
        Some("invalid_request_error" | "invalid_prompt") => ProviderErrorKind::InvalidRequest,
        Some("server_error" | "service_unavailable") => ProviderErrorKind::Unavailable,
        _ => ProviderErrorKind::Response,
    }
}

fn api_error(rejection: HttpRejection) -> ProviderError {
    let status = rejection.status();
    let fallback = status
        .canonical_reason()
        .unwrap_or("OpenAI request failed")
        .to_owned();
    let body = String::from_utf8_lossy(rejection.body());
    let message = serde_json::from_str::<ApiErrorEnvelope>(&body)
        .ok()
        .and_then(|envelope| envelope.error.message)
        .or_else(|| (!body.trim().is_empty()).then(|| body.into_owned()))
        .map_or(fallback, |message| {
            sanitize_message(&message, rejection.redactions())
        });

    ProviderError::Api {
        status: status.as_u16(),
        message,
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

    use serde_json::json;

    use super::*;
    use crate::Message;

    #[test]
    fn default_constructor_uses_openai_endpoint_and_redacts_auth_debug() {
        let provider = OpenAi::new("openai-test-secret").unwrap();
        let bearer = ResponsesAuth::Bearer("openai-test-secret".to_owned());
        let header = ResponsesAuth::Header("x-api-key".to_owned(), "custom-test-secret".to_owned());

        assert_eq!(provider.endpoint.as_str(), RESPONSES_ENDPOINT);
        assert!(!format!("{bearer:?}").contains("openai-test-secret"));
        assert!(!format!("{header:?}").contains("custom-test-secret"));
    }

    #[test]
    fn validates_http_endpoint_policy_and_url_components() {
        for (endpoint, allow_http) in [
            ("http://example.com/v1/responses", true),
            ("http://127.0.0.1/v1/responses", false),
            ("https://user:password@example.com/v1/responses", false),
            ("https://example.com/v1/responses#fragment", false),
        ] {
            let error = OpenAi::with_endpoint(endpoint, ResponsesAuth::NoAuth, [], allow_http)
                .err()
                .expect("endpoint must be rejected");
            assert!(matches!(error, ProviderError::Configuration(_)));
        }

        OpenAi::with_endpoint("http://[::1]/v1/responses", ResponsesAuth::NoAuth, [], true)
            .expect("IPv6 loopback HTTP should be accepted");
    }

    #[test]
    fn rejects_static_overrides_duplicate_headers_and_invalid_headers() {
        for name in [
            "authorization",
            "accept",
            "connection",
            "content-length",
            "content-type",
            "expect",
            "host",
            "http2-settings",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "proxy-connection",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
            "user-agent",
        ] {
            let error = OpenAi::with_endpoint(
                "https://example.com/v1/responses",
                ResponsesAuth::NoAuth,
                [(name.to_owned(), "value".to_owned())],
                false,
            )
            .err()
            .expect("controlled header must be rejected");
            assert!(matches!(error, ProviderError::Configuration(_)));
        }

        let error = OpenAi::with_endpoint(
            "https://example.com/v1/responses",
            ResponsesAuth::Header("x-api-key".to_owned(), "secret".to_owned()),
            [("x-api-key".to_owned(), "override".to_owned())],
            false,
        )
        .err()
        .expect("authentication header override must be rejected");
        assert!(matches!(error, ProviderError::Configuration(_)));

        for headers in [
            vec![
                ("x-test".to_owned(), "one".to_owned()),
                ("X-Test".to_owned(), "two".to_owned()),
            ],
            vec![("bad header".to_owned(), "value".to_owned())],
            vec![("x-test".to_owned(), "bad\r\nvalue".to_owned())],
        ] {
            let error = OpenAi::with_endpoint(
                "https://example.com/v1/responses",
                ResponsesAuth::NoAuth,
                headers,
                false,
            )
            .err()
            .expect("invalid or duplicate header must be rejected");
            assert!(matches!(error, ProviderError::Configuration(_)));
        }
    }

    #[test]
    fn preserves_nonempty_auth_secrets_without_trimming() {
        let (headers, redactions) = build_headers(
            ResponsesAuth::Header("x-api-key".to_owned(), " ".to_owned()),
            [],
        )
        .unwrap();

        assert_eq!(headers["x-api-key"], " ");
        assert_eq!(redactions, [" "]);
    }

    #[test]
    fn serializes_provider_request_as_responses_input() {
        let request = ModelRequest::new(
            "gpt-test",
            vec![Message::user("hello"), Message::assistant("hi")],
            512,
        );

        let body = serde_json::to_value(ResponsesRequest::new(
            &request,
            ResponsesRequestKind::Standard,
        ))
        .unwrap();

        assert_eq!(
            body,
            json!({
                "model": "gpt-test",
                "input": [
                    {"role": "user", "content": "hello"},
                    {"role": "assistant", "content": "hi"}
                ],
                "max_output_tokens": 512,
                "stream": true,
                "store": false
            })
        );
    }

    #[test]
    fn maps_the_system_prompt_to_the_instructions_field() {
        let request = ModelRequest::new("gpt-test", vec![Message::user("ping")], 64)
            .with_system("You are QQ.");
        let body = serde_json::to_value(ResponsesRequest::new(
            &request,
            ResponsesRequestKind::Standard,
        ))
        .unwrap();
        assert_eq!(body["instructions"], "You are QQ.");
        assert_eq!(body["input"][0]["content"], "ping");

        let without = ModelRequest::new("gpt-test", vec![Message::user("ping")], 64);
        let body = serde_json::to_value(ResponsesRequest::new(
            &without,
            ResponsesRequestKind::Standard,
        ))
        .unwrap();
        assert!(body.get("instructions").is_none());
    }

    #[tokio::test]
    async fn sends_tool_declarations_and_tool_history_items() {
        let body = "data: {\"type\":\"response.completed\"}\n\n";
        let (endpoint, server) = serve_once("/v1/responses", "200 OK", "text/event-stream", body);
        let provider = OpenAi::with_endpoint(&endpoint, ResponsesAuth::NoAuth, [], true).unwrap();
        let request = ModelRequest::new(
            "gpt-test",
            vec![
                Message::user("read the config"),
                Message::new(
                    Role::Assistant,
                    vec![
                        ContentBlock::Text {
                            text: "Reading it now.".to_owned(),
                        },
                        ContentBlock::ToolCall {
                            id: "call_1".to_owned(),
                            name: "read_file".to_owned(),
                            arguments: json!({"path": "config.ron"}),
                        },
                    ],
                ),
                Message::tool_results(vec![ContentBlock::ToolResult {
                    call_id: "call_1".to_owned(),
                    content: "(config)".to_owned(),
                    is_error: false,
                }]),
            ],
            128,
        )
        .with_tools(vec![ToolSpec::new(
            "read_file",
            "Reads one file",
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        )]);
        let events = provider.stream(request).collect::<Vec<_>>().await;

        assert!(matches!(
            &events[0],
            Ok(ProviderEvent::Completed { usage: None })
        ));

        let request = server.join().unwrap();
        let request_body = request.split_once("\r\n\r\n").unwrap().1;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(request_body).unwrap(),
            json!({
                "model": "gpt-test",
                "input": [
                    {"role": "user", "content": "read the config"},
                    {"role": "assistant", "content": "Reading it now."},
                    {"type": "function_call", "call_id": "call_1", "name": "read_file",
                     "arguments": "{\"path\":\"config.ron\"}"},
                    {"type": "function_call_output", "call_id": "call_1", "output": "(config)"}
                ],
                "tools": [
                    {"type": "function", "name": "read_file", "description": "Reads one file",
                     "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}
                ],
                "max_output_tokens": 128,
                "stream": true,
                "store": false
            })
        );
    }

    #[test]
    fn decodes_text_refusal_and_terminal_events() {
        let delta = decode_event(
            r#"{"type":"response.output_text.delta","delta":"hello"}"#,
            &[],
        )
        .unwrap();
        let refusal = decode_event(
            r#"{"type":"response.refusal.delta","delta":"cannot help"}"#,
            &[],
        )
        .unwrap();
        let completed = decode_event(
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":17,"input_tokens_details":{"cached_tokens":5},"output_tokens":9}}}"#,
            &[],
        )
        .unwrap();

        assert!(matches!(delta, DecodedEvent::OutputTextDelta(text) if text == "hello"));
        assert!(matches!(refusal, DecodedEvent::RefusalDelta(text) if text == "cannot help"));
        assert_eq!(
            completed,
            DecodedEvent::Completed(Some(ProviderUsage {
                input_tokens: 12,
                cache_read_input_tokens: 5,
                cache_write_input_tokens: 0,
                output_tokens: 9,
            }))
        );
    }

    #[test]
    fn rejects_invalid_responses_usage() {
        let error = decode_event(
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":2,"input_tokens_details":{"cached_tokens":3},"output_tokens":1}}}"#,
            &[],
        )
        .unwrap_err();

        assert!(matches!(error, ProviderError::Protocol(_)));
    }

    #[test]
    fn ignores_events_added_by_openai() {
        let event = decode_event(r#"{"type":"response.future_event","value":1}"#, &[]).unwrap();

        assert!(matches!(event, DecodedEvent::Ignored));
    }

    #[test]
    fn surfaces_stream_errors() {
        let error = decode_event(
            r#"{"type":"error","code":"rate_limit_exceeded","message":"rate limited"}"#,
            &[],
        )
        .unwrap_err();
        // Official OpenAI/Codex Responses error events nest the diagnostic
        // under `error` and may include sequence/param metadata.
        let nested = decode_event(
            r#"{"type":"error","sequence_number":3,"error":{"type":"invalid_request_error","code":"invalid_prompt","message":"boom","param":null}}"#,
            &[],
        )
        .unwrap_err();
        let missing_message =
            decode_event(r#"{"type":"error","code":"server_error"}"#, &[]).unwrap_err();
        let nested_failed = decode_event(
            r#"{"type":"response.failed","response":{"error":{"code":"server_error"}}}"#,
            &[],
        )
        .unwrap_err();
        let undecodable = decode_event(r#"{"type":"response.completed","response":[]}"#, &[])
            .unwrap_err()
            .to_string();

        assert_eq!(error.to_string(), "provider response failed: rate limited");
        assert_eq!(error.kind(), crate::ProviderErrorKind::RateLimited);
        assert_eq!(nested.to_string(), "provider response failed: boom");
        assert_eq!(nested.kind(), crate::ProviderErrorKind::InvalidRequest);
        assert_eq!(
            missing_message.to_string(),
            "provider response failed: OpenAI did not provide a reason"
        );
        assert_eq!(
            missing_message.kind(),
            crate::ProviderErrorKind::Unavailable
        );
        assert_eq!(
            nested_failed.to_string(),
            "provider response failed: OpenAI did not provide a reason"
        );
        assert_eq!(nested_failed.kind(), crate::ProviderErrorKind::Unavailable);
        assert!(undecodable.contains("could not decode OpenAI event:"));
        assert!(undecodable.contains("payload="));
        assert!(undecodable.contains("response.completed"));
    }

    #[test]
    fn classifies_failed_and_incomplete_responses() {
        let failed = decode_event(
            r#"{"type":"response.failed","response":{"error":{"message":"bad request"}}}"#,
            &[],
        )
        .unwrap_err();
        let incomplete = decode_event(
            r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#,
            &[],
        )
        .unwrap_err();

        assert!(matches!(failed, ProviderError::ResponseFailed { .. }));
        assert!(matches!(incomplete, ProviderError::ResponseIncomplete(_)));
    }

    #[test]
    fn decodes_fragmented_sse_without_unbounded_buffering() {
        let mut decoder = sse_decoder(1_024);

        assert!(
            decoder
                .push(b"data: {\"type\":\"response.")
                .unwrap()
                .is_empty()
        );
        let events = decoder.push(b"completed\"}\r\n\r\n").unwrap();

        assert_eq!(events[0].data, r#"{"type":"response.completed"}"#);
    }

    #[test]
    fn ignores_a_fragmented_utf8_bom() {
        let mut decoder = sse_decoder(1_024);

        assert!(decoder.push(b"\xef").unwrap().is_empty());
        assert!(decoder.push(b"\xbb").unwrap().is_empty());
        let events = decoder
            .push(b"\xbfdata: {\"type\":\"response.completed\"}\n\n")
            .unwrap();

        assert_eq!(events[0].data, r#"{"type":"response.completed"}"#);
    }

    #[test]
    fn rejects_an_oversized_sse_event_before_it_terminates() {
        let mut decoder = sse_decoder(8);

        let error = decoder.push(b"data: this event keeps going").unwrap_err();

        assert!(matches!(error, ProviderError::Protocol(_)));
    }

    #[tokio::test]
    async fn sends_a_responses_request_and_streams_events() {
        let body = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":17,\"input_tokens_details\":{\"cached_tokens\":5},\"output_tokens\":9}}}\n\n",
        );
        let path = "/custom/responses?api-version=42";
        let (endpoint, server) = serve_once(path, "200 OK", "text/event-stream", body);
        let provider = OpenAi::with_endpoint(
            &endpoint,
            ResponsesAuth::Header("x-api-key".to_owned(), "custom-test-secret".to_owned()),
            [("x-client".to_owned(), "qq-tests".to_owned())],
            true,
        )
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
            Ok(ProviderEvent::OutputTextDelta { text }) if text == "hello"
        ));
        assert_eq!(
            events[1].as_ref().unwrap(),
            &ProviderEvent::Completed {
                usage: Some(ProviderUsage {
                    input_tokens: 12,
                    cache_read_input_tokens: 5,
                    cache_write_input_tokens: 0,
                    output_tokens: 9,
                }),
            }
        );

        let request = server.join().unwrap();
        let (head, request_body) = request.split_once("\r\n\r\n").unwrap();
        assert_eq!(
            head.lines().next(),
            Some("POST /custom/responses?api-version=42 HTTP/1.1")
        );
        assert_eq!(request_header(head, "accept"), Some("text/event-stream"));
        assert_eq!(
            request_header(head, "x-api-key"),
            Some("custom-test-secret")
        );
        assert_eq!(request_header(head, "x-client"), Some("qq-tests"));
        assert_eq!(request_header(head, "authorization"), None);

        let request_body: serde_json::Value = serde_json::from_str(request_body).unwrap();
        assert_eq!(request_body["model"], "gpt-test");
        assert_eq!(request_body["input"][0]["content"], "ping");
        assert_eq!(request_body["store"], false);
    }

    #[tokio::test]
    async fn codex_auth_adds_backend_headers_and_omits_max_output_tokens() {
        let body = "data: {\"type\":\"response.completed\"}\n\n";
        let (endpoint, server) = serve_once(
            "/backend-api/codex/responses",
            "200 OK",
            "text/event-stream",
            body,
        );
        let provider = OpenAi::with_endpoint(
            &endpoint,
            ResponsesAuth::Codex {
                access_token: "codex-test-access-token".to_owned(),
                account_id: "workspace-test-id".to_owned(),
                is_fedramp: true,
            },
            [],
            true,
        )
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
        let request = server.join().unwrap();
        let (head, request_body) = request.split_once("\r\n\r\n").unwrap();
        assert_eq!(
            request_header(head, "authorization"),
            Some("Bearer codex-test-access-token")
        );
        assert_eq!(
            request_header(head, "chatgpt-account-id"),
            Some("workspace-test-id")
        );
        assert_eq!(request_header(head, "x-openai-fedramp"), Some("true"));
        assert_eq!(request_header(head, "originator"), Some("qq"));

        let request_body: serde_json::Value = serde_json::from_str(request_body).unwrap();
        assert_eq!(request_body["model"], "gpt-test");
        assert!(
            !request_body
                .as_object()
                .unwrap()
                .contains_key("max_output_tokens")
        );
    }

    #[tokio::test]
    async fn codex_accepts_sse_without_content_type() {
        // Live ChatGPT Codex responses stream SSE frames with no Content-Type.
        let body = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"pong\"}\n\n",
            "data: {\"type\":\"response.completed\"}\n\n",
        );
        let (endpoint, server) = serve_once("/backend-api/codex/responses", "200 OK", "", body);
        let provider = OpenAi::with_endpoint(
            &endpoint,
            ResponsesAuth::Codex {
                access_token: "codex-test-access-token".to_owned(),
                account_id: "workspace-test-id".to_owned(),
                is_fedramp: false,
            },
            [],
            true,
        )
        .unwrap();

        let events = provider
            .stream(ModelRequest::new(
                "gpt-5.4-mini",
                vec![Message::user("ping")],
                128,
            ))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect::<Vec<_>>();

        assert_eq!(
            events,
            vec![
                ProviderEvent::OutputTextDelta {
                    text: "pong".to_owned(),
                },
                ProviderEvent::Completed { usage: None },
            ]
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn standard_responses_still_require_event_stream_content_type() {
        let body = "data: {\"type\":\"response.completed\"}\n\n";
        let (endpoint, server) = serve_once("/v1/responses", "200 OK", "", body);
        let provider = OpenAi::with_endpoint(&endpoint, ResponsesAuth::NoAuth, [], true).unwrap();

        let events = provider
            .stream(ModelRequest::new(
                "gpt-test",
                vec![Message::user("ping")],
                128,
            ))
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(
            &events[..],
            [Err(ProviderError::Protocol(message))]
                if message == "OpenAI returned a non-SSE response"
        ));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn streams_tool_calls_with_attributed_arguments_to_completion() {
        let body = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,",
            "\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Checking.\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,",
            "\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":1,",
            "\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",",
            "\"name\":\"read_file\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",",
            "\"delta\":\"{\\\"path\\\":\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",",
            "\"delta\":\"\\\"a.rs\\\"}\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_1\",",
            "\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":1,",
            "\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",",
            "\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}\n\n",
            "data: {\"type\":\"response.completed\"}\n\n",
        );
        let (endpoint, server) = serve_once("/v1/responses", "200 OK", "text/event-stream", body);
        let provider = OpenAi::with_endpoint(&endpoint, ResponsesAuth::NoAuth, [], true).unwrap();
        let events = provider
            .stream(ModelRequest::new(
                "gpt-test",
                vec![Message::user("ping")],
                128,
            ))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect::<Vec<_>>();

        assert_eq!(
            events,
            vec![
                ProviderEvent::OutputTextDelta {
                    text: "Checking.".to_owned(),
                },
                ProviderEvent::ToolCallStarted {
                    id: "call_1".to_owned(),
                    name: "read_file".to_owned(),
                },
                ProviderEvent::ToolCallArgumentsDelta {
                    id: "call_1".to_owned(),
                    json: "{\"path\":".to_owned(),
                },
                ProviderEvent::ToolCallArgumentsDelta {
                    id: "call_1".to_owned(),
                    json: "\"a.rs\"}".to_owned(),
                },
                ProviderEvent::ToolCallCompleted {
                    id: "call_1".to_owned(),
                },
                ProviderEvent::Completed { usage: None },
            ]
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn rejects_tool_arguments_for_an_unknown_call() {
        let body = concat!(
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_9\",",
            "\"delta\":\"{}\"}\n\n",
        );
        let (endpoint, server) = serve_once("/v1/responses", "200 OK", "text/event-stream", body);
        let provider = OpenAi::with_endpoint(&endpoint, ResponsesAuth::NoAuth, [], true).unwrap();
        let error = provider
            .stream(ModelRequest::new(
                "gpt-test",
                vec![Message::user("ping")],
                128,
            ))
            .next()
            .await
            .unwrap()
            .unwrap_err();

        assert!(matches!(error, ProviderError::Protocol(_)));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn preserves_openai_api_errors() {
        let body = r#"{"error":{"message":"invalid key"}}"#;
        let (endpoint, server) = serve_once(
            "/v1/responses",
            "401 Unauthorized",
            "application/json",
            body,
        );
        let provider = OpenAi::with_endpoint(
            &endpoint,
            ResponsesAuth::Bearer("test-key".to_owned()),
            [],
            true,
        )
        .unwrap();
        let error = provider
            .stream(ModelRequest::new(
                "gpt-test",
                vec![Message::user("ping")],
                128,
            ))
            .next()
            .await
            .unwrap()
            .unwrap_err();

        assert!(matches!(
            error,
            ProviderError::Api {
                status: 401,
                ref message,
            } if message == "invalid key"
        ));
        assert_eq!(error.kind(), crate::ProviderErrorKind::Authentication);
        let request = server.join().unwrap();
        let head = request.split_once("\r\n\r\n").unwrap().0;
        assert_eq!(
            request_header(head, "authorization"),
            Some("Bearer test-key")
        );
    }

    #[tokio::test]
    async fn redacts_known_values_echoed_in_an_error_body() {
        let body = r#"{"error":{"message":"invalid auth-test-secret and tenant-test-secret"}}"#;
        let (endpoint, server) = serve_once(
            "/v1/responses",
            "401 Unauthorized",
            "application/json",
            body,
        );
        let provider = OpenAi::with_endpoint(
            &endpoint,
            ResponsesAuth::Bearer("auth-test-secret".to_owned()),
            [("x-tenant".to_owned(), "tenant-test-secret".to_owned())],
            true,
        )
        .unwrap();
        let error = provider
            .stream(ModelRequest::new(
                "gpt-test",
                vec![Message::user("ping")],
                128,
            ))
            .next()
            .await
            .unwrap()
            .unwrap_err();

        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("auth-test-secret"));
        assert!(!rendered.contains("tenant-test-secret"));
        assert!(rendered.contains("[REDACTED]"));
        server.join().unwrap();
    }

    fn serve_once(
        path: &str,
        status: &str,
        content_type: &str,
        body: &str,
    ) -> (String, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}{path}", listener.local_addr().unwrap());
        let status = status.to_owned();
        let content_type = content_type.to_owned();
        let body = body.to_owned();

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let request = read_request(&mut stream);
            // Empty content_type omits the header so Codex's live missing
            // Content-Type behavior can be regression-tested.
            let content_type_header = if content_type.is_empty() {
                String::new()
            } else {
                format!("Content-Type: {content_type}\r\n")
            };
            let response = format!(
                "HTTP/1.1 {status}\r\n{content_type_header}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8(request).unwrap()
        });

        (endpoint, server)
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
