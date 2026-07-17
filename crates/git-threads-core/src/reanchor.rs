//! Re-anchoring (SPEC.md §4.2) — the pure half of the algorithm.
//!
//! Step 1 (blob identity) and candidate discovery (rename detection) need
//! git object access and live in the CLI crate. This module implements the
//! content-matching steps 2–3: finding a snippet's unique new position in a
//! candidate file.

use crate::anchor::LineRange;
use crate::snippet::{Snippet, SnippetTarget};
use sha2::{Digest, Sha256};
use std::fmt;

/// Fuzz levels drop up to this many outer context lines per side (§4.2
/// step 3, `git apply` fuzz semantics).
pub const MAX_FUZZ: usize = 3;

/// Where the algorithm stopped (SPEC.md §4.2). Step 4 (outdated) is the absence
/// of a match, represented by the caller, not a status here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReanchorStatus {
    /// Step 1: the anchored blob itself is in the target tree; lines map 1:1.
    Exact,
    /// Step 2: the full snippet matched verbatim at a unique position.
    Relocated,
    /// Step 3: unique match after dropping `f` outer context lines per side
    /// and/or comparing lines trailing-whitespace-insensitively.
    Fuzzy(u8),
}

impl fmt::Display for ReanchorStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReanchorStatus::Exact => write!(f, "exact"),
            ReanchorStatus::Relocated => write!(f, "relocated"),
            ReanchorStatus::Fuzzy(level) => write!(f, "fuzzy({level})"),
        }
    }
}

/// Find the unique new position of `snippet` in `content` (§4.2 steps 2–3).
/// Single-file convenience over [`locate_snippet_among`].
pub fn locate_snippet(snippet: &Snippet, content: &str) -> Option<(LineRange, ReanchorStatus)> {
    locate_snippet_among(snippet, [content]).map(|(_, lines, status)| (lines, status))
}

/// Find the unique new position of `snippet` among `candidates` (§4.2 steps
/// 2–3), returning the index of the candidate that matched. Uniqueness is
/// judged across all candidates at once: a match in each of two files is as
/// ambiguous as two matches in one file.
///
/// Byte-exact comparison is tried first (fuzz 0 → `Relocated`, fuzz 1–3 →
/// `Fuzzy`), then trailing-whitespace-insensitive comparison (fuzz 0–3, all
/// `Fuzzy`). Two matches at a level fail that level, and — since raising the
/// fuzz level or dropping whitespace-sensitivity only relaxes the pattern —
/// every relaxation of an ambiguous level is skipped ("never pick-first"):
/// byte-exact ambiguity at fuzz `f` rules out byte-exact levels above `f`
/// and whitespace-insensitive levels at or above `f`; whitespace-insensitive
/// ambiguity ends the search. The axes are not totally ordered, though —
/// byte-exact ambiguity at fuzz 3 (a bare target duplicated) says nothing
/// about whitespace-insensitive fuzz 0, whose fuller context can still
/// disambiguate — so the whitespace pass keeps running below the ambiguous
/// fuzz level.
pub fn locate_snippet_among<'a>(
    snippet: &Snippet,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<(usize, LineRange, ReanchorStatus)> {
    let files: Vec<Vec<&str>> = candidates.into_iter().map(|c| c.lines().collect()).collect();
    // A level's verdict across all candidate files: ambiguity in any file, or
    // unique matches in two files, is ambiguity of the level.
    let level = |fuzz: usize, ws_insensitive: bool| -> Level {
        let mut unique: Option<(usize, usize, usize)> = None;
        for (index, lines) in files.iter().enumerate() {
            match find_matches(snippet, lines, fuzz, ws_insensitive) {
                Matches::None => {}
                Matches::Ambiguous => return Level::Ambiguous,
                Matches::Unique { pattern_start, before_len } => {
                    if unique.is_some() {
                        return Level::Ambiguous;
                    }
                    unique = Some((index, pattern_start, before_len));
                }
            }
        }
        match unique {
            Some((candidate, pattern_start, before_len)) => {
                Level::Unique { candidate, pattern_start, before_len }
            }
            None => Level::None,
        }
    };
    let position = |pattern_start: usize, before_len: usize, status: ReanchorStatus| {
        let start = (pattern_start + before_len) as u32 + 1;
        let end = start + snippet.target.line_count() as u32 - 1;
        (LineRange { start, end }, status)
    };

    // Byte-exact pass; on ambiguity at fuzz `f`, only whitespace-insensitive
    // levels below `f` can still hold a unique match.
    let mut ws_levels = MAX_FUZZ + 1;
    for fuzz in 0..=MAX_FUZZ {
        match level(fuzz, false) {
            Level::None => {}
            Level::Ambiguous => {
                ws_levels = fuzz;
                break;
            }
            Level::Unique { candidate, pattern_start, before_len } => {
                let status = if fuzz == 0 {
                    ReanchorStatus::Relocated
                } else {
                    ReanchorStatus::Fuzzy(fuzz as u8)
                };
                let (lines, status) = position(pattern_start, before_len, status);
                return Some((candidate, lines, status));
            }
        }
    }
    for fuzz in 0..ws_levels {
        match level(fuzz, true) {
            Level::None => {}
            Level::Ambiguous => break,
            Level::Unique { candidate, pattern_start, before_len } => {
                let (lines, status) =
                    position(pattern_start, before_len, ReanchorStatus::Fuzzy(fuzz as u8));
                return Some((candidate, lines, status));
            }
        }
    }
    None
}

