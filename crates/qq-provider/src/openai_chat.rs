//! OpenAI-compatible Chat Completions API adapter.

use std::{borrow::Cow, collections::BTreeMap, fmt, sync::Arc};

use async_stream::try_stream;
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ContentBlock, Message, ModelRequest, Provider, ProviderError, ProviderErrorKind, ProviderEvent,
    ProviderStream, Role, ToolSpec,
    http::{build_client, build_direct_client, validate_endpoint},
    limits::StreamLimits,
    request_auth::RequestAuthorizer,
    sanitize::sanitize_message,
};

const CHAT_COMPLETIONS_ENDPOINT: &str = "https://api.openai.com/v1/chat/completions";
const ERROR_BODY_BYTES_LIMIT: usize = 16 * 1_024;

/// Authentication applied by an OpenAI-compatible Chat Completions client.
#[derive(Clone, PartialEq, Eq)]
pub enum ChatCompletionsAuth {
    NoAuth,
    Bearer(String),
    Header(String, String),
}

impl fmt::Debug for ChatCompletionsAuth {
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
        }
    }
}

/// A client for OpenAI-compatible Chat Completions endpoints.
pub struct OpenAiChatCompletions {
    client: reqwest::Client,
    endpoint: reqwest::Url,
    headers: HeaderMap,
    redactions: Arc<[String]>,
    authorizer: RequestAuthorizer,
}

impl OpenAiChatCompletions {
    /// Creates a client for OpenAI's standard Chat Completions endpoint.
    pub fn new(api_key: &str) -> Result<Self, ProviderError> {
        Self::with_endpoint(
            CHAT_COMPLETIONS_ENDPOINT,
            ChatCompletionsAuth::Bearer(api_key.to_owned()),
            [],
            false,
        )
    }

    /// Creates a client for an exact OpenAI-compatible endpoint URL.
    ///
    /// Plain HTTP is accepted only when `allow_http` is true and the URL host is
    /// loopback. Header names and values are validated while constructing the client.
    pub fn with_endpoint(
        endpoint: &str,
        auth: ChatCompletionsAuth,
        static_headers: impl IntoIterator<Item = (String, String)>,
        allow_http: bool,
    ) -> Result<Self, ProviderError> {
        let endpoint = validate_endpoint(endpoint, allow_http)?;
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
        auth: ChatCompletionsAuth,
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
        auth: ChatCompletionsAuth,
        static_headers: impl IntoIterator<Item = (String, String)>,
        authorizer: RequestAuthorizer,
    ) -> Result<Self, ProviderError> {
        let (headers, redactions) = build_headers(auth, static_headers)?;

        Ok(Self {
            client,
            endpoint,
            headers,
            redactions: Arc::from(redactions),
            authorizer,
        })
    }
}

impl Provider for OpenAiChatCompletions {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let client = self.client.clone();
        let endpoint = self.endpoint.clone();
        let headers = self.headers.clone();
        let redactions = Arc::clone(&self.redactions);
        let authorizer = self.authorizer.clone();

