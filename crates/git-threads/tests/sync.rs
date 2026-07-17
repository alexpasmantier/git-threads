//! Integration tests for the commit/push/pull loop (SPEC.md §7.2): two clones of
//! a shared bare remote, including the concurrent-publish race that forces
//! the tree-union merge path.

use git_threads::commands::{self, CommentOpts};
use git_threads::store::Store;
use git_threads_core::Side;
use std::path::{Path, PathBuf};
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
        &CommentOpts { target: None, file: None, message: message.into(), side: Side::New },
    )
    .unwrap()
}

#[test]
fn publish_then_pull_transfers_threads() {
    let (_root, alice, bob) = setup();
    let alice_store = Store::open(&alice).unwrap();
    let bob_store = Store::open(&bob).unwrap();

    let thread_id = comment(&alice_store, "does three need to be a string?");
    commands::commit(&alice_store).unwrap();
    commands::push(&alice_store, "origin").unwrap();

    commands::pull(&bob_store, "origin").unwrap();
    let thread = bob_store.read_thread(&thread_id).unwrap().expect("thread arrived");
    assert_eq!(thread.events[0].1.body.as_deref(), Some("does three need to be a string?"));

    // Bob replies and publishes; Alice pulls the reply back.
    commands::reply(&bob_store, thread_id.as_str(), "no, fixing").unwrap();
    commands::commit(&bob_store).unwrap();
    commands::push(&bob_store, "origin").unwrap();
    commands::pull(&alice_store, "origin").unwrap();
    let thread = alice_store.read_thread(&thread_id).unwrap().unwrap();
    assert_eq!(thread.events.len(), 2);
}

#[test]
fn plain_git_fetch_is_enough_after_init() {
    let (_root, alice, bob) = setup();
    let alice_store = Store::open(&alice).unwrap();
    comment(&alice_store, "left for bob");
    commands::commit(&alice_store).unwrap();
    commands::push(&alice_store, "origin").unwrap();

    // init's own fetch already integrates existing data.
    let bob_store = Store::open(&bob).unwrap();
    commands::init(&bob_store, "origin").unwrap();
    assert_eq!(bob_store.threads().unwrap().len(), 1);

    // From then on a plain `git fetch` delivers new data into the tracking
    // ref, and the opportunistic integration folds it in — no `threads pull`.
    comment(&alice_store, "second round");
    commands::commit(&alice_store).unwrap();
    commands::push(&alice_store, "origin").unwrap();
    git(&bob, &["fetch", "-q", "origin"]);
    assert_eq!(bob_store.threads().unwrap().len(), 1, "fetched but not yet integrated");
    commands::integrate_fetched(&bob_store).unwrap();
    assert_eq!(bob_store.threads().unwrap().len(), 2);
}

#[test]
fn deinit_removes_everything_but_refuses_to_orphan_work() {
    let (_root, alice, _bob) = setup();
    let store = Store::open(&alice).unwrap();
    commands::init(&store, "origin").unwrap();
    comment(&store, "not shared yet");

    // Drafts, then committed-but-unpushed events, block a plain deinit.
    let err = commands::deinit(&store, false).unwrap_err();
    assert!(err.to_string().contains("drafted"), "unexpected error: {err:#}");
    commands::commit(&store).unwrap();
    let err = commands::deinit(&store, false).unwrap_err();
    assert!(err.to_string().contains("push"), "unexpected error: {err:#}");

    // Once shared, deinit removes the refspec and every threads ref.
    commands::push(&store, "origin").unwrap();
    commands::deinit(&store, false).unwrap();
    assert!(git(&alice, &["for-each-ref", "refs/threads"]).is_empty());
    assert!(!git(&alice, &["config", "--get-all", "remote.origin.fetch"]).contains("threads"));

    // The clean state a fresh init starts from — and the data comes back.
    commands::init(&store, "origin").unwrap();
    assert_eq!(store.threads().unwrap().len(), 1);

    // --force discards unshared work knowingly.
    comment(&store, "expendable");
    commands::deinit(&store, true).unwrap();
    assert!(git(&alice, &["for-each-ref", "refs/threads"]).is_empty());
}

