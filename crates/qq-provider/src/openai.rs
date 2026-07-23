//! OpenAI Responses API adapter.

use std::{collections::HashMap, fmt, sync::Arc};

use async_stream::try_stream;
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ContentBlock, ModelRequest, Provider, ProviderError, ProviderErrorKind, ProviderEvent,
    ProviderStream, ProviderUsage, Role, ToolSpec,
    http::{build_client, build_direct_client, validate_endpoint},
    limits::StreamLimits,
    request_auth::RequestAuthorizer,
    sanitize::sanitize_message,
};

const RESPONSES_ENDPOINT: &str = "https://api.openai.com/v1/responses";
const ERROR_BODY_BYTES_LIMIT: usize = 16 * 1_024;

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
    client: reqwest::Client,
    endpoint: reqwest::Url,
    headers: HeaderMap,
    redactions: Arc<[String]>,
    request_kind: ResponsesRequestKind,
    authorizer: RequestAuthorizer,
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
        Self::with_client_and_authorizer(
            client,
            endpoint,
            auth,
            static_headers,
            RequestAuthorizer::default(),
        )
    }

    pub(crate) fn with_client_and_authorizer(
        client: reqwest::Client,
        endpoint: reqwest::Url,
        auth: ResponsesAuth,
        static_headers: impl IntoIterator<Item = (String, String)>,
        authorizer: RequestAuthorizer,
    ) -> Result<Self, ProviderError> {
        let request_kind = if matches!(&auth, ResponsesAuth::Codex { .. }) {
            ResponsesRequestKind::Codex
        } else {
            ResponsesRequestKind::Standard
        };
        let (headers, redactions) = build_headers(auth, static_headers)?;

        Ok(Self {
            client,
            endpoint,
            headers,
            redactions: Arc::from(redactions),
            request_kind,
            authorizer,
        })
    }
}

