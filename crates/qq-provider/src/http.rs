use std::{net::IpAddr, time::Duration};

use reqwest::Url;

use crate::ProviderError;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(300);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);

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
