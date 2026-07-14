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
    CommentOpts { target: None, file: None, message: message.into(), side: Side::New }
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
    range.file = Some("src/lib.rs:2-3".into());
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
    bad.file = Some("src/lib.rs:2-9".into());
    let err = commands::comment(&store, &bad).unwrap_err();
    assert!(err.to_string().contains("out of range"), "unexpected error: {err:#}");
}

#[test]
fn positional_target_takes_commit_or_file() {
    let dir = setup_repo();
    let store = Store::open(dir.path()).unwrap();

    // A file (with lines) as the positional anchors on HEAD's change.
    let mut spec = opts("on lines");
    spec.target = Some("src/lib.rs:2-3".into());
    let thread = store.read_thread(&commands::comment(&store, &spec).unwrap()).unwrap().unwrap();
    assert_eq!(thread.anchor.kind, AnchorKind::Range);
    assert_eq!(thread.anchor.path.as_deref(), Some("src/lib.rs"));

    // A commit-ish as the positional anchors the whole change.
    let mut rev = opts("on a commit");
    rev.target = Some("HEAD".into());
    let thread = store.read_thread(&commands::comment(&store, &rev).unwrap()).unwrap().unwrap();
    assert_eq!(thread.anchor.kind, AnchorKind::Commit);

    // Neither a commit nor a file is an error.
    let mut bad = opts("nope");
    bad.target = Some("no/such/thing".into());
    let err = commands::comment(&store, &bad).unwrap_err();
    assert!(err.to_string().contains("neither"), "unexpected error: {err:#}");
}

#[test]
fn range_target_sets_the_diff_and_takes_a_file_positional() {
    let dir = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let rev = |spec: &str| git_out(dir.path(), &["rev-parse", spec]);

    // A two-dot range names the diff base explicitly.
    let mut range = opts("across both commits");
    range.target = Some("HEAD~1..HEAD".into());
    range.file = Some("src/lib.rs:2-3".into());
    let thread = store.read_thread(&commands::comment(&store, &range).unwrap()).unwrap().unwrap();
    assert_eq!(thread.anchor.kind, AnchorKind::Range);
    assert_eq!(thread.anchor.diff.base.as_str(), rev("HEAD~1"));
    assert_eq!(thread.anchor.diff.head.as_str(), rev("HEAD"));

    // Three dots diff against the merge base; an empty side means HEAD.
    let mut merge = opts("branch-level");
    merge.target = Some("HEAD~1...".into());
    let thread = store.read_thread(&commands::comment(&store, &merge).unwrap()).unwrap().unwrap();
    assert_eq!(thread.anchor.kind, AnchorKind::Commit);
    assert_eq!(thread.anchor.diff.base.as_str(), rev("HEAD~1"));
    assert_eq!(thread.anchor.diff.head.as_str(), rev("HEAD"));

    // Anything that looks like a range is one — a bad side errors instead of
    // falling back to the file interpretation.
    let mut bad = opts("nope");
    bad.target = Some("nope..HEAD".into());
    let err = commands::comment(&store, &bad).unwrap_err();
    assert!(err.to_string().contains("cannot resolve"), "unexpected error: {err:#}");
}

#[test]
#[cfg(unix)]
fn missing_message_opens_git_editor() {
    use std::os::unix::fs::PermissionsExt;
    let dir = setup_repo();
    let path = dir.path();
    let editor = path.join("fake-editor.sh");
    fs::write(&editor, "#!/bin/sh\necho 'from the editor' > \"$1\"\n").unwrap();
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();

    let run = |editor: &Path| {
        Command::new(env!("CARGO_BIN_EXE_git-threads"))
            .current_dir(path)
            .env("GIT_EDITOR", editor)
            .arg("comment")
            .output()
            .unwrap()
    };
    let output = run(&editor);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let store = Store::open(path).unwrap();
    let thread = store.threads().unwrap().pop().unwrap();
    assert_eq!(thread.events[0].1.body.as_deref(), Some("from the editor"));

    // An editor that leaves only the commented hint aborts the comment.
    let noop = path.join("noop-editor.sh");
    fs::write(&noop, "#!/bin/sh\ntrue\n").unwrap();
    fs::set_permissions(&noop, fs::Permissions::from_mode(0o755)).unwrap();
    let output = run(&noop);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    // A quiet notice like git's, not an Error dump.
    assert!(stderr.contains("empty message"), "unexpected stderr: {stderr}");
    assert!(!stderr.contains("Error"), "unexpected stderr: {stderr}");
    assert_eq!(store.threads().unwrap().len(), 1);
}

