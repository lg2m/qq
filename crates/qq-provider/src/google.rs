//! Google Gemini GenerateContent API adapter.

use std::{collections::BTreeMap, fmt, sync::Arc};

use async_stream::try_stream;
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    ContentBlock, Message, ModelRequest, Provider, ProviderError, ProviderErrorKind, ProviderEvent,
    ProviderStream, ProviderUsage, Role, ToolSpec,
    http::{build_client, validate_endpoint},
    limits::StreamLimits,
    sanitize::sanitize_message,
};

const GENERATIVE_AI_ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta";
const X_GOOG_API_KEY: HeaderName = HeaderName::from_static("x-goog-api-key");
const ERROR_BODY_BYTES_LIMIT: usize = 16 * 1_024;

/// Authentication applied by a Google GenerateContent-compatible client.
#[derive(Clone, PartialEq, Eq)]
pub enum GoogleAuth {
    NoAuth,
    XGoogApiKey(String),
    Bearer(String),
    Header(String, String),
}

impl fmt::Debug for GoogleAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAuth => formatter.write_str("NoAuth"),
            Self::XGoogApiKey(_) => formatter
                .debug_tuple("XGoogApiKey")
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
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum GoogleEndpoint {
    Base,
    Exact,
}

/// A client for Google GenerateContent-compatible endpoints.
pub struct GoogleGenerateContent {
    client: reqwest::Client,
    endpoint: reqwest::Url,
    endpoint_kind: GoogleEndpoint,
    headers: HeaderMap,
    redactions: Arc<[String]>,
}

impl GoogleGenerateContent {
    /// Creates a client for Google's Gemini Developer API.
    pub fn new(api_key: &str) -> Result<Self, ProviderError> {
        let endpoint = validate_endpoint(GENERATIVE_AI_ENDPOINT, false)?;
        Self::with_client(
            build_client()?,
            endpoint,
            GoogleEndpoint::Base,
            GoogleAuth::XGoogApiKey(api_key.to_owned()),
            [],
        )
    }

    pub(crate) fn with_client(
        client: reqwest::Client,
        endpoint: reqwest::Url,
        endpoint_kind: GoogleEndpoint,
        auth: GoogleAuth,
        static_headers: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, ProviderError> {
        let (headers, redactions) = build_headers(auth, static_headers)?;
        Ok(Self {
            client,
            endpoint,
            endpoint_kind,
            headers,
            redactions: Arc::from(redactions),
        })
    }

    fn request_endpoint(&self, model: &str) -> Result<reqwest::Url, ProviderError> {
        if matches!(self.endpoint_kind, GoogleEndpoint::Exact) {
            return Ok(self.endpoint.clone());
        }

        let model = model.strip_prefix("models/").unwrap_or(model);
        if model.is_empty()
            || model.contains('/')
            || model.contains(['?', '#', '\\'])
            || model == "."
            || model == ".."
        {
            return Err(ProviderError::Configuration(
                "Google model identifier must be one non-empty URL path segment".to_owned(),
            ));
        }

        let mut endpoint = self.endpoint.clone();
        if endpoint.query().is_some() {
            return Err(ProviderError::Configuration(
                "Google base endpoint URL must not contain a query".to_owned(),
            ));
        }
        endpoint
            .path_segments_mut()
            .map_err(|()| {
                ProviderError::Configuration(
                    "Google base endpoint URL cannot contain protocol paths".to_owned(),
                )
            })?
            .pop_if_empty()
            .push("models")
            .push(&format!("{model}:streamGenerateContent"));
        endpoint.query_pairs_mut().append_pair("alt", "sse");
        Ok(endpoint)
    }
}

impl Provider for GoogleGenerateContent {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let client = self.client.clone();
        let endpoint = self.request_endpoint(request.model());
        let headers = self.headers.clone();
        let redactions = Arc::clone(&self.redactions);

