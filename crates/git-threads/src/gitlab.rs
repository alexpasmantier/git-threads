//! GitLab adapter (SPEC.md §8), both directions over one seam.
//!
//! Import: MR discussions become anchored threads. GitLab hands over what
//! GitHub makes us compute — each note's position carries the merge-base
//! (`base_sha`) and file coordinates on either side — so the anchor mapping
//! is direct. Export: the forge-neutral planner's output posted through the
//! discussions API, where even positionless discussions are resolvable
//! threads, so nothing degrades to the PR-level pseudo-thread GitHub needs.
//!
//! Fetching and posting shell out to `glab api` — auth and host selection
//! (self-managed instances included) for free, no HTTP stack in the tree.
//! Like the GitHub pair: imports are deterministic (every event's bytes a
//! function of forge data alone — GitLab's fractional-second, offset-bearing
//! timestamps are normalized to the format's UTC seconds), and every posted
//! event is recorded as a `mirror` before the run moves on.

use crate::commands;
use crate::export::{self, ChangeState, ForeignThread, Position, Post, Target, ThreadPlan};
use crate::import::{self, ImportReport};
use crate::reanchor;
use crate::store::{Append, Batch, NewThread, Store};
use crate::ui::short;
use anyhow::{Context, Result, anyhow, bail};
use git_threads_core::{
    Anchor, AnchorKind, Author, DiffRef, Event, EventId, EventKind, GitOid, LineRange, Side,
    ThreadId, Timestamp,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// One discussion as the API reports it — the input to the deterministic
/// mapping. Public so the mapping is testable from recorded or hand-built
/// data, no network involved.
#[derive(Clone, Debug, Deserialize)]
pub struct Discussion {
    /// Discussion ID; the synthetic `resolve` event's origin.
    pub id: String,
    /// A lone MR comment, not a thread: no replies, no resolution.
    #[serde(default)]
    pub individual_note: bool,
    pub notes: Vec<Note>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Note {
    /// Note ID; the event's origin (stringified — GitLab IDs are numeric).
    pub id: u64,
    pub body: String,
    pub author: GlUser,
    /// RFC 3339, any offset and precision; normalized on the way in.
    pub created_at: String,
    /// Forge-generated activity ("changed the description"): never imported.
    #[serde(default)]
    pub system: bool,
    #[serde(default)]
    pub resolvable: bool,
    #[serde(default)]
    pub resolved: Option<bool>,
    #[serde(default)]
    pub resolved_by: Option<GlUser>,
    /// Present on diff notes; its `head_sha`/`base_sha` are the anchor's
    /// commits and its lines are file coordinates — the anchor shape (§3).
    #[serde(default)]
    pub position: Option<NotePosition>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GlUser {
    pub id: u64,
    pub username: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct NotePosition {
    /// The merge-base of the diff the note was made on.
    pub base_sha: String,
    pub head_sha: String,
    /// `text` (lines) or `file` (whole file).
    #[serde(default)]
    pub position_type: Option<String>,
    #[serde(default)]
    pub old_path: Option<String>,
    #[serde(default)]
    pub new_path: Option<String>,
    /// File coordinates: `new_line` on the head side, `old_line` on the
    /// base side; both set for unchanged (context) lines.
    #[serde(default)]
    pub old_line: Option<u32>,
    #[serde(default)]
    pub new_line: Option<u32>,
    #[serde(default)]
    pub line_range: Option<PositionRange>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PositionRange {
    #[serde(default)]
    pub start: Option<PositionLine>,
    #[serde(default)]
    pub end: Option<PositionLine>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PositionLine {
    #[serde(default)]
    pub old_line: Option<u32>,
    #[serde(default)]
    pub new_line: Option<u32>,
}

// ---- import ------------------------------------------------------------

/// Import discussions from GitLab: one MR (a number, `!N`, or URL), or with
/// `all`, every MR of the project. Each MR with anything new becomes one
/// publish commit, so a long `--all` run keeps its progress on failure.
pub fn import(store: &Store, remote: &str, spec: Option<&str>, all: bool) -> Result<ImportReport> {
    let (workdir, host, project) = project_of(store, remote, spec)?;
    let encoded = project.replace('/', "%2F");

    let mrs: Vec<MrSummary> = if all {
        fetch_all_mrs(&workdir, &encoded)?
    } else {
        let spec = spec.context("pass an MR number or URL, or --all")?;
        let (_, iid) = parse_spec(spec)
            .with_context(|| format!("cannot parse {spec:?} as an MR number or GitLab MR URL"))?;
        let mr = fetch_mr(&workdir, &encoded, iid)?
            .with_context(|| format!("no merge request !{iid} in {project}"))?;
        vec![MrSummary { iid: mr.iid, web_url: mr.web_url }]
    };

    let mut report = ImportReport::default();
    for mr in &mrs {
        let discussions = fetch_discussions(&workdir, &encoded, mr.iid)?;
        let diff_discussions = discussions.iter().filter(|d| is_diff_discussion(d)).count();
        if diff_discussions == 0 {
            if !all {
                println!("MR !{}: no diff discussions", mr.iid);
            }
            report.prs += 1;
            continue;
        }
        ensure_objects(store, &workdir, remote, mr.iid, &discussions);
        let outcome = apply(store, &host, &mr.web_url, &discussions)
            .with_context(|| format!("importing MR !{}", mr.iid))?;
        if outcome.events > 0 || !all {
            println!(
                "MR !{}: {} event{} in {} thread{}{}",
                mr.iid,
                outcome.events,
                if outcome.events == 1 { "" } else { "s" },
                outcome.threads,
                if outcome.threads == 1 { "" } else { "s" },
                if outcome.known > 0 {
                    format!(" ({} already imported)", outcome.known)
                } else {
                    String::new()
                },
            );
        }
        report.absorb(outcome);
        report.prs += 1;
    }
    Ok(report)
}

/// Map discussions onto the store and publish them as one commit (none when
/// nothing is new). The deterministic core: given the same discussion data
/// and the same git objects, every clone writes byte-identical events.
/// Discussions without a diff position (MR-level chatter, system activity)
/// are not imported — same scope as the GitHub importer's review threads.
pub fn apply(
    store: &Store,
    host: &str,
    mr_url: &str,
    discussions: &[Discussion],
) -> Result<ImportReport> {
    let index = import::origin_index(store)?;
    let mut batch = Batch::default();
    let mut report = ImportReport::default();

    for discussion in discussions.iter().filter(|d| is_diff_discussion(d)) {
        match map_discussion(store, host, mr_url, discussion, &index, &mut batch, &mut report) {
            Ok(()) => {}
            Err(err) => {
                eprintln!("warning: skipping discussion {}: {err:#}", discussion.id);
                report.skipped += 1;
            }
        }
    }
    if !batch.is_empty() {
        store.write(&batch)?;
    }
    Ok(report)
}

/// A discussion the importer maps: a real thread whose root note carries a
/// diff position.
fn is_diff_discussion(discussion: &Discussion) -> bool {
    !discussion.individual_note
        && discussion.notes.iter().find(|n| !n.system).is_some_and(|root| root.position.is_some())
}

fn map_discussion(
    store: &Store,
    host: &str,
    mr_url: &str,
    discussion: &Discussion,
    index: &BTreeMap<String, (ThreadId, EventId)>,
    batch: &mut Batch,
    report: &mut ImportReport,
) -> Result<()> {
    let notes: Vec<&Note> = discussion.notes.iter().filter(|n| !n.system).collect();
    let root = *notes.first().context("discussion has no notes")?;

    let mut events: Vec<Event> = Vec::new();
    let existing = index.get(root.id.to_string().as_str()).map(|(thread, _)| thread.clone());

    let root_event = message_event(host, mr_url, root, EventKind::Comment, None)?;
    // A known root keeps its indexed identity — for an exported comment the
    // local event's bytes differ from this reconstruction (SPEC.md §8.2).
    let root_id = match index.get(root.id.to_string().as_str()) {
        Some((_, event)) => event.clone(),
        None => root_event.id()?,
    };
    if existing.is_some() {
        report.known += 1;
    }

    for note in notes.iter().skip(1) {
        if index.contains_key(note.id.to_string().as_str()) {
            report.known += 1;
            continue;
        }
        // GitLab threads are flat with no per-note reply target; everything
        // answers the root.
        let event = message_event(host, mr_url, note, EventKind::Reply, Some(root_id.clone()))?;
        events.push(event);
    }

    // Resolution becomes one synthetic resolve event, its origin the
    // discussion ID so it imports exactly once; its timestamp derives from
    // the data (GitLab does not report when a thread was resolved).
    let resolved = root.resolvable && root.resolved == Some(true);
    if resolved && !index.contains_key(discussion.id.as_str()) {
        let last_ts = notes
            .iter()
            .map(|n| normalize_ts(&n.created_at))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .max()
            .context("discussion has no notes")?;
        let author = root.resolved_by.as_ref().unwrap_or(&root.author);
        let mut event = Event {
            v: 1,
            kind: EventKind::Resolve,
            author: author_of(host, author),
            ts: last_ts,
            body: None,
            in_reply_to: None,
            supersedes: None,
            resolved: Some(true),
            anchor: None,
            of: None,
            extra: Default::default(),
        };
        event.extra.insert("origin".into(), import::origin_value("gitlab", &discussion.id, None));
        event.validate()?;
        events.push(event);
    }

    match existing {
        Some(thread_id) => {
            if !events.is_empty() {
                report.events += events.len();
                report.threads += 1;
                batch.appends.push(Append { thread: thread_id, events });
            }
        }
        None => {
            let anchor =
                map_anchor(store, root.position.as_ref().context("root note has no position")?)?;
            report.events += 1 + events.len();
            report.threads += 1;
            batch.new_threads.push(NewThread { anchor, root: root_event, events });
        }
    }
    Ok(())
}

/// The thread's anchor, straight from GitLab's position: `head_sha` and
/// `base_sha` are the anchor's commits (the base is already the merge-base),
/// lines are file coordinates on their side, and the blob is read from the
/// local tree.
fn map_anchor(store: &Store, position: &NotePosition) -> Result<Anchor> {
    let repo = store.repo();
    let head = import::commit(repo, &position.head_sha)
        .context("the commented commit is not fetchable")?;
    let base =
        import::commit(repo, &position.base_sha).context("the diff base is not fetchable")?;

    let side = match (position.new_line, position.old_line) {
        (Some(_), _) => Side::New,
        (None, Some(_)) => Side::Old,
        // File comments live on the head side.
        (None, None) => Side::New,
    };
    let path = match side {
        Side::New => position.new_path.clone().or_else(|| position.old_path.clone()),
        Side::Old => position.old_path.clone().or_else(|| position.new_path.clone()),
    }
    .context("position names no path")?;
    let renamed =
        side == Side::New && position.old_path.is_some() && position.old_path != position.new_path;

    let lines = match (side, position.new_line, position.old_line) {
        (_, None, None) => None,
        (Side::New, Some(end), _) | (Side::Old, _, Some(end)) => {
            // A range spanning both sides has no single-side spelling; keep
            // the side the note ends on.
            let start = position
                .line_range
                .as_ref()
                .and_then(|r| r.start.as_ref())
                .and_then(|s| match side {
                    Side::New => s.new_line,
                    Side::Old => s.old_line,
                })
                .unwrap_or(end);
            Some(LineRange { start: start.min(end), end: end.max(start) })
        }
        _ => None,
    };
    let side_commit = match side {
        Side::New => head,
        Side::Old => base,
    };
    let blob = reanchor::blob_at(repo, side_commit, &path)?
        .with_context(|| format!("{path:?} not found in the commented tree"))?;

    let anchor = Anchor {
        v: 1,
        kind: if lines.is_some() { AnchorKind::Range } else { AnchorKind::File },
        diff: DiffRef {
            base: GitOid::from_hex(base.to_string())?,
            head: GitOid::from_hex(head.to_string())?,
        },
        path: Some(path),
        old_path: if renamed { position.old_path.clone() } else { None },
        side: Some(side),
        lines,
        blob: Some(GitOid::from_hex(blob.to_string())?),
        cols: None,
        extra: Default::default(),
    };
    anchor.validate()?;
    Ok(anchor)
}

/// A comment or reply event from one note. The body is imported verbatim —
/// it is a historical record, not a new message.
fn message_event(
    host: &str,
    mr_url: &str,
    note: &Note,
    kind: EventKind,
    in_reply_to: Option<EventId>,
) -> Result<Event> {
    let mut event = Event {
        v: 1,
        kind,
        author: author_of(host, &note.author),
        ts: normalize_ts(&note.created_at)?,
        body: Some(note.body.clone()),
        in_reply_to,
        supersedes: None,
        resolved: None,
        anchor: None,
        of: None,
        extra: Default::default(),
    };
    let url = format!("{mr_url}#note_{}", note.id);
    event
        .extra
        .insert("origin".into(), import::origin_value("gitlab", &note.id.to_string(), Some(&url)));
    event.validate()?;
    Ok(event)
}

/// Forge identity → author, in GitLab's noreply form for the instance —
/// self-managed hosts get their own domain, mirroring what the instance
/// itself puts on commits.
fn author_of(host: &str, user: &GlUser) -> Author {
    Author {
        name: user.username.clone(),
        email: format!("{}-{}@users.noreply.{host}", user.id, user.username),
    }
}

/// GitLab reports RFC 3339 at whatever precision and offset it likes
/// ("2026-08-04T12:23:45.123+02:00"); the format stores UTC at second
/// precision (SPEC.md §2.2). The normalization is deterministic, so
/// independent imports still mint identical event IDs.
pub(crate) fn normalize_ts(s: &str) -> Result<Timestamp> {
    let instant: jiff::Timestamp = s.parse().map_err(|e| anyhow!("bad timestamp {s:?}: {e}"))?;
    Timestamp::parse(instant.strftime("%FT%TZ").to_string())
        .map_err(|e| anyhow!("normalized timestamp still invalid: {e}"))
}

// ---- export ------------------------------------------------------------

/// Export this change's threads onto a GitLab merge request (a number, `!N`,
/// or URL). Same shape as the GitHub executor: sequential paced posting,
/// each thread's mirrors published as soon as its posts land.
pub fn export(
    store: &Store,
    remote: &str,
    spec: &str,
    dry_run: bool,
) -> Result<export::ExportReport> {
    let (workdir, host, project) = project_of(store, remote, Some(spec))?;
    let encoded = project.replace('/', "%2F");
    let (_, iid) = parse_spec(spec)
        .with_context(|| format!("cannot parse {spec:?} as an MR number or GitLab MR URL"))?;
    let mr = fetch_mr(&workdir, &encoded, iid)?
        .with_context(|| format!("no merge request !{iid} in {project}"))?;
    let discussions = fetch_discussions(&workdir, &encoded, iid)?;
    ensure_objects(store, &workdir, remote, iid, &discussions);
    let _ = commands::git(
        &workdir,
        &["fetch", "--quiet", remote, &mr.diff_refs.head_sha, &mr.diff_refs.base_sha],
    );

    let threads = discussions
        .iter()
        .filter_map(|d| {
            let notes: Vec<&Note> = d.notes.iter().filter(|n| !n.system).collect();
            let root = notes.first()?;
            Some(ForeignThread {
                id: d.id.clone(),
                is_resolved: root.resolvable && root.resolved == Some(true),
                comments: notes.iter().map(|n| n.id.to_string()).collect(),
                // A lone MR comment: replies convert it into a real thread,
                // but until then there is no resolution to toggle — the
                // planner holds resolves back for one run, and the next
                // fetch sees a converted, resolvable discussion.
                is_pr_level: d.individual_note,
            })
        })
        .collect();
    let change = ChangeState {
        number: iid,
        base_ref_oid: mr.diff_refs.base_sha.clone(),
        head_ref_oid: mr.diff_refs.head_sha.clone(),
        threads,
    };
    let plan = export::plan(store, &change)?;
    let mut report =
        export::ExportReport { skipped: plan.skips.len(), dry_run, ..Default::default() };
    for skip in &plan.skips {
        eprintln!("warning: skipping thread {}: {}", short(&skip.thread), skip.reason);
    }
    if dry_run {
        export::print_plan(&plan);
        report.threads = plan.threads.len();
        report.posts = plan.posts();
        return Ok(report);
    }
    if plan.is_empty() {
        return Ok(report);
    }

    let me = viewer(&workdir, &host)?;
    let mut pace = export::Pace::default();
    for thread_plan in &plan.threads {
        let mut mirrors: Vec<Event> = Vec::new();
        let outcome = export_thread(
            &workdir,
            &encoded,
            &mr,
            &me,
            thread_plan,
            &mut pace,
            &mut mirrors,
            &mut report,
        );
        // Whatever happened, what was posted is recorded before anything
        // else — an error must not orphan notes already on the forge.
        if !mirrors.is_empty() {
            let batch = Batch {
                appends: vec![Append { thread: thread_plan.thread.clone(), events: mirrors }],
                ..Default::default()
            };
            store.write(&batch)?;
            report.threads += 1;
        }
        outcome.with_context(|| format!("exporting thread {}", short(&thread_plan.thread)))?;
    }
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
fn export_thread(
    workdir: &Path,
    project: &str,
    mr: &MrInfo,
    me: &Author,
    plan: &ThreadPlan,
    pace: &mut export::Pace,
    mirrors: &mut Vec<Event>,
    report: &mut export::ExportReport,
) -> Result<()> {
    let mr_url = &mr.web_url;
    let mirror = |post: &Post, note: &CreatedNote| -> Result<Event> {
        let url = format!("{mr_url}#note_{}", note.id);
        let ts = normalize_ts(&note.created_at)?;
        export::mirror_event(me, post, "gitlab", &note.id.to_string(), Some(&url), ts.as_str())
    };
    match &plan.target {
        Target::New { position } => {
            let first = plan.posts.first().expect("a new thread always has a first post");
            let body = match position {
                Position::ChangeLevel { context } => {
                    change_level_body(mr_url, &mr.diff_refs.head_sha, context.as_ref(), &first.body)
                }
                _ => first.body.clone(),
            };
            pace.wait();
            let created = create_discussion(workdir, project, mr, position, &body)?;
            let root = created.notes.first().context("the created discussion reports no notes")?;
            println!(
                "thread {}: created a discussion ({})",
                short(&plan.thread),
                export::describe(position),
            );
            mirrors.push(mirror(first, root)?);
            report.posts += 1;
            for post in &plan.posts[1..] {
                pace.wait();
                let note = add_note(workdir, project, mr.iid, &created.id, &post.body)?;
                mirrors.push(mirror(post, &note)?);
                report.posts += 1;
            }
            // Unlike GitHub, creation returns the discussion ID and every
            // discussion — positioned or not — is resolvable: no refetch,
            // no unrepresentable case.
            if let Some(action) = &plan.resolve {
                pace.wait();
                toggle_resolve(workdir, project, mr.iid, &created.id, action.to)?;
                mirrors.push(export::resolve_mirror(me, action, "gitlab", &created.id)?);
                report.resolves += 1;
            }
        }
        Target::Existing { foreign_thread } => {
            // Lone MR comments included: replying to one converts it into a
            // real thread (verified live; only *system* notes refuse), so
            // everything routes through the discussion.
            for post in &plan.posts {
                pace.wait();
                let note = add_note(workdir, project, mr.iid, foreign_thread, &post.body)?;
                mirrors.push(mirror(post, &note)?);
                report.posts += 1;
            }
            if !plan.posts.is_empty() {
                println!(
                    "thread {}: posted {} repl{}",
                    short(&plan.thread),
                    plan.posts.len(),
                    if plan.posts.len() == 1 { "y" } else { "ies" }
                );
            }
            // The planner never plans a toggle for a lone-note trail, so a
            // resolve here always has a discussion to act on.
            if let Some(action) = &plan.resolve {
                pace.wait();
                toggle_resolve(workdir, project, mr.iid, foreign_thread, action.to)?;
                mirrors.push(export::resolve_mirror(me, action, "gitlab", foreign_thread)?);
                report.resolves += 1;
                println!(
                    "thread {}: {} on the MR",
                    short(&plan.thread),
                    if action.to { "resolved" } else { "reopened" }
                );
            }
        }
    }
    Ok(())
}

/// A change-level discussion says where it belongs, since the diff can't:
/// the materialized snippet (§8.2) plus a permalink into the head tree.
fn change_level_body(
    mr_url: &str,
    head: &str,
    context: Option<&export::SnippetContext>,
    body: &str,
) -> String {
    let Some(context) = context else { return body.to_string() };
    let project_web = mr_url.split("/-/").next().unwrap_or(mr_url);
    let lines = match context.lines.start == context.lines.end {
        true => format!("{}", context.lines.start),
        false => format!("{}-{}", context.lines.start, context.lines.end),
    };
    let fragment = match context.lines.start == context.lines.end {
        true => format!("L{}", context.lines.start),
        false => format!("L{}-{}", context.lines.start, context.lines.end),
    };
    format!(
        "**On [`{path}:{lines}`]({project_web}/-/blob/{head}/{path}#{fragment}) — not visible in this diff:**\n\n```\n{snippet}\n```\n\n{body}",
        path = context.path,
        snippet = context.text,
    )
}

/// The account posting everything, in the instance's noreply form — the
/// same mapping the importer uses, so exported-then-imported comments
/// round-trip to one identity.
fn viewer(workdir: &Path, host: &str) -> Result<Author> {
    let out = glab(workdir, &["api", "user"])?;
    let user: GlUser = serde_json::from_str(&out).context("unexpected glab api user output")?;
    Ok(author_of(host, &user))
}

// ---- GitLab API calls ----------------------------------------------------

#[derive(Deserialize)]
struct CreatedDiscussion {
    id: String,
    notes: Vec<CreatedNote>,
}

#[derive(Deserialize)]
struct CreatedNote {
    id: u64,
    created_at: String,
}

fn create_discussion(
    workdir: &Path,
    project: &str,
    mr: &MrInfo,
    position: &Position,
    body: &str,
) -> Result<CreatedDiscussion> {
    let endpoint = format!("projects/{project}/merge_requests/{}/discussions", mr.iid);
    // Context (unchanged) lines must carry both coordinates, added lines
    // only the new one — GitLab rejects anything else with a 400.
    let old_line = match position {
        Position::Line { path, side: Side::New, lines } => {
            old_line_of(workdir, &mr.diff_refs.base_sha, &mr.diff_refs.head_sha, path, lines.end)?
        }
        _ => None,
    };
    let payload = discussion_payload(&mr.diff_refs, position, old_line, body);
    serde_json::from_str(&glab_json(workdir, "POST", &endpoint, &payload)?)
        .context("unexpected glab api output for a created discussion")
}

/// The create-discussion payload. GitLab wants `position` as a nested JSON
/// object — flat `position[key]` form fields are silently ignored and would
/// demote the thread to a positionless discussion.
fn discussion_payload(
    diff_refs: &MrDiffRefs,
    position: &Position,
    old_line: Option<u32>,
    body: &str,
) -> serde_json::Value {
    let mut payload = serde_json::json!({ "body": body });
    let base = |position_type: &str, path: &str| {
        serde_json::json!({
            "position_type": position_type,
            "base_sha": diff_refs.base_sha,
            "start_sha": diff_refs.start_sha,
            "head_sha": diff_refs.head_sha,
            "old_path": path,
            "new_path": path,
        })
    };
    match position {
        Position::Line { path, side, lines } => {
            let mut pos = base("text", path);
            // Multi-line ranges need GitLab's hashed line codes; v1 pins the
            // range's last line, which is where the conversation reads best.
            match side {
                Side::Old => pos["old_line"] = lines.end.into(),
                Side::New => {
                    pos["new_line"] = lines.end.into();
                    if let Some(old) = old_line {
                        pos["old_line"] = old.into();
                    }
                }
            }
            payload["position"] = pos;
        }
        Position::File { path } => payload["position"] = base("file", path),
        Position::ChangeLevel { .. } => {} // positionless: a plain, still resolvable, discussion
    }
    payload
}

fn add_note(
    workdir: &Path,
    project: &str,
    iid: u64,
    discussion: &str,
    body: &str,
) -> Result<CreatedNote> {
    let endpoint =
        format!("projects/{project}/merge_requests/{iid}/discussions/{discussion}/notes");
    let body_field = format!("body={body}");
    serde_json::from_str(&glab(
        workdir,
        &["api", "--method", "POST", &endpoint, "-f", &body_field],
    )?)
    .context("unexpected glab api output for a posted note")
}

fn toggle_resolve(
    workdir: &Path,
    project: &str,
    iid: u64,
    discussion: &str,
    resolved: bool,
) -> Result<()> {
    let endpoint = format!("projects/{project}/merge_requests/{iid}/discussions/{discussion}");
    let field = format!("resolved={resolved}");
    glab(workdir, &["api", "--method", "PUT", &endpoint, "-f", &field])?;
    Ok(())
}

/// For a context (unchanged) line, GitLab requires both coordinates; for an
/// added line, only `new_line`. Classified against the change's own diff:
/// inside an added span → added; otherwise the old coordinate is the new one
/// shifted by the growth of every hunk above.
fn old_line_of(
    workdir: &Path,
    base: &str,
    head: &str,
    path: &str,
    new_line: u32,
) -> Result<Option<u32>> {
    let diff = commands::git(workdir, &["diff", "--unified=0", base, head, "--", path])?;
    let mut delta: i64 = 0;
    for (old, new) in hunk_pairs(&diff) {
        let (_, old_len) = old;
        let (new_start, new_len) = new;
        if new_len > 0 && new_start <= new_line && new_line < new_start + new_len {
            return Ok(None); // an added line
        }
        let above =
            if new_len == 0 { new_start < new_line } else { new_start + new_len <= new_line };
        if above {
            delta += new_len as i64 - old_len as i64;
        }
    }
    Ok(Some((new_line as i64 - delta).try_into().context("line maps below the file start")?))
}

/// Both sides' (start, len) of each `@@ -a,b +c,d @@` header.
fn hunk_pairs(diff: &str) -> Vec<((u32, u32), (u32, u32))> {
    let span = |token: Option<&str>, sign: char| -> Option<(u32, u32)> {
        let token = token?.strip_prefix(sign)?;
        Some(match token.split_once(',') {
            Some((start, len)) => (start.parse().ok()?, len.parse().ok()?),
            None => (token.parse().ok()?, 1),
        })
    };
    diff.lines()
        .filter(|line| line.starts_with("@@"))
        .filter_map(|line| {
            let mut fields = line.split_whitespace().skip(1);
            Some((span(fields.next(), '-')?, span(fields.next(), '+')?))
        })
        .collect()
}

// ---- fetch ---------------------------------------------------------------

#[derive(Clone, Debug, Deserialize)]
struct MrInfo {
    iid: u64,
    web_url: String,
    diff_refs: MrDiffRefs,
}

/// What the MR *list* endpoint reports — no `diff_refs` there. Import never
/// needs them: note positions carry their own SHAs.
#[derive(Clone, Debug, Deserialize)]
struct MrSummary {
    iid: u64,
    web_url: String,
}

#[derive(Clone, Debug, Deserialize)]
struct MrDiffRefs {
    base_sha: String,
    start_sha: String,
    head_sha: String,
}

/// The repository's GitLab coordinates: (workdir, host, project path). A
/// URL spec must name the same project — cross-project export/import is
/// not supported.
fn project_of(
    store: &Store,
    remote: &str,
    spec: Option<&str>,
) -> Result<(PathBuf, String, String)> {
    let workdir =
        store.repo().workdir().context("import/export requires a non-bare repository")?.to_owned();
    let remote_url = commands::git(&workdir, &["remote", "get-url", remote])?;
    let (host, project) = gitlab_slug(remote_url.trim())
        .with_context(|| format!("cannot parse remote {remote:?} as a GitLab URL"))?;
    if let Some((Some((spec_host, spec_project)), _)) = spec.and_then(parse_spec)
        && (spec_host != host || spec_project != project)
    {
        bail!(
            "{spec_host}/{spec_project} is not what {remote:?} points at ({host}/{project}); \
             add it as a remote and pass --remote"
        );
    }
    Ok((workdir, host, project))
}

fn fetch_mr(workdir: &Path, project: &str, iid: u64) -> Result<Option<MrInfo>> {
    let endpoint = format!("projects/{project}/merge_requests/{iid}");
    match glab(workdir, &["api", &endpoint]) {
        Ok(out) => {
            Ok(Some(serde_json::from_str(&out).context("unexpected glab api output for an MR")?))
        }
        Err(err) if format!("{err:#}").contains("404") => Ok(None),
        Err(err) => Err(err),
    }
}

fn fetch_discussions(workdir: &Path, project: &str, iid: u64) -> Result<Vec<Discussion>> {
    let mut all = Vec::new();
    for page in 1.. {
        let endpoint =
            format!("projects/{project}/merge_requests/{iid}/discussions?per_page=100&page={page}");
        let out = glab(workdir, &["api", &endpoint])?;
        let items: Vec<Discussion> =
            serde_json::from_str(&out).context("unexpected glab api discussions output")?;
        let last_page = items.len() < 100;
        all.extend(items);
        if last_page {
            break;
        }
    }
    Ok(all)
}

/// Every MR of the project, oldest first, with progress on stderr.
fn fetch_all_mrs(workdir: &Path, project: &str) -> Result<Vec<MrSummary>> {
    let mut all = Vec::new();
    for page in 1.. {
        let endpoint = format!(
            "projects/{project}/merge_requests?state=all&order_by=created_at&sort=asc&per_page=100&page={page}"
        );
        let out = glab(workdir, &["api", &endpoint])?;
        let items: Vec<MrSummary> =
            serde_json::from_str(&out).context("unexpected glab api merge_requests output")?;
        let last_page = items.len() < 100;
        all.extend(items);
        eprintln!("scanned {} MRs", all.len());
        if last_page {
            break;
        }
    }
    Ok(all)
}

/// Make the commits the discussions anchor to readable locally, best-effort:
/// one fetch of the MR head ref (GitLab keeps `refs/merge-requests/N/head`),
/// then one fetch by SHA for anything still missing. Mapping skips what
/// stays missing, honestly.
fn ensure_objects(
    store: &Store,
    workdir: &Path,
    remote: &str,
    iid: u64,
    discussions: &[Discussion],
) {
    let wanted: Vec<&str> = discussions
        .iter()
        .flat_map(|d| &d.notes)
        .filter_map(|n| n.position.as_ref())
        .flat_map(|p| [p.head_sha.as_str(), p.base_sha.as_str()])
        .collect();
    let missing = |oid: &str| import::commit(store.repo(), oid).is_err();
    if !wanted.iter().any(|oid| missing(oid)) {
        return;
    }
    let head_ref = format!("refs/merge-requests/{iid}/head");
    let _ = commands::git(workdir, &["fetch", "--quiet", remote, &head_ref]);
    let mut still: Vec<&str> = wanted.into_iter().filter(|oid| missing(oid)).collect();
    still.sort_unstable();
    still.dedup();
    if !still.is_empty() {
        let mut args = vec!["fetch", "--quiet", remote];
        args.extend(still);
        let _ = commands::git(workdir, &args);
    }
}

/// `(host, group/…/project)` from a remote URL in its scp, ssh, or https
/// spellings. Any host — self-managed instances are the norm — and nested
/// groups are allowed.
pub(crate) fn gitlab_slug(url: &str) -> Option<(String, String)> {
    let (host, path) = if let Some(rest) = url.strip_prefix("ssh://") {
        let rest = rest.split_once('@').map_or(rest, |(_, r)| r);
        let (host, path) = rest.split_once('/')?;
        (host.split(':').next()?.to_string(), path)
    } else if let Some(rest) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))
    {
        let (host, path) = rest.split_once('/')?;
        (host.to_string(), path)
    } else {
        // scp form: git@host:group/project.git
        let rest = url.split_once('@').map_or(url, |(_, r)| r);
        let (host, path) = rest.split_once(':')?;
        (host.to_string(), path)
    };
    let path = path.strip_suffix(".git").unwrap_or(path).trim_matches('/');
    (!host.is_empty() && host.contains('.') && path.contains('/')).then(|| (host, path.to_string()))
}

/// An MR spec: a number (`7`, `!7`) or a full MR URL, which also names the
/// host and project.
pub(crate) fn parse_spec(spec: &str) -> Option<(Option<(String, String)>, u64)> {
    if let Some(rest) = spec.strip_prefix("https://").or_else(|| spec.strip_prefix("http://")) {
        let (host, path) = rest.split_once('/')?;
        let (project, tail) = path.split_once("/-/merge_requests/")?;
        let iid = tail.split(['#', '?', '/']).next()?.parse().ok()?;
        return Some((Some((host.to_string(), project.to_string())), iid));
    }
    spec.strip_prefix('!').unwrap_or(spec).parse().ok().map(|iid| (None, iid))
}

fn glab(workdir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("glab")
        .current_dir(workdir) // host and auth resolve from the repo's remotes
        .args(args)
        .output()
        .context("failed to run glab (is the GitLab CLI installed?)")?;
    if !output.status.success() {
        bail!(
            "glab {} failed: {}",
            args.iter().take(2).copied().collect::<Vec<_>>().join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A request with a literal JSON body (`--input -`): `glab api` sends
/// `-f` fields flat and does not nest bracketed keys, so structured
/// payloads have to go through stdin.
fn glab_json(
    workdir: &Path,
    method: &str,
    endpoint: &str,
    payload: &serde_json::Value,
) -> Result<String> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new("glab")
        .current_dir(workdir)
        // glab does not infer the content type for --input bodies.
        .args([
            "api",
            "--method",
            method,
            endpoint,
            "--input",
            "-",
            "-H",
            "Content-Type: application/json",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run glab (is the GitLab CLI installed?)")?;
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(payload.to_string().as_bytes())
        .context("failed to write the request body to glab")?;
    let output = child.wait_with_output().context("failed to run glab")?;
    if !output.status.success() {
        bail!("glab api {endpoint} failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::{gitlab_slug, hunk_pairs, normalize_ts, parse_spec};

    #[test]
    fn timestamps_normalize_to_utc_seconds() {
        for (input, expected) in [
            ("2026-08-04T12:23:45.123Z", "2026-08-04T12:23:45Z"),
            ("2026-08-04T12:23:45.999+02:00", "2026-08-04T10:23:45Z"),
            ("2026-08-04T12:23:45Z", "2026-08-04T12:23:45Z"),
        ] {
            assert_eq!(normalize_ts(input).unwrap().as_str(), expected, "{input}");
        }
        assert!(normalize_ts("yesterday-ish").is_err());
    }

    #[test]
    fn slugs_parse_from_all_url_forms_and_hosts() {
        for url in [
            "git@gitlab.com:group/sub/proj.git",
            "https://gitlab.example.com/group/sub/proj",
            "https://gitlab.example.com/group/sub/proj.git",
            "ssh://git@gitlab.example.com/group/sub/proj.git",
            "ssh://git@gitlab.example.com:2222/group/sub/proj.git",
        ] {
            let (host, path) = gitlab_slug(url).expect(url);
            assert!(host.starts_with("gitlab."), "{url}: {host}");
            assert_eq!(path, "group/sub/proj", "{url}");
        }
        assert_eq!(gitlab_slug("not a url"), None);
    }

    #[test]
    fn specs_parse_as_numbers_or_urls() {
        assert_eq!(parse_spec("7"), Some((None, 7)));
        assert_eq!(parse_spec("!7"), Some((None, 7)));
        assert_eq!(
            parse_spec("https://gitlab.example.com/group/proj/-/merge_requests/7#note_1"),
            Some((Some(("gitlab.example.com".into(), "group/proj".into())), 7))
        );
        assert_eq!(parse_spec("https://gitlab.example.com/group/proj/-/issues/7"), None);
        assert_eq!(parse_spec("nope"), None);
    }

    #[test]
    fn hunk_pairs_read_both_sides() {
        let diff = "@@ -10,2 +10,5 @@ fn x()\n@@ -20 +23,0 @@\n";
        assert_eq!(hunk_pairs(diff), vec![((10, 2), (10, 5)), ((20, 1), (23, 0))]);
    }

    #[test]
    fn discussion_payloads_nest_the_position() {
        use crate::export::Position;
        use git_threads_core::{LineRange, Side};
        let diff_refs = super::MrDiffRefs {
            base_sha: "b".repeat(40),
            start_sha: "s".repeat(40),
            head_sha: "h".repeat(40),
        };
        let line = Position::Line {
            path: "README.md".into(),
            side: Side::New,
            lines: LineRange { start: 3, end: 5 },
        };
        // An added line carries only the new coordinate...
        let payload = super::discussion_payload(&diff_refs, &line, None, "hi");
        assert_eq!(payload["body"], "hi");
        assert_eq!(payload["position"]["position_type"], "text");
        assert_eq!(payload["position"]["new_line"], 5);
        assert!(payload["position"].get("old_line").is_none());
        assert_eq!(payload["position"]["base_sha"], diff_refs.base_sha);
        // ...a context line both, an old-side line only the old one.
        let payload = super::discussion_payload(&diff_refs, &line, Some(4), "hi");
        assert_eq!(payload["position"]["old_line"], 4);
        let old_side = Position::Line {
            path: "README.md".into(),
            side: Side::Old,
            lines: LineRange { start: 3, end: 3 },
        };
        let payload = super::discussion_payload(&diff_refs, &old_side, None, "hi");
        assert_eq!(payload["position"]["old_line"], 3);
        assert!(payload["position"].get("new_line").is_none());
        // Change-level: no position at all.
        let payload = super::discussion_payload(
            &diff_refs,
            &Position::ChangeLevel { context: None },
            None,
            "hi",
        );
        assert!(payload.get("position").is_none());
    }
}