/// One (fuzz, comparison-mode) level's verdict across all candidate files.
enum Level {
    None,
    Unique { candidate: usize, pattern_start: usize, before_len: usize },
    Ambiguous,
}

enum Matches {
    None,
    Unique { pattern_start: usize, before_len: usize },
    Ambiguous,
}

fn find_matches(snippet: &Snippet, lines: &[&str], fuzz: usize, ws_insensitive: bool) -> Matches {
    let eq = |a: &str, b: &str| {
        if ws_insensitive { a.trim_end() == b.trim_end() } else { a == b }
    };
    // "Outer" context lines are the ones farthest from the target.
    let before = &snippet.before[fuzz.min(snippet.before.len())..];
    let after = &snippet.after[..snippet.after.len() - fuzz.min(snippet.after.len())];

    // The pattern is two matched runs around an unconstrained gap (empty for
    // full targets). A truncated target's middle is only line-counted, so in
    // byte-exact mode the stored hash must confirm the full range.
    let (part_a, gap, part_b, full_sha256): (Vec<&str>, usize, Vec<&str>, Option<&str>) =
        match &snippet.target {
            SnippetTarget::Full(target) => (
                before.iter().chain(target).map(String::as_str).collect(),
                0,
                after.iter().map(String::as_str).collect(),
                None,
            ),
            SnippetTarget::Truncated { head, tail, omitted, full_sha256 } => (
                before.iter().chain(head).map(String::as_str).collect(),
                *omitted,
                tail.iter().chain(after).map(String::as_str).collect(),
                (!ws_insensitive).then_some(full_sha256.as_str()),
            ),
        };
    let total = part_a.len() + gap + part_b.len();
    if total > lines.len() {
        return Matches::None;
    }

    let target_len = snippet.target.line_count();
    let mut unique: Option<usize> = None;
    'pos: for i in 0..=lines.len() - total {
        for (j, expected) in part_a.iter().enumerate() {
            if !eq(lines[i + j], expected) {
                continue 'pos;
            }
        }
        for (j, expected) in part_b.iter().enumerate() {
            if !eq(lines[i + part_a.len() + gap + j], expected) {
                continue 'pos;
            }
        }
        if let Some(expected) = full_sha256 {
            let target_start = i + before.len();
            let range = lines[target_start..target_start + target_len].join("\n");
            let digest = Sha256::digest(range.as_bytes());
            let actual = digest.iter().map(|b| format!("{b:02x}")).collect::<String>();
            if actual != expected {
                continue 'pos;
            }
        }
        if unique.is_some() {
            return Matches::Ambiguous;
        }
        unique = Some(i);
    }
    match unique {
        Some(pattern_start) => Matches::Unique { pattern_start, before_len: before.len() },
        None => Matches::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snippet::derive_snippet;

    fn lines(range: std::ops::RangeInclusive<usize>) -> String {
        range.map(|i| format!("line {i}\n")).collect()
    }

    fn snippet_of(content: &str, start: u32, end: u32) -> Snippet {
        derive_snippet(content, LineRange { start, end }).unwrap()
    }

    #[test]
    fn verbatim_match_at_new_position_is_relocated() {
        let original = lines(1..=20);
        let snippet = snippet_of(&original, 10, 12);
        // Five lines inserted above: everything shifts down by five.
        let edited = format!("{}{}", "new\n".repeat(5), original);
        let (found, status) = locate_snippet(&snippet, &edited).unwrap();
        assert_eq!(status, ReanchorStatus::Relocated);
        assert_eq!(found, LineRange { start: 15, end: 17 });
    }

    #[test]
    fn changed_context_line_matches_with_fuzz() {
        let original = lines(1..=20);
        let snippet = snippet_of(&original, 10, 12);
        // Outermost after-context line (line 15) rewritten: fuzz 1 drops it.
        let edited = original.replace("line 15\n", "rewritten\n");
        let (found, status) = locate_snippet(&snippet, &edited).unwrap();
        assert_eq!(status, ReanchorStatus::Fuzzy(1));
        assert_eq!(found, LineRange { start: 10, end: 12 });

        // An *inner* context line changed: only fuzz 3 (all context dropped,
        // bare target) still matches.
        let edited = original.replace("line 13\n", "rewritten\n");
        let (found, status) = locate_snippet(&snippet, &edited).unwrap();
        assert_eq!(status, ReanchorStatus::Fuzzy(3));
        assert_eq!(found, LineRange { start: 10, end: 12 });
    }

    #[test]
    fn trailing_whitespace_change_matches_as_fuzzy_zero() {
        let original = lines(1..=20);
        let snippet = snippet_of(&original, 10, 12);
        let edited = original.replace("line 11\n", "line 11   \n");
        let (found, status) = locate_snippet(&snippet, &edited).unwrap();
        assert_eq!(status, ReanchorStatus::Fuzzy(0));
        assert_eq!(found, LineRange { start: 10, end: 12 });
    }

    #[test]
    fn ambiguity_is_failure_not_pick_first() {
        let original = lines(1..=20);
        let snippet = snippet_of(&original, 10, 12);
        // The whole snippet region appears twice.
        let region: String = lines(7..=15);
        let edited = format!("{original}{region}");
        assert!(locate_snippet(&snippet, &edited).is_none());
    }

    #[test]
    fn deleted_target_is_no_match() {
        let original = lines(1..=20);
        let snippet = snippet_of(&original, 10, 12);
        let edited =
            original.replace("line 10\n", "").replace("line 11\n", "").replace("line 12\n", "");
        assert!(locate_snippet(&snippet, &edited).is_none());
    }

    #[test]
    fn file_boundary_snippet_relocates() {
        let original = lines(1..=5);
        let snippet = snippet_of(&original, 1, 1); // no before-context at all
        let edited = format!("{}{}", "new\n".repeat(3), original);
        let (found, status) = locate_snippet(&snippet, &edited).unwrap();
        assert_eq!(status, ReanchorStatus::Relocated);
        assert_eq!(found, LineRange { start: 4, end: 4 });
    }

    #[test]
    fn byte_exact_ambiguity_does_not_abort_the_whitespace_pass() {
        // The anchored context carries trailing whitespace that a formatter
        // has since stripped, and the (unchanged) target line also appears
        // bare elsewhere. Byte-exact matching fails at fuzz 0-2 (whitespace)
        // and is ambiguous at fuzz 3 (two bare targets) — but that ambiguity
        // says nothing about the whitespace-insensitive full-context level,
        // which still identifies the true location uniquely.
        let original = "before one   \nbefore two \nbefore three  \nthe target\nafter one \nafter two   \nafter three \n";
        let snippet = snippet_of(original, 4, 4);
        let formatted = "before one\nbefore two\nbefore three\nthe target\nafter one\nafter two\nafter three\nunrelated\nthe target\nsomething else\n";
        let (found, status) =
            locate_snippet(&snippet, formatted).expect("unique ws-insensitive match");
        assert_eq!(status, ReanchorStatus::Fuzzy(0));
        assert_eq!(found, LineRange { start: 4, end: 4 });
    }

    #[test]
    fn matches_split_across_candidates_are_ambiguous() {
        let original = lines(1..=20);
        let snippet = snippet_of(&original, 10, 12);
        // The snippet region relocated within one candidate: unique.
        let shifted = format!("{}{}", "new\n".repeat(5), original);
        let (index, found, status) =
            locate_snippet_among(&snippet, [shifted.as_str(), "unrelated\n"]).unwrap();
        assert_eq!((index, status), (0, ReanchorStatus::Relocated));
        assert_eq!(found, LineRange { start: 15, end: 17 });

        // The same region present in both candidates: as ambiguous as two
        // matches in one file — never pick-first.
        assert!(locate_snippet_among(&snippet, [shifted.as_str(), original.as_str()]).is_none());
    }

    #[test]
    fn truncated_target_requires_hash_match_for_relocation() {
        let original = lines(1..=60);
        let snippet = snippet_of(&original, 10, 40); // > threshold: truncated
        let shifted = format!("{}{}", "new\n".repeat(4), original);
        let (found, status) = locate_snippet(&snippet, &shifted).unwrap();
        assert_eq!(status, ReanchorStatus::Relocated);
        assert_eq!(found, LineRange { start: 14, end: 44 });

        // A middle line (invisible to head/tail matching) changed: the hash
        // rejects byte-exact relocation and only the ws-insensitive pass —
        // which cannot verify the hash — accepts, downgrading to fuzzy.
        let tampered = shifted.replace("line 25\n", "tampered\n");
        let (found, status) = locate_snippet(&snippet, &tampered).unwrap();
        assert_eq!(status, ReanchorStatus::Fuzzy(0));
        assert_eq!(found, LineRange { start: 14, end: 44 });
    }
}
