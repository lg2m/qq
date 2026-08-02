//! Protocol-side support kit shared by the HTTP protocol adapters.
//!
//! Everything here passes the deletion test: it replaces implementation that
//! was repeated in at least two adapter files. Wire schemas, stream state
//! machines, and protocol-owned headers stay in the adapters.

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{ProviderError, ProviderErrorKind, http::HttpRejection, sanitize::sanitize_message};

/// Interprets a rejected HTTP exchange against the adapter's error envelope.
///
/// The message extracted from the decoded envelope — or, failing that, the
/// non-empty body text — is sanitized against the rejection's redactions;
/// `fallback` names the protocol for statuses without a canonical reason.
pub(crate) fn api_error<E: DeserializeOwned>(
    rejection: HttpRejection,
    fallback: &str,
    message: impl FnOnce(E) -> Option<String>,
) -> ProviderError {
    let status = rejection.status();
    let fallback = status.canonical_reason().unwrap_or(fallback).to_owned();
    let body_text = String::from_utf8_lossy(rejection.body());
    let message = serde_json::from_slice::<E>(rejection.body())
        .ok()
        .and_then(message)
        .or_else(|| (!body_text.trim().is_empty()).then(|| body_text.into_owned()))
        .map_or(fallback, |message| {
            sanitize_message(&message, rejection.redactions())
        });

    ProviderError::Api {
        status: status.as_u16(),
        message,
    }
}

/// Maps an HTTP-shaped status embedded in an error payload to an error kind.
pub(crate) fn status_error_kind(status: u16) -> ProviderErrorKind {
    match status {
        400 | 404 | 409 | 422 => ProviderErrorKind::InvalidRequest,
        401 | 403 => ProviderErrorKind::Authentication,
        429 => ProviderErrorKind::RateLimited,
        500..=599 => ProviderErrorKind::Unavailable,
        _ => ProviderErrorKind::Response,
    }
}

/// Reads a status carried as either a JSON number or a numeric string.
pub(crate) fn value_as_status(value: &Value) -> Option<u16> {
    value
        .as_u64()
        .and_then(|status| u16::try_from(status).ok())
        .or_else(|| value.as_str()?.parse().ok())
}