        Box::pin(try_stream! {
            let limits = StreamLimits::new(request.max_output_tokens());
            let body = ChatCompletionsRequest::from(&request);
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

            if !is_event_stream(&response) {
                Err(ProviderError::Protocol(
                    "OpenAI-compatible provider returned a non-SSE response".to_owned(),
                ))?;
            }

            let mut chunks = response.bytes_stream();
            let mut decoder = SseDecoder::new(limits.event);
            let mut output_bytes = 0_usize;
            let mut wire_bytes = 0_usize;
            // Maps streamed tool-call array indexes to call ids so argument
            // fragments and the finish reason can be attributed after the
            // first fragment. Ordered so completion drains in index order.
            let mut tool_calls: BTreeMap<u64, String> = BTreeMap::new();

            while let Some(chunk) = chunks.next().await {
                let chunk = chunk
                    .map_err(|error| transport_error(error, redactions.as_ref()))?;
                add_wire_bytes(&mut wire_bytes, chunk.len(), limits.wire)?;

                for data in decoder.push(&chunk)? {
                    let data = data.trim();
                    if data.is_empty() {
                        continue;
                    }
                    if data == "[DONE]" {
                        yield ProviderEvent::Completed;
                        return;
                    }

                    for delta in decode_event(data, redactions.as_ref())? {
                        match delta {
                            DecodedDelta::OutputText(text) => {
                                add_output_bytes(
                                    &mut output_bytes,
                                    text.len(),
                                    limits.output,
                                )?;
                                yield ProviderEvent::OutputTextDelta { text };
                            }
                            DecodedDelta::Refusal(text) => {
                                add_output_bytes(
                                    &mut output_bytes,
                                    text.len(),
                                    limits.output,
                                )?;
                                yield ProviderEvent::RefusalDelta { text };
                            }
                            DecodedDelta::ToolCallStarted { index, id, name } => {
                                if tool_calls.insert(index, id.clone()).is_some() {
                                    Err(ProviderError::Protocol(
                                        "OpenAI-compatible stream reused a tool-call index"
                                            .to_owned(),
                                    ))?;
                                }
                                yield ProviderEvent::ToolCallStarted { id, name };
                            }
                            DecodedDelta::ToolCallArguments { index, json } => {
                                match tool_calls.get(&index) {
                                    Some(id) => {
                                        add_output_bytes(
                                            &mut output_bytes,
                                            json.len(),
                                            limits.output,
                                        )?;
                                        yield ProviderEvent::ToolCallArgumentsDelta {
                                            id: id.clone(),
                                            json,
                                        };
                                    }
                                    None => {
                                        Err(ProviderError::Protocol(
                                            "OpenAI-compatible stream sent arguments for an unknown tool call"
                                                .to_owned(),
                                        ))?;
                                    }
                                }
                            }
                            DecodedDelta::ToolCallsFinished => {
                                for (_, id) in std::mem::take(&mut tool_calls) {
                                    yield ProviderEvent::ToolCallCompleted { id };
                                }
                            }
                            DecodedDelta::TerminalError(error) => Err(error)?,
                        }
                    }
                }
            }

            Err(ProviderError::Protocol(
                "OpenAI-compatible stream ended before [DONE]".to_owned(),
            ))?;
        })
    }
}

fn build_headers(
    auth: ChatCompletionsAuth,
    static_headers: impl IntoIterator<Item = (String, String)>,
) -> Result<(HeaderMap, Vec<String>), ProviderError> {
    let mut redactions = Vec::new();
    let auth_header = match auth {
        ChatCompletionsAuth::NoAuth => None,
        ChatCompletionsAuth::Bearer(secret) => {
            if secret.trim().is_empty() {
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
            Some((AUTHORIZATION, value))
        }
        ChatCompletionsAuth::Header(name, secret) => {
            if secret.trim().is_empty() {
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
            Some((name, value))
        }
    };
    let auth_name = auth_header.as_ref().map(|(name, _)| name);

    let mut headers = HeaderMap::new();
    for (name, value) in static_headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            ProviderError::Configuration("static header name is invalid".to_owned())
        })?;
        if name == AUTHORIZATION
            || auth_name.is_some_and(|auth_name| auth_name == name)
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
        if !value.trim().is_empty() {
            redactions.push(value);
        }
        headers.insert(name, header_value);
    }

    if let Some((name, value)) = auth_header {
        headers.insert(name, value);
    }

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

