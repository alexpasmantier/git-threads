//! Snippet derivation (SPEC.md §4.1).
//!
//! Snippets are derived, never stored: a pure function of a file's content
//! and an anchor's line range. They are what the re-anchoring algorithm (§4.2)
//! searches for, and what exporters materialize for consumers without git
//! object access — so the parameters here are normative, not tunable.

use crate::anchor::LineRange;
use sha2::{Digest, Sha256};

/// Context lines on each side of the target (diff's own default).
pub const CONTEXT_LINES: usize = 3;
/// Ranges longer than this store head + tail instead of every line.
pub const TRUNCATION_THRESHOLD: usize = 20;
/// Lines kept at each edge of a truncated target.
pub const TRUNCATION_EDGE: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snippet {
    /// 1-based line number of the first `before` line (or of the target's
    /// first line when the range starts at the top of the file).
    pub first_line: u32,
    pub before: Vec<String>,
    pub target: SnippetTarget,
    pub after: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnippetTarget {
    Full(Vec<String>),
    /// Ranges over [`TRUNCATION_THRESHOLD`] lines: first and last
    /// [`TRUNCATION_EDGE`] lines plus a SHA-256 of the full range (the
    /// target lines joined with `\n`, no trailing newline).
    Truncated {
        head: Vec<String>,
        tail: Vec<String>,
        omitted: usize,
        full_sha256: String,
    },
}

impl SnippetTarget {
    pub fn line_count(&self) -> usize {
        match self {
            SnippetTarget::Full(lines) => lines.len(),
            SnippetTarget::Truncated { head, tail, omitted, .. } => {
                head.len() + tail.len() + omitted
            }
        }
    }
}

/// Derive the snippet for `lines` out of `content`. Returns `None` when the
/// range does not fit the content (e.g. anchor and blob disagree — callers
/// should have verified the anchor's `blob` integrity check first).
pub fn derive_snippet(content: &str, lines: LineRange) -> Option<Snippet> {
    let all: Vec<&str> = content.lines().collect();
    let start = lines.start as usize;
    let end = lines.end as usize;
    if start < 1 || end < start || end > all.len() {
        return None;
    }
    let before_start = start.saturating_sub(CONTEXT_LINES + 1); // 0-based index
    let before: Vec<String> = all[before_start..start - 1].iter().map(|s| s.to_string()).collect();
    let after_end = (end + CONTEXT_LINES).min(all.len());
    let after: Vec<String> = all[end..after_end].iter().map(|s| s.to_string()).collect();

    let target_lines = &all[start - 1..end];
    let target = if target_lines.len() > TRUNCATION_THRESHOLD {
        let full = target_lines.join("\n");
        let digest = Sha256::digest(full.as_bytes());
        let full_sha256 = digest.iter().map(|b| format!("{b:02x}")).collect::<String>();
        SnippetTarget::Truncated {
            head: target_lines[..TRUNCATION_EDGE].iter().map(|s| s.to_string()).collect(),
            tail: target_lines[target_lines.len() - TRUNCATION_EDGE..]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            omitted: target_lines.len() - 2 * TRUNCATION_EDGE,
            full_sha256,
        }
    } else {
        SnippetTarget::Full(target_lines.iter().map(|s| s.to_string()).collect())
    };

    Some(Snippet {
        first_line: (before_start + 1) as u32,
        before,
        target,
        after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content(n: usize) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn middle_of_file_gets_full_context() {
        let snippet = derive_snippet(&content(30), LineRange { start: 10, end: 12 }).unwrap();
        assert_eq!(snippet.first_line, 7);
        assert_eq!(snippet.before, vec!["line 7", "line 8", "line 9"]);
        assert_eq!(
            snippet.target,
            SnippetTarget::Full(vec!["line 10".into(), "line 11".into(), "line 12".into()])
        );
        assert_eq!(snippet.after, vec!["line 13", "line 14", "line 15"]);
    }

    #[test]
    fn file_boundaries_shrink_context() {
        let snippet = derive_snippet(&content(5), LineRange { start: 1, end: 1 }).unwrap();
        assert_eq!(snippet.first_line, 1);
        assert!(snippet.before.is_empty());
        assert_eq!(snippet.after.len(), 3);

        let snippet = derive_snippet(&content(5), LineRange { start: 4, end: 5 }).unwrap();
        assert_eq!(snippet.before, vec!["line 1", "line 2", "line 3"]);
        assert!(snippet.after.is_empty());
    }

    #[test]
    fn long_ranges_truncate_with_hash() {
        let snippet = derive_snippet(&content(100), LineRange { start: 11, end: 40 }).unwrap();
        let SnippetTarget::Truncated { head, tail, omitted, full_sha256 } = snippet.target else {
            panic!("expected truncated target");
        };
        assert_eq!(head.first().map(String::as_str), Some("line 11"));
        assert_eq!(head.len(), 10);
        assert_eq!(tail.last().map(String::as_str), Some("line 40"));
        assert_eq!(tail.len(), 10);
        assert_eq!(omitted, 10);
        assert_eq!(full_sha256.len(), 64);

        // Exactly at the threshold stays full.
        let snippet = derive_snippet(&content(100), LineRange { start: 11, end: 30 }).unwrap();
        assert!(matches!(snippet.target, SnippetTarget::Full(ref l) if l.len() == 20));
    }

    #[test]
    fn out_of_range_is_none() {
        assert!(derive_snippet(&content(5), LineRange { start: 4, end: 6 }).is_none());
        assert!(derive_snippet("", LineRange { start: 1, end: 1 }).is_none());
    }
}
