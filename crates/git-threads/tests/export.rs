//! Integration tests for the export planner: `plan` takes the foreign
//! change's state as data, so everything below the network seam is exercised
//! against a real repository — the same seam the import tests use.

use git_threads::export::{self, ChangeState, ForeignThread, Position, Target};
use git_threads::import::{self, GhUser, IdRef, OidRef, Page, PageInfo, ReviewComment};
use git_threads::store::{Batch, NewThread, Store};
use git_threads_core::{
    Anchor, AnchorKind, Author, DiffRef, Event, EventId, EventKind, GitOid, LineRange, Side,
    Timestamp, fold_thread,
};
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

/// A repo shaped like a PR: 20 numbered lines in `src/lib.rs`, of which the
/// head commit changes only line 10; `README.md` is untouched by the change.
/// Returns (dir, base oid, head oid).
fn setup_repo() -> (tempfile::TempDir, String, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path();
    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["config", "user.name", "Test"]);
    git(path, &["config", "user.email", "test@example.com"]);
    let numbered: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    fs::create_dir(path.join("src")).unwrap();
    fs::write(path.join("src/lib.rs"), &numbered).unwrap();
    fs::write(path.join("README.md"), "# readme\nkeep me\nend\n").unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-q", "-m", "base"]);
    let base = git(path, &["rev-parse", "HEAD"]);
    fs::write(path.join("src/lib.rs"), numbered.replace("line 10\n", "line ten, changed\n"))
        .unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-q", "-m", "feature"]);
    let head = git(path, &["rev-parse", "HEAD"]);
    (dir, base, head)
}