impl Provider for OpenAi {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let client = self.client.clone();
        let endpoint = self.endpoint.clone();
        let headers = self.headers.clone();
        let redactions = Arc::clone(&self.redactions);
        let request_kind = self.request_kind;
        let authorizer = self.authorizer.clone();
        Box::pin(try_stream! {
            let limits = StreamLimits::new(request.max_output_tokens());
            let body = ResponsesRequest::new(&request, request_kind);
            let mut wire_request = client
                .post(endpoint)
                .headers(headers)
                .header(ACCEPT, "text/event-stream")
                .json(&body)
                .build()
                .map_err(|error| transport_error(error, redactions.as_ref()))?;
            authorizer.authorize(&mut wire_request).await?;
            let response = client
                .execute(wire_request)
                .await
                .map_err(|error| transport_error(error, redactions.as_ref()))?;

            let response = if response.status().is_success() {
                response
            } else {
                Err(api_error(response, redactions.as_ref()).await)?
            };

            let is_event_stream = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| {
                    value
                        .split(';')
                        .next()
                        .is_some_and(|media_type| {
                            media_type.trim().eq_ignore_ascii_case("text/event-stream")
                        })
                });
            if !is_event_stream {
                Err(ProviderError::Protocol(
                    "OpenAI returned a non-SSE response".to_owned(),
                ))?;
            }

            let mut chunks = response.bytes_stream();
            let mut decoder = SseDecoder::new(limits.event);
            let mut output_bytes = 0;
            let mut wire_bytes = 0_usize;
            // Maps streamed function-call item ids to call ids so argument
            // deltas and item completions can be attributed after the added
            // event; the call id is what round-trips into function_call_output.
            let mut tool_calls: HashMap<String, String> = HashMap::new();
            while let Some(chunk) = chunks.next().await {
                let chunk = chunk
                    .map_err(|error| transport_error(error, redactions.as_ref()))?;
                wire_bytes = wire_bytes.checked_add(chunk.len()).ok_or_else(|| {
                    ProviderError::Protocol("OpenAI wire size overflowed".to_owned())
                })?;
                if wire_bytes > limits.wire {
                    Err(ProviderError::Protocol(
                        "OpenAI stream exceeded the configured wire size limit".to_owned(),
                    ))?;
                }

                for data in decoder.push(&chunk)? {
                    if data == "[DONE]" || data.trim().is_empty() {
                        continue;
                    }

                    match decode_event(&data, redactions.as_ref())? {
                        DecodedEvent::OutputTextDelta(text) => {
                            add_output_bytes(&mut output_bytes, text.len(), limits.output)?;
                            yield ProviderEvent::OutputTextDelta { text };
                        }
                        DecodedEvent::RefusalDelta(text) => {
                            add_output_bytes(&mut output_bytes, text.len(), limits.output)?;
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
                                    add_output_bytes(&mut output_bytes, json.len(), limits.output)?;
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

    let mut headers = HeaderMap::new();
    for (name, value) in static_headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            ProviderError::Configuration("static header name is invalid".to_owned())
        })?;
        if name == AUTHORIZATION
            || auth_headers.contains_key(&name)
            || (codex_auth
                && matches!(
                    name.as_str(),
                    "chatgpt-account-id" | "originator" | "x-openai-fedramp"
                ))
            || is_request_controlled_header(&name)
        {
            return Err(ProviderError::Configuration(format!(
                "static header `{name}` is controlled by the provider"
            )));
        }
        if headers.contains_key(&name) {
            return Err(ProviderError::Configuration(format!(
                "static header `{name}` is duplicated"
            )));
        }

        let mut header_value = HeaderValue::from_str(&value).map_err(|_| {
            ProviderError::Configuration("static header value is invalid".to_owned())
        })?;
        header_value.set_sensitive(true);
        if !value.is_empty() {
            redactions.push(value);
        }
        headers.insert(name, header_value);
    }

    headers.extend(auth_headers);

    redactions.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    redactions.dedup();
    Ok((headers, redactions))
}

fn is_request_controlled_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "accept"
            | "connection"
            | "content-length"
            | "content-type"
            | "expect"
            | "host"
            | "http2-settings"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

struct SseDecoder {
    bom_prefix: Vec<u8>,
    bom_checked: bool,
    line: Vec<u8>,
    data: Vec<u8>,
    event_bytes: usize,
    max_event_bytes: usize,
    skip_line_feed: bool,
}

impl SseDecoder {
    const fn new(max_event_bytes: usize) -> Self {
        Self {
            bom_prefix: Vec::new(),
            bom_checked: false,
            line: Vec::new(),
            data: Vec::new(),
            event_bytes: 0,
            max_event_bytes,
            skip_line_feed: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, ProviderError> {
        let mut events = Vec::new();

        for &byte in bytes {
            if !self.bom_checked {
                self.bom_prefix.push(byte);
                if b"\xef\xbb\xbf".starts_with(&self.bom_prefix) {
                    if self.bom_prefix.len() == 3 {
                        self.bom_prefix.clear();
                        self.bom_checked = true;
                    }
                    continue;
                }

                self.bom_checked = true;
                for prefix_byte in std::mem::take(&mut self.bom_prefix) {
                    self.push_byte(prefix_byte, &mut events)?;
                }
                continue;
            }

            self.push_byte(byte, &mut events)?;
        }

        Ok(events)
    }

    fn push_byte(&mut self, byte: u8, events: &mut Vec<String>) -> Result<(), ProviderError> {
        if self.skip_line_feed {
            self.skip_line_feed = false;
            if byte == b'\n' {
                return Ok(());
            }
        }

        match byte {
            b'\r' => {
                if let Some(data) = self.finish_line()? {
                    events.push(data);
                }
                self.skip_line_feed = true;
            }
            b'\n' => {
                if let Some(data) = self.finish_line()? {
                    events.push(data);
                }
            }
            _ => {
                self.event_bytes = self.event_bytes.checked_add(1).ok_or_else(|| {
                    ProviderError::Protocol("OpenAI event size overflowed".to_owned())
                })?;
                if self.event_bytes > self.max_event_bytes {
                    return Err(ProviderError::Protocol(
                        "OpenAI event exceeded the configured size limit".to_owned(),
                    ));
                }
                self.line.push(byte);
            }
        }

        Ok(())
    }

    fn finish_line(&mut self) -> Result<Option<String>, ProviderError> {
        if self.line.is_empty() {
            self.event_bytes = 0;
            if self.data.is_empty() {
                return Ok(None);
            }

            self.data.pop();
            let data = String::from_utf8(std::mem::take(&mut self.data)).map_err(|error| {
                ProviderError::Protocol(format!("OpenAI event was not UTF-8: {error}"))
            })?;
            return Ok(Some(data));
        }

        let line = std::mem::take(&mut self.line);
        if line.starts_with(b":") {
            return Ok(None);
        }

        let (field, value) = line.iter().position(|byte| *byte == b':').map_or_else(
            || (line.as_slice(), &[][..]),
            |colon| {
                let value = line[colon + 1..]
                    .strip_prefix(b" ")
                    .unwrap_or(&line[colon + 1..]);
                (&line[..colon], value)
            },
        );
        if field == b"data" {
            self.data.extend_from_slice(value);
            self.data.push(b'\n');
        }

        Ok(None)
    }
}

#[derive(Serialize)]
struct ResponsesRequest<'a> {
    model: &'a str,
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
        message: String,
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
    message: String,
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
        ProviderError::Protocol(sanitize_message(
            &format!("could not decode OpenAI event: {error}"),
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
            |error| ProviderError::ResponseFailed {
                kind: openai_error_kind(error.code.as_deref()),
                message: sanitize_message(&error.message, redactions),
            },
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
        StreamingEvent::Error { code, message } => Err(ProviderError::ResponseFailed {
            kind: openai_error_kind(code.as_deref()),
            message: sanitize_message(&message, redactions),
        }),
        StreamingEvent::Other => Ok(DecodedEvent::Ignored),
    }
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

fn add_output_bytes(
    current: &mut usize,
    additional: usize,
    limit: usize,
) -> Result<(), ProviderError> {
    *current = current
        .checked_add(additional)
        .ok_or_else(|| ProviderError::Protocol("OpenAI output size overflowed".to_owned()))?;
    if *current > limit {
        return Err(ProviderError::Protocol(
            "OpenAI output exceeded the configured size limit".to_owned(),
        ));
    }

    Ok(())
}

fn transport_error(error: reqwest::Error, redactions: &[String]) -> ProviderError {
    ProviderError::Transport(sanitize_message(
        &error.without_url().to_string(),
        redactions,
    ))
}

async fn api_error(response: reqwest::Response, redactions: &[String]) -> ProviderError {
    let status = response.status();
    let fallback = status
        .canonical_reason()
        .unwrap_or("OpenAI request failed")
        .to_owned();
    let body = read_error_body(response).await;
    let body = String::from_utf8_lossy(&body);
    let message = serde_json::from_str::<ApiErrorEnvelope>(&body)
        .map(|envelope| envelope.error.message)
        .ok()
        .or_else(|| (!body.trim().is_empty()).then(|| body.into_owned()))
        .map_or(fallback, |message| sanitize_message(&message, redactions));

    ProviderError::Api {
        status: status.as_u16(),
        message,
    }
}

async fn read_error_body(response: reqwest::Response) -> Vec<u8> {
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();

    while let Some(chunk) = chunks.next().await {
        let Ok(chunk) = chunk else {
            break;
        };
        let remaining = ERROR_BODY_BYTES_LIMIT.saturating_sub(body.len());
        if remaining == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if body.len() == ERROR_BODY_BYTES_LIMIT {
            break;
        }
    }

    body
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
            "host",
            "content-length",
            "connection",
            "transfer-encoding",
            "accept",
            "content-type",
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

        assert_eq!(error.to_string(), "provider response failed: rate limited");
        assert_eq!(error.kind(), crate::ProviderErrorKind::RateLimited);
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
        let mut decoder = SseDecoder::new(1_024);

        assert!(
            decoder
                .push(b"data: {\"type\":\"response.")
                .unwrap()
                .is_empty()
        );
        let events = decoder.push(b"completed\"}\r\n\r\n").unwrap();

        assert_eq!(events, [r#"{"type":"response.completed"}"#]);
    }

    #[test]
    fn ignores_a_fragmented_utf8_bom() {
        let mut decoder = SseDecoder::new(1_024);

        assert!(decoder.push(b"\xef").unwrap().is_empty());
        assert!(decoder.push(b"\xbb").unwrap().is_empty());
        let events = decoder
            .push(b"\xbfdata: {\"type\":\"response.completed\"}\n\n")
            .unwrap();

        assert_eq!(events, [r#"{"type":"response.completed"}"#]);
    }

    #[test]
    fn rejects_an_oversized_sse_event_before_it_terminates() {
        let mut decoder = SseDecoder::new(8);

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
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
