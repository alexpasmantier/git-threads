//! Integration tests for the publish/pull loop (SPEC.md §7.2): two clones of
//! a shared bare remote, including the concurrent-publish race that forces
//! the tree-union merge path.

use git_threads::commands::{self, CommentOpts};
use git_threads::store::Store;
use git_threads_core::Side;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A bare "hosting" repo and two developer clones with shared history.
fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let root = tempfile::tempdir().expect("tempdir");
    let bare = root.path().join("remote.git");
    let seed = root.path().join("seed");

    git(root.path(), &["init", "-q", "--bare", "remote.git"]);
    git(root.path(), &["init", "-q", "-b", "main", "seed"]);
    git(&seed, &["config", "user.name", "Seed"]);
    git(&seed, &["config", "user.email", "seed@example.com"]);
    std::fs::write(seed.join("code.txt"), "one\ntwo\nthree\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "seed"]);
    git(&seed, &["commit", "-q", "--allow-empty", "-m", "change under review"]);
    git(&seed, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&seed, &["push", "-q", "origin", "main"]);

    let clone = |name: &str, email: &str| {
        let path = root.path().join(name);
        git(root.path(), &["clone", "-q", bare.to_str().unwrap(), name]);
        git(&path, &["config", "user.name", name]);
        git(&path, &["config", "user.email", email]);
        path
    };
    let alice = clone("alice", "alice@example.com");
    let bob = clone("bob", "bob@example.com");
    (root, alice, bob)
}

fn comment(store: &Store, message: &str) -> git_threads_core::ThreadId {
    commands::comment(
        store,
        &CommentOpts {
            commit: "HEAD".into(),
            message: message.into(),
            file: None,
            lines: None,
            side: Side::New,
            base: None,
        },
    )
    .unwrap()
}

#[test]
fn publish_then_pull_transfers_threads() {
    let (_root, alice, bob) = setup();
    let alice_store = Store::open(&alice).unwrap();
    let bob_store = Store::open(&bob).unwrap();

    let thread_id = comment(&alice_store, "does three need to be a string?");
    commands::publish(&alice_store, "origin").unwrap();

    commands::pull(&bob_store, "origin").unwrap();
    let thread = bob_store.read_thread(&thread_id).unwrap().expect("thread arrived");
    assert_eq!(
        thread.events[0].1.body.as_deref(),
        Some("does three need to be a string?")
    );

    // Bob replies and publishes; Alice pulls the reply back.
    commands::reply(&bob_store, thread_id.as_str(), "no, fixing").unwrap();
    commands::publish(&bob_store, "origin").unwrap();
    commands::pull(&alice_store, "origin").unwrap();
    let thread = alice_store.read_thread(&thread_id).unwrap().unwrap();
    assert_eq!(thread.events.len(), 2);
}

#[test]
fn concurrent_publishes_converge_via_union_merge() {
    let (_root, alice, bob) = setup();
    let alice_store = Store::open(&alice).unwrap();
    let bob_store = Store::open(&bob).unwrap();

    // Both comment while offline from each other.
    let alice_thread = comment(&alice_store, "alice's concern");
    let bob_thread = comment(&bob_store, "bob's concern");

    // Alice wins the race; Bob's publish must lose, re-integrate, and retry.
    commands::publish(&alice_store, "origin").unwrap();
    commands::publish(&bob_store, "origin").unwrap();

    commands::pull(&alice_store, "origin").unwrap();

    for store in [&alice_store, &bob_store] {
        let threads = store.threads().unwrap();
        assert_eq!(threads.len(), 2, "both threads visible in both clones");
        for id in [&alice_thread, &bob_thread] {
            assert!(store.read_thread(id).unwrap().is_some());
        }
    }

    // Bob's tip is a genuine merge commit: two threads-history parents.
    let parents = git(&bob, &["log", "--pretty=%P", "-1", "refs/threads/data"]);
    assert_eq!(parents.split_whitespace().count(), 2, "union merge has two parents");

    // Idempotence: re-publishing and re-pulling changes nothing.
    let tip_before = git(&bob, &["rev-parse", "refs/threads/data"]);
    commands::publish(&bob_store, "origin").unwrap();
    commands::pull(&bob_store, "origin").unwrap();
    assert_eq!(git(&bob, &["rev-parse", "refs/threads/data"]), tip_before);
}

#[test]
fn pull_from_empty_remote_is_graceful() {
    let (_root, alice, _bob) = setup();
    let alice_store = Store::open(&alice).unwrap();
    commands::pull(&alice_store, "origin").unwrap();
    assert!(alice_store.tip().unwrap().is_none());
}

#[test]
fn interleaved_conversation_converges() {
    let (_root, alice, bob) = setup();
    let alice_store = Store::open(&alice).unwrap();
    let bob_store = Store::open(&bob).unwrap();

    let thread_id = comment(&alice_store, "root");
    commands::publish(&alice_store, "origin").unwrap();
    commands::pull(&bob_store, "origin").unwrap();

    // Concurrent replies to the same thread from both sides.
    commands::reply(&alice_store, thread_id.as_str(), "alice again").unwrap();
    commands::reply(&bob_store, thread_id.as_str(), "bob here").unwrap();
    commands::resolve(&bob_store, thread_id.as_str(), true).unwrap();
    commands::publish(&bob_store, "origin").unwrap();
    commands::publish(&alice_store, "origin").unwrap();
    commands::pull(&bob_store, "origin").unwrap();

    for store in [&alice_store, &bob_store] {
        let thread = store.read_thread(&thread_id).unwrap().unwrap();
        assert_eq!(thread.events.len(), 4, "root + 2 replies + resolve");
        let folded = git_threads_core::fold_thread(thread.events);
        assert!(folded.resolved);
        assert_eq!(folded.events.len(), 3);
    }
}