fn is_event_stream(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(';').next().is_some_and(|media_type| {
                media_type.trim().eq_ignore_ascii_case("text/event-stream")
            })
        })
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
                    ProviderError::Protocol(
                        "OpenAI-compatible SSE event size overflowed".to_owned(),
                    )
                })?;
                if self.event_bytes > self.max_event_bytes {
                    return Err(ProviderError::Protocol(
                        "OpenAI-compatible SSE event exceeded the configured size limit".to_owned(),
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
                ProviderError::Protocol(format!(
                    "OpenAI-compatible SSE event was not UTF-8: {error}"
                ))
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
struct ChatCompletionsRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ChatTool<'a>>,
    stream: bool,
    max_tokens: u32,
}

impl<'a> From<&'a ModelRequest> for ChatCompletionsRequest<'a> {
    fn from(request: &'a ModelRequest) -> Self {
        let mut messages = Vec::with_capacity(request.messages().len());
        for message in request.messages() {
            append_chat_messages(message, &mut messages);
        }
        Self {
            model: request.model(),
            messages,
            tools: request.tools().iter().map(ChatTool::from).collect(),
            stream: true,
            max_tokens: request.max_output_tokens(),
        }
    }
}

/// Appends the wire messages for one provider-neutral message.
///
/// Chat Completions has no multi-result message, so each `ToolResult` block
/// becomes its own `tool`-role message; `is_error` has no wire field because
/// the runtime frames errors inside the result content. Any remaining text and
/// tool calls follow as one message so a lone text block keeps its legacy
/// plain-string shape.
fn append_chat_messages<'a>(message: &'a Message, messages: &mut Vec<ChatMessage<'a>>) {
    let role = match message.role() {
        Role::User => ChatRole::User,
        Role::Assistant => ChatRole::Assistant,
    };
    if let [ContentBlock::Text { text }] = message.content() {
        messages.push(ChatMessage {
            role,
            content: Some(Cow::Borrowed(text.as_str())),
            tool_calls: None,
            tool_call_id: None,
        });
        return;
    }

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut wrote_results = false;
    for block in message.content() {
        match block {
            ContentBlock::Text { text: fragment } => text.push_str(fragment),
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => tool_calls.push(ChatToolCall::Function {
                id,
                function: ChatFunctionCall {
                    name,
                    arguments: arguments.to_string(),
                },
            }),
            ContentBlock::ToolResult {
                call_id,
                content,
                is_error: _,
            } => {
                wrote_results = true;
                messages.push(ChatMessage {
                    role: ChatRole::Tool,
                    content: Some(Cow::Borrowed(content.as_str())),
                    tool_calls: None,
                    tool_call_id: Some(call_id),
                });
            }
        }
    }

    if !text.is_empty() || !tool_calls.is_empty() || !wrote_results {
        let content = if text.is_empty() && !tool_calls.is_empty() {
            None
        } else {
            Some(Cow::Owned(text))
        };
        messages.push(ChatMessage {
            role,
            content,
            tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            tool_call_id: None,
        });
    }
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: ChatRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<Cow<'a, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ChatToolCall<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ChatToolCall<'a> {
    Function {
        id: &'a str,
        function: ChatFunctionCall<'a>,
    },
}

#[derive(Serialize)]
struct ChatFunctionCall<'a> {
    name: &'a str,
    /// Chat Completions carries tool arguments as a JSON-encoded string.
    arguments: String,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ChatTool<'a> {
    Function { function: ChatFunction<'a> },
}

impl<'a> From<&'a ToolSpec> for ChatTool<'a> {
    fn from(tool: &'a ToolSpec) -> Self {
        Self::Function {
            function: ChatFunction {
                name: tool.name(),
                description: tool.description(),
                parameters: tool.input_schema(),
            },
        }
    }
}

#[derive(Serialize)]
struct ChatFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum ChatRole {
    User,
    Assistant,
    Tool,
}

#[derive(Deserialize)]
struct ChatCompletionChunk {
    #[serde(default)]
    choices: Vec<ChatChoice>,
    error: Option<WireApiError>,
}