        Box::pin(try_stream! {
            let endpoint = endpoint?;
            let max_output_tokens = i32::try_from(request.max_output_tokens()).map_err(|_| {
                ProviderError::Configuration(
                    "Google max output tokens must not exceed 2147483647".to_owned(),
                )
            })?;
            let limits = StreamLimits::new(request.max_output_tokens());
            let body = GenerateContentRequest::new(&request, max_output_tokens)?;
            let response = client
                .post(endpoint)
                .headers(headers)
                .header(ACCEPT, "text/event-stream")
                .json(&body)
                .send()
                .await
                .map_err(|error| transport_error(error, redactions.as_ref()))?;

            let response = if response.status().is_success() {
                response
            } else {
                Err(api_error(response, redactions.as_ref()).await)?
            };
            if !is_event_stream(&response) {
                Err(ProviderError::Protocol(
                    "Google GenerateContent provider returned a non-SSE response".to_owned(),
                ))?;
            }

            let mut chunks = response.bytes_stream();
            let mut decoder = SseDecoder::new(limits.event);
            let mut output_bytes = 0_usize;
            let mut wire_bytes = 0_usize;
            let mut usage = None;
            // Gemini assigns no tool-call ids; a per-stream ordinal keeps the
            // synthesized ids deterministic.
            let mut tool_call_ordinal = 0_u64;

            while let Some(chunk) = chunks.next().await {
                let chunk = chunk
                    .map_err(|error| transport_error(error, redactions.as_ref()))?;
                add_wire_bytes(&mut wire_bytes, chunk.len(), limits.wire)?;

                for data in decoder.push(&chunk)? {
                    for event in decode_event(&data, &mut tool_call_ordinal, redactions.as_ref())? {
                        match event {
                            DecodedEvent::OutputText(text) => {
                                add_output_bytes(&mut output_bytes, text.len(), limits.output)?;
                                yield ProviderEvent::OutputTextDelta { text };
                            }
                            DecodedEvent::Usage(event_usage) => {
                                if usage.replace(event_usage).is_some() {
                                    Err(ProviderError::Protocol(
                                        "Google GenerateContent stream reported usage more than once".to_owned(),
                                    ))?;
                                }
                            }
                            DecodedEvent::ToolCall { id, name, arguments } => {
                                add_output_bytes(&mut output_bytes, arguments.len(), limits.output)?;
                                yield ProviderEvent::ToolCallStarted {
                                    id: id.clone(),
                                    name,
                                };
                                yield ProviderEvent::ToolCallArgumentsDelta {
                                    id: id.clone(),
                                    json: arguments,
                                };
                                yield ProviderEvent::ToolCallCompleted { id };
                            }
                            DecodedEvent::Completed => {
                                yield ProviderEvent::Completed { usage };
                                return;
                            }
                        }
                    }
                }
            }

            Err(ProviderError::Protocol(
                "Google GenerateContent stream ended before a terminal finish reason".to_owned(),
            ))?;
        })
    }
}

fn build_headers(
    auth: GoogleAuth,
    static_headers: impl IntoIterator<Item = (String, String)>,
) -> Result<(HeaderMap, Vec<String>), ProviderError> {
    let mut redactions = Vec::new();
    let auth_header = match auth {
        GoogleAuth::NoAuth => None,
        GoogleAuth::XGoogApiKey(secret) => {
            let value = sensitive_secret_header(&secret, "x-goog-api-key")?;
            redactions.push(secret);
            Some((X_GOOG_API_KEY, value))
        }
        GoogleAuth::Bearer(secret) => {
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
        GoogleAuth::Header(name, secret) => {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                ProviderError::Configuration("authentication header name is invalid".to_owned())
            })?;
            if name == X_GOOG_API_KEY || is_request_controlled_header(&name) {
                return Err(ProviderError::Configuration(
                    "authentication header is controlled by the provider".to_owned(),
                ));
            }
            let value = sensitive_secret_header(&secret, "authentication header")?;
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
            || name == X_GOOG_API_KEY
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

fn sensitive_secret_header(secret: &str, name: &str) -> Result<HeaderValue, ProviderError> {
    if secret.trim().is_empty() {
        return Err(ProviderError::Configuration(format!(
            "{name} secret must not be empty"
        )));
    }
    let mut value = HeaderValue::from_str(secret).map_err(|_| {
        ProviderError::Configuration(format!("{name} secret is not a valid HTTP header value"))
    })?;
    value.set_sensitive(true);
    Ok(value)
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
            | "user-agent"
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
                        "Google GenerateContent SSE event size overflowed".to_owned(),
                    )
                })?;
                if self.event_bytes > self.max_event_bytes {
                    return Err(ProviderError::Protocol(
                        "Google GenerateContent SSE event exceeded the configured size limit"
                            .to_owned(),
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
            return String::from_utf8(std::mem::take(&mut self.data))
                .map(Some)
                .map_err(|_| {
                    ProviderError::Protocol(
                        "Google GenerateContent SSE event data was not UTF-8".to_owned(),
                    )
                });
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
#[serde(rename_all = "camelCase")]
struct GenerateContentRequest<'a> {
    contents: Vec<GoogleContent<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<GoogleTool<'a>>,
    generation_config: GenerationConfig,
}

impl<'a> GenerateContentRequest<'a> {
    /// Builds the wire request, resolving each tool result back to the name of
    /// the call it answers. Gemini identifies function responses by name, so a
    /// result whose `call_id` matches no earlier tool call cannot be sent.
    fn new(request: &'a ModelRequest, max_output_tokens: i32) -> Result<Self, ProviderError> {
        let messages = request.messages();
        let mut contents = Vec::with_capacity(messages.len());
        for (index, message) in messages.iter().enumerate() {
            let mut parts = Vec::with_capacity(message.content().len());
            for block in message.content() {
                parts.push(match block {
                    ContentBlock::Text { text } => GooglePart::Text { text },
                    ContentBlock::ToolCall {
                        name, arguments, ..
                    } => GooglePart::FunctionCall {
                        function_call: FunctionCallPart {
                            name,
                            args: arguments,
                        },
                    },
                    ContentBlock::ToolResult {
                        call_id,
                        content,
                        is_error,
                    } => {
                        let name = messages[..index]
                            .iter()
                            .flat_map(Message::content)
                            .find_map(|earlier| match earlier {
                                ContentBlock::ToolCall { id, name, .. } if id == call_id => {
                                    Some(name.as_str())
                                }
                                _ => None,
                            })
                            .ok_or_else(|| {
                                ProviderError::Configuration(format!(
                                    "Google tool result `{call_id}` does not match any earlier tool call"
                                ))
                            })?;
                        GooglePart::FunctionResponse {
                            function_response: FunctionResponsePart {
                                name,
                                response: if *is_error {
                                    FunctionResponseBody::Error { error: content }
                                } else {
                                    FunctionResponseBody::Output { output: content }
                                },
                            },
                        }
                    }
                });
            }
            contents.push(GoogleContent {
                role: match message.role() {
                    Role::User => GoogleRole::User,
                    Role::Assistant => GoogleRole::Model,
                },
                parts,
            });
        }

        let tools = if request.tools().is_empty() {
            Vec::new()
        } else {
            vec![GoogleTool {
                function_declarations: request
                    .tools()
                    .iter()
                    .map(FunctionDeclaration::from)
                    .collect(),
            }]
        };

        Ok(Self {
            contents,
            tools,
            generation_config: GenerationConfig { max_output_tokens },
        })
    }
}

#[derive(Serialize)]
struct GoogleContent<'a> {
    role: GoogleRole,
    parts: Vec<GooglePart<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum GoogleRole {
    User,
    Model,
}

#[derive(Serialize)]
#[serde(untagged)]
enum GooglePart<'a> {
    Text {
        text: &'a str,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: FunctionCallPart<'a>,
    },
    FunctionResponse {
        #[serde(rename = "functionResponse")]
        function_response: FunctionResponsePart<'a>,
    },
}

#[derive(Serialize)]
struct FunctionCallPart<'a> {
    name: &'a str,
    args: &'a Value,
}