fn anchor(dir: &Path, base: &str, head: &str, path: &str, lines: Option<(u32, u32)>) -> Anchor {
    let blob = git(dir, &["rev-parse", &format!("{head}:{path}")]);
    Anchor {
        v: 1,
        kind: if lines.is_some() { AnchorKind::Range } else { AnchorKind::File },
        diff: DiffRef {
            base: GitOid::from_hex(base).unwrap(),
            head: GitOid::from_hex(head).unwrap(),
        },
        path: Some(path.into()),
        old_path: None,
        side: Some(Side::New),
        lines: lines.map(|(start, end)| LineRange { start, end }),
        blob: Some(GitOid::from_hex(blob).unwrap()),
        cols: None,
        extra: Default::default(),
    }
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

fn event(kind: EventKind, author: (&str, &str), ts: &str) -> Event {
    Event {
        v: 1,
        kind,
        author: Author { name: author.0.into(), email: author.1.into() },
        ts: Timestamp::parse(ts).unwrap(),
        body: None,
        in_reply_to: None,
        supersedes: None,
        resolved: None,
        anchor: None,
        of: None,
        extra: Default::default(),
    }
}

/// The exporter's own identity, per the fixture repo's git config.
const ME: (&str, &str) = ("Test", "test@example.com");
const BOB: (&str, &str) = ("bob", "bob@example.com");

fn comment(author: (&str, &str), ts: &str, body: &str) -> Event {
    let mut e = event(EventKind::Comment, author, ts);
    e.body = Some(body.into());
    e
}

fn reply(author: (&str, &str), ts: &str, to: &EventId, body: &str) -> Event {
    let mut e = event(EventKind::Reply, author, ts);
    e.in_reply_to = Some(to.clone());
    e.body = Some(body.into());
    e
}

fn resolve(author: (&str, &str), ts: &str, resolved: bool) -> Event {
    let mut e = event(EventKind::Resolve, author, ts);
    e.resolved = Some(resolved);
    e
}

fn with_origin(mut e: Event, id: &str) -> Event {
    e.extra.insert(
        "origin".into(),
        serde_json::json!({
            "forge": "github",
            "id": id,
            "url": format!("https://github.com/o/r/pull/1#discussion_{id}"),
        }),
    );
    e
}

fn mirror(ts: &str, of: &EventId, foreign_id: &str) -> Event {
    let mut e = event(EventKind::Mirror, ME, ts);
    e.of = Some(of.clone());
    with_origin(e, foreign_id)
}

fn write_thread(store: &Store, anchor: Anchor, root: Event, events: Vec<Event>) -> EventId {
    let id = root.id().unwrap();
    let batch = Batch { new_threads: vec![NewThread { anchor, root, events }], appends: vec![] };
    store.write(&batch).unwrap();
    id
}

fn change(base: &str, head: &str, threads: Vec<ForeignThread>) -> ChangeState {
    ChangeState {
        number: 1,
        base_ref_oid: base.to_string(),
        head_ref_oid: head.to_string(),
        threads,
    }
}

fn foreign(id: &str, resolved: bool, comments: &[&str]) -> ForeignThread {
    ForeignThread {
        id: id.into(),
        is_resolved: resolved,
        comments: comments.iter().map(|c| c.to_string()).collect(),
        is_pr_level: false,
    }
}

#[test]
fn new_thread_on_visible_lines_plans_a_line_comment() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let root = comment(ME, "2026-01-01T10:00:00Z", "is ten right?");
    let root_id = root.id().unwrap();
    let answer = reply(BOB, "2026-01-01T11:00:00Z", &root_id, "yes");
    write_thread(
        &store,
        anchor(dir.path(), &base, &head, "src/lib.rs", Some((10, 10))),
        root,
        vec![answer],
    );
    // A thread anchored outside the change never enters the plan.
    write_thread(
        &store,
        anchor(dir.path(), &base, &base, "src/lib.rs", Some((1, 1))),
        comment(ME, "2026-01-01T10:00:00Z", "elsewhere"),
        vec![],
    );

    let plan = export::plan(&store, &change(&base, &head, vec![])).unwrap();
    assert!(plan.skips.is_empty());
    assert_eq!(plan.threads.len(), 1);
    let thread = &plan.threads[0];
    assert_eq!(thread.thread, root_id);
    match &thread.target {
        Target::New { position: Position::Line { path, side, lines } } => {
            assert_eq!(path, "src/lib.rs");
            assert_eq!(*side, Side::New);
            assert_eq!((lines.start, lines.end), (10, 10));
        }
        other => panic!("expected a line position, got {other:?}"),
    }
    assert_eq!(thread.posts.len(), 2);
    // The exporter's own comment posts bare; anyone else's carries the
    // attribution header (everything goes out under one account).
    assert_eq!(thread.posts[0].body, "is ten right?");
    assert_eq!(thread.posts[1].body, "**bob** · 2026-01-01 · via git-threads\n\nyes");
    assert!(thread.resolve.is_none());
}

#[test]
fn imported_thread_contributes_only_whats_new() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let root = with_origin(comment(BOB, "2026-01-01T10:00:00Z", "from github"), "C_1");
    let root_id = root.id().unwrap();
    let answer = reply(ME, "2026-01-02T10:00:00Z", &root_id, "answered locally");
    let answer_id = answer.id().unwrap();
    let resolution = resolve(ME, "2026-01-02T10:00:01Z", true);
    let resolution_id = resolution.id().unwrap();
    write_thread(
        &store,
        anchor(dir.path(), &base, &head, "src/lib.rs", Some((10, 10))),
        root,
        vec![answer, resolution],
    );

    let plan =
        export::plan(&store, &change(&base, &head, vec![foreign("T_1", false, &["C_1"])])).unwrap();
    assert_eq!(plan.threads.len(), 1);
    let thread = &plan.threads[0];
    match &thread.target {
        Target::Existing { foreign_thread } => assert_eq!(foreign_thread, "T_1"),
        other => panic!("expected the existing foreign thread, got {other:?}"),
    }
    assert_eq!(thread.posts.len(), 1, "the imported root is not re-posted");
    assert_eq!(thread.posts[0].event, answer_id);
    let action = thread.resolve.as_ref().expect("a toggle is planned");
    assert_eq!((action.event.clone(), action.to), (resolution_id, true));
}