#[test]
fn concurrent_publishes_converge_via_union_merge() {
    let (_root, alice, bob) = setup();
    let alice_store = Store::open(&alice).unwrap();
    let bob_store = Store::open(&bob).unwrap();

    // Both comment while offline from each other. Bob promotes his drafts
    // locally before fetching — the race window where the remote moves after
    // your local commit — so his publish must union-merge.
    let alice_thread = comment(&alice_store, "alice's concern");
    let bob_thread = comment(&bob_store, "bob's concern");
    bob_store.commit_drafts().unwrap().expect("bob's drafts promoted");

    commands::commit(&alice_store).unwrap();
    commands::push(&alice_store, "origin").unwrap();
    commands::commit(&bob_store).unwrap();
    commands::push(&bob_store, "origin").unwrap();

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
    commands::commit(&bob_store).unwrap();
    commands::push(&bob_store, "origin").unwrap();
    commands::pull(&bob_store, "origin").unwrap();
    assert_eq!(git(&bob, &["rev-parse", "refs/threads/data"]), tip_before);
}

#[test]
fn init_refspec_never_clobbers_unpublished_local_data() {
    let (_root, alice, bob) = setup();
    let alice_store = Store::open(&alice).unwrap();
    let bob_store = Store::open(&bob).unwrap();

    commands::init(&alice_store, "origin").unwrap();
    let refspecs = git(&alice, &["config", "--get-all", "remote.origin.fetch"]);
    assert!(
        refspecs.contains("+refs/threads/data*:refs/threads/remotes/origin/data*"),
        "init writes the tracking refspec, got: {refspecs}"
    );

    // Alice has unpublished local data (drafts promoted locally, not yet
    // pushed); Bob publishes; a plain fetch must land Bob's state in the
    // tracking ref, not on Alice's local ref.
    comment(&alice_store, "unpublished");
    alice_store.commit_drafts().unwrap().expect("drafts promoted");
    let local_tip = git(&alice, &["rev-parse", "refs/threads/data"]);
    comment(&bob_store, "bob's thread");
    commands::commit(&bob_store).unwrap();
    commands::push(&bob_store, "origin").unwrap();

    git(&alice, &["fetch", "-q", "origin"]);
    assert_eq!(git(&alice, &["rev-parse", "refs/threads/data"]), local_tip);
    assert!(alice_store.tracking_tip("origin").unwrap().is_some());
}

#[test]
fn status_tracks_the_draft_commit_push_cycle() {
    let (_root, alice, _bob) = setup();
    let store = Store::open(&alice).unwrap();
    let status = || {
        let output = Command::new(env!("CARGO_BIN_EXE_git-threads"))
            .current_dir(&alice)
            .arg("status")
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).into_owned()
    };

    let clean = status();
    assert!(clean.contains("nothing drafted"), "{clean}");
    assert!(clean.contains("up to date with origin"), "{clean}");

    let thread_id = comment(&store, "does three need to be a string?");
    commands::reply(&store, thread_id.as_str(), "asking for a friend").unwrap();
    let drafted = status();
    assert!(drafted.contains("2 drafted events in 1 thread"), "{drafted}");
    assert!(drafted.contains("does three need to be a string?"), "{drafted}");
    assert!(drafted.contains(&format!("thread {}", &thread_id.as_str()[..12])), "{drafted}");

    commands::commit(&store).unwrap();
    let sealed = status();
    assert!(sealed.contains("nothing drafted"), "{sealed}");
    assert!(sealed.contains("2 events not yet on origin"), "{sealed}");

    commands::push(&store, "origin").unwrap();
    let pushed = status();
    assert!(pushed.contains("up to date with origin"), "{pushed}");
}

