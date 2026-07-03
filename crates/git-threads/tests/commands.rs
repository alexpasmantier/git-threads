//! Integration tests for the command layer: comment / reply / resolve / list
//! against a real temporary repository with actual file history.

use git_threads::commands::{self, CommentOpts};
use git_threads::store::Store;
use git_threads_core::{AnchorKind, EventKind, Side, fold_thread};
use std::fs;
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("failed to run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A repo where src/lib.rs gains a second function in the second commit.
fn setup_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path();
    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["config", "user.name", "Test"]);
    git(path, &["config", "user.email", "test@example.com"]);
    fs::create_dir(path.join("src")).unwrap();
    fs::write(path.join("src/lib.rs"), "fn a() {}\n").unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-q", "-m", "add a"]);
    fs::write(path.join("src/lib.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-q", "-m", "add b and c"]);
    dir
}

fn opts(message: &str) -> CommentOpts {
    CommentOpts {
        commit: "HEAD".into(),
        message: message.into(),
        file: None,
        lines: None,
        side: Side::New,
        base: None,
    }
}

#[test]
fn commit_level_comment_creates_thread() {
    let dir = setup_repo();
    let store = Store::open(dir.path()).unwrap();

    let thread_id = commands::comment(&store, &opts("whole change looks rushed")).unwrap();
    let thread = store.read_thread(&thread_id).unwrap().expect("thread exists");
    assert_eq!(thread.anchor.kind, AnchorKind::Commit);
    assert!(thread.anchor.path.is_none());
    assert_eq!(thread.events.len(), 1);
    let root = &thread.events[0].1;
    assert_eq!(root.kind, EventKind::Comment);
    assert_eq!(root.author.email, "test@example.com");
    assert_eq!(root.body.as_deref(), Some("whole change looks rushed"));
}

