//! Export planner (SPEC.md §8.2): decide what a foreign change (PR/MR) is
//! missing and where each piece goes, before any network call.
//!
//! The planner is forge-neutral and does git reads only — no network, no
//! writes. It takes the foreign change's state as data ([`ChangeState`]:
//! fetched by an executor, or hand-built in tests) and returns a [`Plan`]
//! an executor turns into API calls, which is also what `--dry-run` prints.
//!
//! Selection is per event, not per thread: anything already carrying a
//! foreign identity — its own `origin` (imported) or a `mirror` naming it
//! (exported before) — is skipped, so a thread imported from the change
//! contributes exactly the events added locally since. Resolution follows
//! its own rule (§8.2): it is toggled only when the latest local `resolve`
//! has no foreign identity — a new local intent — and the folded state
//! disagrees with the forge; a stale local state is never pushed back.

use crate::commands::{self, ChangeMembership, HUNK_CONTEXT};
use crate::import::{self, IdRef, Page};
use crate::reanchor::{self, Reanchor};
use crate::store::{Append, Batch, Store};
use anyhow::{Context, Result, anyhow, bail};
use git_threads_core::{
    Anchor, Author, Event, EventId, EventKind, LineRange, Side, SnippetTarget, ThreadId, Timestamp,
    derive_snippet, fold_thread,
};
use gix::ObjectId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The foreign change as an executor fetched it (or a test built it):
/// everything the planner knows about the other side.
#[derive(Clone, Debug)]
pub struct ChangeState {
    /// PR/MR number, matched against imported `origin` URLs.
    pub number: u64,
    /// Tip of the change's base branch.
    pub base_ref_oid: String,
    /// The change's head commit.
    pub head_ref_oid: String,
    /// Review threads already on the change.
    pub threads: Vec<ForeignThread>,
}

/// One place a comment already lives on the foreign change: a review
/// thread, or (GitHub) a PR-level comment posing as one so change-level
/// exports are recognized on later runs.
#[derive(Clone, Debug)]
pub struct ForeignThread {
    /// What reply/resolve calls address.
    pub id: String,
    pub is_resolved: bool,
    /// Foreign IDs of its comments.
    pub comments: Vec<String>,
    /// A PR-level comment trail, not a review thread: no reply threading,
    /// no resolution to toggle.
    pub is_pr_level: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct Plan {
    pub threads: Vec<ThreadPlan>,
    pub skips: Vec<Skip>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.threads.is_empty()
    }

    pub fn posts(&self) -> usize {
        self.threads.iter().map(|t| t.posts.len()).sum()
    }
}

/// What one thread contributes to the change.
#[derive(Debug, Serialize)]
pub struct ThreadPlan {
    pub thread: ThreadId,
    pub target: Target,
    /// Messages to post, in display order. Bodies are final (folded text,
    /// attribution header applied). Under [`Target::New`] the first post
    /// creates the foreign thread.
    pub posts: Vec<Post>,
    pub resolve: Option<ResolveAction>,
}

/// Toggle the foreign resolved bit to `to`, on behalf of a local resolve
/// event. Its `ts` seeds the mirror — the forge reports no toggle time.
#[derive(Debug, Serialize)]
pub struct ResolveAction {
    pub event: EventId,
    pub ts: Timestamp,
    pub to: bool,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Target {
    /// The thread already exists on the change; post into it.
    Existing { foreign_thread: String },
    /// Create it, at the best position the change's diff can express.
    New { position: Position },
}

/// Where a new foreign thread lands — the §8.2 cascade, decided locally.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Position {
    /// The discussed lines are visible in the change's diff.
    Line { path: String, side: Side, lines: LineRange },
    /// The file is in the diff but the lines aren't: a file-level comment.
    File { path: String },
    /// Not visible at all (or a `commit` anchor): a change-level comment,
    /// carrying the materialized snippet when the anchor has one.
    ChangeLevel { context: Option<SnippetContext> },
}

/// The §4.1 snippet, materialized for readers without git object access.
#[derive(Debug, Serialize)]
pub struct SnippetContext {
    pub path: String,
    pub lines: LineRange,
    /// The target lines; long ranges keep head and tail around an
    /// `... n lines omitted ...` marker, as the derived snippet does.
    pub text: String,
}

/// One message to post.
#[derive(Debug, Serialize)]
pub struct Post {
    /// The local event being exported — what the mirror's `of` will name.
    pub event: EventId,
    pub author: Author,
    pub ts: Timestamp,
    pub body: String,
}

#[derive(Debug, Serialize)]
pub struct Skip {
    pub thread: ThreadId,
    pub reason: String,
}

