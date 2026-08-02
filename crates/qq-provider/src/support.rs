//! Protocol-side support kit shared by the HTTP protocol adapters.
//!
//! Everything here passes the deletion test: it replaces implementation that
//! was repeated in at least two adapter files. Wire schemas, stream state
//! machines, and protocol-owned headers stay in the adapters.

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    ProviderError, ProviderErrorKind, ProviderUsage, http::HttpRejection,
    sanitize::sanitize_message,
};

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

/// Attributes streamed tool-call fragments and stops to their call ids.
///
/// Keyed by whatever the protocol streams — a function-call item id, a
/// tool-call array index, or a content-block index. Backed by an ordered map
/// so [`ToolCallLedger::drain`] completes calls in key order (Chat
/// Completions drains open calls when the choice finishes with `tool_calls`).
pub(crate) struct ToolCallLedger<K> {
    calls: BTreeMap<K, String>,
    reused_key: &'static str,
    unknown_key: &'static str,
}

impl<K: Ord> ToolCallLedger<K> {
    /// Creates an empty ledger with the protocol's attribution errors.
    pub(crate) fn new(reused_key: &'static str, unknown_key: &'static str) -> Self {
        Self {
            calls: BTreeMap::new(),
            reused_key,
            unknown_key,
        }
    }

    /// Records a started call, rejecting a key the stream already used.
    pub(crate) fn insert(&mut self, key: K, id: String) -> Result<(), ProviderError> {
        if self.calls.insert(key, id).is_some() {
            return Err(ProviderError::Protocol(self.reused_key.to_owned()));
        }
        Ok(())
    }

    /// Resolves the call id an argument fragment belongs to.
    pub(crate) fn get(&self, key: &K) -> Result<&str, ProviderError> {
        self.calls
            .get(key)
            .map(String::as_str)
            .ok_or_else(|| ProviderError::Protocol(self.unknown_key.to_owned()))
    }

    /// Closes the call the stopped key started, if any.
    pub(crate) fn remove(&mut self, key: &K) -> Option<String> {
        self.calls.remove(key)
    }

    /// Completes every open call in key order.
    pub(crate) fn drain(&mut self) -> impl Iterator<Item = String> {
        std::mem::take(&mut self.calls).into_values()
    }
}

/// A set-once slot for the usage a stream reports.
pub(crate) struct UsageOnce {
    usage: Option<ProviderUsage>,
    duplicate: &'static str,
}

impl UsageOnce {
    /// Creates an empty slot with the protocol's duplicate-report error.
    pub(crate) fn new(duplicate: &'static str) -> Self {
        Self {
            usage: None,
            duplicate,
        }
    }

    /// Stores the reported usage, rejecting a second report.
    pub(crate) fn set(&mut self, usage: ProviderUsage) -> Result<(), ProviderError> {
        if self.usage.replace(usage).is_some() {
            return Err(ProviderError::Protocol(self.duplicate.to_owned()));
        }
        Ok(())
    }

    /// The stored usage, for protocols that update it cumulatively.
    pub(crate) fn stored_mut(&mut self) -> Option<&mut ProviderUsage> {
        self.usage.as_mut()
    }

    /// Consumes the slot when the stream completes.
    pub(crate) fn finish(self) -> Option<ProviderUsage> {
        self.usage
    }
}

/// Subtracts provider-reported cached tokens from the total input tokens.
///
/// Providers report cache reads inside the prompt/input total; the neutral
/// model carries them separately, so an underflowing report is a protocol
/// error rather than a silent wrap.
pub(crate) fn subtract_cached_input_tokens(
    total: u64,
    cached: u64,
    underflow: &'static str,
) -> Result<u64, ProviderError> {
    total
        .checked_sub(cached)
        .ok_or_else(|| ProviderError::Protocol(underflow.to_owned()))
}