#[test]
fn old_side_comment_reads_base_blob() {
    let dir = setup_repo();
    let store = Store::open(dir.path()).unwrap();

    let mut old = opts("this only had one line before");
    old.file = Some("src/lib.rs:1".into());
    old.side = Side::Old;
    let thread_id = commands::comment(&store, &old).unwrap();

    let thread = store.read_thread(&thread_id).unwrap().unwrap();
    // Base version has exactly 1 line, so line 1 is valid but line 2 is not.
    let mut bad = opts("nope");
    bad.file = Some("src/lib.rs:2".into());
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
fn comments_must_touch_the_diff_unless_it_is_empty() {
    let dir = setup_repo();
    let path = dir.path();
    // A 12-line file, then a change to its last line only.
    let lines: Vec<String> = (1..=12).map(|n| format!("line {n}")).collect();
    fs::write(path.join("notes.txt"), lines.join("\n") + "\n").unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-q", "-m", "add notes"]);
    let mut changed = lines.clone();
    changed[11] = "line 12, revised".into();
    fs::write(path.join("notes.txt"), changed.join("\n") + "\n").unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-q", "-m", "revise last line"]);
    let store = Store::open(path).unwrap();

    // Changed and nearby-context lines are fine; far-away lines are not.
    let mut on_change = opts("on the change");
    on_change.file = Some("notes.txt:10-12".into());
    commands::comment(&store, &on_change).unwrap();
    let mut outside = opts("nope");
    outside.file = Some("notes.txt:2".into());
    let err = commands::comment(&store, &outside).unwrap_err();
    assert!(err.to_string().contains("outside the change"), "unexpected error: {err:#}");
    assert!(err.to_string().contains("an empty diff"), "unexpected error: {err:#}");

    // A file the diff never touches is rejected at any granularity.
    let mut untouched = opts("nope");
    untouched.file = Some("src/lib.rs".into());
    let err = commands::comment(&store, &untouched).unwrap_err();
    assert!(err.to_string().contains("unchanged"), "unexpected error: {err:#}");

    // The escape hatch: an empty diff annotates the snapshot, no questions asked.
    let mut snapshot = opts("audit note");
    snapshot.target = Some("HEAD..HEAD".into());
    snapshot.file = Some("notes.txt:2".into());
    let thread =
        store.read_thread(&commands::comment(&store, &snapshot).unwrap()).unwrap().unwrap();
    assert_eq!(thread.anchor.diff.base, thread.anchor.diff.head);
}

#[test]
fn list_filters_by_change_and_open_state() {
    let dir = setup_repo();
    let path = dir.path();
    fs::write(path.join("src/lib.rs"), "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\n").unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-q", "-m", "add d"]);
    let store = Store::open(path).unwrap();

    let mut on_middle = opts("on the middle commit");
    on_middle.target = Some("HEAD~1".into());
    commands::comment(&store, &on_middle).unwrap();
    let on_tip = commands::comment(&store, &opts("on the tip")).unwrap();
    commands::resolve(&store, on_tip.as_str(), true).unwrap();
    let mut on_file = opts("on the file");
    on_file.file = Some("src/lib.rs:4".into());
    commands::comment(&store, &on_file).unwrap();

    let list = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_git-threads"))
            .current_dir(path)
            .arg("list")
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).into_owned()
    };

    // A range keeps only threads whose anchored head is one of its commits.
    let tip_only = list(&["HEAD~1..HEAD"]);
    assert!(tip_only.contains("on the tip") && !tip_only.contains("middle"), "{tip_only}");
    let both = list(&["HEAD~2..HEAD"]);
    assert!(both.contains("on the tip") && both.contains("middle"), "{both}");

    // --open hides resolved threads, --resolved keeps only them; an emptied
    // listing says so.
    let open = list(&["HEAD~2..HEAD", "--open"]);
    assert!(open.contains("middle") && !open.contains("on the tip"), "{open}");
    let open_tip = list(&["HEAD~1..HEAD", "--open"]);
    assert!(open_tip.contains("on the file") && !open_tip.contains("on the tip"), "{open_tip}");
    let resolved = list(&["--resolved"]);
    assert!(resolved.contains("on the tip") && !resolved.contains("middle"), "{resolved}");

    // A lone path filters across all changes; directories match by prefix,
    // a line suffix by overlap with the anchored lines.
    let by_file = list(&["src/lib.rs"]);
    assert!(by_file.contains("on the file") && !by_file.contains("on the tip"), "{by_file}");
    assert!(list(&["src"]).contains("on the file"));
    assert_eq!(list(&["src/lib.rs:1"]).trim(), "no threads");

    // Change and path filters compose.
    let composed = list(&["HEAD~1..HEAD", "src/lib.rs"]);
    assert!(composed.contains("on the file") && !composed.contains("on the tip"), "{composed}");

    // A spec that is neither a commit nor a known path is an error, not an
    // empty listing.
    let output = Command::new(env!("CARGO_BIN_EXE_git-threads"))
        .current_dir(path)
        .args(["list", "nope"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("neither"));
}