#[test]
fn range_comment_resolves_blob_and_validates_lines() {
    let dir = setup_repo();
    let store = Store::open(dir.path()).unwrap();

    let mut range = opts("why is b empty?");
    range.file = Some("src/lib.rs".into());
    range.lines = Some("2-3".into());
    let thread_id = commands::comment(&store, &range).unwrap();

    let thread = store.read_thread(&thread_id).unwrap().unwrap();
    assert_eq!(thread.anchor.kind, AnchorKind::Range);
    assert_eq!(thread.anchor.path.as_deref(), Some("src/lib.rs"));
    assert_eq!(thread.anchor.lines.unwrap().start, 2);
    assert_eq!(thread.anchor.side, Some(Side::New));
    // The recorded blob must be the actual blob of src/lib.rs at HEAD.
    let expected_blob = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["rev-parse", "HEAD:src/lib.rs"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(thread.anchor.blob.as_ref().unwrap().as_str(), expected_blob.trim());

    // Out-of-range lines are rejected (file has 3 lines).
    let mut bad = opts("nope");
    bad.file = Some("src/lib.rs".into());
    bad.lines = Some("2-9".into());
    let err = commands::comment(&store, &bad).unwrap_err();
    assert!(err.to_string().contains("out of range"), "unexpected error: {err:#}");
}

#[test]
fn old_side_comment_reads_base_blob() {
    let dir = setup_repo();
    let store = Store::open(dir.path()).unwrap();

    let mut old = opts("this only had one line before");
    old.file = Some("src/lib.rs".into());
    old.lines = Some("1".into());
    old.side = Side::Old;
    let thread_id = commands::comment(&store, &old).unwrap();

    let thread = store.read_thread(&thread_id).unwrap().unwrap();
    // Base version has exactly 1 line, so line 1 is valid but line 2 is not.
    let mut bad = opts("nope");
    bad.file = Some("src/lib.rs".into());
    bad.lines = Some("2".into());
    bad.side = Side::Old;
    assert!(commands::comment(&store, &bad).is_err());
    assert_eq!(thread.anchor.side, Some(Side::Old));
}

#[test]
fn reply_and_resolve_round_trip_through_fold() {
    let dir = setup_repo();
    let store = Store::open(dir.path()).unwrap();

    let thread_id = commands::comment(&store, &opts("root")).unwrap();
    commands::reply(&store, thread_id.as_str(), "on it").unwrap();
    commands::resolve(&store, &thread_id.as_str()[..8], true).unwrap();

    let thread = store.read_thread(&thread_id).unwrap().unwrap();
    let folded = fold_thread(thread.events);
    assert!(folded.resolved);
    assert_eq!(folded.events.len(), 2);
    // Same-second events order by ID tie-break, so locate the reply by kind.
    let reply = folded
        .events
        .iter()
        .find(|e| e.event.kind == EventKind::Reply)
        .expect("reply present");
    assert_eq!(reply.event.in_reply_to, Some(thread_id.clone()));

    // Same-second resolve toggles are order-undefined (LWW ties break on
    // event-ID hash), so give the reopen a strictly later timestamp.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    commands::resolve(&store, thread_id.as_str(), false).unwrap();
    let thread = store.read_thread(&thread_id).unwrap().unwrap();
    assert!(!fold_thread(thread.events).resolved);
}

#[test]
fn thread_prefix_lookup_errors() {
    let dir = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    commands::comment(&store, &opts("only thread")).unwrap();

    let err = commands::reply(&store, "ffffffff", "to nobody").unwrap_err();
    assert!(err.to_string().contains("no comment or reply matches"));
    // Empty prefix matches everything — fine with one thread, ambiguous needs 2+.
    commands::comment(&store, &opts("second thread")).unwrap();
    let err = commands::reply(&store, "", "to everybody").unwrap_err();
    assert!(err.to_string().contains("ambiguous"));
}

#[test]
fn root_commit_requires_explicit_base() {
    let dir = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let mut on_root = opts("first!");
    on_root.commit = "HEAD~1".into(); // the root commit — no parent
    let err = commands::comment(&store, &on_root).unwrap_err();
    assert!(err.to_string().contains("--base"), "unexpected error: {err:#}");
}

#[test]
fn edit_replaces_body_and_chains() {
    let dir = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let thread_id = commands::comment(&store, &opts("orignal")).unwrap();

    let first_edit = commands::edit(&store, thread_id.as_str(), "original").unwrap();
    let thread = store.read_thread(&thread_id).unwrap().unwrap();
    let folded = fold_thread(thread.events.clone());
    assert_eq!(folded.events[0].effective_body.as_deref(), Some("original"));
    assert!(folded.events[0].edited);
    // The stored root body is untouched: edits are append-only events.
    assert_eq!(thread.events.iter().find(|(id, _)| *id == thread_id).unwrap().1.body.as_deref(), Some("orignal"));

    // A second edit supersedes the first edit (the chain tip), not the root.
    let second_edit = commands::edit(&store, thread_id.as_str(), "original, take 3").unwrap();
    let thread = store.read_thread(&thread_id).unwrap().unwrap();
    let event = &thread.events.iter().find(|(id, _)| *id == second_edit).unwrap().1;
    assert_eq!(event.supersedes, Some(first_edit));
    let folded = fold_thread(thread.events);
    assert_eq!(folded.events[0].effective_body.as_deref(), Some("original, take 3"));
}

#[test]
fn delete_retracts_and_blocks_further_edits() {
    let dir = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let thread_id = commands::comment(&store, &opts("root")).unwrap();
    let reply_id = commands::reply(&store, thread_id.as_str(), "oops, wrong thread").unwrap();

    commands::delete(&store, reply_id.as_str()).unwrap();
    let thread = store.read_thread(&thread_id).unwrap().unwrap();
    let folded = fold_thread(thread.events);
    let reply = folded.events.iter().find(|e| e.id == reply_id).unwrap();
    assert!(reply.retracted);
    // Content is tombstoned, not erased.
    assert_eq!(reply.event.body.as_deref(), Some("oops, wrong thread"));

    let err = commands::edit(&store, reply_id.as_str(), "revive").unwrap_err();
    assert!(err.to_string().contains("retracted"), "unexpected error: {err:#}");
    let err = commands::delete(&store, reply_id.as_str()).unwrap_err();
    assert!(err.to_string().contains("already retracted"), "unexpected error: {err:#}");
}

#[test]
fn only_the_author_can_edit_or_delete() {
    let dir = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let thread_id = commands::comment(&store, &opts("mine")).unwrap();

    git(dir.path(), &["config", "user.name", "Mallory"]);
    git(dir.path(), &["config", "user.email", "mallory@example.com"]);
    let store = Store::open(dir.path()).unwrap();
    let err = commands::edit(&store, thread_id.as_str(), "hijacked").unwrap_err();
    assert!(err.to_string().contains("only the author"), "unexpected error: {err:#}");
    let err = commands::delete(&store, thread_id.as_str()).unwrap_err();
    assert!(err.to_string().contains("only the author"), "unexpected error: {err:#}");
}

#[test]
fn replying_to_a_reply_targets_it_within_the_same_thread() {
    let dir = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let thread_id = commands::comment(&store, &opts("root question")).unwrap();
    let first_reply = commands::reply(&store, thread_id.as_str(), "first answer").unwrap();

    // Reply by naming the reply, not the thread.
    let second_reply = commands::reply(&store, first_reply.as_str(), "disagree with that").unwrap();
    let thread = store.read_thread(&thread_id).unwrap().expect("same thread");
    let event = &thread.events.iter().find(|(id, _)| *id == second_reply).unwrap().1;
    assert_eq!(event.in_reply_to, Some(first_reply.clone()));

    // resolve accepts any message ID from the thread too.
    commands::resolve(&store, second_reply.as_str(), true).unwrap();
    let thread = store.read_thread(&thread_id).unwrap().unwrap();
    assert!(fold_thread(thread.events).resolved);
}