/// What the change is missing. Errors mean the change's commits aren't
/// readable locally — executors fetch them first (as import does).
pub fn plan(store: &Store, change: &ChangeState) -> Result<Plan> {
    let repo = store.repo();
    let head = commands::resolve_commit(repo, &change.head_ref_oid)
        .context("the change's head commit is not fetchable")?;
    let base_tip = commands::resolve_commit(repo, &change.base_ref_oid)
        .context("the change's base commit is not fetchable")?;
    let merge_base = repo
        .merge_base(base_tip, head)
        .with_context(|| format!("no merge base between {base_tip} and {head}"))?
        .detach();

    let mut membership = ChangeMembership::new(repo, merge_base, head)?;
    // Foreign IDs that land on this change: its comments, and the threads
    // themselves (resolve records carry thread IDs).
    let on_change: BTreeMap<&str, &ForeignThread> = change
        .threads
        .iter()
        .flat_map(|t| t.comments.iter().map(move |c| (c.as_str(), t)).chain([(t.id.as_str(), t)]))
        .collect();
    let changed = changed_paths(repo, merge_base, head)?;
    let number = change.number.to_string();
    let me = commands::identity(repo).ok();

    let mut plan = Plan::default();
    for record in store.threads()? {
        let folded = fold_thread(record.events.clone());

        // Selection: anchored within the change (patch-id twins included, on
        // either the original or the moved-to anchor), or imported from this
        // very change even if a rewrite pushed its anchor out of the range.
        let anchored_here = membership.contains(record.anchor.diff.head.as_str())
            || folded
                .moved
                .as_ref()
                .and_then(|(_, e)| e.anchor.as_ref())
                .is_some_and(|a| membership.contains(a.diff.head.as_str()));
        let imported_from_here = record.events.iter().any(|(_, e)| commands::origin_pr(e, &number));
        if !anchored_here && !imported_from_here {
            continue;
        }
        if record.drafts.contains(&record.id) {
            plan.skips.push(Skip {
                thread: record.id.clone(),
                reason: "still a draft; `git threads commit` first".into(),
            });
            continue;
        }

        // Foreign identities: an event's own origin (imported), or the
        // origin of a mirror naming it (exported before).
        let foreign_ids: BTreeMap<&EventId, &str> = record
            .events
            .iter()
            .filter_map(|(id, e)| {
                let origin = e.extra.get("origin")?.get("id")?.as_str()?;
                Some((if e.kind == EventKind::Mirror { e.of.as_ref()? } else { id }, origin))
            })
            .collect();

        // Any foreign identity mapping onto the change names the thread to
        // post into; foreign identities that all point elsewhere mean the
        // thread already lives on another change (or forge).
        let onto = foreign_ids.values().find_map(|fid| on_change.get(fid).copied());
        let (target, foreign_resolved) = match (onto, foreign_ids.is_empty()) {
            (Some(foreign), _) => {
                (Target::Existing { foreign_thread: foreign.id.clone() }, foreign.is_resolved)
            }
            (None, false) => {
                plan.skips.push(Skip {
                    thread: record.id.clone(),
                    reason: "already lives elsewhere (its foreign identity is not on this change)"
                        .into(),
                });
                continue;
            }
            (None, true) => {
                let effective = commands::effective_anchor(&record, &folded);
                (
                    Target::New {
                        position: position(store, effective, merge_base, head, &changed)?,
                    },
                    false,
                )
            }
        };

        let posts: Vec<Post> = folded
            .events
            .iter()
            .filter(|e| !e.retracted && !record.drafts.contains(&e.id))
            .filter(|e| !foreign_ids.contains_key(&e.id))
            .filter_map(|e| {
                Some(Post {
                    event: e.id.clone(),
                    author: e.event.author.clone(),
                    ts: e.event.ts.clone(),
                    body: attributed_body(e.effective_body.as_deref()?, &e.event, me.as_ref()),
                })
            })
            .collect();

        // §8.2: toggle only on a new local intent that the forge disagrees
        // with. An imported or already-mirrored latest resolve means the
        // forge has the newer say — a stale local state is never pushed.
        let resolve = record
            .events
            .iter()
            .filter(|(_, e)| e.kind == EventKind::Resolve)
            .max_by_key(|(id, e)| (e.ts.clone(), (*id).clone()))
            .and_then(|(id, e)| {
                let new_intent = !foreign_ids.contains_key(id) && !record.drafts.contains(id);
                (new_intent && folded.resolved != foreign_resolved).then(|| ResolveAction {
                    event: id.clone(),
                    ts: e.ts.clone(),
                    to: folded.resolved,
                })
            });

        // Nothing new to say and nothing to toggle — or a would-be new
        // thread with nothing to open it with (a thread cannot be created
        // out of a resolve alone).
        if posts.is_empty() && (matches!(target, Target::New { .. }) || resolve.is_none()) {
            continue;
        }
        plan.threads.push(ThreadPlan { thread: record.id.clone(), target, posts, resolve });
    }
    Ok(plan)
}

