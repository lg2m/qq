use std::fmt;

use serde::{Deserialize, Serialize};

/// A literal secret embedded in configuration.
///
/// Formatting is always redacted; callers must opt in to exposing the value.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretLiteral(String);

impl SecretLiteral {
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretLiteral {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// A provider-neutral reference to secret material.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SecretRef {
    Env(String),
    Stored(String),
    Value(SecretLiteral),
}

impl SecretRef {
    #[must_use]
    pub const fn is_literal(&self) -> bool {
        matches!(self, Self::Value(_))
    }
}

impl fmt::Debug for SecretRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Env(name) => formatter.debug_tuple("Env").field(name).finish(),
            Self::Stored(name) => formatter.debug_tuple("Stored").field(name).finish(),
            Self::Value(_) => formatter.write_str("Value(<redacted>)"),
        }
    }
}
