//! Events (SPEC.md §2.1–§2.3).

use crate::anchor::Anchor;
use crate::canonical::{CanonicalError, to_canonical_json};
use crate::id::EventId;
use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EventError {
    #[error("event of type {kind:?} is missing required field {field:?}")]
    MissingField { kind: String, field: &'static str },
    #[error("event of type {kind:?} must not carry field {field:?}")]
    ForbiddenField { kind: String, field: &'static str },
    #[error("unsupported schema version {0} (expected 1)")]
    UnsupportedVersion(u32),
}

#[derive(Debug, Error)]
#[error(
    "invalid timestamp {0:?}: expected ISO 8601 UTC with second precision (YYYY-MM-DDTHH:MM:SSZ)"
)]
pub struct TimestampError(String);

/// ISO 8601 UTC timestamp with second precision, e.g. `2026-07-03T14:12:09Z`.
///
/// Stored as its fixed-width string form: lexicographic order on that form is
/// chronological order, which is what the state fold's `(ts, id)` ordering
/// relies on — so `Ord` derives directly from the inner string.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Timestamp(String);

impl Timestamp {
    pub fn parse(s: impl Into<String>) -> Result<Self, TimestampError> {
        let s = s.into();
        let b = s.as_bytes();
        let digits = |range: std::ops::Range<usize>| b[range].iter().all(u8::is_ascii_digit);
        let num = |range: std::ops::Range<usize>| -> u32 {
            s[range].parse().expect("digits checked above")
        };
        let structure_ok = b.len() == 20
            && digits(0..4)
            && b[4] == b'-'
            && digits(5..7)
            && b[7] == b'-'
            && digits(8..10)
            && b[10] == b'T'
            && digits(11..13)
            && b[13] == b':'
            && digits(14..16)
            && b[16] == b':'
            && digits(17..19)
            && b[19] == b'Z';
        if !structure_ok {
            return Err(TimestampError(s));
        }
        let ranges_ok = (1..=12).contains(&num(5..7))
            && (1..=31).contains(&num(8..10))
            && num(11..13) <= 23
            && num(14..16) <= 59
            && num(17..19) <= 59;
        if !ranges_ok {
            return Err(TimestampError(s));
        }
        Ok(Timestamp(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Timestamp {
    type Error = TimestampError;
    fn try_from(s: String) -> Result<Self, TimestampError> {
        Timestamp::parse(s)
    }
}

impl From<Timestamp> for String {
    fn from(ts: Timestamp) -> String {
        ts.0
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Same semantics as git commit authorship.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    pub name: String,
    pub email: String,
}

/// Event type. Unknown types are preserved as [`EventKind::Other`] so that
/// readers "MUST preserve events of unknown type when re-serializing"
/// (SPEC.md §2.1) holds by construction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum EventKind {
    Comment,
    Reply,
    Edit,
    Resolve,
    Delete,
    Move,
    Other(String),
}

impl From<String> for EventKind {
    fn from(s: String) -> Self {
        match s.as_str() {
            "comment" => EventKind::Comment,
            "reply" => EventKind::Reply,
            "edit" => EventKind::Edit,
            "resolve" => EventKind::Resolve,
            "delete" => EventKind::Delete,
            "move" => EventKind::Move,
            _ => EventKind::Other(s),
        }
    }
}

impl From<EventKind> for String {
    fn from(kind: EventKind) -> String {
        match kind {
            EventKind::Comment => "comment".into(),
            EventKind::Reply => "reply".into(),
            EventKind::Edit => "edit".into(),
            EventKind::Resolve => "resolve".into(),
            EventKind::Delete => "delete".into(),
            EventKind::Move => "move".into(),
            EventKind::Other(s) => s,
        }
    }
}

/// One event document (SPEC.md §2.2). All type-specific fields are optional at
/// the schema level; [`Event::validate`] enforces the per-type requirements.
/// Unknown fields round-trip through `extra`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub v: u32,
    #[serde(rename = "type")]
    pub kind: EventKind,
    pub author: Author,
    pub ts: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<EventId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<EventId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<Anchor>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Event {
    /// Canonical bytes of this event — what gets stored and hashed.
    pub fn canonical_json(&self) -> Result<Vec<u8>, CanonicalError> {
        to_canonical_json(self)
    }

    /// Content-addressed ID (SPEC.md §2.3).
    pub fn id(&self) -> Result<EventId, CanonicalError> {
        Ok(EventId::compute(&self.canonical_json()?))
    }

    /// Enforce per-type field requirements (table in SPEC.md §2.1). Events of
    /// unknown type are accepted as-is.
    pub fn validate(&self) -> Result<(), EventError> {
        if self.v != 1 {
            return Err(EventError::UnsupportedVersion(self.v));
        }
        let (required, forbidden): (&[&str], &[&str]) = match self.kind {
            EventKind::Comment => (&["body"], &["in_reply_to", "supersedes", "resolved", "anchor"]),
            EventKind::Reply => (&["body", "in_reply_to"], &["supersedes", "resolved", "anchor"]),
            EventKind::Edit => (&["body", "supersedes"], &["in_reply_to", "resolved", "anchor"]),
            EventKind::Resolve => (&["resolved"], &["body", "in_reply_to", "supersedes", "anchor"]),
            EventKind::Delete => (&["supersedes"], &["body", "in_reply_to", "resolved", "anchor"]),
            EventKind::Move => (&["anchor"], &["body", "in_reply_to", "supersedes", "resolved"]),
            EventKind::Other(_) => (&[], &[]),
        };
        let kind = String::from(self.kind.clone());
        let present = |field: &str| match field {
            "body" => self.body.is_some(),
            "in_reply_to" => self.in_reply_to.is_some(),
            "supersedes" => self.supersedes.is_some(),
            "resolved" => self.resolved.is_some(),
            "anchor" => self.anchor.is_some(),
            _ => unreachable!(),
        };
        for &field in required {
            if !present(field) {
                return Err(EventError::MissingField { kind: kind.clone(), field: leak(field) });
            }
        }
        for &field in forbidden {
            if present(field) {
                return Err(EventError::ForbiddenField { kind: kind.clone(), field: leak(field) });
            }
        }
        Ok(())
    }
}

fn leak(field: &str) -> &'static str {
    match field {
        "body" => "body",
        "in_reply_to" => "in_reply_to",
        "supersedes" => "supersedes",
        "resolved" => "resolved",
        "anchor" => "anchor",
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn author() -> Author {
        Author { name: "Alex Pasmant".into(), email: "alex.pasmant@gmail.com".into() }
    }

    pub(crate) fn comment(ts: &str, body: &str) -> Event {
        Event {
            v: 1,
            kind: EventKind::Comment,
            author: author(),
            ts: Timestamp::parse(ts).unwrap(),
            body: Some(body.into()),
            in_reply_to: None,
            supersedes: None,
            resolved: None,
            anchor: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn canonical_bytes_golden() {
        let event = comment("2026-07-03T14:12:09Z", "why not a BTreeMap here?");
        assert_eq!(
            String::from_utf8(event.canonical_json().unwrap()).unwrap(),
            r#"{"author":{"email":"alex.pasmant@gmail.com","name":"Alex Pasmant"},"body":"why not a BTreeMap here?","ts":"2026-07-03T14:12:09Z","type":"comment","v":1}"#
        );
    }

    #[test]
    fn id_golden_vector() {
        // Independently computed with `printf '%s' <canonical bytes> | sha256sum`.
        // Any implementation of the spec must reproduce this exact ID.
        let event = comment("2026-07-03T14:12:09Z", "why not a BTreeMap here?");
        assert_eq!(event.id().unwrap().as_str(), "0dfeaf728bb362b9b0b8f40fb20b17ff7c96b1cc");
    }

    #[test]
    fn id_ignores_json_field_order() {
        let a: Event = serde_json::from_str(
            r#"{"v":1,"type":"comment","author":{"name":"n","email":"e"},"ts":"2026-01-01T00:00:00Z","body":"b"}"#,
        )
        .unwrap();
        let b: Event = serde_json::from_str(
            r#"{"body":"b","ts":"2026-01-01T00:00:00Z","author":{"email":"e","name":"n"},"type":"comment","v":1}"#,
        )
        .unwrap();
        assert_eq!(a.id().unwrap(), b.id().unwrap());
    }

    #[test]
    fn unknown_fields_and_types_round_trip() {
        let raw = r#"{"v":1,"type":"reaction","author":{"name":"n","email":"e"},"ts":"2026-01-01T00:00:00Z","emoji":"+1"}"#;
        let event: Event = serde_json::from_str(raw).unwrap();
        assert_eq!(event.kind, EventKind::Other("reaction".into()));
        assert!(event.validate().is_ok());
        assert_eq!(
            String::from_utf8(event.canonical_json().unwrap()).unwrap(),
            r#"{"author":{"email":"e","name":"n"},"emoji":"+1","ts":"2026-01-01T00:00:00Z","type":"reaction","v":1}"#
        );
    }

    #[test]
    fn validate_enforces_field_table() {
        let mut event = comment("2026-01-01T00:00:00Z", "b");
        event.validate().unwrap();
        event.resolved = Some(true);
        assert!(matches!(
            event.validate(),
            Err(EventError::ForbiddenField { field: "resolved", .. })
        ));
        event.resolved = None;
        event.kind = EventKind::Reply;
        assert!(matches!(
            event.validate(),
            Err(EventError::MissingField { field: "in_reply_to", .. })
        ));

        let mut event = comment("2026-01-01T00:00:00Z", "b");
        event.kind = EventKind::Move;
        event.body = None;
        assert!(matches!(event.validate(), Err(EventError::MissingField { field: "anchor", .. })));
    }

    #[test]
    fn timestamp_validation() {
        assert!(Timestamp::parse("2026-07-03T14:12:09Z").is_ok());
        for bad in [
            "2026-07-03T14:12:09",    // missing Z
            "2026-07-03 14:12:09Z",   // space separator
            "2026-13-03T14:12:09Z",   // month 13
            "2026-07-03T24:12:09Z",   // hour 24
            "2026-07-03T14:12:09.5Z", // sub-second
            "2026-7-3T14:12:09Z",     // non-fixed-width
        ] {
            assert!(Timestamp::parse(bad).is_err(), "should reject {bad:?}");
        }
    }
}
