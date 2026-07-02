//! Anchors (SPEC.md §3).

use crate::id::GitOid;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AnchorError {
    #[error("anchor of kind {kind:?} is missing required field {field:?}")]
    MissingField { kind: String, field: &'static str },
    #[error("anchor of kind {kind:?} must not carry field {field:?}")]
    ForbiddenField { kind: String, field: &'static str },
    #[error("invalid range: start must be >= 1 and end >= start (got {start}..{end})")]
    InvalidRange { start: u32, end: u32 },
    #[error("old_path requires path")]
    OldPathWithoutPath,
    #[error("unsupported schema version {0} (expected 1)")]
    UnsupportedVersion(u32),
}

/// The two commits whose diff the comment was made against (SPEC.md §3.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffRef {
    pub base: GitOid,
    pub head: GitOid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Old,
    New,
}

/// 1-based, inclusive, file coordinates on the anchor's `side` — never
/// diff/patch positions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

/// Reserved sub-line span (SPEC.md §3.1); clients MAY ignore.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColRange {
    pub start: u32,
    pub end: u32,
}

/// Anchor kind. Unknown kinds are preserved for forward compatibility, same
/// policy as unknown event types.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum AnchorKind {
    Commit,
    File,
    Range,
    Other(String),
}

impl From<String> for AnchorKind {
    fn from(s: String) -> Self {
        match s.as_str() {
            "commit" => AnchorKind::Commit,
            "file" => AnchorKind::File,
            "range" => AnchorKind::Range,
            _ => AnchorKind::Other(s),
        }
    }
}

impl From<AnchorKind> for String {
    fn from(kind: AnchorKind) -> String {
        match kind {
            AnchorKind::Commit => "commit".into(),
            AnchorKind::File => "file".into(),
            AnchorKind::Range => "range".into(),
            AnchorKind::Other(s) => s,
        }
    }
}

/// A thread's immutable `anchor.json` (SPEC.md §3). Anchors are always valid
/// against their own `diff`; displaying a thread elsewhere is re-anchoring,
/// which is computed, never stored.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    pub v: u32,
    pub kind: AnchorKind,
    pub diff: DiffRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<Side>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<LineRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<GitOid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<ColRange>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Anchor {
    /// Enforce per-kind field requirements (SPEC.md §3.1): `commit` carries no
    /// location fields, `file` locates a file version without lines, `range`
    /// requires the full location. Anchors of unknown kind are accepted as-is.
    pub fn validate(&self) -> Result<(), AnchorError> {
        if self.v != 1 {
            return Err(AnchorError::UnsupportedVersion(self.v));
        }
        if self.old_path.is_some() && self.path.is_none() {
            return Err(AnchorError::OldPathWithoutPath);
        }
        let kind = String::from(self.kind.clone());
        let (required, forbidden): (&[&str], &[&str]) = match self.kind {
            AnchorKind::Commit => (&[], &["path", "old_path", "side", "lines", "blob", "cols"]),
            AnchorKind::File => (&["path", "side", "blob"], &["lines", "cols"]),
            AnchorKind::Range => (&["path", "side", "lines", "blob"], &[]),
            AnchorKind::Other(_) => (&[], &[]),
        };
        let present = |field: &str| match field {
            "path" => self.path.is_some(),
            "old_path" => self.old_path.is_some(),
            "side" => self.side.is_some(),
            "lines" => self.lines.is_some(),
            "blob" => self.blob.is_some(),
            "cols" => self.cols.is_some(),
            _ => unreachable!(),
        };
        for &field in required {
            if !present(field) {
                return Err(AnchorError::MissingField {
                    kind: kind.clone(),
                    field: leak_field(field),
                });
            }
        }
        for &field in forbidden {
            if present(field) {
                return Err(AnchorError::ForbiddenField {
                    kind: kind.clone(),
                    field: leak_field(field),
                });
            }
        }
        for range in [
            self.lines.map(|l| (l.start, l.end)),
            self.cols.map(|c| (c.start, c.end)),
        ]
        .into_iter()
        .flatten()
        {
            let (start, end) = range;
            if start < 1 || end < start {
                return Err(AnchorError::InvalidRange { start, end });
            }
        }
        Ok(())
    }
}

fn leak_field(field: &str) -> &'static str {
    match field {
        "path" => "path",
        "old_path" => "old_path",
        "side" => "side",
        "lines" => "lines",
        "blob" => "blob",
        "cols" => "cols",
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range_anchor() -> Anchor {
        Anchor {
            v: 1,
            kind: AnchorKind::Range,
            diff: DiffRef {
                base: GitOid::from_hex("a".repeat(40)).unwrap(),
                head: GitOid::from_hex("b".repeat(40)).unwrap(),
            },
            path: Some("src/parser.rs".into()),
            old_path: None,
            side: Some(Side::New),
            lines: Some(LineRange { start: 120, end: 128 }),
            blob: Some(GitOid::from_hex("c".repeat(40)).unwrap()),
            cols: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn serde_matches_spec_example_shape() {
        let anchor = range_anchor();
        anchor.validate().unwrap();
        let json = serde_json::to_value(&anchor).unwrap();
        assert_eq!(json["kind"], "range");
        assert_eq!(json["side"], "new");
        assert_eq!(json["lines"]["start"], 120);
        assert!(json.get("old_path").is_none());
        let back: Anchor = serde_json::from_value(json).unwrap();
        assert_eq!(back, anchor);
    }

    #[test]
    fn validate_per_kind_rules() {
        let mut anchor = range_anchor();
        anchor.lines = None;
        assert!(matches!(
            anchor.validate(),
            Err(AnchorError::MissingField { field: "lines", .. })
        ));

        let mut anchor = range_anchor();
        anchor.kind = AnchorKind::Commit;
        assert!(matches!(
            anchor.validate(),
            Err(AnchorError::ForbiddenField { field: "path", .. })
        ));

        let mut anchor = range_anchor();
        anchor.kind = AnchorKind::File;
        anchor.lines = None;
        anchor.validate().unwrap();

        let mut anchor = range_anchor();
        anchor.lines = Some(LineRange { start: 0, end: 5 });
        assert!(matches!(anchor.validate(), Err(AnchorError::InvalidRange { .. })));
        anchor.lines = Some(LineRange { start: 9, end: 3 });
        assert!(matches!(anchor.validate(), Err(AnchorError::InvalidRange { .. })));
    }

    #[test]
    fn unknown_kind_is_preserved() {
        let raw = r#"{"v":1,"kind":"symbol","diff":{"base":"__A__","head":"__B__"},"symbol_id":"x"}"#
            .replace("__A__", &"a".repeat(40))
            .replace("__B__", &"b".repeat(40));
        let anchor: Anchor = serde_json::from_str(&raw).unwrap();
        assert_eq!(anchor.kind, AnchorKind::Other("symbol".into()));
        anchor.validate().unwrap();
        let json = serde_json::to_value(&anchor).unwrap();
        assert_eq!(json["symbol_id"], "x");
    }
}
