#![cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "exchange compatibility helpers remain until HTTP adapter migrations finish"
    )
)]

use std::{net::IpAddr, pin::Pin, sync::Arc, time::Duration};

use futures_core::Stream;
use futures_util::StreamExt;
use reqwest::{
    StatusCode, Url,
    header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue},
};

use crate::{
    ProviderError, limits::ByteCounter, request_auth::RequestAuthorizer, sanitize::sanitize_message,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(300);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const ERROR_BODY_BYTES_LIMIT: usize = 16 * 1_024;

pub(crate) struct SafeHeaders {
    headers: HeaderMap,
    protocol_owned: Vec<HeaderName>,
    redactions: Vec<String>,
}

impl SafeHeaders {
    pub(crate) fn new(protocol_owned: impl IntoIterator<Item = HeaderName>) -> Self {
        Self {
            headers: HeaderMap::new(),
            protocol_owned: protocol_owned.into_iter().collect(),
            redactions: Vec::new(),
        }
    }

    pub(crate) fn insert_configured(
        &mut self,
        configured: impl IntoIterator<Item = (String, String)>,
        redact_whitespace_only: bool,
    ) -> Result<(), ProviderError> {
        for (name, value) in configured {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                ProviderError::Configuration("static header name is invalid".to_owned())
            })?;
            if is_request_controlled_header(&name)
                || self.protocol_owned.iter().any(|owned| owned == &name)
            {
                return Err(ProviderError::Configuration(format!(
                    "static header `{name}` is controlled by the provider"
                )));
            }
            if self.headers.contains_key(&name) {
                return Err(ProviderError::Configuration(format!(
                    "static header `{name}` is duplicated"
                )));
            }

            let mut header_value = HeaderValue::from_str(&value).map_err(|_| {
                ProviderError::Configuration("static header value is invalid".to_owned())
            })?;
            header_value.set_sensitive(true);
            if if redact_whitespace_only {
                !value.is_empty()
            } else {
                !value.trim().is_empty()
            } {
                self.redactions.push(value);
            }
            self.headers.insert(name, header_value);
        }
        Ok(())
    }

    pub(crate) fn insert_owned(&mut self, name: HeaderName, value: HeaderValue) {
        self.headers.insert(name, value);
    }

    pub(crate) fn push_redaction(&mut self, value: String) {
        self.redactions.push(value);
    }

    pub(crate) fn finish(mut self) -> (HeaderMap, Vec<String>) {
        normalize_redactions(&mut self.redactions);
        (self.headers, self.redactions)
    }
}

pub(crate) fn is_request_controlled_header(name: &HeaderName) -> bool {
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

#[derive(Clone)]
pub(crate) struct HttpExchange {
    client: reqwest::Client,
    authorizer: RequestAuthorizer,
    redactions: Arc<[String]>,
}

pub(crate) struct ExchangeMessages {
    pub(crate) wire_overflow: &'static str,
    pub(crate) wire_limit: &'static str,
}

pub(crate) enum ExchangeOutcome {
    Success(HttpResponse),
    Rejected(HttpRejection),
}

pub(crate) struct HttpResponse {
    status: StatusCode,
    headers: HeaderMap,
    redactions: Arc<[String]>,
    body: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, ProviderError>> + Send>>,
}

pub(crate) struct HttpRejection {
    status: StatusCode,
    body: Vec<u8>,
    redactions: Arc<[String]>,
}

impl HttpExchange {
    pub(crate) fn new(
        client: reqwest::Client,
        authorizer: RequestAuthorizer,
        redactions: Arc<[String]>,
    ) -> Self {
        Self {
            client,
            authorizer,
            redactions,
        }
    }

    pub(crate) fn request(&self, method: reqwest::Method, url: Url) -> reqwest::RequestBuilder {
        self.client.request(method, url)
    }

    pub(crate) fn static_redactions(&self) -> &[String] {
        &self.redactions
    }

    pub(crate) async fn execute(
        &self,
        mut request: reqwest::Request,
        wire_limit: usize,
        messages: ExchangeMessages,
    ) -> Result<ExchangeOutcome, ProviderError> {
        let mut redactions = self.redactions.as_ref().to_vec();
        redactions.extend(self.authorizer.authorize(&mut request).await?);
        normalize_redactions(&mut redactions);
        let redactions: Arc<[String]> = Arc::from(redactions);

        let response = self
            .client
            .execute(request)
            .await
            .map_err(|error| transport_error(error, redactions.as_ref()))?;
        let status = response.status();
        if !status.is_success() {
            let body = read_error_body(response).await;
            return Ok(ExchangeOutcome::Rejected(HttpRejection {
                status,
                body,
                redactions,
            }));
        }

        let headers = response.headers().clone();
        let chunks = response.bytes_stream();
        let body_redactions = Arc::clone(&redactions);
        let body = Box::pin(async_stream::try_stream! {
            let mut chunks = chunks;
            let mut wire_bytes = ByteCounter::new(
                wire_limit,
                messages.wire_overflow,
                messages.wire_limit,
            );
            while let Some(chunk) = chunks.next().await {
                let chunk = chunk
                    .map_err(|error| transport_error(error, body_redactions.as_ref()))?;
                wire_bytes.add(chunk.len())?;
                yield chunk;
            }
        });

        Ok(ExchangeOutcome::Success(HttpResponse {
            status,
            headers,
            redactions,
            body,
        }))
    }
}

impl HttpResponse {
    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }

    pub(crate) fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub(crate) fn redactions(&self) -> &[String] {
        &self.redactions
    }

    pub(crate) fn into_body(
        self,
    ) -> Pin<Box<dyn Stream<Item = Result<bytes::Bytes, ProviderError>> + Send>> {
        self.body
    }
}