#[test]
fn list_grep_searches_folded_bodies() {
    let dir = setup_repo();
    let path = dir.path();
    let store = Store::open(path).unwrap();

    let questions = commands::comment(&store, &opts("does this handle empty input?")).unwrap();
    commands::reply(&store, questions.as_str(), "only for UTF-8 payloads").unwrap();
    let retracted = commands::comment(&store, &opts("payloads look wrong")).unwrap();
    commands::delete(&store, retracted.as_str()).unwrap();

    let list = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_git-threads"))
            .current_dir(path)
            .args(["list", "--oneline"])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).into_owned()
    };

    // Case-insensitive, and replies count, not just the root.
    let hit = list(&["--grep", "EMPTY input"]);
    assert!(hit.contains("empty input"), "{hit}");
    let via_reply = list(&["--grep", "utf-8"]);
    assert!(via_reply.contains("empty input"), "{via_reply}");

    // Retracted text no longer matches; the other thread still does.
    let gone = list(&["--grep", "payloads"]);
    assert!(gone.contains("empty input") && !gone.contains("look wrong"), "{gone}");
    assert_eq!(list(&["--grep", "no such text"]).trim(), "no threads");
}

#[test]
fn json_output_carries_folded_state_and_placement() {
    let dir = setup_repo();
    let path = dir.path();
    let store = Store::open(path).unwrap();

    let mut on_lines = opts("does b need a body?");
    on_lines.file = Some("src/lib.rs:2".into());
    let thread_id = commands::comment(&store, &on_lines).unwrap();
    let reply_id = commands::reply(&store, thread_id.as_str(), "wrong thread, sorry").unwrap();
    commands::delete(&store, reply_id.as_str()).unwrap();

    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_git-threads"))
            .current_dir(path)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        serde_json::from_slice::<serde_json::Value>(&output.stdout).expect("valid JSON")
    };

    let listed = run(&["list", "--json"]);
    let threads = listed.as_array().unwrap();
    assert_eq!(threads.len(), 1);
    let thread = &threads[0];
    assert_eq!(thread["id"], thread_id.as_str());
    assert_eq!(thread["resolved"], false);
    assert_eq!(thread["anchor"]["kind"], "range");
    assert_eq!(thread["anchor"]["lines"]["start"], 2);
    // The anchored blob is HEAD's, so placement is an exact hit.
    assert_eq!(thread["placement"]["kind"], "located");
    assert_eq!(thread["placement"]["status"], "exact");
    let messages = thread["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    // Same-second events order by ID tie-break; locate by kind.
    let root = messages.iter().find(|m| m["type"] == "comment").unwrap();
    assert_eq!(root["body"], "does b need a body?");
    assert_eq!(root["draft"], true);
    // Retraction folds into the output: flag set, body withheld.
    let reply = messages.iter().find(|m| m["type"] == "reply").unwrap();
    assert_eq!(reply["retracted"], true);
    assert_eq!(reply["body"], serde_json::Value::Null);

    // show --json emits the same object, and filters compose with list.
    let shown = run(&["show", &thread_id.as_str()[..8], "--json"]);
    assert_eq!(&shown, thread);
    let none = run(&["list", "--json", "--resolved"]);
    assert_eq!(none.as_array().unwrap().len(), 0);
}

