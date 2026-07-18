//! Re-anchoring algorithm (SPEC.md §4.2), git-facing half: candidate discovery
//! (anchored path, then rename detection between the anchor's head and the
//! target) and step 1 blob identity. The content matching (steps 2–3) is
//! core's `locate_snippet`.

use crate::store::Store;
use anyhow::{Context, Result};
use git_threads_core::{Anchor, LineRange, ReanchorStatus, derive_snippet, locate_snippet};
use gix::ObjectId;

/// Where to display a thread on a target commit (SPEC.md §4.2). Pure
/// function of (anchor, target) — cacheable, never stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reanchor {
    /// Commit-kind anchors describe the whole change; nothing to re-map.
    WholeCommit,
    /// The thread maps onto the target at `path` (and `lines` for ranges).
    Located { path: String, lines: Option<LineRange>, status: ReanchorStatus },
    /// Step 4: no unique match; display against the anchor's own diff.
    Outdated,
}

/// The `placement` object `--json` documents: `kind` tags the variant, and
/// a located placement carries `path`, `lines` (when the anchor has them),
/// `status` (`exact` / `relocated` / `fuzzy`), and `fuzz` when fuzzy.
impl serde::Serialize for Reanchor {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        match self {
            Reanchor::WholeCommit => map.serialize_entry("kind", "whole-commit")?,
            Reanchor::Outdated => map.serialize_entry("kind", "outdated")?,
            Reanchor::Located { path, lines, status } => {
                map.serialize_entry("kind", "located")?;
                map.serialize_entry("path", path)?;
                if let Some(lines) = lines {
                    map.serialize_entry("lines", lines)?;
                }
                let (status, fuzz) = match status {
                    ReanchorStatus::Exact => ("exact", None),
                    ReanchorStatus::Relocated => ("relocated", None),
                    ReanchorStatus::Fuzzy(f) => ("fuzzy", Some(*f)),
                };
                map.serialize_entry("status", status)?;
                if let Some(fuzz) = fuzz {
                    map.serialize_entry("fuzz", &fuzz)?;
                }
            }
        }
        map.end()
    }
}

pub fn reanchor(store: &Store, anchor: &Anchor, target: ObjectId) -> Result<Reanchor> {
    let repo = store.repo();
    let Some(path) = anchor.path.as_deref() else {
        return Ok(Reanchor::WholeCommit);
    };
    let anchor_blob = anchor
        .blob
        .as_ref()
        .context("anchor with a path but no blob")
        .and_then(|b| Ok(ObjectId::from_hex(b.as_str().as_bytes())?))?;

    // Candidates in order: the anchored path, then its rename-detected
    // successor. Rename detection is best-effort — it shells out to
    // `git diff -M` and a failure (e.g. the anchor's head missing locally)
    // just means no rename candidate.
    let mut candidates = vec![path.to_string()];
    let anchor_head = ObjectId::from_hex(anchor.diff.head.as_str().as_bytes())?;
    if let Some(renamed) = detect_rename(store, anchor_head, target, path) {
        candidates.push(renamed);
    }

    // Step 1: blob identity — the anchored file version exists unchanged.
    for candidate in &candidates {
        if blob_at(repo, target, candidate)? == Some(anchor_blob) {
            return Ok(Reanchor::Located {
                path: candidate.clone(),
                lines: anchor.lines,
                status: ReanchorStatus::Exact,
            });
        }
    }

    // File-kind anchors have no lines to match; the path surviving (with
    // changed content) is the best remaining signal.
    let Some(lines) = anchor.lines else {
        for candidate in &candidates {
            if blob_at(repo, target, candidate)?.is_some() {
                return Ok(Reanchor::Located {
                    path: candidate.clone(),
                    lines: None,
                    status: ReanchorStatus::Relocated,
                });
            }
        }
        return Ok(Reanchor::Outdated);
    };

    // Steps 2–3: search each candidate file for the derived snippet.
    let anchor_content = blob_content(repo, anchor_blob)?;
    let Some(snippet) = derive_snippet(&anchor_content, lines) else {
        // Anchor and blob disagree; §3.1 says flag, never guess.
        return Ok(Reanchor::Outdated);
    };
    for candidate in &candidates {
        let Some(blob) = blob_at(repo, target, candidate)? else {
            continue;
        };
        let content = blob_content(repo, blob)?;
        if let Some((lines, status)) = locate_snippet(&snippet, &content) {
            return Ok(Reanchor::Located { path: candidate.clone(), lines: Some(lines), status });
        }
    }
    Ok(Reanchor::Outdated)
}

/// The path `path` from `from` was renamed to in `to`, per `git diff -M`.
fn detect_rename(store: &Store, from: ObjectId, to: ObjectId, path: &str) -> Option<String> {
    let dir = store.repo().workdir().unwrap_or_else(|| store.repo().path()).to_owned();
    let out = crate::commands::git(
        &dir,
        &[
            "diff",
            "-M",
            "--name-status",
            "--diff-filter=R",
            &from.to_string(),
            &to.to_string(),
        ],
    )
    .ok()?;
    for line in out.lines() {
        let mut fields = line.split('\t');
        let (Some(_status), Some(old), Some(new)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if old == path {
            return Some(new.to_string());
        }
    }
    None
}

/// Blob ID of `path` in `commit`'s tree, if it is a file.
pub fn blob_at(repo: &gix::Repository, commit: ObjectId, path: &str) -> Result<Option<ObjectId>> {
    let tree = repo.find_commit(commit)?.tree()?;
    match tree.lookup_entry_by_path(path)? {
        Some(entry) if entry.mode().is_blob() => Ok(Some(entry.object_id())),
        _ => Ok(None),
    }
}

pub fn blob_content(repo: &gix::Repository, blob: ObjectId) -> Result<String> {
    Ok(String::from_utf8_lossy(&repo.find_blob(blob)?.data).into_owned())
}