#[test]
fn stale_local_resolution_is_never_pushed_back() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let root = with_origin(comment(BOB, "2026-01-01T10:00:00Z", "from github"), "C_1");
    // The synthetic resolve import writes: resolved on the forge back then.
    let imported = with_origin(resolve(BOB, "2026-01-01T11:00:00Z", true), "T_1");
    write_thread(
        &store,
        anchor(dir.path(), &base, &head, "src/lib.rs", Some((10, 10))),
        root,
        vec![imported],
    );

    // Someone reopened the thread on the forge since: the forge has the
    // newer say, and the stale local state must not be exported.
    let plan =
        export::plan(&store, &change(&base, &head, vec![foreign("T_1", false, &["C_1"])])).unwrap();
    assert!(plan.threads.is_empty(), "{:?}", plan.threads);
}

#[test]
fn mirrored_events_are_not_reposted() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let root = comment(ME, "2026-01-01T10:00:00Z", "exported earlier");
    let root_id = root.id().unwrap();
    let marker = mirror("2026-01-01T10:05:00Z", &root_id, "C_9");
    write_thread(
        &store,
        anchor(dir.path(), &base, &head, "src/lib.rs", Some((10, 10))),
        root,
        vec![marker],
    );

    let plan =
        export::plan(&store, &change(&base, &head, vec![foreign("T_9", false, &["C_9"])])).unwrap();
    assert!(plan.threads.is_empty(), "re-export is a no-op: {:?}", plan.threads);
    assert!(plan.skips.is_empty());
}

#[test]
fn thread_from_another_change_is_skipped() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let root = with_origin(comment(BOB, "2026-01-01T10:00:00Z", "imported from PR 7"), "C_7");
    write_thread(
        &store,
        anchor(dir.path(), &base, &head, "src/lib.rs", Some((10, 10))),
        root,
        vec![],
    );

    let plan = export::plan(&store, &change(&base, &head, vec![])).unwrap();
    assert!(plan.threads.is_empty());
    assert_eq!(plan.skips.len(), 1);
    assert!(plan.skips[0].reason.contains("elsewhere"), "{}", plan.skips[0].reason);
}

#[test]
fn positions_degrade_to_file_then_change_level() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    // Line 1 is far from the change's only hunk (line 10 ± context), but the
    // file is in the diff: file-level.
    let far = comment(ME, "2026-01-01T10:00:00Z", "far from the hunk");
    let far_id = far.id().unwrap();
    write_thread(&store, anchor(dir.path(), &base, &head, "src/lib.rs", Some((1, 1))), far, vec![]);
    // README.md is untouched by the change: change-level, snippet materialized.
    let off = comment(ME, "2026-01-01T10:01:00Z", "about the readme");
    let off_id = off.id().unwrap();
    write_thread(&store, anchor(dir.path(), &base, &head, "README.md", Some((2, 2))), off, vec![]);
    // A whole-change comment: change-level, nothing to materialize.
    let whole = comment(ME, "2026-01-01T10:02:00Z", "about the change");
    let whole_id = whole.id().unwrap();
    write_thread(&store, commit_anchor(&base, &head), whole, vec![]);

    let plan = export::plan(&store, &change(&base, &head, vec![])).unwrap();
    let position_of = |id: &EventId| {
        let thread = plan.threads.iter().find(|t| t.thread == *id).expect("planned");
        match &thread.target {
            Target::New { position } => position,
            other => panic!("expected a new thread, got {other:?}"),
        }
    };
    match position_of(&far_id) {
        Position::File { path } => assert_eq!(path, "src/lib.rs"),
        other => panic!("expected file-level, got {other:?}"),
    }
    match position_of(&off_id) {
        Position::ChangeLevel { context: Some(context) } => {
            assert_eq!(context.path, "README.md");
            assert_eq!(context.text, "keep me");
        }
        other => panic!("expected change-level with a snippet, got {other:?}"),
    }
    match position_of(&whole_id) {
        Position::ChangeLevel { context: None } => {}
        other => panic!("expected bare change-level, got {other:?}"),
    }
}

