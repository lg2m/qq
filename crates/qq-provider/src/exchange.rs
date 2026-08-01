//! Shared SSE request/stream driver for the HTTP protocol adapters.
//!
//! [`sse_exchange`] owns the request/response prologue every SSE adapter
//! repeats: build the JSON request, execute it through [`HttpExchange`]
//! (retry, wire limits, bounded rejection buffering), gate the success
//! Content-Type, and decode the body into [`SseEvent`]s. Adapters keep
//! ownership of their error body schema (via [`SseExchangeError::Rejected`])
//! and of folding events into `ProviderEvent`s.

use std::{collections::VecDeque, sync::Arc};

use futures_util::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap};
use serde::Serialize;

use crate::{
    ProviderError,
    http::{
        ExchangeMessages, ExchangeOutcome, HttpExchange, HttpRejection, is_event_stream_headers,
        transport_error,
    },
    sse::{SseDecoder, SseEvent},
};

/// How a success response's Content-Type is validated before streaming.
#[derive(Clone, Copy)]
pub(crate) enum ContentTypeGate {
    /// The response must declare `text/event-stream`.
    Strict,
    /// ChatGPT Codex streams valid SSE frames but often omits Content-Type
    /// entirely. A present non-SSE Content-Type is still rejected so a JSON
    /// success body cannot be misread.
    AllowMissingContentType,
}

impl ContentTypeGate {
    pub(crate) fn accepts(self, headers: &HeaderMap) -> bool {
        if is_event_stream_headers(headers) {
            return true;
        }
        matches!(self, Self::AllowMissingContentType) && headers.get(CONTENT_TYPE).is_none()
    }
}

/// Protocol-owned strings and policies for one SSE exchange.
pub(crate) struct SseExchangeSpec {
    pub(crate) messages: ExchangeMessages,
    pub(crate) non_sse_response: &'static str,
    pub(crate) content_type_gate: ContentTypeGate,
}

/// Why an SSE exchange could not begin streaming.
pub(crate) enum SseExchangeError {
    /// The provider rejected the request. The adapter interprets the buffered
    /// error body against its own error schema.
    Rejected(HttpRejection),
    Provider(ProviderError),
}

impl From<ProviderError> for SseExchangeError {
    fn from(error: ProviderError) -> Self {
        Self::Provider(error)
    }
}

impl SseExchangeError {
    /// Collapses the error into a `ProviderError`, delegating rejected
    /// requests to the adapter's error-body interpreter.
    pub(crate) fn into_provider_error(
        self,
        interpret_rejection: impl FnOnce(HttpRejection) -> ProviderError,
    ) -> ProviderError {
        match self {
            Self::Rejected(rejection) => interpret_rejection(rejection),
            Self::Provider(error) => error,
        }
    }
}

/// Decoded SSE events from an accepted exchange.
pub(crate) struct SseExchangeStream {
    redactions: Arc<[String]>,
    chunks: futures_core::stream::BoxStream<'static, Result<bytes::Bytes, ProviderError>>,
    decoder: SseDecoder,
    pending: VecDeque<SseEvent>,
}

impl SseExchangeStream {
    /// Values echoed by the provider that must never appear in errors.
    pub(crate) fn redactions(&self) -> &Arc<[String]> {
        &self.redactions
    }

    /// Returns the next decoded SSE event, or `None` when the body ends.
    pub(crate) async fn next_event(&mut self) -> Result<Option<SseEvent>, ProviderError> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            match self.chunks.next().await {
                Some(chunk) => self.pending.extend(self.decoder.push(&chunk?)?),
                None => return Ok(None),
            }
        }
    }
}