impl HttpRejection {
    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }

    pub(crate) fn body(&self) -> &[u8] {
        &self.body
    }

    pub(crate) fn redactions(&self) -> &[String] {
        &self.redactions
    }
}

fn normalize_redactions(redactions: &mut Vec<String>) {
    redactions.retain(|redaction| !redaction.is_empty());
    redactions.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    redactions.dedup();
}

pub(crate) fn build_client() -> Result<reqwest::Client, ProviderError> {
    client_builder()
        .build()
        .map_err(|error| ProviderError::Configuration(error.to_string()))
}

pub(crate) fn build_direct_client() -> Result<reqwest::Client, ProviderError> {
    client_builder()
        .no_proxy()
        .build()
        .map_err(|error| ProviderError::Configuration(error.to_string()))
}

fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .use_rustls_tls()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("qq/", env!("CARGO_PKG_VERSION")))
}

pub(crate) fn transport_error(error: reqwest::Error, redactions: &[String]) -> ProviderError {
    ProviderError::Transport(sanitize_message(
        &error.without_url().to_string(),
        redactions,
    ))
}

pub(crate) fn is_event_stream(response: &reqwest::Response) -> bool {
    is_event_stream_headers(response.headers())
}

pub(crate) fn is_event_stream_headers(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(';').next().is_some_and(|media_type| {
                media_type.trim().eq_ignore_ascii_case("text/event-stream")
            })
        })
}

