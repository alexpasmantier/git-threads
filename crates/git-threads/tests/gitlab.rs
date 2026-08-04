//! Integration tests for the GitLab adapter's mapping layer: `apply` takes
//! discussion data as the API shapes it, so everything below the `glab`
//! seam is exercised against a real repository — the same seam the GitHub
//! import tests use.

use git_threads::gitlab::{self, Discussion, GlUser, Note, NotePosition};
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

/// A repo shaped like an MR: `base` is the target branch tip, `head` the
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

const HOST: &str = "gitlab.example.com";
const MR_URL: &str = "https://gitlab.example.com/o/r/-/merge_requests/7";

fn user(id: u64, username: &str) -> GlUser {
    GlUser { id, username: username.into() }
}

fn position(base: &str, head: &str, new_line: Option<u32>, old_line: Option<u32>) -> NotePosition {
    NotePosition {
        base_sha: base.into(),
        head_sha: head.into(),
        position_type: Some("text".into()),
        old_path: Some("src/lib.rs".into()),
        new_path: Some("src/lib.rs".into()),
        old_line,
        new_line,
        line_range: None,
    }
}

fn note(id: u64, body: &str, ts: &str, position: Option<NotePosition>) -> Note {
    Note {
        id,
        body: body.into(),
        author: user(2, "bob"),
        created_at: ts.into(),
        system: false,
        resolvable: position.is_some(),
        resolved: Some(false),
        resolved_by: None,
        position,
    }
}

fn discussion(id: &str, notes: Vec<Note>) -> Discussion {
    Discussion { id: id.into(), individual_note: false, notes }
}

#[test]
fn import_maps_discussions_events_and_anchor() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let mut root = note(
        1,
        "does b need a doc?",
        // Fractional seconds and an offset, as GitLab reports them.
        "2026-01-01T11:00:00.123+01:00",
        Some(position(&base, &head, Some(2), None)),
    );
    root.resolved = Some(true);
    root.resolved_by = Some(user(1, "alice"));
    let reply = note(2, "yes, adding one", "2026-01-01T11:30:00.000Z", None);
    // MR-level chatter is out of scope, exactly like GitHub's issue comments.
    let positionless = discussion("D_2", vec![note(9, "nice MR!", "2026-01-01T12:00:00Z", None)]);

    let report =
        gitlab::apply(&store, HOST, MR_URL, &[discussion("D_1", vec![root, reply]), positionless])
            .unwrap();
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
    assert_eq!(root.event.author.email, "2-bob@users.noreply.gitlab.example.com");
    // The offset-bearing timestamp landed as UTC seconds.
    assert_eq!(root.event.ts.as_str(), "2026-01-01T10:00:00Z");
    assert_eq!(root.event.extra["origin"]["forge"], "gitlab");
    assert_eq!(root.event.extra["origin"]["id"], "1");
    assert_eq!(root.event.extra["origin"]["url"], format!("{MR_URL}#note_1"));
    let reply = &folded.events[1];
    assert_eq!(reply.event.in_reply_to, Some(root.id.clone()));
    let resolve = record.events.iter().find(|(_, e)| e.kind == EventKind::Resolve).unwrap();
    assert_eq!(resolve.1.author.name, "alice");
    assert_eq!(resolve.1.extra["origin"]["id"], "D_1");

    // Anchored-commit retention: the publish commit pins the reviewed head.
    let tip_parents = git(dir.path(), &["rev-list", "--parents", "-1", "refs/threads/data"]);
    assert!(tip_parents.contains(&head), "import commit pins {head}: {tip_parents}");
}

#[test]
fn reimport_is_a_noop_and_new_notes_append() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let root = note(
        1,
        "does b need a doc?",
        "2026-01-01T10:00:00Z",
        Some(position(&base, &head, Some(2), None)),
    );
    let thread = discussion("D_1", vec![root.clone()]);

    gitlab::apply(&store, HOST, MR_URL, std::slice::from_ref(&thread)).unwrap();
    let tip = store.tip().unwrap();

    let again = gitlab::apply(&store, HOST, MR_URL, &[thread]).unwrap();
    assert_eq!((again.threads, again.events, again.known), (0, 0, 1));
    assert_eq!(store.tip().unwrap(), tip, "no-op reimport writes nothing");

    let grown =
        discussion("D_1", vec![root, note(2, "grown on the forge", "2026-01-02T10:00:00Z", None)]);
    let update = gitlab::apply(&store, HOST, MR_URL, &[grown]).unwrap();
    assert_eq!((update.threads, update.events, update.known), (1, 1, 1));
    let records = store.threads().unwrap();
    assert_eq!(records.len(), 1, "reply appended, no second thread");
}

