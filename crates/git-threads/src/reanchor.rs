//! Re-anchoring algorithm (SPEC.md §4.2), git-facing half: candidate discovery
//! (anchored path, then rename detection between the anchor's head and the
//! target) and step 1 blob identity. The content matching (steps 2–3) is
//! core's `locate_snippet`.

use crate::store::Store;
use anyhow::{Context, Result};
use git_threads_core::{
    Anchor, EventId, LineRange, ReanchorStatus, derive_snippet, locate_snippet,
    locate_snippet_among, to_canonical_json,
};
use gix::ObjectId;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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

/// The inverse of the custom `Serialize`, so cached placements — and anyone
/// holding the documented `--json` shape — can round-trip.
impl<'de> serde::Deserialize<'de> for Reanchor {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        #[derive(serde::Deserialize)]
        struct Raw {
            kind: String,
            #[serde(default)]
            path: Option<String>,
            #[serde(default)]
            lines: Option<LineRange>,
            #[serde(default)]
            status: Option<String>,
            #[serde(default)]
            fuzz: Option<u8>,
        }
        let raw = Raw::deserialize(deserializer)?;
        match raw.kind.as_str() {
            "whole-commit" => Ok(Reanchor::WholeCommit),
            "outdated" => Ok(Reanchor::Outdated),
            "located" => {
                let path = raw.path.ok_or_else(|| D::Error::missing_field("path"))?;
                let status = match raw.status.as_deref() {
                    Some("exact") => ReanchorStatus::Exact,
                    Some("relocated") => ReanchorStatus::Relocated,
                    Some("fuzzy") => ReanchorStatus::Fuzzy(raw.fuzz.unwrap_or(0)),
                    other => {
                        return Err(D::Error::custom(format!(
                            "unknown placement status {other:?}"
                        )));
                    }
                };
                Ok(Reanchor::Located { path, lines: raw.lines, status })
            }
            other => Err(D::Error::custom(format!("unknown placement kind {other:?}"))),
        }
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

    // Candidates: the anchored path, plus its rename-detected successor.
    // Rename detection only makes sense when the anchored path is gone
    // from the target tree (`git diff -M` reports renames of deleted
    // paths, so a surviving path can never have one) — which also keeps
    // the subprocess it shells out to off the common path. It is
    // best-effort: a failure (e.g. the anchor's head missing locally) just
    // means no rename candidate. At every step below, a match must be
    // unique across ALL candidates (SPEC.md §4.2) — never pick-first.
    let mut candidates = vec![path.to_string()];
    if blob_at(repo, target, path)?.is_none() {
        let anchor_head = ObjectId::from_hex(anchor.diff.head.as_str().as_bytes())?;
        if let Some(renamed) = detect_rename(store, anchor_head, target, path) {
            candidates.push(renamed);
        }
    }
    // The candidates that exist in the target tree, with their blobs.
    let mut present: Vec<(String, ObjectId)> = Vec::new();
    for candidate in candidates {
        if let Some(blob) = blob_at(repo, target, &candidate)? {
            present.push((candidate, blob));
        }
    }

    // Step 1: blob identity — the anchored file version exists unchanged.
    // Two identical copies fail the step; the snippet search below then sees
    // the same ambiguity and lands on outdated.
    let mut identical = present.iter().filter(|(_, blob)| *blob == anchor_blob);
    if let (Some((path, _)), None) = (identical.next(), identical.next()) {
        return Ok(Reanchor::Located {
            path: path.clone(),
            lines: anchor.lines,
            status: ReanchorStatus::Exact,
        });
    }

    // File-kind anchors have no lines to match; the path surviving (with
    // changed content) is the best remaining signal — if it is unique.
    let Some(lines) = anchor.lines else {
        return Ok(match present.as_slice() {
            [(path, _)] => Reanchor::Located {
                path: path.clone(),
                lines: None,
                status: ReanchorStatus::Relocated,
            },
            _ => Reanchor::Outdated,
        });
    };

    // Steps 2–3: search the candidate files for the derived snippet.
    let anchor_content = blob_content(repo, anchor_blob)?;
    let Some(snippet) = derive_snippet(&anchor_content, lines) else {
        // Anchor and blob disagree; §3.1 says flag, never guess.
        return Ok(Reanchor::Outdated);
    };
    let contents =
        present.iter().map(|(_, blob)| blob_content(repo, *blob)).collect::<Result<Vec<_>>>()?;
    if let Some((index, lines, status)) =
        locate_snippet_among(&snippet, contents.iter().map(String::as_str))
    {
        return Ok(Reanchor::Located {
            path: present[index].0.clone(),
            lines: Some(lines),
            status,
        });
    }
    Ok(Reanchor::Outdated)
}