pub(crate) async fn read_error_body(response: reqwest::Response) -> Vec<u8> {
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

pub(crate) fn validate_endpoint(endpoint: &str, allow_http: bool) -> Result<Url, ProviderError> {
    let url = Url::parse(endpoint).map_err(|_| {
        ProviderError::Configuration("endpoint must be a valid absolute URL".to_owned())
    })?;

    if url.fragment().is_some() {
        return Err(ProviderError::Configuration(
            "endpoint URL must not contain a fragment".to_owned(),
        ));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || endpoint_authority_contains_at_sign(endpoint)
    {
        return Err(ProviderError::Configuration(
            "endpoint URL must not contain user information".to_owned(),
        ));
    }
    if url.host_str().is_none() {
        return Err(ProviderError::Configuration(
            "endpoint URL must contain a host".to_owned(),
        ));
    }

    match url.scheme() {
        "https" => Ok(url),
        "http" if allow_http && is_loopback_host(&url) => Ok(url),
        "http" => Err(ProviderError::Configuration(
            "plain HTTP is allowed only for explicitly enabled loopback endpoints".to_owned(),
        )),
        _ => Err(ProviderError::Configuration(
            "endpoint URL must use HTTPS".to_owned(),
        )),
    }
}

fn endpoint_authority_contains_at_sign(endpoint: &str) -> bool {
    let Some((_, remainder)) = endpoint.split_once("://") else {
        return false;
    };
    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    remainder[..authority_end].contains('@')
}

fn is_loopback_host(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }

    let address = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    address
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        pin::Pin,
        thread::{self, JoinHandle},
    };

    use crate::request_auth::{
        RequestCredential, RequestCredentialError, RequestCredentialProvider,
        SharedRequestCredentialProvider,
    };

    use super::*;

    const WIRE_OVERFLOW: &str = "test wire size overflowed";
    const WIRE_LIMIT: &str = "test wire size exceeded the limit";

    fn messages() -> ExchangeMessages {
        ExchangeMessages {
            wire_overflow: WIRE_OVERFLOW,
            wire_limit: WIRE_LIMIT,
        }
    }

    fn exchange(authorizer: RequestAuthorizer, redactions: Vec<String>) -> HttpExchange {
        HttpExchange::new(
            build_direct_client().unwrap(),
            authorizer,
            Arc::from(redactions),
        )
    }

    #[tokio::test]
    async fn exchange_splits_success_and_exposes_narrow_metadata() {
        let body = b"event bytes".to_vec();
        let (url, server) = serve_response(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
                body.len()
            ),
            body.clone(),
        );
        let request = build_direct_client().unwrap().get(url).build().unwrap();

        let outcome = exchange(
            RequestAuthorizer::default(),
            vec![
                "short".to_owned(),
                "longer-redaction".to_owned(),
                "short".to_owned(),
                String::new(),
            ],
        )
        .execute(request, body.len(), messages())
        .await
        .unwrap();
        let ExchangeOutcome::Success(response) = outcome else {
            panic!("successful status must produce a successful exchange");
        };

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        assert_eq!(
            response.redactions(),
            &["longer-redaction".to_owned(), "short".to_owned()]
        );
        let streamed = response
            .into_body()
            .map(|chunk| chunk.unwrap())
            .collect::<Vec<_>>()
            .await
            .concat();
        assert_eq!(streamed, body);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn exchange_bounds_rejections_and_preserves_status() {
        let body = vec![b'x'; ERROR_BODY_BYTES_LIMIT + 1_024];
        let (url, server) = serve_response(
            format!(
                "HTTP/1.1 429 Too Many Requests\r\nContent-Length: {}\r\n\r\n",
                body.len()
            ),
            body,
        );
        let request = build_direct_client().unwrap().get(url).build().unwrap();

        let outcome = exchange(RequestAuthorizer::default(), Vec::new())
            .execute(request, usize::MAX, messages())
            .await
            .unwrap();
        let ExchangeOutcome::Rejected(rejection) = outcome else {
            panic!("non-success status must produce a rejection");
        };

        assert_eq!(rejection.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(rejection.body().len(), ERROR_BODY_BYTES_LIMIT);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn exchange_success_body_enforces_exact_and_over_wire_limits() {
        let exact_body = b"12345".to_vec();
        let (url, server) = serve_response(
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n".to_owned(),
            exact_body.clone(),
        );
        let request = build_direct_client().unwrap().get(url).build().unwrap();
        let ExchangeOutcome::Success(response) = exchange(RequestAuthorizer::default(), Vec::new())
            .execute(request, exact_body.len(), messages())
            .await
            .unwrap()
        else {
            panic!("expected success");
        };
        let streamed = response.into_body().collect::<Vec<_>>().await;
        assert_eq!(streamed.len(), 1);
        assert_eq!(streamed[0].as_ref().unwrap(), exact_body.as_slice());
        server.join().unwrap();

        let (url, server) = serve_response(
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n".to_owned(),
            exact_body,
        );
        let request = build_direct_client().unwrap().get(url).build().unwrap();
        let ExchangeOutcome::Success(response) = exchange(RequestAuthorizer::default(), Vec::new())
            .execute(request, 4, messages())
            .await
            .unwrap()
        else {
            panic!("expected success");
        };
        let error = response.into_body().next().await.unwrap().unwrap_err();
        assert_eq!(
            error.to_string(),
            "provider stream was invalid: test wire size exceeded the limit"
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn exchange_authorizes_immediately_before_send_and_normalizes_redactions() {
        let secret = "dynamic-bearer-secret";
        let provider = SharedRequestCredentialProvider::new(StaticBearer(secret.to_owned()));
        let authorizer = RequestAuthorizer::request_credentials(provider);
        let (url, server) = serve_authorized_rejection(secret);
        let request = build_direct_client().unwrap().get(url).build().unwrap();

        let outcome = exchange(
            authorizer,
            vec!["short".to_owned(), secret.to_owned(), "short".to_owned()],
        )
        .execute(request, usize::MAX, messages())
        .await
        .unwrap();
        let ExchangeOutcome::Rejected(rejection) = outcome else {
            panic!("expected rejection");
        };

        assert_eq!(rejection.body(), b"rejected");
        assert_eq!(
            rejection.redactions(),
            &[secret.to_owned(), "short".to_owned()]
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn exchange_body_read_errors_redact_dynamic_credentials() {
        let secret = "dynamic-body-read-secret";
        let provider = SharedRequestCredentialProvider::new(StaticBearer(secret.to_owned()));
        let authorizer = RequestAuthorizer::request_credentials(provider);
        let (url, server) = serve_authorized_malformed_body(secret);
        let request = build_direct_client()
            .unwrap()
            .get(format!("{url}?credential={secret}"))
            .build()
            .unwrap();

        let ExchangeOutcome::Success(response) = exchange(authorizer, Vec::new())
            .execute(request, usize::MAX, messages())
            .await
            .unwrap()
        else {
            panic!("expected successful response metadata");
        };
        let error = response.into_body().next().await.unwrap().unwrap_err();
        let rendered = error.to_string();

        assert!(!rendered.contains(secret));
        assert!(!rendered.contains("credential="));
        assert!(!rendered.contains('\n'));
        server.join().unwrap();
    }

    struct StaticBearer(String);

    impl RequestCredentialProvider for StaticBearer {
        fn credential(
            &self,
        ) -> Pin<
            Box<dyn Future<Output = Result<RequestCredential, RequestCredentialError>> + Send + '_>,
        > {
            Box::pin(async { RequestCredential::bearer(self.0.clone()) })
        }
    }

    #[tokio::test]
    async fn error_body_stops_at_exactly_sixteen_kibibytes() {
        let body = vec![b'x'; ERROR_BODY_BYTES_LIMIT + 1_024];
        let (url, server) = serve_response(
            format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\n\r\n",
                body.len()
            ),
            body,
        );
        let response = build_direct_client()
            .unwrap()
            .get(url)
            .send()
            .await
            .unwrap();

        let body = read_error_body(response).await;

        assert_eq!(body.len(), ERROR_BODY_BYTES_LIMIT);
        assert!(body.iter().all(|byte| *byte == b'x'));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn error_body_preserves_partial_bytes_when_the_read_fails() {
        let partial = b"partial provider error".to_vec();
        let (url, server) = serve_response(
            "HTTP/1.1 400 Bad Request\r\nContent-Length: 1024\r\n\r\n".to_owned(),
            partial.clone(),
        );
        let response = build_direct_client()
            .unwrap()
            .get(url)
            .send()
            .await
            .unwrap();

        let body = read_error_body(response).await;

        assert_eq!(body, partial);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn successful_response_streams_all_wire_bytes() {
        let body = b"first second third".to_vec();
        let (url, server) = serve_response(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
                body.len()
            ),
            body.clone(),
        );
        let response = build_direct_client()
            .unwrap()
            .get(url)
            .send()
            .await
            .unwrap();
        assert!(is_event_stream(&response));

        let streamed = response
            .bytes_stream()
            .map(|chunk| chunk.unwrap())
            .collect::<Vec<_>>()
            .await
            .concat();

        assert_eq!(streamed, body);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn body_read_transport_errors_are_url_free_and_redacted() {
        let secret = "body-read-test-secret";
        let (url, server) = serve_response(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_owned(),
            format!("{secret}\r\n").into_bytes(),
        );
        let response = build_direct_client()
            .unwrap()
            .get(format!("{url}?credential={secret}"))
            .send()
            .await
            .unwrap();
        let error = response
            .bytes_stream()
            .next()
            .await
            .expect("malformed chunk framing must produce a body item")
            .unwrap_err();

        let error = transport_error(error, &[secret.to_owned()]);
        let rendered = error.to_string();

        assert!(!rendered.contains(secret));
        assert!(!rendered.contains("credential="));
        assert!(!rendered.contains('\n'));
        server.join().unwrap();
    }

    fn serve_response(headers: String, body: Vec<u8>) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/response", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request_head(&mut stream);
            stream.write_all(headers.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
        });
        (url, server)
    }

    fn serve_authorized_rejection(secret: &str) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/response", listener.local_addr().unwrap());
        let expected = format!("authorization: Bearer {secret}\r\n").to_ascii_lowercase();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request_head(&mut stream);
            assert!(
                String::from_utf8_lossy(&request)
                    .to_ascii_lowercase()
                    .contains(&expected)
            );
            stream
                .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 8\r\n\r\nrejected")
                .unwrap();
        });
        (url, server)
    }

    fn serve_authorized_malformed_body(secret: &str) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/response", listener.local_addr().unwrap());
        let expected = format!("authorization: Bearer {secret}\r\n").to_ascii_lowercase();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request_head(&mut stream);
            assert!(
                String::from_utf8_lossy(&request)
                    .to_ascii_lowercase()
                    .contains(&expected)
            );
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\ninvalid\r\n")
                .unwrap();
        });
        (url, server)
    }

    fn read_request_head(stream: &mut TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1_024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
        }
        request
    }
}