#[test]
fn old_side_note_maps_to_the_base_blob() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    let thread = discussion(
        "D_1",
        vec![note(
            1,
            "why was this fine before?",
            "2026-01-01T10:00:00Z",
            Some(position(&base, &head, None, Some(1))),
        )],
    );

    let report = gitlab::apply(&store, HOST, MR_URL, &[thread]).unwrap();
    assert_eq!((report.threads, report.skipped), (1, 0));
    let anchor = &store.threads().unwrap()[0].anchor;
    assert_eq!(anchor.side, Some(Side::Old));
    assert_eq!(anchor.lines.map(|l| (l.start, l.end)), Some((1, 1)));
    let base_blob = git(dir.path(), &["rev-parse", &format!("{base}:src/lib.rs")]);
    assert_eq!(anchor.blob.as_ref().map(|b| b.as_str()), Some(base_blob.as_str()));
}

// ---- round-trip: what the executor writes, read back by the importer ----

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
        of: None,
        extra: Default::default(),
    }
}

fn mirror(ts: &str, of: &EventId, foreign_id: &str) -> Event {
    let mut e = event(EventKind::Mirror, ts);
    e.of = Some(of.clone());
    e.extra.insert(
        "origin".into(),
        serde_json::json!({
            "forge": "gitlab",
            "id": foreign_id,
            "url": format!("{MR_URL}#note_{foreign_id}"),
        }),
    );
    e
}

#[test]
fn export_then_import_round_trips() {
    let (dir, base, head) = setup_repo();
    let store = Store::open(dir.path()).unwrap();
    // A local thread after an export run: root mirrored to note 101, its
    // resolution to discussion D_1 — exactly what the executor writes.
    let mut root = event(EventKind::Comment, "2026-01-01T10:00:00Z");
    root.body = Some("exported body".into());
    let root_id = root.id().unwrap();
    let mut resolution = event(EventKind::Resolve, "2026-01-01T11:00:00Z");
    resolution.resolved = Some(true);
    let resolution_id = resolution.id().unwrap();
    let blob = git(dir.path(), &["rev-parse", &format!("{head}:src/lib.rs")]);
    let anchor = Anchor {
        v: 1,
        kind: AnchorKind::Range,
        diff: DiffRef {
            base: GitOid::from_hex(&base).unwrap(),
            head: GitOid::from_hex(&head).unwrap(),
        },
        path: Some("src/lib.rs".into()),
        old_path: None,
        side: Some(Side::New),
        lines: Some(LineRange { start: 2, end: 2 }),
        blob: Some(GitOid::from_hex(blob).unwrap()),
        cols: None,
        extra: Default::default(),
    };
    let events = vec![
        resolution,
        mirror("2026-01-01T12:00:00Z", &root_id, "101"),
        mirror("2026-01-01T11:00:00Z", &resolution_id, "D_1"),
    ];
    store
        .write(&Batch { new_threads: vec![NewThread { anchor, root, events }], appends: vec![] })
        .unwrap();

    // GitLab now reports our note, the resolution, and a fresh foreign
    // reply. Import must add exactly the reply — wired to the local root —
    // and boomerang neither our note nor a synthetic resolve.
    let mut our_note = note(
        101,
        "exported body",
        "2026-01-01T12:00:00Z",
        Some(position(&base, &head, Some(2), None)),
    );
    our_note.resolved = Some(true);
    let thread = discussion(
        "D_1",
        vec![our_note, note(102, "reply from the forge", "2026-01-02T09:00:00Z", None)],
    );
    let report = gitlab::apply(&store, HOST, MR_URL, std::slice::from_ref(&thread)).unwrap();
    assert_eq!((report.threads, report.events, report.known), (1, 1, 1));

    let records = store.threads().unwrap();
    assert_eq!(records.len(), 1, "the exported thread must not come back as a second one");
    let folded = fold_thread(records[0].events.clone());
    let reply =
        folded.events.iter().find(|e| e.event.kind == EventKind::Reply).expect("reply imported");
    assert_eq!(reply.event.in_reply_to, Some(root_id), "wired to the local event, not a copy");
    assert!(folded.resolved, "no duplicate resolve was needed or written");

    // And once everything is known, re-import writes nothing at all.
    let tip = store.tip().unwrap();
    let again = gitlab::apply(&store, HOST, MR_URL, &[thread]).unwrap();
    assert_eq!((again.threads, again.events), (0, 0));
    assert_eq!(store.tip().unwrap(), tip);
}
