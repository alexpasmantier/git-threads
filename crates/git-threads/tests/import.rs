//! Integration tests for the GitHub importer's mapping layer: `apply` takes
//! review-thread data as the GraphQL API shapes it, so everything below the
//! network seam is exercised against a real repository.

use git_threads::import::{self, GhUser, IdRef, OidRef, Page, PageInfo, ReviewComment, ReviewThread};
use git_threads::store::Store;
use git_threads_core::{AnchorKind, EventKind, Side, fold_thread};
use std::fs;
use std::path::Path;
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

/// A repo shaped like a PR: `base` is the target branch tip, `head` the
/// reviewed commit on top of it. Returns (dir, base oid, head oid).
fn setup_repo() -> (tempfile::TempDir, String, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path();
    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["config", "user.name", "Test"]);
    git(path, &["config", "user.email", "test@example.com"]);
    fs::create_dir(path.join("src")).unwrap();
    fs::write(path.join("src/lib.rs"), "fn a() {}\n").unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-q", "-m", "base"]);
    let base = git(path, &["rev-parse", "HEAD"]);
    fs::write(path.join("src/lib.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-q", "-m", "feature"]);
    let head = git(path, &["rev-parse", "HEAD"]);
    (dir, base, head)
}

fn page<T>(nodes: Vec<T>) -> Page<T> {
    Page { page_info: PageInfo { has_next_page: false, end_cursor: None }, nodes }
}

fn user(login: &str, id: u64) -> Option<GhUser> {
    Some(GhUser { login: login.into(), database_id: Some(id) })
}

fn comment(node_id: &str, body: &str, ts: &str, head: &str, reply_to: Option<&str>) -> ReviewComment {
    ReviewComment {
        id: node_id.into(),
        url: format!("https://github.com/o/r/pull/1#discussion_{node_id}"),
        body: body.into(),
        created_at: ts.into(),
        author: user("bob", 2),
        original_commit: Some(OidRef { oid: head.into() }),
        reply_to: reply_to.map(|id| IdRef { id: id.into() }),
    }
}

fn line_thread(head: &str, comments: Vec<ReviewComment>) -> ReviewThread {
    let _ = head;
    ReviewThread {
        id: "THREAD_1".into(),
        is_resolved: true,
        path: "src/lib.rs".into(),
        subject_type: Some("LINE".into()),
        diff_side: Some("RIGHT".into()),
        start_diff_side: None,
        original_line: Some(2),
        original_start_line: None,
        resolved_by: user("alice", 1),
        comments: page(comments),
    }
}

#[test]
fn import_maps_threads_events_and_anchor() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let thread = line_thread(
        &head,
        vec![
            comment("C_1", "does b need a doc?", "2026-01-01T10:00:00Z", &head, None),
            comment("C_2", "yes, adding one", "2026-01-01T11:00:00Z", &head, Some("C_1")),
        ],
    );

    let report = import::apply(&store, &base, &[thread]).unwrap();
    assert_eq!((report.threads, report.events, report.skipped), (1, 3, 0));

    let records = store.threads().unwrap();
    assert_eq!(records.len(), 1);
    let record = &records[0];

    let anchor = &record.anchor;
    assert_eq!(anchor.kind, AnchorKind::Range);
    assert_eq!(anchor.path.as_deref(), Some("src/lib.rs"));
    assert_eq!(anchor.side, Some(Side::New));
    assert_eq!(anchor.lines.map(|l| (l.start, l.end)), Some((2, 2)));
    assert_eq!(anchor.diff.base.as_str(), base);
    assert_eq!(anchor.diff.head.as_str(), head);
    let blob = git(dir.path(), &["rev-parse", &format!("{head}:src/lib.rs")]);
    assert_eq!(anchor.blob.as_ref().map(|b| b.as_str()), Some(blob.as_str()));

    assert_eq!(record.events.len(), 3);
    let folded = fold_thread(record.events.clone());
    assert!(folded.resolved);
    let root = &folded.events[0];
    assert_eq!(root.event.kind, EventKind::Comment);
    assert_eq!(root.event.author.name, "bob");
    assert_eq!(root.event.author.email, "2+bob@users.noreply.github.com");
    assert_eq!(root.event.extra["origin"]["id"], "C_1");
    assert_eq!(root.event.extra["origin"]["forge"], "github");
    let reply = &folded.events[1];
    assert_eq!(reply.event.in_reply_to, Some(root.id.clone()));
    let resolve = record.events.iter().find(|(_, e)| e.kind == EventKind::Resolve).unwrap();
    assert_eq!(resolve.1.author.name, "alice");
    assert_eq!(resolve.1.ts.as_str(), "2026-01-01T11:00:00Z");
    assert_eq!(resolve.1.extra["origin"]["id"], "THREAD_1");

    // Anchored-commit retention: the publish commit pins the reviewed head.
    let tip_parents = git(dir.path(), &["rev-list", "--parents", "-1", "refs/threads/data"]);
    assert!(tip_parents.contains(&head), "import commit pins {head}: {tip_parents}");
}