/// What GitHub reports after an export: our posted comment plus whatever
/// grew around it — the round-trip's import half.
fn gh_thread(id: &str, resolved: bool, comments: Vec<ReviewComment>) -> import::ReviewThread {
    import::ReviewThread {
        id: id.into(),
        is_resolved: resolved,
        path: "src/lib.rs".into(),
        subject_type: Some("LINE".into()),
        diff_side: Some("RIGHT".into()),
        start_diff_side: None,
        original_line: Some(10),
        original_start_line: None,
        resolved_by: None,
        comments: Page {
            page_info: PageInfo { has_next_page: false, end_cursor: None },
            nodes: comments,
        },
    }
}

fn gh_comment(id: &str, body: &str, ts: &str, head: &str, reply_to: Option<&str>) -> ReviewComment {
    ReviewComment {
        id: id.into(),
        url: format!("https://github.com/o/r/pull/1#discussion_{id}"),
        body: body.into(),
        created_at: ts.into(),
        author: Some(GhUser { login: "bob".into(), database_id: Some(2) }),
        original_commit: Some(OidRef { oid: head.into() }),
        reply_to: reply_to.map(|id| IdRef { id: id.into() }),
    }
}

#[test]
fn export_then_import_round_trips() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    // A local thread after an export run: the root is mirrored to comment
    // C_1, its resolution to thread T_1 — exactly what the executor writes.
    let root = comment(ME, "2026-01-01T10:00:00Z", "exported body");
    let root_id = root.id().unwrap();
    let resolution = resolve(ME, "2026-01-01T11:00:00Z", true);
    let resolution_id = resolution.id().unwrap();
    let markers = vec![
        mirror("2026-01-01T12:00:00Z", &root_id, "C_1"),
        mirror("2026-01-01T11:00:00Z", &resolution_id, "T_1"),
    ];
    write_thread(
        &store,
        anchor(dir.path(), &base, &head, "src/lib.rs", Some((10, 10))),
        root,
        [vec![resolution], markers].concat(),
    );
    let events_before = store.threads().unwrap()[0].events.len();

    // GitHub now reports our comment, the resolution, and a fresh foreign
    // reply. Import must add exactly the reply — wired to the local root —
    // and boomerang neither our comment nor a synthetic resolve.
    let thread = gh_thread(
        "T_1",
        true,
        vec![
            gh_comment("C_1", "exported body", "2026-01-01T12:00:00Z", &head, None),
            gh_comment("C_2", "reply from the forge", "2026-01-02T09:00:00Z", &head, Some("C_1")),
        ],
    );
    let report = import::apply(&store, &base, std::slice::from_ref(&thread)).unwrap();
    assert_eq!((report.threads, report.events, report.known), (1, 1, 1));

    let records = store.threads().unwrap();
    assert_eq!(records.len(), 1, "the exported thread must not come back as a second one");
    assert_eq!(records[0].events.len(), events_before + 1);
    let folded = fold_thread(records[0].events.clone());
    let reply =
        folded.events.iter().find(|e| e.event.kind == EventKind::Reply).expect("reply imported");
    assert_eq!(reply.event.in_reply_to, Some(root_id), "wired to the local event, not a copy");
    assert!(folded.resolved, "no duplicate resolve was needed or written");

    // And once everything is known, re-import writes nothing at all.
    let tip = store.tip().unwrap();
    let again = import::apply(&store, &base, &[thread]).unwrap();
    assert_eq!((again.threads, again.events), (0, 0));
    assert_eq!(store.tip().unwrap(), tip);
}

#[test]
fn draft_threads_are_refused() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let root = comment(ME, "2026-01-01T10:00:00Z", "not sealed yet");
    let batch = Batch {
        new_threads: vec![NewThread {
            anchor: anchor(dir.path(), &base, &head, "src/lib.rs", Some((10, 10))),
            root,
            events: vec![],
        }],
        appends: vec![],
    };
    store.draft(&batch).unwrap();

    let plan = export::plan(&store, &change(&base, &head, vec![])).unwrap();
    assert!(plan.threads.is_empty());
    assert_eq!(plan.skips.len(), 1);
    assert!(plan.skips[0].reason.contains("draft"), "{}", plan.skips[0].reason);
}