/// Executes one SSE request and prepares its decoded event stream.
pub(crate) async fn sse_exchange(
    exchange: &HttpExchange,
    endpoint: reqwest::Url,
    headers: HeaderMap,
    body: &impl Serialize,
    decoder: SseDecoder,
    wire_limit: usize,
    spec: SseExchangeSpec,
) -> Result<SseExchangeStream, SseExchangeError> {
    let request = exchange
        .request(reqwest::Method::POST, endpoint)
        .headers(headers)
        .header(ACCEPT, "text/event-stream")
        .json(body)
        .build()
        .map_err(|error| transport_error(error, exchange.static_redactions()))?;
    let response = match exchange.execute(request, wire_limit, spec.messages).await? {
        ExchangeOutcome::Success(response) => response,
        ExchangeOutcome::Rejected(rejection) => return Err(SseExchangeError::Rejected(rejection)),
    };

    if !spec.content_type_gate.accepts(response.headers()) {
        return Err(ProviderError::Protocol(spec.non_sse_response.to_owned()).into());
    }

    Ok(SseExchangeStream {
        redactions: Arc::from(response.redactions()),
        chunks: response.into_body(),
        decoder,
        pending: VecDeque::new(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        http::build_direct_client, request_auth::RequestAuthorizer, sse::Utf8ErrorMessage,
        test_support::LoopbackServer,
    };

    const MESSAGES: ExchangeMessages = ExchangeMessages {
        wire_overflow: "test wire size overflowed",
        wire_limit: "test stream exceeded the configured wire size limit",
    };

    fn spec(content_type_gate: ContentTypeGate) -> SseExchangeSpec {
        SseExchangeSpec {
            messages: MESSAGES,
            non_sse_response: "test provider returned a non-SSE response",
            content_type_gate,
        }
    }

    fn decoder() -> SseDecoder {
        SseDecoder::data_only(
            64 * 1_024,
            "test event size overflowed",
            "test event exceeded the configured size limit",
            Utf8ErrorMessage::Static("test event was not UTF-8"),
        )
    }

    fn exchange(redactions: Vec<String>) -> HttpExchange {
        HttpExchange::new(
            build_direct_client().unwrap(),
            RequestAuthorizer::default(),
            Arc::from(redactions),
        )
        .with_retry_policy(crate::http::RetryPolicy::disabled())
    }

    async fn drive(
        server: &LoopbackServer,
        content_type_gate: ContentTypeGate,
    ) -> Result<SseExchangeStream, SseExchangeError> {
        let endpoint = reqwest::Url::parse(&format!("{}/v1/test", server.base_url)).unwrap();
        sse_exchange(
            &exchange(vec!["loopback-secret".to_owned()]),
            endpoint,
            HeaderMap::new(),
            &json!({"model": "test-model"}),
            decoder(),
            1_024 * 1_024,
            spec(content_type_gate),
        )
        .await
    }

    #[tokio::test]
    async fn sends_the_request_and_decodes_events_until_the_body_ends() {
        let server = LoopbackServer::sse("data: first\n\ndata: second\n\n");

        let mut sse = drive(&server, ContentTypeGate::Strict).await.ok().unwrap();

        let mut data = Vec::new();
        while let Some(event) = sse.next_event().await.unwrap() {
            data.push(event.data);
        }
        assert_eq!(data, ["first", "second"]);
        assert_eq!(sse.redactions().as_ref(), ["loopback-secret"]);

        let request = server.capture();
        assert_eq!(request.request_line(), Some("POST /v1/test HTTP/1.1"));
        assert_eq!(request.header("accept"), Some("text/event-stream"));
        assert_eq!(request.json_body()["model"], "test-model");
    }

    #[tokio::test]
    async fn rejections_hand_the_buffered_error_body_back_to_the_adapter() {
        let server = LoopbackServer::respond(503, "application/json", "{\"error\":\"overloaded\"}");

        let error = drive(&server, ContentTypeGate::Strict).await.err().unwrap();

        let SseExchangeError::Rejected(rejection) = error else {
            panic!("expected a rejection");
        };
        assert_eq!(rejection.status().as_u16(), 503);
        assert_eq!(rejection.body(), b"{\"error\":\"overloaded\"}");
    }

    #[tokio::test]
    async fn strict_gates_reject_missing_and_wrong_content_types() {
        for server in [
            LoopbackServer::respond_chunks(200, None, vec![b"data: x\n\n".to_vec()]),
            LoopbackServer::respond(200, "application/json", "{}"),
        ] {
            let error = drive(&server, ContentTypeGate::Strict).await.err().unwrap();

            let error = error.into_provider_error(|_| unreachable!("no rejection expected"));
            assert!(matches!(
                &error,
                ProviderError::Protocol(message)
                    if message == "test provider returned a non-SSE response"
            ));
        }
    }

    #[tokio::test]
    async fn the_codex_gate_accepts_missing_but_not_wrong_content_types() {
        let missing = LoopbackServer::respond_chunks(200, None, vec![b"data: ok\n\n".to_vec()]);
        let mut sse = drive(&missing, ContentTypeGate::AllowMissingContentType)
            .await
            .ok()
            .unwrap();
        assert_eq!(
            sse.next_event().await.unwrap().map(|event| event.data),
            Some("ok".to_owned())
        );

        let wrong = LoopbackServer::respond(200, "application/json", "{}");
        let error = drive(&wrong, ContentTypeGate::AllowMissingContentType)
            .await
            .err()
            .unwrap();
        assert!(matches!(
            error.into_provider_error(|_| unreachable!("no rejection expected")),
            ProviderError::Protocol(message)
                if message == "test provider returned a non-SSE response"
        ));
    }
}
