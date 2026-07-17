//! Integration tests for the storage layer (SPEC.md §5), against a real
//! temporary repository. Repo setup shells out to git; everything under test
//! goes through `Store`.

use git_threads_core::{
    Anchor, AnchorKind, Author, DiffRef, Event, EventId, EventKind, GitOid, Timestamp,
};
use std::path::Path;
use std::process::Command;

use git_threads::store::{Append, Batch, NewThread, Store};

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

/// Init a repo with identity configured and two empty commits to anchor to.
/// Returns (tempdir, base commit, head commit).
fn setup_repo() -> (tempfile::TempDir, String, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path();
    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["config", "user.name", "Test"]);
    git(path, &["config", "user.email", "test@example.com"]);
    git(path, &["commit", "-q", "--allow-empty", "-m", "c1"]);
    let base = git(path, &["rev-parse", "HEAD"]);
    git(path, &["commit", "-q", "--allow-empty", "-m", "c2"]);
    let head = git(path, &["rev-parse", "HEAD"]);
    (dir, base, head)
}

fn commit_anchor(base: &str, head: &str) -> Anchor {
    Anchor {
        v: 1,
        kind: AnchorKind::Commit,
        diff: DiffRef {
            base: GitOid::from_hex(base).unwrap(),
            head: GitOid::from_hex(head).unwrap(),
        },
        path: None,
        old_path: None,
        side: None,
        lines: None,
        blob: None,
        cols: None,
        extra: Default::default(),
    }
}

fn event(kind: EventKind, ts: &str) -> Event {
    Event {
        v: 1,
        kind,
        author: Author { name: "Test".into(), email: "test@example.com".into() },
        ts: Timestamp::parse(ts).unwrap(),
        body: None,
        in_reply_to: None,
        supersedes: None,
        resolved: None,
        anchor: None,
        extra: Default::default(),
    }
}

fn comment(ts: &str, body: &str) -> Event {
    let mut e = event(EventKind::Comment, ts);
    e.body = Some(body.into());
    e
}

fn reply(ts: &str, to: &EventId, body: &str) -> Event {
    let mut e = event(EventKind::Reply, ts);
    e.in_reply_to = Some(to.clone());
    e.body = Some(body.into());
    e
}

fn new_thread_batch(base: &str, head: &str, body: &str) -> (Batch, Event) {
    let root = comment("2026-07-03T10:00:00Z", body);
    let batch = Batch {
        new_threads: vec![NewThread {
            anchor: commit_anchor(base, head),
            root: root.clone(),
            events: vec![],
        }],
        appends: vec![],
    };
    (batch, root)
}

#[test]
fn write_then_read_round_trips() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let (batch, root) = new_thread_batch(&base, &head, "does this handle merge commits?");

    let tip = store.write(&batch).unwrap();
    assert_eq!(store.tip().unwrap(), Some(tip));

    let threads = store.threads().unwrap();
    assert_eq!(threads.len(), 1);
    let thread = &threads[0];
    assert_eq!(thread.id, root.id().unwrap());
    assert_eq!(thread.anchor, batch.new_threads[0].anchor);
    assert_eq!(thread.events, vec![(root.id().unwrap(), root.clone())]);

    let by_id = store.read_thread(&thread.id).unwrap().expect("thread found");
    assert_eq!(by_id.events.len(), 1);
}

#[test]
fn on_disk_layout_is_sharded_and_canonical() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let (batch, root) = new_thread_batch(&base, &head, "layout check");
    store.write(&batch).unwrap();

    let tid = root.id().unwrap();
    let anchor_path = format!("refs/threads/data:threads/{}/{tid}/anchor.json", &tid.as_str()[..2]);
    let on_disk = git(dir.path(), &["cat-file", "-p", &anchor_path]);
    let expected = String::from_utf8(
        git_threads_core::to_canonical_json(&batch.new_threads[0].anchor).unwrap(),
    )
    .unwrap();
    assert_eq!(on_disk, expected);

    let event_path =
        format!("refs/threads/data:threads/{}/{tid}/events/{tid}.json", &tid.as_str()[..2]);
    let event_on_disk = git(dir.path(), &["cat-file", "-p", &event_path]);
    assert_eq!(event_on_disk.as_bytes(), root.canonical_json().unwrap());
}

#[test]
fn anchored_commit_becomes_extra_parent() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let (batch, _) = new_thread_batch(&base, &head, "retention check");
    store.write(&batch).unwrap();

    // Initial write: no previous tip, so the anchored head is the sole parent.
    let parents = git(dir.path(), &["log", "--pretty=%P", "-1", "refs/threads/data"]);
    assert_eq!(parents, head);

    // Deleting the branch must not orphan the anchored commit.
    git(dir.path(), &["checkout", "-q", "--detach"]);
    git(dir.path(), &["branch", "-D", "main"]);
    let reachable = git(dir.path(), &["rev-list", "refs/threads/data"]);
    assert!(reachable.lines().any(|line| line == head));
}