/// The §8.2 position cascade for a new foreign thread: a line comment when
/// the discussed code is visible in the change's diff, a file comment when
/// only the file is, a change-level comment otherwise.
fn position(
    store: &Store,
    anchor: &Anchor,
    merge_base: ObjectId,
    head: ObjectId,
    changed: &BTreeSet<String>,
) -> Result<Position> {
    let repo = store.repo();
    // Commit anchors describe the whole change.
    let Some(anchor_path) = anchor.path.as_deref() else {
        return Ok(Position::ChangeLevel { context: None });
    };

    // Old-side anchors (deleted lines) translate directly — their line
    // numbers are base coordinates — but only when the anchor's base *is*
    // the change's base.
    if anchor.side == Some(Side::Old) {
        if let Some(lines) = anchor.lines
            && anchor.diff.base.as_str() == merge_base.to_string()
            && in_diff(repo, merge_base, head, anchor_path, Side::Old, lines)?
        {
            return Ok(Position::Line { path: anchor_path.to_string(), side: Side::Old, lines });
        }
    } else {
        // New-side: wherever the code lives at the change's head.
        if let Reanchor::Located { path, lines, .. } = reanchor::reanchor(store, anchor, head)? {
            if let Some(lines) = lines
                && in_diff(repo, merge_base, head, &path, Side::New, lines)?
            {
                return Ok(Position::Line { path, side: Side::New, lines });
            }
            if changed.contains(&path) {
                return Ok(Position::File { path });
            }
        }
    }
    if changed.contains(anchor_path) {
        return Ok(Position::File { path: anchor_path.to_string() });
    }
    Ok(Position::ChangeLevel { context: snippet_context(store, anchor) })
}

/// Whether `lines` of `path` on `side` are visible in the change's diff —
/// within a hunk or its display context, the same rule `comment` placement
/// enforces (and what forges will accept a line comment on).
fn in_diff(
    repo: &gix::Repository,
    base: ObjectId,
    head: ObjectId,
    path: &str,
    side: Side,
    lines: LineRange,
) -> Result<bool> {
    let diff = commands::git(
        repo.git_dir(),
        &["diff", "--unified=0", &base.to_string(), &head.to_string(), "--", path],
    )?;
    Ok(commands::hunk_spans(&diff, side).iter().any(|&(start, len)| {
        let lo = start.saturating_sub(HUNK_CONTEXT).max(1);
        let hi = start + len.max(1) - 1 + HUNK_CONTEXT;
        lines.start <= hi && lines.end >= lo
    }))
}

fn changed_paths(
    repo: &gix::Repository,
    base: ObjectId,
    head: ObjectId,
) -> Result<BTreeSet<String>> {
    let out = commands::git(
        repo.git_dir(),
        &["diff", "--name-only", &base.to_string(), &head.to_string()],
    )?;
    Ok(out.lines().map(str::to_string).collect())
}

/// The anchor's derived snippet (§4.1) as display text: the target lines,
/// long ranges elided in the middle. Best-effort — `None` when the anchored
/// blob is unreadable or disagrees with the anchor.
fn snippet_context(store: &Store, anchor: &Anchor) -> Option<SnippetContext> {
    let path = anchor.path.clone()?;
    let lines = anchor.lines?;
    let blob = ObjectId::from_hex(anchor.blob.as_ref()?.as_str().as_bytes()).ok()?;
    let content = reanchor::blob_content(store.repo(), blob).ok()?;
    let snippet = derive_snippet(&content, lines)?;
    let text = match snippet.target {
        SnippetTarget::Full(lines) => lines.join("\n"),
        SnippetTarget::Truncated { head, tail, omitted, .. } => {
            format!("{}\n... {omitted} lines omitted ...\n{}", head.join("\n"), tail.join("\n"))
        }
    };
    Some(SnippetContext { path, lines, text })
}

/// The body as posted. Everything goes out under the exporting account, so
/// events by anyone but the exporter (per local git identity — the best
/// available notion of "me") carry an attribution header.
fn attributed_body(body: &str, event: &Event, me: Option<&Author>) -> String {
    if me.is_some_and(|me| me.email == event.author.email) {
        return body.to_string();
    }
    let date = event.ts.as_str().get(..10).unwrap_or(event.ts.as_str());
    format!("**{}** · {date} · via git-threads\n\n{body}", event.author.name)
}

// ---- GitHub executor ---------------------------------------------------
//
// Same seam philosophy as the importer: `gh` provides auth, the planner's
// output is what gets posted, and every successful post is recorded as a
// `mirror` event before the run moves on — an interrupted export resumes
// where it stopped instead of double-posting.

/// Tallies of one export run.
#[derive(Debug, Default)]
pub struct ExportReport {
    /// Threads touched (created, replied into, or resolved).
    pub threads: usize,
    /// Messages posted.
    pub posts: usize,
    /// Resolution toggles.
    pub resolves: usize,
    /// Threads skipped with a reason.
    pub skipped: usize,
    /// True when --dry-run stopped before posting.
    pub dry_run: bool,
}