#[test]
fn reimport_is_a_noop_and_new_replies_append() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let root = comment("C_1", "does b need a doc?", "2026-01-01T10:00:00Z", &head, None);
    let thread = line_thread(&head, vec![root.clone()]);

    import::apply(&store, &base, &[thread.clone()]).unwrap();
    let tip = store.tip().unwrap();

    let again = import::apply(&store, &base, &[thread]).unwrap();
    assert_eq!((again.threads, again.events, again.known), (0, 0, 1));
    assert_eq!(store.tip().unwrap(), tip, "no-op reimport writes nothing");

    // The same thread grew a reply on GitHub: only the reply lands, in the
    // thread the first import created.
    let mut grown = line_thread(
        &head,
        vec![root, comment("C_2", "yes", "2026-01-01T12:00:00Z", &head, Some("C_1"))],
    );
    grown.is_resolved = false;
    let update = import::apply(&store, &base, &[grown]).unwrap();
    assert_eq!((update.threads, update.events, update.known), (1, 1, 1));
    let records = store.threads().unwrap();
    assert_eq!(records.len(), 1, "reply appended, no second thread");
    assert_eq!(records[0].events.len(), 3, "comment + resolve + new reply");
}

#[test]
fn unreconstructable_code_skips_the_thread() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let gone = "0123456789012345678901234567890123456789";
    let thread = line_thread(
        &head,
        vec![comment("C_1", "on a force-pushed head", "2026-01-01T10:00:00Z", gone, None)],
    );

    let report = import::apply(&store, &base, &[thread]).unwrap();
    assert_eq!((report.threads, report.events, report.skipped), (0, 0, 1));
    assert!(store.threads().unwrap().is_empty());
}

#[test]
fn file_level_comment_maps_to_a_file_anchor() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let mut thread = line_thread(
        &head,
        vec![comment("C_1", "rename this file?", "2026-01-01T10:00:00Z", &head, None)],
    );
    thread.subject_type = Some("FILE".into());
    thread.original_line = None;
    thread.is_resolved = false;

    let report = import::apply(&store, &base, &[thread]).unwrap();
    assert_eq!((report.threads, report.events), (1, 1));
    let records = store.threads().unwrap();
    let anchor = &records[0].anchor;
    assert_eq!(anchor.kind, AnchorKind::File);
    assert!(anchor.lines.is_none());
    assert!(anchor.blob.is_some());
}

#[test]
fn list_pr_filters_by_origin_url() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let thread = line_thread(
        &head,
        vec![comment("C_1", "from the PR", "2026-01-01T10:00:00Z", &head, None)],
    );
    import::apply(&store, &base, &[thread]).unwrap();

    let list = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_git-threads"))
            .current_dir(dir.path())
            .arg("list")
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    // The fixture's origin URLs point into pull/1; the segment must match
    // exactly, and --mr is the same flag.
    let hit = list(&["--pr", "1"]);
    assert!(hit.contains("from the PR"), "{hit}");
    assert_eq!(list(&["--pr", "11"]).trim(), "no threads");
    assert!(list(&["--mr", "1"]).contains("from the PR"));
}