#[derive(Deserialize)]
struct ChatChoice {
    #[serde(default)]
    delta: ChatDelta,
    finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct ChatDelta {
    content: Option<String>,
    refusal: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ChatToolCallDelta>,
}

#[derive(Deserialize)]
struct ChatToolCallDelta {
    index: u64,
    id: Option<String>,
    #[serde(default)]
    function: ChatFunctionDelta,
}

#[derive(Default, Deserialize)]
struct ChatFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct ApiErrorEnvelope {
    error: WireApiError,
}

#[derive(Deserialize)]
struct WireApiError {
    message: Option<String>,
    code: Option<Value>,
    #[serde(rename = "type")]
    error_type: Option<String>,
    status: Option<Value>,
}

#[derive(Debug)]
enum DecodedDelta {
    OutputText(String),
    Refusal(String),
    ToolCallStarted {
        index: u64,
        id: String,
        name: String,
    },
    ToolCallArguments {
        index: u64,
        json: String,
    },
    ToolCallsFinished,
    TerminalError(ProviderError),
}

fn decode_event(data: &str, redactions: &[String]) -> Result<Vec<DecodedDelta>, ProviderError> {
    let chunk: ChatCompletionChunk = serde_json::from_str(data).map_err(|error| {
        ProviderError::Protocol(sanitize_message(
            &format!("could not decode OpenAI-compatible event: {error}"),
            redactions,
        ))
    })?;

    if let Some(error) = chunk.error {
        return Err(wire_api_error(error, redactions));
    }

    let mut deltas = Vec::new();
    for choice in chunk.choices {
        if let Some(text) = choice.delta.content.filter(|text| !text.is_empty()) {
            deltas.push(DecodedDelta::OutputText(text));
        }
        if let Some(text) = choice.delta.refusal.filter(|text| !text.is_empty()) {
            deltas.push(DecodedDelta::Refusal(text));
        }
        for tool_call in choice.delta.tool_calls {
            // The first fragment of a call carries `id` and `function.name`;
            // later fragments carry only `function.arguments`.
            if let Some(id) = tool_call.id {
                let Some(name) = tool_call.function.name else {
                    return Err(ProviderError::Protocol(
                        "OpenAI-compatible stream started a tool call without a name".to_owned(),
                    ));
                };
                deltas.push(DecodedDelta::ToolCallStarted {
                    index: tool_call.index,
                    id,
                    name,
                });
            }
            if let Some(json) = tool_call.function.arguments.filter(|json| !json.is_empty()) {
                deltas.push(DecodedDelta::ToolCallArguments {
                    index: tool_call.index,
                    json,
                });
            }
        }
        if let Some(reason) = choice.finish_reason {
            let error = match reason.as_str() {
                "stop" => continue,
                "tool_calls" => {
                    deltas.push(DecodedDelta::ToolCallsFinished);
                    continue;
                }
                "length" => ProviderError::ResponseIncomplete(
                    "OpenAI-compatible response reached its output token limit".to_owned(),
                ),
                "content_filter" => ProviderError::ResponseIncomplete(
                    "OpenAI-compatible response was stopped by a content filter".to_owned(),
                ),
                "function_call" => ProviderError::Protocol(
                    "OpenAI-compatible response requested legacy function execution".to_owned(),
                ),
                _ => ProviderError::Protocol(
                    "OpenAI-compatible response used an unsupported finish reason".to_owned(),
                ),
            };
            deltas.push(DecodedDelta::TerminalError(error));
        }
    }
    Ok(deltas)
}

fn wire_api_error(error: WireApiError, redactions: &[String]) -> ProviderError {
    let kind = wire_error_kind(&error);
    let message = error.message.as_deref().map_or_else(
        || "OpenAI-compatible provider did not provide an error message".to_owned(),
        |message| sanitize_message(message, redactions),
    );
    ProviderError::ResponseFailed { kind, message }
}

fn wire_error_kind(error: &WireApiError) -> ProviderErrorKind {
    if let Some(status) = error.status.as_ref().and_then(value_as_status) {
        return status_error_kind(status);
    }
    if let Some(status) = error.code.as_ref().and_then(value_as_status) {
        return status_error_kind(status);
    }

    for name in [
        error.code.as_ref().and_then(Value::as_str),
        error.error_type.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let kind = named_error_kind(name);
        if kind != ProviderErrorKind::Response {
            return kind;
        }
    }

    ProviderErrorKind::Response
}

fn value_as_status(value: &Value) -> Option<u16> {
    value
        .as_u64()
        .and_then(|status| u16::try_from(status).ok())
        .or_else(|| value.as_str()?.parse().ok())
}

fn status_error_kind(status: u16) -> ProviderErrorKind {
    match status {
        400 | 404 | 409 | 422 => ProviderErrorKind::InvalidRequest,
        401 | 403 => ProviderErrorKind::Authentication,
        429 => ProviderErrorKind::RateLimited,
        500..=599 => ProviderErrorKind::Unavailable,
        _ => ProviderErrorKind::Response,
    }
}

fn named_error_kind(name: &str) -> ProviderErrorKind {
    match name.to_ascii_lowercase().as_str() {
        "invalid_api_key" | "authentication_error" | "permission_error" => {
            ProviderErrorKind::Authentication
        }
        "rate_limit_exceeded" | "insufficient_quota" => ProviderErrorKind::RateLimited,
        "context_length_exceeded"
        | "invalid_prompt"
        | "invalid_request_error"
        | "invalid_value"
        | "model_not_found"
        | "unsupported_value" => ProviderErrorKind::InvalidRequest,
        "server_error" | "service_unavailable" => ProviderErrorKind::Unavailable,
        _ => ProviderErrorKind::Response,
    }
}

fn add_output_bytes(
    current: &mut usize,
    additional: usize,
    limit: usize,
) -> Result<(), ProviderError> {
    *current = current.checked_add(additional).ok_or_else(|| {
        ProviderError::Protocol("OpenAI-compatible output size overflowed".to_owned())
    })?;
    if *current > limit {
        return Err(ProviderError::Protocol(
            "OpenAI-compatible output exceeded the configured size limit".to_owned(),
        ));
    }
    Ok(())
}

fn add_wire_bytes(
    current: &mut usize,
    additional: usize,
    limit: usize,
) -> Result<(), ProviderError> {
    *current = current.checked_add(additional).ok_or_else(|| {
        ProviderError::Protocol("OpenAI-compatible wire size overflowed".to_owned())
    })?;
    if *current > limit {
        return Err(ProviderError::Protocol(
            "OpenAI-compatible stream exceeded the configured wire size limit".to_owned(),
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
        .unwrap_or("OpenAI-compatible request failed")
        .to_owned();
    let body = read_error_body(response).await;
    let body_text = String::from_utf8_lossy(&body);
    let message = serde_json::from_slice::<ApiErrorEnvelope>(&body)
        .ok()
        .and_then(|envelope| envelope.error.message)
        .or_else(|| (!body_text.trim().is_empty()).then(|| body_text.into_owned()))
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

    #[test]
    fn default_constructor_uses_openai_endpoint_and_redacts_auth_debug() {
        let provider = OpenAiChatCompletions::new("openai-test-secret").unwrap();
        let auth = ChatCompletionsAuth::Bearer("openai-test-secret".to_owned());

        assert_eq!(provider.endpoint.as_str(), CHAT_COMPLETIONS_ENDPOINT);
        assert!(!format!("{auth:?}").contains("openai-test-secret"));
    }

    #[test]
    fn validates_http_endpoint_policy_and_url_components() {
        for (endpoint, allow_http) in [
            ("http://example.com/v1/chat/completions", true),
            ("http://127.0.0.1/v1/chat/completions", false),
            (
                "https://user:password@example.com/v1/chat/completions",
                false,
            ),
            ("https://example.com/v1/chat/completions#fragment", false),
        ] {
            let error = OpenAiChatCompletions::with_endpoint(
                endpoint,
                ChatCompletionsAuth::NoAuth,
                [],
                allow_http,
            )
            .err()
            .expect("endpoint must be rejected");
            assert!(matches!(error, ProviderError::Configuration(_)));
        }

        OpenAiChatCompletions::with_endpoint(
            "http://[::1]/v1/chat/completions",
            ChatCompletionsAuth::NoAuth,
            [],
            true,
        )
        .expect("IPv6 loopback HTTP should be accepted");
    }

    #[test]
    fn rejects_static_overrides_and_invalid_headers() {
        for name in [
            "authorization",
            "host",
            "content-length",
            "connection",
            "transfer-encoding",
            "accept",
            "content-type",
        ] {
            let error = OpenAiChatCompletions::with_endpoint(
                "https://example.com/v1/chat/completions",
                ChatCompletionsAuth::NoAuth,
                [(name.to_owned(), "value".to_owned())],
                false,
            )
            .err()
            .expect("controlled header must be rejected");
            assert!(matches!(error, ProviderError::Configuration(_)));
        }

        let error = OpenAiChatCompletions::with_endpoint(
            "https://example.com/v1/chat/completions",
            ChatCompletionsAuth::Header("x-api-key".to_owned(), "secret".to_owned()),
            [("x-api-key".to_owned(), "override".to_owned())],
            false,
        )
        .err()
        .expect("authentication header override must be rejected");
        assert!(matches!(error, ProviderError::Configuration(_)));

        for headers in [
            vec![("bad header".to_owned(), "value".to_owned())],
            vec![("x-test".to_owned(), "bad\r\nvalue".to_owned())],
        ] {
            let error = OpenAiChatCompletions::with_endpoint(
                "https://example.com/v1/chat/completions",
                ChatCompletionsAuth::NoAuth,
                headers,
                false,
            )
            .err()
            .expect("invalid header must be rejected");
            assert!(matches!(error, ProviderError::Configuration(_)));
        }
    }

    #[test]
    fn decodes_fragmented_bom_comments_line_endings_and_multiline_data() {
        let source = concat!(
            "\u{feff}: comment\r\n",
            "data: {\"choices\":[{\"delta\":\r",
            "data: {\"content\":\"hello\"}}]}\n\r",
            "data: [DONE]\r\r",
        );
        let mut decoder = SseDecoder::new(1_024);
        let mut events = Vec::new();

        for byte in source.as_bytes() {
            events.extend(decoder.push(std::slice::from_ref(byte)).unwrap());
        }

        assert_eq!(
            events,
            [
                "{\"choices\":[{\"delta\":\n{\"content\":\"hello\"}}]}",
                "[DONE]",
            ]
        );
    }

    #[test]
    fn classifies_top_level_stream_errors() {
        let rate_limit_error = decode_event(
            r#"{"error":{"message":"slow down","code":"rate_limit_exceeded"}}"#,
            &[],
        )
        .unwrap_err();
        let authentication_error = decode_event(
            r#"{"error":{"message":"bad key","code":"provider_specific","type":"authentication_error"}}"#,
            &[],
        )
        .unwrap_err();

        assert!(matches!(
            rate_limit_error,
            ProviderError::ResponseFailed { .. }
        ));
        assert_eq!(rate_limit_error.kind(), ProviderErrorKind::RateLimited);
        assert_eq!(
            authentication_error.kind(),
            ProviderErrorKind::Authentication
        );
    }

    #[test]
    fn rejects_incomplete_and_unsupported_finish_reasons() {
        let length = decode_event(
            r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#,
            &[],
        )
        .unwrap()
        .pop()
        .expect("length must produce a terminal outcome");
        let function_call = decode_event(
            r#"{"choices":[{"delta":{},"finish_reason":"function_call"}]}"#,
            &[],
        )
        .unwrap()
        .pop()
        .expect("function call must produce a terminal outcome");
        let tool_calls = decode_event(
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            &[],
        )
        .unwrap()
        .pop()
        .expect("tool calls must finish the open calls");

        assert!(matches!(
            length,
            DecodedDelta::TerminalError(ProviderError::ResponseIncomplete(_))
        ));
        assert!(matches!(
            function_call,
            DecodedDelta::TerminalError(ProviderError::Protocol(_))
        ));
        assert!(matches!(tool_calls, DecodedDelta::ToolCallsFinished));
    }

    #[test]
    fn enforces_event_output_and_wire_limits() {
        let event_error = SseDecoder::new(8)
            .push(b"data: this event keeps going")
            .unwrap_err();
        let output_error = add_output_bytes(&mut 3, 2, 4).unwrap_err();
        let wire_error = add_wire_bytes(&mut 7, 2, 8).unwrap_err();

        assert!(matches!(event_error, ProviderError::Protocol(_)));
        assert!(matches!(output_error, ProviderError::Protocol(_)));
        assert!(matches!(wire_error, ProviderError::Protocol(_)));
    }

    #[tokio::test]
    async fn sends_exact_request_and_streams_fragmented_deltas() {
        let chunks = vec![
            b"\xef".to_vec(),
            b"\xbb".to_vec(),
            b"\xbf: heartbeat\r".to_vec(),
            b"\ndata: {\"choices\":[{\"delta\":\r\n".to_vec(),
            b"data: {\"content\":\"h\xc3".to_vec(),
            b"\xa9l\"}}]}\r\n\r".to_vec(),
            b"\ndata: {\"choices\":[{\"delta\":{\"refusal\":\"cannot\"}}]}\n\n".to_vec(),
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\r\r".to_vec(),
            b"data: [DO".to_vec(),
            b"NE]\r\n\r\n".to_vec(),
        ];
        let path = "/custom/chat/completions?api-version=42";
        let (endpoint, server) =
            serve_once(path, "200 OK", "text/event-stream; charset=utf-8", chunks);
        let provider = OpenAiChatCompletions::with_endpoint(
            &endpoint,
            ChatCompletionsAuth::Header("x-api-key".to_owned(), "custom-test-secret".to_owned()),
            [("x-client".to_owned(), "qq-tests".to_owned())],
            true,
        )
        .unwrap();
        let events = provider
            .stream(ModelRequest::new(
                "chat-test",
                vec![Message::user("ping"), Message::assistant("pong")],
                321,
            ))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[0],
            Ok(ProviderEvent::OutputTextDelta { text }) if text == "hél"
        ));
        assert!(matches!(
            &events[1],
            Ok(ProviderEvent::RefusalDelta { text }) if text == "cannot"
        ));
        assert!(matches!(&events[2], Ok(ProviderEvent::Completed)));

        let request = String::from_utf8(server.join().unwrap()).unwrap();
        let (head, body) = request.split_once("\r\n\r\n").unwrap();
        assert_eq!(
            head.lines().next(),
            Some("POST /custom/chat/completions?api-version=42 HTTP/1.1")
        );
        assert_eq!(request_header(head, "accept"), Some("text/event-stream"));
        assert_eq!(
            request_header(head, "content-type"),
            Some("application/json")
        );
        assert_eq!(
            request_header(head, "x-api-key"),
            Some("custom-test-secret")
        );
        assert_eq!(request_header(head, "x-client"), Some("qq-tests"));
        assert_eq!(request_header(head, "authorization"), None);
        assert_eq!(
            serde_json::from_str::<Value>(body).unwrap(),
            json!({
                "model": "chat-test",
                "messages": [
                    {"role": "user", "content": "ping"},
                    {"role": "assistant", "content": "pong"}
                ],
                "stream": true,
                "max_tokens": 321
            })
        );
    }

    #[tokio::test]
    async fn sends_tool_declarations_and_tool_history_messages() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (endpoint, server) = serve_once(
            "/v1/chat/completions",
            "200 OK",
            "text/event-stream",
            vec![body.as_bytes().to_vec()],
        );
        let provider =
            OpenAiChatCompletions::with_endpoint(&endpoint, ChatCompletionsAuth::NoAuth, [], true)
                .unwrap();
        let request = ModelRequest::new(
            "chat-test",
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
                Message::tool_results(vec![
                    ContentBlock::ToolResult {
                        call_id: "call_1".to_owned(),
                        content: "(config)".to_owned(),
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        call_id: "call_2".to_owned(),
                        content: "not found".to_owned(),
                        is_error: true,
                    },
                ]),
            ],
            128,
        )
        .with_tools(vec![ToolSpec::new(
            "read_file",
            "Reads one file",
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        )]);
        let events = provider.stream(request).collect::<Vec<_>>().await;

        assert!(matches!(&events[0], Ok(ProviderEvent::Completed)));

        let request = String::from_utf8(server.join().unwrap()).unwrap();
        let body = request.split_once("\r\n\r\n").unwrap().1;
        assert_eq!(
            serde_json::from_str::<Value>(body).unwrap(),
            json!({
                "model": "chat-test",
                "messages": [
                    {"role": "user", "content": "read the config"},
                    {"role": "assistant", "content": "Reading it now.",
                     "tool_calls": [
                        {"id": "call_1", "type": "function",
                         "function": {"name": "read_file",
                                      "arguments": "{\"path\":\"config.ron\"}"}}
                     ]},
                    {"role": "tool", "tool_call_id": "call_1", "content": "(config)"},
                    {"role": "tool", "tool_call_id": "call_2", "content": "not found"}
                ],
                "tools": [
                    {"type": "function",
                     "function": {"name": "read_file", "description": "Reads one file",
                                  "parameters": {"type": "object",
                                                 "properties": {"path": {"type": "string"}}}}}
                ],
                "stream": true,
                "max_tokens": 128
            })
        );
    }

