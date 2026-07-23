use std::{net::IpAddr, time::Duration};

use futures_util::StreamExt;
use reqwest::{Url, header::CONTENT_TYPE};

use crate::{ProviderError, sanitize::sanitize_message};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(300);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const ERROR_BODY_BYTES_LIMIT: usize = 16 * 1_024;

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
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    use super::*;

    #[tokio::test]
    async fn error_body_is_bounded_to_sixteen_kibibytes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1_024];
            let _ = stream.read(&mut request).unwrap();
            let body = vec![b'x'; ERROR_BODY_BYTES_LIMIT + 1_024];
            write!(
                stream,
                "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        });
        let response = build_direct_client()
            .unwrap()
            .get(format!("http://{address}/error"))
            .send()
            .await
            .unwrap();

        let body = read_error_body(response).await;

        assert_eq!(body.len(), ERROR_BODY_BYTES_LIMIT);
        assert!(body.iter().all(|byte| *byte == b'x'));
        server.join().unwrap();
    }
}
