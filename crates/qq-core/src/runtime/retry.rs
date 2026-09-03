use std::time::Duration;

use qq_provider::ProviderErrorKind;

/// Turn-level retry for transient provider failures.
///
/// A model turn whose provider stream fails with a transient error
/// (`Unavailable`, `RateLimited`, `Transport`) — or ends without a terminal
/// event — is re-issued with exponential backoff instead of failing the run,
/// but only while nothing user-visible has streamed, so a retry can never
/// duplicate output. Non-transient failures (authentication, configuration,
/// invalid requests, protocol violations) always fail fast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnRetryPolicy {
    max_attempts: u32,
    base_delay: Duration,
    max_delay: Duration,
}

impl Default for TurnRetryPolicy {
    /// Eight attempts with 1 s → 60 s exponential backoff (about two minutes
    /// of waiting in total), sized to ride out provider overload blips
    /// without surrendering the run.
    fn default() -> Self {
        Self {
            max_attempts: 8,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
        }
    }
}

impl TurnRetryPolicy {
    /// Fails the run on the first provider error of any kind.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            max_attempts: 1,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }

    pub(crate) const fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    pub(crate) const fn base_delay(&self) -> Duration {
        self.base_delay
    }

    pub(crate) const fn max_delay(&self) -> Duration {
        self.max_delay
    }

    pub(crate) fn delay(&self, completed_attempts: u32) -> Duration {
        let exponent = completed_attempts.saturating_sub(1).min(31);
        self.base_delay
            .saturating_mul(1_u32 << exponent)
            .min(self.max_delay)
    }
}

pub(crate) const fn is_transient_provider_failure(kind: ProviderErrorKind) -> bool {
    matches!(
        kind,
        ProviderErrorKind::Unavailable
            | ProviderErrorKind::RateLimited
            | ProviderErrorKind::Transport
    )
}

/// Appends the attempt count to a terminal failure message when retries ran.
pub(crate) fn attempts_message(message: String, attempts: u32) -> String {
    if attempts > 1 {
        format!("{message} (gave up after {attempts} attempts with backoff)")
    } else {
        message
    }
}
