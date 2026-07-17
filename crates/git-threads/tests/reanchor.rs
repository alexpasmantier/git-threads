//! Integration tests for the full re-anchoring algorithm (SPEC.md §4.2) against
//! a real repository: blob identity, snippet relocation, fuzz, rename
//! detection, and the outdated fallback.

use git_threads::commands::{self, CommentOpts};
use git_threads::reanchor::{Cache, Reanchor, reanchor};
use git_threads::store::Store;
use git_threads_core::{Anchor, LineRange, ReanchorStatus, Side, ThreadId};
use gix::ObjectId;
use std::fs;
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
    let output =
        Command::new("git").arg("-C").arg(dir).args(args).output().expect("failed to run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

const ORIGINAL: &str = "\
use std::fmt;

fn alpha() {
    println!(\"alpha\");
}

fn beta() {
    println!(\"beta\");
}

fn gamma() {
    println!(\"gamma\");
}
";

/// Repo with two commits; a thread anchored to `fn beta()` (lines 7–9) of HEAD.
fn setup() -> (tempfile::TempDir, Store, ThreadId) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path();
    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["config", "user.name", "Test"]);
    git(path, &["config", "user.email", "test@example.com"]);
    fs::write(path.join("code.rs"), "// placeholder\n").unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-q", "-m", "seed"]);
    fs::write(path.join("code.rs"), ORIGINAL).unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-q", "-m", "add functions"]);

    let store = Store::open(path).unwrap();
    let thread_id = commands::comment(
        &store,
        &CommentOpts {
            target: None,
            file: Some("code.rs:7-9".into()),
            message: "beta needs a doc comment".into(),
            side: Side::New,
        },
    )
    .unwrap();
    (dir, store, thread_id)
}

fn anchor_of(store: &Store, id: &ThreadId) -> Anchor {
    store.read_thread(id).unwrap().unwrap().anchor
}

fn commit_all(dir: &Path, message: &str) -> ObjectId {
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", message]);
    ObjectId::from_hex(git(dir, &["rev-parse", "HEAD"]).as_bytes()).unwrap()
}

#[test]
fn unchanged_blob_is_exact_with_identical_lines() {
    let (dir, store, id) = setup();
    let anchor = anchor_of(&store, &id);
    // A commit touching an unrelated file: code.rs's blob is unchanged.
    fs::write(dir.path().join("other.txt"), "noise\n").unwrap();
    let target = commit_all(dir.path(), "unrelated change");

    let result = reanchor(&store, &anchor, target).unwrap();
    assert_eq!(
        result,
        Reanchor::Located {
            path: "code.rs".into(),
            lines: Some(LineRange { start: 7, end: 9 }),
            status: ReanchorStatus::Exact,
        }
    );
}

#[test]
fn shifted_lines_relocate() {
    let (dir, store, id) = setup();
    let anchor = anchor_of(&store, &id);
    let edited = format!("// new header\n// more header\n{ORIGINAL}");
    fs::write(dir.path().join("code.rs"), edited).unwrap();
    let target = commit_all(dir.path(), "prepend header");

    let result = reanchor(&store, &anchor, target).unwrap();
    assert_eq!(
        result,
        Reanchor::Located {
            path: "code.rs".into(),
            lines: Some(LineRange { start: 9, end: 11 }),
            status: ReanchorStatus::Relocated,
        }
    );
}

#[test]
fn changed_context_matches_fuzzily() {
    let (dir, store, id) = setup();
    let anchor = anchor_of(&store, &id);
    // Rewrite fn gamma()'s body: the outermost after-context line of the
    // beta snippet ("fn gamma() {" is 3 lines below the target) survives,
    // but "    println!(\"alpha\");" two lines above changes.
    let edited = ORIGINAL.replace("println!(\"alpha\")", "eprintln!(\"ALPHA\")");
    fs::write(dir.path().join("code.rs"), edited).unwrap();
    let target = commit_all(dir.path(), "rewrite alpha body");

    let result = reanchor(&store, &anchor, target).unwrap();
    let Reanchor::Located { lines, status, .. } = result else {
        panic!("expected a located result, got {result:?}");
    };
    assert_eq!(lines, Some(LineRange { start: 7, end: 9 }));
    assert!(matches!(status, ReanchorStatus::Fuzzy(_)), "got {status}");
}

#[test]
fn renamed_file_is_found_via_rename_detection() {
    let (dir, store, id) = setup();
    let anchor = anchor_of(&store, &id);
    git(dir.path(), &["mv", "code.rs", "renamed.rs"]);
    let target = commit_all(dir.path(), "rename");

    let result = reanchor(&store, &anchor, target).unwrap();
    assert_eq!(
        result,
        Reanchor::Located {
            path: "renamed.rs".into(),
            lines: Some(LineRange { start: 7, end: 9 }),
            status: ReanchorStatus::Exact,
        }
    );
}

#[test]
fn deleted_target_is_outdated() {
    let (dir, store, id) = setup();
    let anchor = anchor_of(&store, &id);
    let edited = ORIGINAL.replace("fn beta() {\n    println!(\"beta\");\n}\n", "");
    fs::write(dir.path().join("code.rs"), edited).unwrap();
    let target = commit_all(dir.path(), "drop beta");

    assert_eq!(reanchor(&store, &anchor, target).unwrap(), Reanchor::Outdated);
}

#[test]
fn ambiguous_match_is_outdated_not_pick_first() {
    let (dir, store, id) = setup();
    let anchor = anchor_of(&store, &id);
    // Duplicate the whole file body so the snippet appears twice, and break
    // blob identity by the duplication itself.
    fs::write(dir.path().join("code.rs"), format!("{ORIGINAL}\n{ORIGINAL}")).unwrap();
    let target = commit_all(dir.path(), "duplicate everything");

    assert_eq!(reanchor(&store, &anchor, target).unwrap(), Reanchor::Outdated);
}

#[test]
fn commit_anchor_has_nothing_to_remap() {
    let (dir, store, _) = setup();
    let whole = commands::comment(
        &store,
        &CommentOpts { target: None, file: None, message: "commit-level".into(), side: Side::New },
    )
    .unwrap();
    let anchor = anchor_of(&store, &whole);
    let head = ObjectId::from_hex(git(dir.path(), &["rev-parse", "HEAD"]).as_bytes()).unwrap();
    assert_eq!(reanchor(&store, &anchor, head).unwrap(), Reanchor::WholeCommit);
}

#[test]
fn placements_are_cached_per_target_and_served_from_the_cache() {
    let (dir, store, id) = setup();
    let anchor = anchor_of(&store, &id);
    let head = ObjectId::from_hex(git(dir.path(), &["rev-parse", "HEAD"]).as_bytes()).unwrap();

    let mut cache = Cache::open(store.repo(), head);
    let fresh = cache.placement(&store, &anchor).unwrap();
    assert_eq!(fresh, reanchor(&store, &anchor, head).unwrap());
    cache.save();
    let file = dir.path().join(".git/threads/reanchor").join(format!("{head}.json"));
    assert!(file.exists(), "cache file written on save");

    // Poison the stored entry: getting the poisoned value back is the proof
    // that placement() reads the cache, not the repository.
    let key =
        git_threads_core::EventId::compute(&git_threads_core::to_canonical_json(&anchor).unwrap());
    let poisoned =
        format!("{{\"v\":1,\"entries\":{{\"{}\":{{\"kind\":\"outdated\"}}}}}}", key.as_str());
    fs::write(&file, poisoned).unwrap();
    let mut cache = Cache::open(store.repo(), head);
    assert_eq!(cache.placement(&store, &anchor).unwrap(), Reanchor::Outdated);

    // A corrupt file is just a cold cache, never an error.
    fs::write(&file, "not json").unwrap();
    let mut cache = Cache::open(store.repo(), head);
    assert_eq!(cache.placement(&store, &anchor).unwrap(), fresh);
}