/// Export this change's threads onto a GitHub pull request (a number or
/// URL). Posting is sequential and paced — forges rate-limit content
/// creation — and each thread's mirrors are published as soon as its posts
/// land.
pub fn github(store: &Store, remote: &str, spec: &str, dry_run: bool) -> Result<ExportReport> {
    let workdir =
        store.repo().workdir().context("export requires a non-bare repository")?.to_owned();
    let remote_url = commands::git(&workdir, &["remote", "get-url", remote])?;
    let remote_slug = import::github_slug(remote_url.trim());
    let (slug_from_spec, number) = import::parse_spec(spec)
        .with_context(|| format!("cannot parse {spec:?} as a PR number or GitHub PR URL"))?;
    let (slug, source) = match slug_from_spec {
        Some(slug) if remote_slug.as_ref() != Some(&slug) => {
            let source = format!("https://github.com/{}/{}", slug.0, slug.1);
            (slug, source)
        }
        _ => {
            let slug = remote_slug
                .with_context(|| format!("remote {remote:?} does not point at github.com"))?;
            (slug, remote.to_string())
        }
    };

    let pr = fetch_pr(&slug, number)?
        .with_context(|| format!("no pull request #{number} in {}/{}", slug.0, slug.1))?;
    ensure_commits(store, &workdir, &source, number, &[&pr.base_ref_oid, &pr.head_ref_oid]);

    let change = ChangeState {
        number,
        base_ref_oid: pr.base_ref_oid.clone(),
        head_ref_oid: pr.head_ref_oid.clone(),
        threads: pr.threads,
    };
    let plan = plan(store, &change)?;
    let mut report = ExportReport { skipped: plan.skips.len(), dry_run, ..Default::default() };
    for skip in &plan.skips {
        eprintln!("warning: skipping thread {}: {}", short(&skip.thread), skip.reason);
    }
    if dry_run {
        print_plan(&plan);
        report.threads = plan.threads.len();
        report.posts = plan.posts();
        return Ok(report);
    }
    if plan.is_empty() {
        return Ok(report);
    }

    let me = viewer()?;
    // Review threads (as opposed to PR-level comment trails) are the only
    // targets replies and resolves address by thread ID.
    let review_threads: BTreeSet<&str> =
        change.threads.iter().filter(|t| !t.is_pr_level).map(|t| t.id.as_str()).collect();
    let mut pace = Pace::default();
    // Threads created this run that still need their resolution toggled:
    // the thread node ID only exists after a refetch.
    let mut created_resolves: Vec<(ThreadId, String, ResolveAction)> = Vec::new();

    for thread_plan in &plan.threads {
        let mut mirrors: Vec<Event> = Vec::new();
        let outcome = export_thread(
            &slug,
            number,
            &change,
            &me,
            thread_plan,
            &review_threads,
            &mut pace,
            &mut mirrors,
            &mut created_resolves,
            &mut report,
        );
        // Whatever happened, what was posted is recorded before anything
        // else — an error must not orphan comments already on the forge.
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

    // Resolutions on threads created this run: one refetch maps each root
    // comment to the thread GitHub minted for it.
    if !created_resolves.is_empty() {
        let refreshed = fetch_pr(&slug, number)?.context("the pull request vanished mid-run")?;
        let thread_of: BTreeMap<&str, &str> = refreshed
            .threads
            .iter()
            .flat_map(|t| t.comments.iter().map(move |c| (c.as_str(), t.id.as_str())))
            .collect();
        for (thread, root_foreign, action) in created_resolves {
            let Some(foreign_thread) = thread_of.get(root_foreign.as_str()) else {
                eprintln!(
                    "warning: cannot resolve thread {}: its comment is not on the PR (deleted?)",
                    short(&thread)
                );
                continue;
            };
            pace.wait();
            toggle_resolve(foreign_thread, action.to)?;
            let mirror = resolve_mirror(&me, &action, "github", foreign_thread)?;
            let batch = Batch {
                appends: vec![Append { thread, events: vec![mirror] }],
                ..Default::default()
            };
            store.write(&batch)?;
            report.resolves += 1;
        }
    }
    Ok(report)
}

/// Post one thread's plan. Mirrors for everything that landed are pushed
/// into `mirrors` as it happens — the caller publishes them even when a
/// later post fails.
#[allow(clippy::too_many_arguments)]
fn export_thread(
    slug: &(String, String),
    number: u64,
    change: &ChangeState,
    me: &Author,
    plan: &ThreadPlan,
    review_threads: &BTreeSet<&str>,
    pace: &mut Pace,
    mirrors: &mut Vec<Event>,
    created_resolves: &mut Vec<(ThreadId, String, ResolveAction)>,
    report: &mut ExportReport,
) -> Result<()> {
    match &plan.target {
        Target::New { position } => {
            let first = plan.posts.first().expect("a new thread always has a first post");
            pace.wait();
            let root = match position {
                Position::Line { .. } | Position::File { .. } => create_review_comment(
                    slug,
                    number,
                    &change.head_ref_oid,
                    position,
                    &first.body,
                )?,
                Position::ChangeLevel { context } => {
                    let body = change_level_body(
                        slug,
                        &change.head_ref_oid,
                        context.as_ref(),
                        &first.body,
                    );
                    create_issue_comment(slug, number, &body)?
                }
            };
            println!(
                "thread {}: {} ({})",
                short(&plan.thread),
                match position {
                    Position::ChangeLevel { .. } => "posted as a change-level comment",
                    Position::File { .. } => "created a file-level review thread",
                    Position::Line { .. } => "created a review thread",
                },
                describe(position),
            );
            mirrors.push(mirror_event(
                me,
                first,
                "github",
                &root.node_id,
                Some(&root.html_url),
                &root.created_at,
            )?);
            report.posts += 1;
            for post in &plan.posts[1..] {
                pace.wait();
                let posted = match position {
                    // PR-level trails have no reply threading; each message
                    // is its own comment, in order.
                    Position::ChangeLevel { .. } => create_issue_comment(slug, number, &post.body)?,
                    _ => reply_review_comment(slug, number, root.id, &post.body)?,
                };
                mirrors.push(mirror_event(
                    me,
                    post,
                    "github",
                    &posted.node_id,
                    Some(&posted.html_url),
                    &posted.created_at,
                )?);
                report.posts += 1;
            }
            if let Some(action) = &plan.resolve {
                match position {
                    Position::ChangeLevel { .. } => eprintln!(
                        "note: thread {} is resolved locally, but a change-level comment has no resolution to toggle",
                        short(&plan.thread)
                    ),
                    _ => created_resolves.push((
                        plan.thread.clone(),
                        root.node_id.clone(),
                        ResolveAction {
                            event: action.event.clone(),
                            ts: action.ts.clone(),
                            to: action.to,
                        },
                    )),
                }
            }
        }
        Target::Existing { foreign_thread } => {
            let is_review_thread = review_threads.contains(foreign_thread.as_str());
            for post in &plan.posts {
                pace.wait();
                let posted = if is_review_thread {
                    reply_in_thread(foreign_thread, &post.body)?
                } else {
                    // The thread lives as a PR-level comment trail.
                    create_issue_comment(slug, number, &post.body)?
                };
                mirrors.push(mirror_event(
                    me,
                    post,
                    "github",
                    &posted.node_id,
                    Some(&posted.html_url),
                    &posted.created_at,
                )?);
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
            if let Some(action) = &plan.resolve {
                if is_review_thread {
                    pace.wait();
                    toggle_resolve(foreign_thread, action.to)?;
                    mirrors.push(resolve_mirror(me, action, "github", foreign_thread)?);
                    report.resolves += 1;
                    println!(
                        "thread {}: {} on the PR",
                        short(&plan.thread),
                        if action.to { "resolved" } else { "reopened" }
                    );
                } else {
                    eprintln!(
                        "note: thread {} is resolved locally, but a change-level comment has no resolution to toggle",
                        short(&plan.thread)
                    );
                }
            }
        }
    }
    Ok(())
}

/// What `--dry-run` prints: the plan, one thread per line.
pub(crate) fn print_plan(plan: &Plan) {
    for thread in &plan.threads {
        let action = match &thread.target {
            Target::New { position } => format!("would create {}", describe(position)),
            Target::Existing { .. } => "would post into the existing thread".to_string(),
        };
        let resolve = match &thread.resolve {
            Some(action) if action.to => ", then resolve it",
            Some(_) => ", then reopen it",
            None => "",
        };
        println!(
            "thread {}: {action}; {} message{}{resolve}",
            short(&thread.thread),
            thread.posts.len(),
            if thread.posts.len() == 1 { "" } else { "s" },
        );
    }
    if plan.threads.is_empty() {
        println!("nothing to export");
    }
}

pub(crate) fn describe(position: &Position) -> String {
    match position {
        Position::Line { path, side, lines } => {
            let lines = match lines.start == lines.end {
                true => format!("{}", lines.start),
                false => format!("{}-{}", lines.start, lines.end),
            };
            let side = match side {
                Side::Old => " (old side)",
                Side::New => "",
            };
            format!("a line comment on {path}:{lines}{side}")
        }
        Position::File { path } => format!("a file-level comment on {path}"),
        Position::ChangeLevel { context: Some(context) } => {
            format!("a change-level comment carrying {}:{}", context.path, context.lines.start)
        }
        Position::ChangeLevel { context: None } => "a change-level comment".into(),
    }
}

/// A change-level comment says where it belongs, since the diff can't: the
/// materialized snippet (§8.2) plus a permalink into the head tree.
fn change_level_body(
    slug: &(String, String),
    head: &str,
    context: Option<&SnippetContext>,
    body: &str,
) -> String {
    let Some(context) = context else { return body.to_string() };
    let lines = match context.lines.start == context.lines.end {
        true => format!("{}", context.lines.start),
        false => format!("{}-{}", context.lines.start, context.lines.end),
    };
    let fragment = match context.lines.start == context.lines.end {
        true => format!("L{}", context.lines.start),
        false => format!("L{}-L{}", context.lines.start, context.lines.end),
    };
    format!(
        "**On [`{path}:{lines}`](https://github.com/{owner}/{repo}/blob/{head}/{path}#{fragment}) — not visible in this diff:**\n\n```\n{snippet}\n```\n\n{body}",
        path = context.path,
        owner = slug.0,
        repo = slug.1,
        snippet = context.text,
    )
}

/// The account posting everything, as an author — the same noreply mapping
/// the importer uses, so a comment exported and re-imported round-trips to
/// one identity.
fn viewer() -> Result<Author> {
    #[derive(Deserialize)]
    struct Response {
        data: Option<Data>,
    }
    #[derive(Deserialize)]
    struct Data {
        viewer: Option<import::GhUser>,
    }
    let out = import::gh(&["api", "graphql", "-f", "query=query{viewer{login databaseId}}"])?;
    let response: Response =
        serde_json::from_str(&out).context("unexpected gh api graphql output")?;
    let user = response.data.and_then(|d| d.viewer).context("gh is not logged in")?;
    Ok(import::author_of(&user))
}

/// One `mirror` event (SPEC.md §8.2) for a posted message.
pub(crate) fn mirror_event(
    me: &Author,
    post: &Post,
    forge: &str,
    foreign_id: &str,
    url: Option<&str>,
    created_at: &str,
) -> Result<Event> {
    let mut event = Event {
        v: 1,
        kind: EventKind::Mirror,
        author: me.clone(),
        ts: Timestamp::parse(created_at).map_err(|e| anyhow!("bad forge timestamp: {e}"))?,
        body: None,
        in_reply_to: None,
        supersedes: None,
        resolved: None,
        anchor: None,
        of: Some(post.event.clone()),
        extra: Default::default(),
    };
    event.extra.insert("origin".into(), import::origin_value(forge, foreign_id, url));
    event.validate()?;
    Ok(event)
}

/// The mirror for a resolution toggle: `of` names the local resolve event,
/// the origin is the foreign *thread* — exactly the ID import's synthetic
/// resolve dedups on. The forge reports no toggle time, so `ts` is the
/// mirrored event's own.
pub(crate) fn resolve_mirror(
    me: &Author,
    action: &ResolveAction,
    forge: &str,
    foreign_thread: &str,
) -> Result<Event> {
    let mut mirror = Event {
        v: 1,
        kind: EventKind::Mirror,
        author: me.clone(),
        ts: action.ts.clone(),
        body: None,
        in_reply_to: None,
        supersedes: None,
        resolved: None,
        anchor: None,
        of: Some(action.event.clone()),
        extra: Default::default(),
    };
    mirror.extra.insert("origin".into(), import::origin_value(forge, foreign_thread, None));
    mirror.validate()?;
    Ok(mirror)
}

/// Content-creation pacing: forges rate-limit writes far below reads, so
/// mutations go out at most one per second.
#[derive(Default)]
pub(crate) struct Pace {
    started: bool,
}

impl Pace {
    pub(crate) fn wait(&mut self) {
        if std::mem::replace(&mut self.started, true) {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}

fn short(id: &EventId) -> &str {
    id.as_str().get(..12).unwrap_or(id.as_str())
}

// ---- GitHub API calls ----------------------------------------------------

/// What every posting endpoint returns, REST and GraphQL alike. `node_id`
/// is the origin recorded in mirrors — the same ID space the importer's
/// GraphQL fetch reports, so dedup lines up.
#[derive(Deserialize)]
struct Posted {
    /// Numeric ID (REST replies address root comments by it).
    id: u64,
    node_id: String,
    html_url: String,
    created_at: String,
}

fn create_review_comment(
    slug: &(String, String),
    number: u64,
    head: &str,
    position: &Position,
    body: &str,
) -> Result<Posted> {
    let endpoint = format!("repos/{}/{}/pulls/{number}/comments", slug.0, slug.1);
    let body_field = format!("body={body}");
    let commit_field = format!("commit_id={head}");
    let mut args = vec!["api", &endpoint, "-f", &body_field, "-f", &commit_field];
    // (flag, field) pairs: line numbers must be typed (-F), strings are -f.
    let fields: Vec<(&str, String)> = match position {
        Position::Line { path, side, lines } => {
            let side = match side {
                Side::Old => "LEFT",
                Side::New => "RIGHT",
            };
            let mut fields = vec![("-f", format!("path={path}")), ("-f", format!("side={side}"))];
            if lines.start < lines.end {
                fields.push(("-F", format!("start_line={}", lines.start)));
                fields.push(("-f", format!("start_side={side}")));
            }
            fields.push(("-F", format!("line={}", lines.end)));
            fields
        }
        Position::File { path } => {
            vec![("-f", format!("path={path}")), ("-f", "subject_type=file".to_string())]
        }
        Position::ChangeLevel { .. } => unreachable!("change-level posts are issue comments"),
    };
    for (flag, field) in &fields {
        args.extend([*flag, field]);
    }
    parse_posted(&import::gh(&args)?)
}

fn reply_review_comment(
    slug: &(String, String),
    number: u64,
    comment: u64,
    body: &str,
) -> Result<Posted> {
    let endpoint = format!("repos/{}/{}/pulls/{number}/comments/{comment}/replies", slug.0, slug.1);
    let body_field = format!("body={body}");
    parse_posted(&import::gh(&["api", &endpoint, "-f", &body_field])?)
}

fn create_issue_comment(slug: &(String, String), number: u64, body: &str) -> Result<Posted> {
    let endpoint = format!("repos/{}/{}/issues/{number}/comments", slug.0, slug.1);
    let body_field = format!("body={body}");
    parse_posted(&import::gh(&["api", &endpoint, "-f", &body_field])?)
}

fn parse_posted(out: &str) -> Result<Posted> {
    serde_json::from_str(out).context("unexpected gh api output for a posted comment")
}

/// Reply into an existing review thread, by its node ID — what imported
/// origins record.
fn reply_in_thread(thread: &str, body: &str) -> Result<Posted> {
    #[derive(Deserialize)]
    struct Response {
        data: Option<Data>,
    }
    #[derive(Deserialize)]
    struct Data {
        #[serde(rename = "addPullRequestReviewThreadReply")]
        reply: Option<Comment>,
    }
    #[derive(Deserialize)]
    struct Comment {
        comment: Option<Node>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Node {
        id: String,
        database_id: Option<u64>,
        url: String,
        created_at: String,
    }
    let query = "query=mutation($thread:ID!,$body:String!){\
        addPullRequestReviewThreadReply(input:{pullRequestReviewThreadId:$thread,body:$body})\
        {comment{id databaseId url createdAt}}}";
    let thread_field = format!("thread={thread}");
    let body_field = format!("body={body}");
    let out = import::gh(&["api", "graphql", "-f", query, "-f", &thread_field, "-f", &body_field])?;
    let response: Response =
        serde_json::from_str(&out).context("unexpected gh api graphql output")?;
    let node = response
        .data
        .and_then(|d| d.reply)
        .and_then(|r| r.comment)
        .context("the reply was not created (is the thread still on the PR?)")?;
    Ok(Posted {
        id: node.database_id.unwrap_or_default(),
        node_id: node.id,
        html_url: node.url,
        created_at: node.created_at,
    })
}

fn toggle_resolve(thread: &str, desired: bool) -> Result<()> {
    let mutation = if desired { "resolveReviewThread" } else { "unresolveReviewThread" };
    let query = format!(
        "query=mutation($thread:ID!){{{mutation}(input:{{threadId:$thread}}){{thread{{id}}}}}}"
    );
    let thread_field = format!("thread={thread}");
    let out = import::gh(&["api", "graphql", "-f", &query, "-f", &thread_field])?;
    if !out.contains(thread) {
        bail!("the forge did not confirm the resolution toggle");
    }
    Ok(())
}

// ---- GitHub PR state fetch -------------------------------------------

/// The PR as the executor needs it: its two commits and every place a
/// comment already lives — review threads, and the PR-level comment trail
/// (where change-level exports land; kept so their threads keep appending
/// there instead of being mistaken for foreign ones).
struct PrState {
    base_ref_oid: String,
    head_ref_oid: String,
    threads: Vec<ForeignThread>,
}

fn fetch_pr(slug: &(String, String), number: u64) -> Result<Option<PrState>> {
    #[derive(Deserialize)]
    struct Response {
        data: Option<Data>,
    }
    #[derive(Deserialize)]
    struct Data {
        repository: Option<Repo>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Repo {
        pull_request: Option<Pr>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Pr {
        base_ref_oid: String,
        head_ref_oid: String,
        review_threads: Page<ThreadNode>,
        comments: Page<IdRef>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ThreadNode {
        id: String,
        is_resolved: bool,
        comments: Page<IdRef>,
    }

    let query = "query=query($owner:String!,$name:String!,$number:Int!,$cursor:String){\
repository(owner:$owner,name:$name){pullRequest(number:$number){\
baseRefOid headRefOid \
reviewThreads(first:100,after:$cursor){pageInfo{hasNextPage endCursor}\
nodes{id isResolved comments(first:100){pageInfo{hasNextPage endCursor}nodes{id}}}}\
comments(first:100){pageInfo{hasNextPage endCursor}nodes{id}}}}}";

    let mut state: Option<PrState> = None;
    let mut pr_comments: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let owner = format!("owner={}", slug.0);
        let name = format!("name={}", slug.1);
        let number_field = format!("number={number}");
        let mut args =
            vec!["api", "graphql", "-f", query, "-f", &owner, "-f", &name, "-F", &number_field];
        let cursor_field = cursor.as_ref().map(|c| format!("cursor={c}"));
        if let Some(field) = &cursor_field {
            args.extend(["-f", field]);
        }
        let out = import::gh(&args)?;
        let response: Response =
            serde_json::from_str(&out).context("unexpected gh api graphql output")?;
        let Some(pr) = response.data.and_then(|d| d.repository).and_then(|r| r.pull_request) else {
            return Ok(None);
        };
        if state.is_none() {
            // PR-level comments rarely overflow their slot; when they do,
            // the missing ones just look foreign and are skipped honestly.
            pr_comments = pr.comments.nodes.into_iter().map(|c| c.id).collect();
        }
        let page = pr.review_threads;
        let entry = state.get_or_insert_with(|| PrState {
            base_ref_oid: pr.base_ref_oid,
            head_ref_oid: pr.head_ref_oid,
            threads: Vec::new(),
        });
        for node in page.nodes {
            let mut comments: Vec<String> = node.comments.nodes.into_iter().map(|c| c.id).collect();
            fetch_remaining_comment_ids(&node.id, &node.comments.page_info, &mut comments)?;
            entry.threads.push(ForeignThread {
                id: node.id,
                is_resolved: node.is_resolved,
                comments,
                is_pr_level: false,
            });
        }
        if !page.page_info.has_next_page {
            break;
        }
        cursor = page.page_info.end_cursor;
    }
    let mut state = state.expect("loop ran at least once");
    // Each PR-level comment is its own pseudo-thread: change-level exports
    // land here, and later runs must recognize them as already-exported and
    // keep appending at the PR level.
    state.threads.extend(pr_comments.into_iter().map(|id| ForeignThread {
        id: id.clone(),
        is_resolved: false,
        comments: vec![id],
        is_pr_level: true,
    }));
    Ok(Some(state))
}

/// The rest of an overlong thread's comment IDs.
fn fetch_remaining_comment_ids(
    thread: &str,
    page_info: &import::PageInfo,
    comments: &mut Vec<String>,
) -> Result<()> {
    #[derive(Deserialize)]
    struct Response {
        data: Option<Data>,
    }
    #[derive(Deserialize)]
    struct Data {
        node: Option<Node>,
    }
    #[derive(Deserialize)]
    struct Node {
        comments: Option<Page<IdRef>>,
    }
    let mut has_next = page_info.has_next_page;
    let mut cursor = page_info.end_cursor.clone();
    while has_next {
        let Some(after) = cursor else { break };
        let query = "query=query($id:ID!,$cursor:String){node(id:$id){\
... on PullRequestReviewThread{comments(first:100,after:$cursor){\
pageInfo{hasNextPage endCursor}nodes{id}}}}}";
        let id_field = format!("id={thread}");
        let cursor_field = format!("cursor={after}");
        let out =
            import::gh(&["api", "graphql", "-f", query, "-f", &id_field, "-f", &cursor_field])?;
        let response: Response =
            serde_json::from_str(&out).context("unexpected gh api graphql output")?;
        let Some(page) = response.data.and_then(|d| d.node).and_then(|n| n.comments) else {
            break;
        };
        comments.extend(page.nodes.into_iter().map(|c| c.id));
        has_next = page.page_info.has_next_page;
        cursor = page.page_info.end_cursor;
    }
    Ok(())
}

/// Make the PR's commits readable locally, best-effort: planning needs its
/// head (and base) to diff and re-anchor against.
fn ensure_commits(
    store: &Store,
    workdir: &std::path::Path,
    source: &str,
    number: u64,
    oids: &[&str],
) {
    let missing = |oid: &str| import::commit(store.repo(), oid).is_err();
    if !oids.iter().any(|oid| missing(oid)) {
        return;
    }
    let head_ref = format!("refs/pull/{number}/head");
    let _ = commands::git(workdir, &["fetch", "--quiet", source, &head_ref]);
    let still: Vec<&str> = oids.iter().copied().filter(|oid| missing(oid)).collect();
    if !still.is_empty() {
        let mut args = vec!["fetch", "--quiet", source];
        args.extend(still);
        let _ = commands::git(workdir, &args);
    }
}