    #[tokio::test]
    async fn streams_tool_calls_with_attributed_arguments_to_completion() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Checking.\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"a.rs\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (endpoint, server) = serve_once(
            "/v1/chat/completions",
            "200 OK",
            "text/event-stream",
            vec![body.as_bytes().to_vec()],
        );
        let provider =
            OpenAiChatCompletions::with_endpoint(&endpoint, ChatCompletionsAuth::NoAuth, [], true)
                .unwrap();
        let events = provider
            .stream(test_request())
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
                ProviderEvent::Completed,
            ]
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn rejects_tool_arguments_for_an_unknown_call() {
        let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":4,\"function\":{\"arguments\":\"{}\"}}]}}]}\n\n";
        let (endpoint, server) = serve_once(
            "/v1/chat/completions",
            "200 OK",
            "text/event-stream",
            vec![body.as_bytes().to_vec()],
        );
        let provider =
            OpenAiChatCompletions::with_endpoint(&endpoint, ChatCompletionsAuth::NoAuth, [], true)
                .unwrap();
        let error = provider
            .stream(test_request())
            .next()
            .await
            .unwrap()
            .unwrap_err();

        assert!(matches!(error, ProviderError::Protocol(_)));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn returns_typed_401_without_exposing_secrets() {
        let body = br#"{"error":{"message":"invalid test-api-secret\ncredential","type":"authentication_error"}}"#;
        let (endpoint, server) = serve_once(
            "/v1/chat/completions",
            "401 Unauthorized",
            "application/json",
            vec![body.to_vec()],
        );
        let provider = OpenAiChatCompletions::with_endpoint(
            &endpoint,
            ChatCompletionsAuth::Bearer("test-api-secret".to_owned()),
            [],
            true,
        )
        .unwrap();
        let error = provider
            .stream(test_request())
            .next()
            .await
            .unwrap()
            .unwrap_err();

        assert_eq!(error.kind(), ProviderErrorKind::Authentication);
        assert!(matches!(error, ProviderError::Api { status: 401, .. }));
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("test-api-secret"));
        assert!(!rendered.contains('\n'));

        let request = String::from_utf8(server.join().unwrap()).unwrap();
        let head = request.split_once("\r\n\r\n").unwrap().0;
        assert_eq!(
            request_header(head, "authorization"),
            Some("Bearer test-api-secret")
        );
    }

    #[tokio::test]
    async fn rejects_non_sse_success_responses() {
        let (endpoint, server) = serve_once(
            "/v1/chat/completions",
            "200 OK",
            "application/json",
            vec![b"{}".to_vec()],
        );
        let provider =
            OpenAiChatCompletions::with_endpoint(&endpoint, ChatCompletionsAuth::NoAuth, [], true)
                .unwrap();
        let error = provider
            .stream(test_request())
            .next()
            .await
            .unwrap()
            .unwrap_err();

        assert!(matches!(error, ProviderError::Protocol(_)));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn reports_a_stream_that_ends_before_done() {
        let body = b"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
        let (endpoint, server) = serve_once(
            "/v1/chat/completions",
            "200 OK",
            "text/event-stream",
            vec![body.to_vec()],
        );
        let provider =
            OpenAiChatCompletions::with_endpoint(&endpoint, ChatCompletionsAuth::NoAuth, [], true)
                .unwrap();
        let events = provider.stream(test_request()).collect::<Vec<_>>().await;

        assert!(matches!(
            &events[0],
            Ok(ProviderEvent::OutputTextDelta { text }) if text == "partial"
        ));
        assert!(matches!(&events[1], Err(ProviderError::Protocol(_))));
        server.join().unwrap();
    }

    fn test_request() -> ModelRequest {
        ModelRequest::new("chat-test", vec![Message::user("ping")], 128)
    }

    fn serve_once(
        path: &str,
        status: &str,
        content_type: &str,
        chunks: Vec<Vec<u8>>,
    ) -> (String, JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}{path}", listener.local_addr().unwrap());
        let status = status.to_owned();
        let content_type = content_type.to_owned();
        let content_length = chunks.iter().map(Vec::len).sum::<usize>();

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let request = read_request(&mut stream);
            let headers = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(headers.as_bytes()).unwrap();
            for chunk in chunks {
                stream.write_all(&chunk).unwrap();
                stream.flush().unwrap();
                thread::sleep(Duration::from_millis(1));
            }
            request
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