#[derive(Serialize)]
struct FunctionResponsePart<'a> {
    name: &'a str,
    response: FunctionResponseBody<'a>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum FunctionResponseBody<'a> {
    Output { output: &'a str },
    Error { error: &'a str },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GoogleTool<'a> {
    function_declarations: Vec<FunctionDeclaration<'a>>,
}

#[derive(Serialize)]
struct FunctionDeclaration<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
}

impl<'a> From<&'a ToolSpec> for FunctionDeclaration<'a> {
    fn from(tool: &'a ToolSpec) -> Self {
        Self {
            name: tool.name(),
            description: tool.description(),
            parameters: tool.input_schema(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerationConfig {
    max_output_tokens: i32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateContentResponse {
    #[serde(default)]
    candidates: Vec<Candidate>,
    prompt_feedback: Option<PromptFeedback>,
    error: Option<WireApiError>,
    usage_metadata: Option<UsageMetadata>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageMetadata {
    prompt_token_count: u64,
    candidates_token_count: u64,
    #[serde(default)]
    cached_content_token_count: u64,
    #[serde(default)]
    thoughts_token_count: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Candidate {
    content: Option<ResponseContent>,
    finish_reason: Option<String>,
    finish_message: Option<String>,
    index: Option<u32>,
}

#[derive(Deserialize)]
struct ResponseContent {
    #[serde(default)]
    parts: Vec<ResponsePart>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResponsePart {
    text: Option<String>,
    #[serde(default)]
    thought: bool,
    function_call: Option<WireFunctionCall>,
    executable_code: Option<Value>,
    code_execution_result: Option<Value>,
    inline_data: Option<Value>,
    file_data: Option<Value>,
    thought_signature: Option<String>,
    #[serde(flatten)]
    unknown: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct WireFunctionCall {
    name: String,
    args: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptFeedback {
    block_reason: Option<String>,
}

#[derive(Deserialize)]
struct ApiErrorEnvelope {
    error: WireApiError,
}

#[derive(Deserialize)]
struct WireApiError {
    code: Option<u16>,
    message: Option<String>,
    status: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum DecodedEvent {
    OutputText(String),
    Usage(ProviderUsage),
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    Completed,
}

fn decode_event(
    data: &str,
    tool_call_ordinal: &mut u64,
    redactions: &[String],
) -> Result<Vec<DecodedEvent>, ProviderError> {
    let response: GenerateContentResponse = serde_json::from_str(data).map_err(|error| {
        ProviderError::Protocol(sanitize_message(
            &format!("could not decode Google GenerateContent event: {error}"),
            redactions,
        ))
    })?;
    if let Some(error) = response.error {
        return Err(wire_api_error(error, redactions));
    }
    if let Some(reason) = response
        .prompt_feedback
        .and_then(|feedback| feedback.block_reason)
    {
        return Err(ProviderError::ResponseFailed {
            kind: ProviderErrorKind::Response,
            message: sanitize_message(
                &format!("Google blocked the prompt with reason {reason}"),
                redactions,
            ),
        });
    }

    let usage = response.usage_metadata.map(provider_usage).transpose()?;

    let Some(candidate) = response.candidates.into_iter().next() else {
        return Ok(usage.map_or_else(Vec::new, |usage| vec![DecodedEvent::Usage(usage)]));
    };
    if candidate.index.is_some_and(|index| index != 0) {
        return Err(ProviderError::Protocol(
            "Google GenerateContent response did not contain candidate zero".to_owned(),
        ));
    }

    let mut events = usage.map_or_else(Vec::new, |usage| vec![DecodedEvent::Usage(usage)]);
    if let Some(content) = candidate.content {
        for part in content.parts {
            if part.executable_code.is_some()
                || part.code_execution_result.is_some()
                || part.inline_data.is_some()
                || part.file_data.is_some()
                || !part.unknown.is_empty()
            {
                return Err(ProviderError::Protocol(
                    "Google GenerateContent response contained unsupported non-text content"
                        .to_owned(),
                ));
            }
            if let Some(call) = part.function_call {
                if part.text.is_some() {
                    return Err(ProviderError::Protocol(
                        "Google GenerateContent response mixed text and a function call in one part"
                            .to_owned(),
                    ));
                }
                let arguments = call.args.unwrap_or_else(|| Value::Object(Map::new()));
                let arguments = serde_json::to_string(&arguments).map_err(|error| {
                    ProviderError::Protocol(sanitize_message(
                        &format!("could not serialize Google function-call arguments: {error}"),
                        redactions,
                    ))
                })?;
                // Gemini assigns no call ids; synthesize a deterministic one
                // from the per-stream ordinal and the function name.
                let id = format!("call_{tool_call_ordinal}_{name}", name = call.name);
                *tool_call_ordinal += 1;
                events.push(DecodedEvent::ToolCall {
                    id,
                    name: call.name,
                    arguments,
                });
                continue;
            }
            if part.thought {
                continue;
            }
            if part.thought_signature.is_some() {
                return Err(ProviderError::Protocol(
                    "Google GenerateContent response contained a thought signature without thought content"
                        .to_owned(),
                ));
            }
            if let Some(text) = part.text.filter(|text| !text.is_empty()) {
                events.push(DecodedEvent::OutputText(text));
            }
        }
    }

    if let Some(reason) = candidate.finish_reason {
        match reason.as_str() {
            "STOP" | "TOOL_CALL" | "TOOL_CALLS" => events.push(DecodedEvent::Completed),
            "MAX_TOKENS" => {
                return Err(ProviderError::ResponseIncomplete(
                    "Google response reached its output token limit".to_owned(),
                ));
            }
            "MALFORMED_FUNCTION_CALL"
            | "UNEXPECTED_TOOL_CALL"
            | "TOO_MANY_TOOL_CALLS"
            | "MISSING_THOUGHT_SIGNATURE"
            | "MALFORMED_RESPONSE" => {
                return Err(ProviderError::Protocol(format!(
                    "Google response ended with tool or protocol failure reason {reason}"
                )));
            }
            "FINISH_REASON_UNSPECIFIED" => {
                return Err(ProviderError::Protocol(
                    "Google response used an unspecified finish reason".to_owned(),
                ));
            }
            _ => {
                let detail = candidate
                    .finish_message
                    .filter(|message| !message.trim().is_empty())
                    .map_or_else(
                        || format!("Google response was blocked with reason {reason}"),
                        |message| {
                            format!("Google response was blocked with reason {reason}: {message}")
                        },
                    );
                return Err(ProviderError::ResponseFailed {
                    kind: ProviderErrorKind::Response,
                    message: sanitize_message(&detail, redactions),
                });
            }
        }
    }
    Ok(events)
}

fn provider_usage(usage: UsageMetadata) -> Result<ProviderUsage, ProviderError> {
    let input_tokens = usage
        .prompt_token_count
        .checked_sub(usage.cached_content_token_count)
        .ok_or_else(|| {
            ProviderError::Protocol("Google cached input tokens exceeded prompt tokens".to_owned())
        })?;
    let output_tokens = usage
        .candidates_token_count
        .checked_add(usage.thoughts_token_count)
        .ok_or_else(|| {
            ProviderError::Protocol("Google output token usage overflowed".to_owned())
        })?;
    Ok(ProviderUsage {
        input_tokens,
        cache_read_input_tokens: usage.cached_content_token_count,
        cache_write_input_tokens: 0,
        output_tokens,
    })
}

fn wire_api_error(error: WireApiError, redactions: &[String]) -> ProviderError {
    let kind = error
        .code
        .map(status_error_kind)
        .or_else(|| error.status.as_deref().map(named_error_kind))
        .unwrap_or(ProviderErrorKind::Response);
    let message = error.message.as_deref().map_or_else(
        || "Google did not provide an error message".to_owned(),
        |message| sanitize_message(message, redactions),
    );
    ProviderError::ResponseFailed { kind, message }
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
    match name {
        "UNAUTHENTICATED" | "PERMISSION_DENIED" => ProviderErrorKind::Authentication,
        "RESOURCE_EXHAUSTED" => ProviderErrorKind::RateLimited,
        "INVALID_ARGUMENT" | "FAILED_PRECONDITION" | "NOT_FOUND" | "OUT_OF_RANGE" => {
            ProviderErrorKind::InvalidRequest
        }
        "INTERNAL" | "UNAVAILABLE" | "DEADLINE_EXCEEDED" => ProviderErrorKind::Unavailable,
        _ => ProviderErrorKind::Response,
    }
}

fn add_output_bytes(
    current: &mut usize,
    additional: usize,
    limit: usize,
) -> Result<(), ProviderError> {
    *current = current.checked_add(additional).ok_or_else(|| {
        ProviderError::Protocol("Google GenerateContent output size overflowed".to_owned())
    })?;
    if *current > limit {
        return Err(ProviderError::Protocol(
            "Google GenerateContent output exceeded the configured size limit".to_owned(),
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
        ProviderError::Protocol("Google GenerateContent wire size overflowed".to_owned())
    })?;
    if *current > limit {
        return Err(ProviderError::Protocol(
            "Google GenerateContent stream exceeded the configured wire size limit".to_owned(),
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
        .unwrap_or("Google GenerateContent request failed")
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
    while body.len() < ERROR_BODY_BYTES_LIMIT {
        let Some(chunk) = chunks.next().await else {
            break;
        };
        let Ok(chunk) = chunk else {
            break;
        };
        let remaining = ERROR_BODY_BYTES_LIMIT - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
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

    use futures_util::StreamExt;

    use super::*;

    #[tokio::test]
    async fn streams_text_and_builds_the_google_wire_request() {
        let body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hel\"}]},\"index\":0}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"lo\"}]},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":20,\"cachedContentTokenCount\":6,\"candidatesTokenCount\":7,\"thoughtsTokenCount\":3}}\n\n",
        );
        let (base_url, server) = serve_once(200, "text/event-stream", body);
        let provider = GoogleGenerateContent::with_client(
            crate::http::build_direct_client().unwrap(),
            validate_endpoint(&base_url, true).unwrap(),
            GoogleEndpoint::Base,
            GoogleAuth::XGoogApiKey("google-test-secret".to_owned()),
            [],
        )
        .unwrap();

        let events = provider
            .stream(ModelRequest::new(
                "models/gemini-test",
                vec![Message::user("hello"), Message::assistant("hi")],
                64,
            ))
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(
            &events[..],
            [
                Ok(ProviderEvent::OutputTextDelta { text: first }),
                Ok(ProviderEvent::OutputTextDelta { text: second }),
                Ok(ProviderEvent::Completed {
                    usage: Some(ProviderUsage {
                        input_tokens: 14,
                        cache_read_input_tokens: 6,
                        cache_write_input_tokens: 0,
                        output_tokens: 10,
                    }),
                }),
            ] if first == "Hel" && second == "lo"
        ));
        let request = server.join().unwrap();
        let (head, body) = request.split_once("\r\n\r\n").unwrap();
        assert_eq!(
            head.lines().next(),
            Some("POST /models/gemini-test:streamGenerateContent?alt=sse HTTP/1.1")
        );
        assert_eq!(
            request_header(head, "x-goog-api-key"),
            Some("google-test-secret")
        );
        assert!(!head.contains("google-test-secret?"));
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "contents": [
                    {"role": "user", "parts": [{"text": "hello"}]},
                    {"role": "model", "parts": [{"text": "hi"}]},
                ],
                "generationConfig": {"maxOutputTokens": 64},
            })
        );
    }

    #[tokio::test]
    async fn sends_tool_declarations_and_tool_history_parts() {
        let body = "data: {\"candidates\":[{\"finishReason\":\"STOP\",\"index\":0}]}\n\n";
        let (endpoint, server) = serve_once(200, "text/event-stream", body);
        let provider = GoogleGenerateContent::with_client(
            crate::http::build_direct_client().unwrap(),
            validate_endpoint(&endpoint, true).unwrap(),
            GoogleEndpoint::Base,
            GoogleAuth::NoAuth,
            [],
        )
        .unwrap();
        let request = ModelRequest::new(
            "gemini-test",
            vec![
                Message::user("read the config"),
                Message::new(
                    Role::Assistant,
                    vec![
                        ContentBlock::Text {
                            text: "Reading it now.".to_owned(),
                        },
                        ContentBlock::ToolCall {
                            id: "call_0_read_file".to_owned(),
                            name: "read_file".to_owned(),
                            arguments: serde_json::json!({"path": "config.ron"}),
                        },
                        ContentBlock::ToolCall {
                            id: "call_1_list_dir".to_owned(),
                            name: "list_dir".to_owned(),
                            arguments: serde_json::json!({"path": "."}),
                        },
                    ],
                ),
                Message::tool_results(vec![
                    ContentBlock::ToolResult {
                        call_id: "call_0_read_file".to_owned(),
                        content: "(config)".to_owned(),
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        call_id: "call_1_list_dir".to_owned(),
                        content: "denied".to_owned(),
                        is_error: true,
                    },
                ]),
            ],
            128,
        )
        .with_tools(vec![ToolSpec::new(
            "read_file",
            "Reads one file",
            serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        )]);
        let events = provider.stream(request).collect::<Vec<_>>().await;

        assert!(matches!(
            &events[..],
            [Ok(ProviderEvent::Completed { usage: None })]
        ));
        let request = server.join().unwrap();
        let body = request.split_once("\r\n\r\n").unwrap().1;
        assert_eq!(
            serde_json::from_str::<Value>(body).unwrap(),
            serde_json::json!({
                "contents": [
                    {"role": "user", "parts": [{"text": "read the config"}]},
                    {"role": "model", "parts": [
                        {"text": "Reading it now."},
                        {"functionCall": {"name": "read_file", "args": {"path": "config.ron"}}},
                        {"functionCall": {"name": "list_dir", "args": {"path": "."}}},
                    ]},
                    {"role": "user", "parts": [
                        {"functionResponse": {
                            "name": "read_file",
                            "response": {"output": "(config)"},
                        }},
                        {"functionResponse": {
                            "name": "list_dir",
                            "response": {"error": "denied"},
                        }},
                    ]},
                ],
                "tools": [{"functionDeclarations": [{
                    "name": "read_file",
                    "description": "Reads one file",
                    "parameters": {"type": "object", "properties": {"path": {"type": "string"}}},
                }]}],
                "generationConfig": {"maxOutputTokens": 128},
            })
        );
    }

    #[tokio::test]
    async fn streams_tool_calls_with_deterministic_synthetic_ids_to_completion() {
        let body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Checking.\"}]},\"index\":0}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[",
            "{\"functionCall\":{\"name\":\"read_file\",\"args\":{\"path\":\"a.rs\"}}},",
            "{\"functionCall\":{\"name\":\"read_file\",\"args\":{\"path\":\"b.rs\"}}}",
            "]},\"finishReason\":\"STOP\",\"index\":0}]}\n\n",
        );
        let (endpoint, server) = serve_once(200, "text/event-stream", body);
        let provider = GoogleGenerateContent::with_client(
            crate::http::build_direct_client().unwrap(),
            validate_endpoint(&endpoint, true).unwrap(),
            GoogleEndpoint::Exact,
            GoogleAuth::NoAuth,
            [],
        )
        .unwrap();
        let events = provider
            .stream(ModelRequest::new(
                "gemini-test",
                vec![Message::user("hi")],
                64,
            ))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        server.join().unwrap();

        assert_eq!(
            events,
            vec![
                ProviderEvent::OutputTextDelta {
                    text: "Checking.".to_owned(),
                },
                ProviderEvent::ToolCallStarted {
                    id: "call_0_read_file".to_owned(),
                    name: "read_file".to_owned(),
                },
                ProviderEvent::ToolCallArgumentsDelta {
                    id: "call_0_read_file".to_owned(),
                    json: "{\"path\":\"a.rs\"}".to_owned(),
                },
                ProviderEvent::ToolCallCompleted {
                    id: "call_0_read_file".to_owned(),
                },
                ProviderEvent::ToolCallStarted {
                    id: "call_1_read_file".to_owned(),
                    name: "read_file".to_owned(),
                },
                ProviderEvent::ToolCallArgumentsDelta {
                    id: "call_1_read_file".to_owned(),
                    json: "{\"path\":\"b.rs\"}".to_owned(),
                },
                ProviderEvent::ToolCallCompleted {
                    id: "call_1_read_file".to_owned(),
                },
                ProviderEvent::Completed { usage: None },
            ]
        );
    }

    #[tokio::test]
    async fn rejects_a_tool_result_without_a_matching_call() {
        let provider = GoogleGenerateContent::with_client(
            crate::http::build_direct_client().unwrap(),
            validate_endpoint("https://example.test/custom", false).unwrap(),
            GoogleEndpoint::Exact,
            GoogleAuth::NoAuth,
            [],
        )
        .unwrap();
        let request = ModelRequest::new(
            "gemini-test",
            vec![Message::tool_results(vec![ContentBlock::ToolResult {
                call_id: "call_9_missing".to_owned(),
                content: "(orphaned)".to_owned(),
                is_error: false,
            }])],
            64,
        );

        let error = provider.stream(request).next().await.unwrap().unwrap_err();
        assert!(matches!(error, ProviderError::Configuration(_)));
    }

    #[test]
    fn decoder_handles_fragmented_utf8_multiline_data_and_crlf() {
        let mut decoder = SseDecoder::new(4_096);
        let payload = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hé\"}]},\r\n";
        let suffix = "data: \"finishReason\":\"STOP\",\"index\":0}]}\r\n\r\n";
        let mut bytes = [payload.as_bytes(), suffix.as_bytes()].concat();
        let split = bytes.iter().position(|byte| *byte == 0xc3).unwrap() + 1;
        let remainder = bytes.split_off(split);

        assert!(decoder.push(&bytes).unwrap().is_empty());
        let events = decoder.push(&remainder).unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].contains("hé"));
    }

    #[test]
    fn maps_terminal_and_blocked_responses_without_silent_success() {
        let max_tokens = decode_event(
            r#"{"candidates":[{"finishReason":"MAX_TOKENS","index":0}]}"#,
            &mut 0,
            &[],
        )
        .unwrap_err();
        assert!(matches!(max_tokens, ProviderError::ResponseIncomplete(_)));

        let blocked = decode_event(
            r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#,
            &mut 0,
            &[],
        )
        .unwrap_err();
        assert!(matches!(
            blocked,
            ProviderError::ResponseFailed {
                kind: ProviderErrorKind::Response,
                ..
            }
        ));

        let tool = decode_event(
            r#"{"candidates":[{"finishReason":"UNEXPECTED_TOOL_CALL","index":0}]}"#,
            &mut 0,
            &[],
        )
        .unwrap_err();
        assert!(matches!(tool, ProviderError::Protocol(_)));
    }

    #[test]
    fn usage_subtracts_cached_prompt_and_includes_thoughts() {
        let events = decode_event(
            r#"{"candidates":[{"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":20,"cachedContentTokenCount":6,"candidatesTokenCount":7,"thoughtsTokenCount":3}}"#,
            &mut 0,
            &[],
        )
        .unwrap();
        assert_eq!(
            events,
            [
                DecodedEvent::Usage(ProviderUsage {
                    input_tokens: 14,
                    cache_read_input_tokens: 6,
                    cache_write_input_tokens: 0,
                    output_tokens: 10,
                }),
                DecodedEvent::Completed,
            ]
        );

        for data in [
            r#"{"usageMetadata":{"promptTokenCount":2,"cachedContentTokenCount":3,"candidatesTokenCount":1}}"#,
            r#"{"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":18446744073709551615,"thoughtsTokenCount":1}}"#,
        ] {
            assert!(matches!(
                decode_event(data, &mut 0, &[]),
                Err(ProviderError::Protocol(_))
            ));
        }
    }

    #[test]
    fn streamed_api_errors_are_typed_and_redacted() {
        let secret = "streamed-google-secret";
        let error = decode_event(
            &format!(
                r#"{{"error":{{"code":429,"status":"RESOURCE_EXHAUSTED","message":"quota for {secret}"}}}}"#
            ),
            &mut 0,
            &[secret.to_owned()],
        )
        .unwrap_err();

        assert_eq!(error.kind(), ProviderErrorKind::RateLimited);
        assert!(!error.to_string().contains(secret));
        assert!(error.to_string().contains("[REDACTED]"));
    }

    #[test]
    fn rejects_unknown_and_mixed_non_text_parts() {
        for response in [
            r#"{"candidates":[{"content":{"parts":[{"functionResponse":{}}]},"finishReason":"STOP","index":0}]}"#,
            r#"{"candidates":[{"content":{"parts":[{"text":"unsafe","functionCall":{"name":"noop"}}]},"finishReason":"STOP","index":0}]}"#,
        ] {
            let error = decode_event(response, &mut 0, &[]).unwrap_err();
            assert!(matches!(error, ProviderError::Protocol(_)));
        }
    }

    #[test]
    fn rejects_model_path_injection_and_auth_header_overrides() {
        let endpoint = validate_endpoint("https://example.test/v1beta", false).unwrap();
        let provider = GoogleGenerateContent::with_client(
            crate::http::build_direct_client().unwrap(),
            endpoint.clone(),
            GoogleEndpoint::Base,
            GoogleAuth::NoAuth,
            [],
        )
        .unwrap();
        assert!(provider.request_endpoint("../secret").is_err());
        assert!(provider.request_endpoint("model?key=secret").is_err());

        let exact = GoogleGenerateContent::with_client(
            crate::http::build_direct_client().unwrap(),
            validate_endpoint("https://example.test/custom?alt=sse", false).unwrap(),
            GoogleEndpoint::Exact,
            GoogleAuth::NoAuth,
            [],
        )
        .unwrap();
        assert_eq!(
            exact.request_endpoint("../not-used").unwrap().as_str(),
            "https://example.test/custom?alt=sse"
        );

        assert!(
            GoogleGenerateContent::with_client(
                crate::http::build_direct_client().unwrap(),
                endpoint,
                GoogleEndpoint::Base,
                GoogleAuth::XGoogApiKey("secret".to_owned()),
                [("x-goog-api-key".to_owned(), "override".to_owned())],
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn redacts_api_keys_from_http_errors() {
        let secret = "google-secret-value";
        let (endpoint, server) = serve_once(
            403,
            "application/json",
            &format!(r#"{{"error":{{"message":"bad key {secret}"}}}}"#),
        );
        let provider = GoogleGenerateContent::with_client(
            crate::http::build_direct_client().unwrap(),
            validate_endpoint(&endpoint, true).unwrap(),
            GoogleEndpoint::Exact,
            GoogleAuth::XGoogApiKey(secret.to_owned()),
            [],
        )
        .unwrap();

        let error = provider
            .stream(ModelRequest::new(
                "gemini-test",
                vec![Message::user("hi")],
                64,
            ))
            .next()
            .await
            .unwrap()
            .unwrap_err();
        server.join().unwrap();

        assert!(!error.to_string().contains(secret));
        assert!(error.to_string().contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn rejects_incomplete_and_non_sse_responses() {
        let (endpoint, incomplete_server) = serve_once(
            200,
            "text/event-stream",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]},\"index\":0}]}\n\n",
        );
        let incomplete = GoogleGenerateContent::with_client(
            crate::http::build_direct_client().unwrap(),
            validate_endpoint(&endpoint, true).unwrap(),
            GoogleEndpoint::Exact,
            GoogleAuth::NoAuth,
            [],
        )
        .unwrap()
        .stream(ModelRequest::new(
            "gemini-test",
            vec![Message::user("hi")],
            64,
        ))
        .collect::<Vec<_>>()
        .await;
        incomplete_server.join().unwrap();
        assert!(matches!(
            &incomplete[..],
            [
                Ok(ProviderEvent::OutputTextDelta { text }),
                Err(ProviderError::Protocol(_)),
            ] if text == "partial"
        ));

        let (endpoint, non_sse_server) = serve_once(200, "application/json", "{}");
        let error = GoogleGenerateContent::with_client(
            crate::http::build_direct_client().unwrap(),
            validate_endpoint(&endpoint, true).unwrap(),
            GoogleEndpoint::Exact,
            GoogleAuth::NoAuth,
            [],
        )
        .unwrap()
        .stream(ModelRequest::new(
            "gemini-test",
            vec![Message::user("hi")],
            64,
        ))
        .next()
        .await
        .unwrap()
        .unwrap_err();
        non_sse_server.join().unwrap();
        assert!(matches!(error, ProviderError::Protocol(_)));
    }

    #[test]
    fn enforces_event_output_and_wire_limits() {
        let mut decoder = SseDecoder::new(4);
        assert!(matches!(
            decoder.push(b"data: value"),
            Err(ProviderError::Protocol(_))
        ));

        assert!(matches!(
            add_output_bytes(&mut 0, 5, 4),
            Err(ProviderError::Protocol(_))
        ));
        assert!(matches!(
            add_wire_bytes(&mut 0, 5, 4),
            Err(ProviderError::Protocol(_))
        ));
    }

    fn serve_once(status: u16, content_type: &str, body: &str) -> (String, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let content_type = content_type.to_owned();
        let body = body.to_owned();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let request = read_request(&mut stream);
            let response = format!(
                "HTTP/1.1 {status} Test\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
