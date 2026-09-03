//! Identity vocabulary for compiled agent plans.
//!
//! These types name a plan without describing it. The behavioral descriptor
//! itself lives in the runtime crate; only its digest and the opaque
//! credential epoch are shared vocabulary because later protocol revisions
//! carry them on accepted runs and snapshots. Neither type is on the wire yet.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ContentHash;

/// SHA-256 of one canonical encoding of a secret-free agent plan descriptor.
/// Two runs with equal digests were admitted with behaviorally identical
/// plans: same provider shape, model, prompt version, instructions, tool
/// catalog, policy, and adapter build. Credential rotation does not change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentPlanDigest(ContentHash);

impl AgentPlanDigest {
    #[must_use]
    pub const fn from_hash(hash: ContentHash) -> Self {
        Self(hash)
    }

    #[must_use]
    pub const fn as_hash(&self) -> &ContentHash {
        &self.0
    }
}

impl fmt::Display for AgentPlanDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}

/// Opaque, monotonic identity of the credential set a plan was compiled
/// against. Owned by the credential store: every durable credential mutation
/// advances it, so a cached plan can be reauthorized after rotation without
/// hashing secret material or changing the plan's behavioral digest.
///
/// The value carries no meaning beyond equality and ordering, and `0` means
/// "no credential store contributed" (embedded runtimes and test fixtures).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct CredentialEpoch(u64);

impl CredentialEpoch {
    /// The epoch of a runtime that resolves no stored credentials.
    pub const NONE: Self = Self(0);

    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for CredentialEpoch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_digest_serializes_as_the_hex_hash() {
        let digest = AgentPlanDigest::from_hash(ContentHash::from_bytes([0xab; 32]));
        let json = serde_json::to_string(&digest).unwrap();
        assert_eq!(json, format!("\"{}\"", "ab".repeat(32)));
        let decoded: AgentPlanDigest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, digest);
        assert_eq!(digest.to_string(), "ab".repeat(32));
    }

    #[test]
    fn credential_epoch_is_a_transparent_ordered_integer() {
        let epoch = CredentialEpoch::new(7);
        assert_eq!(serde_json::to_string(&epoch).unwrap(), "7");
        assert_eq!(
            serde_json::from_str::<CredentialEpoch>("8").unwrap(),
            CredentialEpoch::new(8)
        );
        assert!(CredentialEpoch::NONE < epoch);
        assert_eq!(CredentialEpoch::default(), CredentialEpoch::NONE);
        assert_eq!(epoch.get(), 7);
    }
}