#[test]
fn root_commit_suggests_a_range() {
    let dir = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let mut on_root = opts("first!");
    on_root.target = Some("HEAD~1".into()); // the root commit — no parent
    let err = commands::comment(&store, &on_root).unwrap_err();
    assert!(err.to_string().contains("has no parent"), "unexpected error: {err:#}");
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

#[test]
fn session_of_drafts_publishes_as_one_commit() {
    let dir = setup_repo();
    let store = Store::open(dir.path()).unwrap();

    // A "review session": two threads and a reply, all drafted.
    let first = commands::comment(&store, &opts("first thread")).unwrap();
    commands::reply(&store, first.as_str(), "self-reply").unwrap();
    commands::comment(&store, &opts("second thread")).unwrap();
    assert!(store.tip().unwrap().is_none(), "nothing published yet");
    assert!(store.drafts_tip().unwrap().is_some());

    let promoted = store.commit_drafts().unwrap().expect("drafts promoted");
    assert_eq!((promoted.events, promoted.threads), (3, 2));
    assert!(store.drafts_tip().unwrap().is_none(), "drafts ref cleared");

    // One commit, with the batched message, pinning the anchored commit.
    let head = git_out(dir.path(), &["rev-parse", "HEAD"]);
    let count = git_out(dir.path(), &["rev-list", "--count", "refs/threads/data", &format!("^{head}")]);
    assert_eq!(count, "1");
    let subject = git_out(dir.path(), &["log", "--pretty=%s", "-1", "refs/threads/data"]);
    assert_eq!(subject, "threads: 3 events in 2 threads");
    let parents = git_out(dir.path(), &["log", "--pretty=%P", "-1", "refs/threads/data"]);
    assert_eq!(parents, head, "sole parent is the anchored commit pin");

    // Drafts folded into the published snapshot; nothing marked draft anymore.
    let thread = store.read_thread(&first).unwrap().unwrap();
    assert_eq!(thread.events.len(), 2);
    assert!(thread.drafts.is_empty());
}

#[test]
fn drafts_are_visible_and_marked_before_publish() {
    let dir = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let thread_id = commands::comment(&store, &opts("draft me")).unwrap();

    let thread = store.read_thread(&thread_id).unwrap().expect("draft visible");
    assert_eq!(thread.events.len(), 1);
    assert!(thread.drafts.contains(&thread_id), "root marked as draft");

    // Drafted events are addressable: replying to a draft works.
    let reply_id = commands::reply(&store, thread_id.as_str(), "reply to a draft").unwrap();
    let thread = store.read_thread(&thread_id).unwrap().unwrap();
    assert!(thread.drafts.contains(&reply_id));
}

#[test]
fn discard_removes_a_draft_or_a_whole_draft_thread() {
    let dir = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let thread_id = commands::comment(&store, &opts("keep or toss")).unwrap();
    let reply_id = commands::reply(&store, thread_id.as_str(), "toss this").unwrap();

    commands::discard(&store, reply_id.as_str()).unwrap();
    let thread = store.read_thread(&thread_id).unwrap().unwrap();
    assert_eq!(thread.events.len(), 1, "reply gone, root remains");

    // Discarding the root takes the whole draft thread with it.
    commands::discard(&store, thread_id.as_str()).unwrap();
    assert!(store.read_thread(&thread_id).unwrap().is_none());
    assert!(store.drafts_tip().unwrap().is_none(), "empty drafts ref deleted");

    // Published events can never be discarded.
    let published = commands::comment(&store, &opts("published")).unwrap();
    store.commit_drafts().unwrap().unwrap();
    let err = commands::discard(&store, published.as_str()).unwrap_err();
    assert!(err.to_string().contains("no draft matches"), "unexpected error: {err:#}");
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("failed to run git");
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}
