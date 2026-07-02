//! Identifiers (SPEC.md §2.3).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum IdError {
    #[error("invalid event id {0:?}: expected 40 lowercase hex characters")]
    InvalidEventId(String),
    #[error("invalid git object id {0:?}: expected 40 or 64 lowercase hex characters")]
    InvalidGitOid(String),
}

fn is_lowercase_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Content-addressed event identifier: SHA-256 of the event's canonical JSON,
/// truncated to 40 lowercase hex characters. Doubles as the event's filename
/// in the storage tree. A thread's ID is the ID of its root `comment`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EventId(String);

/// A thread is identified by its root comment's event ID.
pub type ThreadId = EventId;

impl EventId {
    pub fn compute(canonical_json: &[u8]) -> Self {
        let digest = Sha256::digest(canonical_json);
        let mut hex = String::with_capacity(40);
        for byte in &digest[..20] {
            hex.push_str(&format!("{byte:02x}"));
        }
        EventId(hex)
    }

    pub fn from_hex(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        if s.len() == 40 && is_lowercase_hex(&s) {
            Ok(EventId(s))
        } else {
            Err(IdError::InvalidEventId(s))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for EventId {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, IdError> {
        EventId::from_hex(s)
    }
}

impl From<EventId> for String {
    fn from(id: EventId) -> String {
        id.0
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A git object ID (commit or blob), as referenced from anchors. Accepts both
/// SHA-1 (40) and SHA-256 (64) hex forms so the format survives git's hash
/// transition.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GitOid(String);

impl GitOid {
    pub fn from_hex(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        if (s.len() == 40 || s.len() == 64) && is_lowercase_hex(&s) {
            Ok(GitOid(s))
        } else {
            Err(IdError::InvalidGitOid(s))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for GitOid {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, IdError> {
        GitOid::from_hex(s)
    }
}

impl From<GitOid> for String {
    fn from(oid: GitOid) -> String {
        oid.0
    }
}

impl fmt::Display for GitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_id_is_40_lowercase_hex() {
        let id = EventId::compute(b"{}");
        assert_eq!(id.as_str().len(), 40);
        assert!(is_lowercase_hex(id.as_str()));
    }

    #[test]
    fn rejects_uppercase_and_wrong_length() {
        assert!(EventId::from_hex("ABC").is_err());
        assert!(EventId::from_hex("a".repeat(39)).is_err());
        assert!(EventId::from_hex("A".repeat(40)).is_err());
        assert!(GitOid::from_hex("a".repeat(41)).is_err());
        assert!(GitOid::from_hex("a".repeat(64)).is_ok());
    }
}