#[test]
fn inbox_tracks_new_activity_across_clones() {
    let (_root, alice, bob) = setup();
    let alice_store = Store::open(&alice).unwrap();
    let bob_store = Store::open(&bob).unwrap();

    let run = |dir: &Path, args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_git-threads"))
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).into_owned()
    };

    // History that predates bob's init is not news: init seeds the mark.
    let first = comment(&alice_store, "pre-existing discussion");
    commands::commit(&alice_store).unwrap();
    commands::push(&alice_store, "origin").unwrap();
    commands::init(&bob_store, "origin").unwrap();
    assert_eq!(run(&bob, &["list", "--new"]).trim(), "no threads");

    // A reply and a fresh thread arrive; both are news to bob.
    commands::reply(&alice_store, first.as_str(), "still relevant?").unwrap();
    let second = comment(&alice_store, "another question");
    commands::commit(&alice_store).unwrap();
    commands::push(&alice_store, "origin").unwrap();
    commands::pull(&bob_store, "origin").unwrap();
    let news = run(&bob, &["list", "--new", "--oneline"]);
    assert!(news.contains("1 new") && news.contains("another question"), "{news}");
    let status = run(&bob, &["status"]);
    assert!(status.contains("2 threads with new activity"), "{status}");

    // show --json reports newness without consuming it; show does consume.
    let json = run(&bob, &["show", first.as_str(), "--json"]);
    assert!(json.contains("\"new\": true"), "{json}");
    let shown = run(&bob, &["show", first.as_str()]);
    assert!(shown.contains("(new)"), "{shown}");
    let remaining = run(&bob, &["list", "--new", "--oneline"]);
    assert!(!remaining.contains(&first.as_str()[..12]), "{remaining}");
    assert!(remaining.contains(&second.as_str()[..12]), "{remaining}");

    // `seen` clears the rest; bob's own writes were never news to him.
    run(&bob, &["seen"]);
    comment(&bob_store, "bob's own thread");
    assert_eq!(run(&bob, &["list", "--new"]).trim(), "no threads");

    // Marks chain: --undo rewinds the bulk `seen` and only that — the
    // thread bob actually read stays read.
    run(&bob, &["seen", "--undo"]);
    let restored = run(&bob, &["list", "--new", "--oneline"]);
    assert!(restored.contains(&second.as_str()[..12]), "{restored}");
    assert!(!restored.contains(&first.as_str()[..12]), "{restored}");

    // Undoing all the way back lands on "nothing seen yet".
    run(&bob, &["seen", "--undo"]); // the show of `first`
    run(&bob, &["seen", "--undo"]); // init's seed mark
    assert!(git(&bob, &["for-each-ref", "refs/threads/seen"]).is_empty());
    let everything = run(&bob, &["list", "--new", "--oneline"]);
    assert!(everything.contains(&first.as_str()[..12]), "{everything}");
    assert!(run(&bob, &["seen", "--undo"]).contains("nothing to undo"));
}

#[test]
fn init_before_any_threads_data_keeps_plain_fetch_working() {
    let (_root, alice, bob) = setup();
    let alice_store = Store::open(&alice).unwrap();

    // No one has pushed threads data yet; a plain fetch must still succeed
    // (an exact refspec would make git fail on the missing remote ref).
    commands::init(&alice_store, "origin").unwrap();
    git(&alice, &["fetch", "-q", "origin"]);

    // Once data appears, the same configured refspec picks it up.
    let bob_store = Store::open(&bob).unwrap();
    comment(&bob_store, "first thread");
    commands::commit(&bob_store).unwrap();
    commands::push(&bob_store, "origin").unwrap();
    git(&alice, &["fetch", "-q", "origin"]);
    commands::integrate_fetched(&alice_store).unwrap();
    assert_eq!(alice_store.threads().unwrap().len(), 1);
}

#[test]
fn init_migrates_legacy_exact_refspec() {
    let (_root, alice, _bob) = setup();
    let store = Store::open(&alice).unwrap();
    git(
        &alice,
        &[
            "config",
            "--add",
            "remote.origin.fetch",
            "+refs/threads/data:refs/threads/remotes/origin/data",
        ],
    );
    commands::init(&store, "origin").unwrap();
    let refspecs = git(&alice, &["config", "--get-all", "remote.origin.fetch"]);
    assert!(!refspecs.contains("+refs/threads/data:"), "legacy refspec removed: {refspecs}");
    assert!(refspecs.contains("+refs/threads/data*:"), "glob refspec added: {refspecs}");
    git(&alice, &["fetch", "-q", "origin"]);
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
    commands::commit(&alice_store).unwrap();
    commands::push(&alice_store, "origin").unwrap();
    commands::pull(&bob_store, "origin").unwrap();

    // Concurrent replies to the same thread from both sides.
    commands::reply(&alice_store, thread_id.as_str(), "alice again").unwrap();
    commands::reply(&bob_store, thread_id.as_str(), "bob here").unwrap();
    commands::resolve(&bob_store, thread_id.as_str(), true).unwrap();
    commands::commit(&bob_store).unwrap();
    commands::push(&bob_store, "origin").unwrap();
    commands::commit(&alice_store).unwrap();
    commands::push(&alice_store, "origin").unwrap();
    commands::pull(&bob_store, "origin").unwrap();

    for store in [&alice_store, &bob_store] {
        let thread = store.read_thread(&thread_id).unwrap().unwrap();
        assert_eq!(thread.events.len(), 4, "root + 2 replies + resolve");
        let folded = git_threads_core::fold_thread(thread.events);
        assert!(folded.resolved);
        assert_eq!(folded.events.len(), 3);
    }
}