#[test]
fn append_chains_onto_previous_tip() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let (batch, root) = new_thread_batch(&base, &head, "root");
    let first_tip = store.write(&batch).unwrap();

    let thread_id = root.id().unwrap();
    let reply = reply("2026-07-03T11:00:00Z", &thread_id, "agreed");
    let second_tip = store
        .write(&Batch {
            new_threads: vec![],
            appends: vec![Append { thread: thread_id.clone(), events: vec![reply.clone()] }],
        })
        .unwrap();
    assert_ne!(first_tip, second_tip);

    let parents = git(dir.path(), &["log", "--pretty=%P", "-1", "refs/threads/data"]);
    assert_eq!(parents, first_tip.to_string());

    let thread = store.read_thread(&thread_id).unwrap().unwrap();
    assert_eq!(thread.events.len(), 2);
    assert!(thread.events.contains(&(reply.id().unwrap(), reply)));
}

#[test]
fn append_to_unknown_thread_fails() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let (batch, root) = new_thread_batch(&base, &head, "root");
    store.write(&batch).unwrap();

    let missing = EventId::from_hex("f".repeat(40)).unwrap();
    let result = store.write(&Batch {
        new_threads: vec![],
        appends: vec![Append {
            thread: missing,
            events: vec![reply("2026-07-03T11:00:00Z", &root.id().unwrap(), "lost")],
        }],
    });
    assert!(result.is_err());
}

/// Add a blob at `path` on top of `refs/threads/data`'s tip tree, bypassing
/// the store — the way a buggy or hostile writer would.
fn inject_file(dir: &Path, path: &str, content: &str) {
    let blob = {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["hash-object", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child.stdin.take().unwrap().write_all(content.as_bytes())?;
                child.wait_with_output()
            })
            .expect("hash-object");
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    git(dir, &["read-tree", "refs/threads/data^{tree}"]);
    git(dir, &["update-index", "--add", "--cacheinfo", &format!("100644,{blob},{path}")]);
    let tree = git(dir, &["write-tree"]);
    let tip = git(dir, &["rev-parse", "refs/threads/data"]);
    let commit = git(dir, &["commit-tree", &tree, "-p", &tip, "-m", "inject"]);
    git(dir, &["update-ref", "refs/threads/data", &commit]);
}

#[test]
fn malformed_files_are_skipped_not_fatal() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let (batch, root) = new_thread_batch(&base, &head, "the valid thread");
    store.write(&batch).unwrap();
    let valid_id = root.id().unwrap();

    // A thread directory whose anchor.json is garbage, with a garbage event.
    let bogus = "ff".repeat(20);
    inject_file(dir.path(), &format!("threads/ff/{bogus}/anchor.json"), "not json");
    inject_file(dir.path(), &format!("threads/ff/{bogus}/events/{bogus}.json"), "{}");
    // A garbage event inside the valid thread.
    let junk =
        format!("threads/{}/{valid_id}/events/{}.json", &valid_id.as_str()[..2], "e".repeat(40));
    inject_file(dir.path(), &junk, "not json either");

    // The valid thread still reads; the junk is skipped, not fatal.
    let threads = store.threads().expect("read must survive malformed files");
    assert_eq!(threads.len(), 1);
    let thread = &threads[0];
    assert_eq!(thread.id, valid_id);
    assert_eq!(thread.events.len(), 1, "junk event skipped");
}

#[test]
fn event_content_must_hash_to_its_filename() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let (batch, root) = new_thread_batch(&base, &head, "say what I wrote");
    store.write(&batch).unwrap();
    let thread_id = root.id().unwrap();

    // Overwrite the root event's blob in place: valid JSON, same author, but
    // different words — the filename no longer matches the content hash.
    let mut forged = root.clone();
    forged.body = Some("something the author never said".into());
    let forged_bytes = String::from_utf8(forged.canonical_json().unwrap()).unwrap();
    let path = format!("threads/{}/{thread_id}/events/{thread_id}.json", &thread_id.as_str()[..2]);
    inject_file(dir.path(), &path, &forged_bytes);

    // The forged content is rejected; the thread survives without it.
    let thread = store.read_thread(&thread_id).unwrap();
    let bodies: Vec<String> =
        thread.into_iter().flat_map(|t| t.events).filter_map(|(_, e)| e.body).collect();
    assert!(
        !bodies.iter().any(|b| b.contains("never said")),
        "forged event content must not be attributed to its original ID: {bodies:?}"
    );
}

#[test]
fn duplicate_publish_is_a_no_op() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let (batch, _) = new_thread_batch(&base, &head, "once");

    let first = store.write(&batch).unwrap();
    let second = store.write(&batch).unwrap();
    assert_eq!(first, second, "identical content must not create a new commit");
    // Exactly one threads commit exists (excluding the anchored code history).
    let count = git(dir.path(), &["rev-list", "--count", "refs/threads/data", &format!("^{head}")]);
    assert_eq!(count, "1");
}