/// Display-side second pass (docs/design/worktree.md): where a placement's
/// lines sit in the working tree, when the placed file is dirty on disk.
/// `None` means the commit placement stands: the file is clean (judged by
/// lines, so checkout CRLF filters don't fake dirtiness), unreadable, or
/// there are no lines to re-locate. The index is never consulted — the
/// editor shows the disk, so locations map to the disk.
pub fn worktree_remap(
    store: &Store,
    anchor: &Anchor,
    target: ObjectId,
    placement: &Reanchor,
) -> Option<Reanchor> {
    let Reanchor::Located { path, lines: Some(_), .. } = placement else { return None };
    let repo = store.repo();
    let disk = std::fs::read_to_string(repo.workdir()?.join(path)).ok()?;
    let placed_blob = blob_at(repo, target, path).ok().flatten()?;
    let placed = blob_content(repo, placed_blob).ok()?;
    if disk.lines().eq(placed.lines()) {
        return None;
    }
    // Same ladder, one candidate: blob identity against the anchored
    // version, then its derived snippet in the disk content.
    let anchor_blob = ObjectId::from_hex(anchor.blob.as_ref()?.as_str().as_bytes()).ok()?;
    let anchor_content = blob_content(repo, anchor_blob).ok()?;
    if disk.lines().eq(anchor_content.lines()) {
        return Some(Reanchor::Located {
            path: path.clone(),
            lines: anchor.lines,
            status: ReanchorStatus::Exact,
        });
    }
    let snippet = derive_snippet(&anchor_content, anchor.lines?)?;
    Some(match locate_snippet(&snippet, &disk) {
        Some((lines, status)) => {
            Reanchor::Located { path: path.clone(), lines: Some(lines), status }
        }
        None => Reanchor::Outdated,
    })
}

/// The path `path` from `from` was renamed to in `to`, per `git diff -M`.
fn detect_rename(store: &Store, from: ObjectId, to: ObjectId, path: &str) -> Option<String> {
    let dir = store.repo().workdir().unwrap_or_else(|| store.repo().path()).to_owned();
    let out = crate::commands::git(
        &dir,
        &["diff", "-M", "--name-status", "--diff-filter=R", &from.to_string(), &to.to_string()],
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

/// Client-local re-anchor cache (SPEC.md "client-local niceties"). Placement
/// is a pure function of (anchor, target commit), so computed answers are
/// remembered in `.git/threads/reanchor/<target>.json` and never expire.
/// Everything about it is best-effort: a missing, torn, or unparsable file
/// just means recomputing, and deleting the directory is always safe.
pub struct Cache {
    target: ObjectId,
    file: PathBuf,
    entries: BTreeMap<String, Reanchor>,
    dirty: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CacheFile {
    v: u32,
    entries: BTreeMap<String, Reanchor>,
}

impl Cache {
    /// Per-target files kept around; older targets age out on save.
    const KEEP: usize = 8;

    pub fn open(repo: &gix::Repository, target: ObjectId) -> Cache {
        let file = repo.git_dir().join("threads").join("reanchor").join(format!("{target}.json"));
        let entries = std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| serde_json::from_str::<CacheFile>(&text).ok())
            .filter(|cache| cache.v == 1)
            .map(|cache| cache.entries)
            .unwrap_or_default();
        Cache { target, file, entries, dirty: false }
    }

    /// The commit placements are computed against.
    pub fn target(&self) -> ObjectId {
        self.target
    }

    /// `anchor`'s placement on the target: served from the cache, else
    /// computed and remembered. A result computed while the anchored head
    /// commit is missing locally is not stored — fetching that commit later
    /// can improve it (rename detection needs the commit).
    pub fn placement(&mut self, store: &Store, anchor: &Anchor) -> Result<Reanchor> {
        let key = anchor_key(anchor)?;
        if let Some(hit) = self.entries.get(&key) {
            return Ok(hit.clone());
        }
        let placement = reanchor(store, anchor, self.target)?;
        let head = ObjectId::from_hex(anchor.diff.head.as_str().as_bytes())?;
        if store.repo().find_commit(head).is_ok() {
            self.entries.insert(key, placement.clone());
            self.dirty = true;
        }
        Ok(placement)
    }

    /// Write the cache back if anything was added, and age out files for
    /// targets not recently used.
    pub fn save(&self) {
        if !self.dirty {
            return;
        }
        let Some(dir) = self.file.parent() else { return };
        let _ = std::fs::create_dir_all(dir);
        let Ok(text) = serde_json::to_string(&CacheFile { v: 1, entries: self.entries.clone() })
        else {
            return;
        };
        // Write-then-rename so a concurrent reader never sees a torn file.
        let tmp = self.file.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &self.file);
        }
        prune(dir, &self.file);
    }
}

/// Content key of an anchor: the SHA-256 of its canonical bytes — the same
/// hash the format names events with.
fn anchor_key(anchor: &Anchor) -> Result<String> {
    Ok(EventId::compute(&to_canonical_json(anchor)?).as_str().to_string())
}

/// Keep the newest [`Cache::KEEP`] files (the current target's included);
/// stale target files and abandoned temp files age out with them.
fn prune(dir: &Path, keep_file: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let modified = entry.metadata().ok()?.modified().ok()?;
            (path != keep_file).then_some((modified, path))
        })
        .collect();
    if files.len() < Cache::KEEP {
        return;
    }
    files.sort();
    for (_, path) in files.iter().take(files.len() + 1 - Cache::KEEP) {
        let _ = std::fs::remove_file(path);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_json_round_trips() {
        for placement in [
            Reanchor::WholeCommit,
            Reanchor::Outdated,
            Reanchor::Located {
                path: "src/lib.rs".into(),
                lines: Some(LineRange { start: 3, end: 9 }),
                status: ReanchorStatus::Fuzzy(2),
            },
            Reanchor::Located { path: "a".into(), lines: None, status: ReanchorStatus::Relocated },
        ] {
            let json = serde_json::to_string(&placement).unwrap();
            assert_eq!(serde_json::from_str::<Reanchor>(&json).unwrap(), placement, "{json}");
        }
    }
}
