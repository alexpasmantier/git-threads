//! GitHub importer (SPEC.md §8): PR review threads become anchored threads.
//!
//! Fetching shells out to `gh api graphql` — auth and pagination for free,
//! and no HTTP stack in the tree; the raw thread data is the seam, so a
//! native fetcher could replace it without touching the mapping. The mapping
//! itself is deterministic: every event's bytes derive only from forge data
//! (login, database ID, `createdAt`, body) and the git DAG (merge-base),
//! never from import time — two clones importing the same PR produce
//! identical event IDs, and the union merge dedupes them.
//!
//! Every imported event carries the forge ID in an `origin` field
//! (`{"forge": "github", "id": ..., "url": ...}`), and events whose origin
//! ID is already in the store are skipped — re-imports are no-ops, and a
//! comment edited on GitHub after the first import cannot mint a duplicate.
//! A thread whose code cannot be reconstructed (its commit or blob is gone
//! from the forge) is skipped with a warning, never half-anchored.

use crate::commands;
use crate::reanchor;
use crate::store::{Append, Batch, NewThread, Store};
use anyhow::{Context, Result, anyhow, bail};
use git_threads_core::{
    Anchor, AnchorKind, Author, DiffRef, Event, EventId, EventKind, GitOid, LineRange, Side,
    ThreadId, Timestamp,
};
use gix::ObjectId;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::process::Command;

/// One review thread as GitHub's GraphQL API reports it — the input to the
/// deterministic mapping. Public so the mapping is testable from recorded
/// or hand-built data, no network involved.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewThread {
    /// Thread node ID; the synthetic `resolve` event's origin.
    pub id: String,
    pub is_resolved: bool,
    pub path: String,
    /// `LINE` or `FILE` (file-level comments carry no lines).
    pub subject_type: Option<String>,
    /// `RIGHT` (new side) or `LEFT` (old side).
    pub diff_side: Option<String>,
    pub start_diff_side: Option<String>,
    /// Position in file coordinates at the root comment's original commit.
    pub original_line: Option<u32>,
    pub original_start_line: Option<u32>,
    pub resolved_by: Option<GhUser>,
    pub comments: Page<ReviewComment>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewComment {
    /// Comment node ID; the event's origin.
    pub id: String,
    pub url: String,
    pub body: String,
    /// ISO 8601 UTC, second precision — the event's `ts` verbatim.
    pub created_at: String,
    /// `None` for deleted accounts (GitHub's "ghost").
    pub author: Option<GhUser>,
    /// The PR head the comment was made against — the anchor's `head`.
    pub original_commit: Option<OidRef>,
    pub reply_to: Option<IdRef>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GhUser {
    pub login: String,
    /// Numeric account ID; part of the stable noreply email.
    pub database_id: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct OidRef {
    pub oid: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct IdRef {
    pub id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    pub page_info: PageInfo,
    pub nodes: Vec<T>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageInfo {
    pub has_next_page: bool,
    pub end_cursor: Option<String>,
}

/// Tallies of one import run.
#[derive(Debug, Default)]
pub struct ImportReport {
    /// PRs that had review threads.
    pub prs: usize,
    /// Threads created or appended to.
    pub threads: usize,
    /// Events written.
    pub events: usize,
    /// Events skipped because their origin ID is already in the store.
    pub known: usize,
    /// Threads skipped because their code could not be reconstructed.
    pub skipped: usize,
}

impl ImportReport {
    fn absorb(&mut self, other: ImportReport) {
        self.prs += other.prs;
        self.threads += other.threads;
        self.events += other.events;
        self.known += other.known;
        self.skipped += other.skipped;
    }
}

/// One PR's review threads, ready to map.
struct PrThreads {
    number: u64,
    base_ref_oid: String,
    threads: Vec<ReviewThread>,
}

/// Import review threads from GitHub: one PR (a number or URL), or with
/// `all`, every PR of the repository. Objects are fetched from `remote`
/// (or the repository a URL names); each PR with anything new becomes one
/// publish commit, so a long `--all` run keeps its progress on failure.
///
/// The network is the slow part, so it is batched: `--all` sweeps review
/// threads for 50 PRs per API request, and all missing commits arrive in
/// two `git fetch` round trips however many PRs the run covers. Progress
/// for each network phase goes to stderr.
pub fn github(store: &Store, remote: &str, spec: Option<&str>, all: bool) -> Result<ImportReport> {
    let workdir = store
        .repo()
        .workdir()
        .context("import requires a non-bare repository")?
        .to_owned();
    let remote_url = commands::git(&workdir, &["remote", "get-url", remote])?;
    let remote_slug = github_slug(remote_url.trim());

    let (slug, source) = match spec.and_then(parse_spec) {
        // A URL names its repository; fetch objects straight from it when
        // it isn't the one `remote` points at.
        Some((Some(slug), _)) if remote_slug.as_ref() != Some(&slug) => {
            let source = format!("https://github.com/{}/{}", slug.0, slug.1);
            (slug, source)
        }
        _ => {
            let slug = remote_slug
                .with_context(|| format!("remote {remote:?} does not point at github.com"))?;
            (slug, remote.to_string())
        }
    };

    let prs: Vec<PrThreads> = if all {
        fetch_all_threads(&slug)?
    } else {
        let spec = spec.context("pass a PR number or URL, or --all")?;
        let (_, number) = parse_spec(spec)
            .with_context(|| format!("cannot parse {spec:?} as a PR number or GitHub PR URL"))?;
        let Some((base_ref_oid, threads)) = fetch_threads(&slug, number)? else {
            bail!("no pull request #{number} in {}/{}", slug.0, slug.1);
        };
        if threads.is_empty() {
            println!("PR #{number}: no review threads");
            return Ok(ImportReport::default());
        }
        vec![PrThreads { number, base_ref_oid, threads }]
    };
    if prs.is_empty() {
        return Ok(ImportReport::default());
    }

    ensure_objects(store, &workdir, &source, &prs);
    // One index for the whole run: a review thread (and every comment in
    // it) belongs to exactly one PR, so nothing written for one PR is ever
    // referenced while mapping another.
    let index = origin_index(store)?;
    let mut report = ImportReport::default();
    for pr in &prs {
        let outcome = apply_with(store, &pr.base_ref_oid, &pr.threads, &index)
            .with_context(|| format!("importing PR #{}", pr.number))?;
        if outcome.events > 0 || !all {
            println!(
                "PR #{}: {} event{} in {} thread{}{}",
                pr.number,
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

/// Map review threads onto the store and publish them as one commit (none
/// when nothing is new). The deterministic core: given the same thread data
/// and the same git objects, every clone writes byte-identical events.
pub fn apply(store: &Store, base_ref_oid: &str, threads: &[ReviewThread]) -> Result<ImportReport> {
    apply_with(store, base_ref_oid, threads, &origin_index(store)?)
}

fn apply_with(
    store: &Store,
    base_ref_oid: &str,
    threads: &[ReviewThread],
    index: &BTreeMap<String, (ThreadId, EventId)>,
) -> Result<ImportReport> {
    let mut batch = Batch::default();
    let mut report = ImportReport::default();

    for thread in threads {
        match map_thread(store, base_ref_oid, thread, index, &mut batch, &mut report) {
            Ok(()) => {}
            Err(err) => {
                eprintln!("warning: skipping thread on {}: {err:#}", thread.path);
                report.skipped += 1;
            }
        }
    }

    if !batch.is_empty() {
        store.write(&batch)?;
    }
    Ok(report)
}

/// Map one thread into `batch`: a new thread when its root is unknown,
/// appends for anything new on an already-imported one. Errors mean the
/// thread cannot be represented faithfully and should be skipped.
fn map_thread(
    store: &Store,
    base_ref_oid: &str,
    thread: &ReviewThread,
    index: &BTreeMap<String, (ThreadId, EventId)>,
    batch: &mut Batch,
    report: &mut ImportReport,
) -> Result<()> {
    let comments = &thread.comments.nodes;
    let root = comments
        .iter()
        .find(|c| c.reply_to.is_none())
        .or_else(|| comments.first())
        .context("thread has no comments")?;

    // GitHub comment node ID → our event ID, for wiring in_reply_to: events
    // built in this call, plus everything previously imported.
    let mut ids: BTreeMap<&str, EventId> = BTreeMap::new();
    let mut events: Vec<Event> = Vec::new();
    let existing = index.get(root.id.as_str()).map(|(thread, _)| thread.clone());

    let root_event = message_event(root, EventKind::Comment, None)?;
    let root_id = root_event.id()?;
    ids.insert(&root.id, root_id.clone());
    let thread_id = match &existing {
        Some(thread) => {
            report.known += 1;
            thread.clone()
        }
        None => root_id,
    };

    for comment in comments {
        if comment.id == root.id {
            continue;
        }
        if index.contains_key(comment.id.as_str()) {
            report.known += 1;
            continue;
        }
        let target = comment
            .reply_to
            .as_ref()
            .and_then(|r| {
                ids.get(r.id.as_str())
                    .cloned()
                    .or_else(|| index.get(r.id.as_str()).map(|(_, event)| event.clone()))
            })
            .unwrap_or_else(|| thread_id.clone());
        let event = message_event(comment, EventKind::Reply, Some(target))?;
        ids.insert(&comment.id, event.id()?);
        events.push(event);
    }

    // Resolution becomes one synthetic resolve event, its origin the thread
    // node ID so it imports exactly once. Its timestamp is derived from the
    // data (the last comment's), never from import time — GitHub does not
    // record when a thread was resolved.
    if thread.is_resolved && !index.contains_key(thread.id.as_str()) {
        let last_ts = comments.iter().map(|c| c.created_at.as_str()).max().unwrap_or_default();
        let author = thread
            .resolved_by
            .as_ref()
            .map(author_of)
            .unwrap_or_else(|| author_of(root.author.as_ref().unwrap_or(&GHOST)));
        let mut event = Event {
            v: 1,
            kind: EventKind::Resolve,
            author,
            ts: Timestamp::parse(last_ts)
                .map_err(|e| anyhow!("bad timestamp on thread: {e}"))?,
            body: None,
            in_reply_to: None,
            supersedes: None,
            resolved: Some(true),
            anchor: None,
            extra: Default::default(),
        };
        event.extra.insert("origin".into(), origin_value(&thread.id, None));
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
            let anchor = map_anchor(store, base_ref_oid, thread, root)?;
            report.events += 1 + events.len();
            report.threads += 1;
            batch.new_threads.push(NewThread { anchor, root: root_event, events });
        }
    }
    Ok(())
}

/// The thread's anchor, reconstructed from GitHub's original position data:
/// `head` is the root comment's original commit, `base` the merge-base with
/// the PR base (what GitHub diffed against), lines are file coordinates on
/// `diffSide`, and the blob is read from the local tree — all functions of
/// the forge data and the DAG, so every clone reconstructs the same anchor.
fn map_anchor(
    store: &Store,
    base_ref_oid: &str,
    thread: &ReviewThread,
    root: &ReviewComment,
) -> Result<Anchor> {
    let repo = store.repo();
    let head_hex = &root.original_commit.as_ref().context("no original commit")?.oid;
    let head = commit(repo, head_hex).context("the commented commit is not fetchable")?;
    let base_tip = commit(repo, base_ref_oid).context("the PR base is not fetchable")?;
    let base = repo
        .merge_base(base_tip, head)
        .with_context(|| format!("no merge base between {base_ref_oid} and {head_hex}"))?
        .detach();

    let side = match thread.diff_side.as_deref() {
        Some("LEFT") => Side::Old,
        _ => Side::New,
    };
    let lines = match (thread.subject_type.as_deref(), thread.original_line) {
        (Some("FILE"), _) | (_, None) => None,
        (_, Some(end)) => {
            // A range spanning both diff sides has no single-file spelling;
            // keep the side the thread ends on and its line.
            let spans_sides = thread.start_diff_side.is_some()
                && thread.start_diff_side != thread.diff_side;
            let start = if spans_sides { end } else { thread.original_start_line.unwrap_or(end) };
            Some(LineRange { start: start.min(end), end })
        }
    };
    let side_commit = match side {
        Side::New => head,
        Side::Old => base,
    };
    let blob = reanchor::blob_at(repo, side_commit, &thread.path)?
        .with_context(|| format!("{:?} not found in the commented tree", thread.path))?;

    let anchor = Anchor {
        v: 1,
        kind: if lines.is_some() { AnchorKind::Range } else { AnchorKind::File },
        diff: DiffRef {
            base: GitOid::from_hex(base.to_string())?,
            head: GitOid::from_hex(head.to_string())?,
        },
        path: Some(thread.path.clone()),
        old_path: None,
        side: Some(side),
        lines,
        blob: Some(GitOid::from_hex(blob.to_string())?),
        cols: None,
        extra: Default::default(),
    };
    anchor.validate()?;
    Ok(anchor)
}

static GHOST: GhUser = GhUser { login: String::new(), database_id: None };

/// A comment or reply event from one GitHub comment. The body is imported
/// verbatim — it is a historical record, not a new message.
fn message_event(
    comment: &ReviewComment,
    kind: EventKind,
    in_reply_to: Option<EventId>,
) -> Result<Event> {
    let mut event = Event {
        v: 1,
        kind,
        author: author_of(comment.author.as_ref().unwrap_or(&GHOST)),
        ts: Timestamp::parse(comment.created_at.as_str())
            .map_err(|e| anyhow!("bad timestamp: {e}"))?,
        body: Some(comment.body.clone()),
        in_reply_to,
        supersedes: None,
        resolved: None,
        anchor: None,
        extra: Default::default(),
    };
    event.extra.insert("origin".into(), origin_value(&comment.id, Some(&comment.url)));
    event.validate()?;
    Ok(event)
}

/// Forge identity → author. The login is the name; the email is GitHub's
/// stable noreply form, keyed on the numeric account ID when known.
fn author_of(user: &GhUser) -> Author {
    let login = if user.login.is_empty() { "ghost" } else { &user.login };
    Author {
        name: login.to_string(),
        email: match user.database_id {
            Some(id) => format!("{id}+{login}@users.noreply.github.com"),
            None => format!("{login}@users.noreply.github.com"),
        },
    }
}

fn origin_value(id: &str, url: Option<&str>) -> serde_json::Value {
    let mut origin = serde_json::Map::new();
    origin.insert("forge".into(), "github".into());
    origin.insert("id".into(), id.into());
    if let Some(url) = url {
        origin.insert("url".into(), url.into());
    }
    serde_json::Value::Object(origin)
}

/// Origin ID → (thread, event) for everything in the store, drafts included.
/// What makes re-imports no-ops.
fn origin_index(store: &Store) -> Result<BTreeMap<String, (ThreadId, EventId)>> {
    let mut index = BTreeMap::new();
    for thread in store.threads()? {
        for (event_id, event) in &thread.events {
            if let Some(id) = event
                .extra
                .get("origin")
                .and_then(|origin| origin.get("id"))
                .and_then(|id| id.as_str())
            {
                index.insert(id.to_string(), (thread.id.clone(), event_id.clone()));
            }
        }
    }
    Ok(index)
}

/// Make the commits the threads anchor to fetchable locally, in two round
/// trips however many PRs are involved: one fetch of every needed PR head
/// ref (GitHub keeps `refs/pull/N/head` after branch deletion), then one
/// fetch by SHA for anything still missing (GitHub serves arbitrary
/// reachable objects). Best effort — mapping skips what stays missing,
/// honestly.
fn ensure_objects(store: &Store, workdir: &std::path::Path, source: &str, prs: &[PrThreads]) {
    let wanted_of = |pr: &PrThreads| {
        std::iter::once(pr.base_ref_oid.clone())
            .chain(
                pr.threads
                    .iter()
                    .flat_map(|t| &t.comments.nodes)
                    .filter_map(|c| c.original_commit.as_ref().map(|c| c.oid.clone())),
            )
            .collect::<Vec<String>>()
    };
    let incomplete: Vec<&PrThreads> = prs
        .iter()
        .filter(|pr| wanted_of(pr).iter().any(|oid| commit(store.repo(), oid).is_err()))
        .collect();
    if incomplete.is_empty() {
        return;
    }
    eprintln!(
        "fetching commits for {} PR{} from {source}",
        incomplete.len(),
        if incomplete.len() == 1 { "" } else { "s" }
    );
    let head_refs: Vec<String> =
        incomplete.iter().map(|pr| format!("refs/pull/{}/head", pr.number)).collect();
    let mut args: Vec<&str> = vec!["fetch", "--quiet", source];
    args.extend(head_refs.iter().map(String::as_str));
    let _ = commands::git(workdir, &args);

    let mut missing: Vec<String> = incomplete
        .iter()
        .flat_map(|pr| wanted_of(pr))
        .filter(|oid| commit(store.repo(), oid).is_err())
        .collect();
    missing.sort_unstable();
    missing.dedup();
    if !missing.is_empty() {
        let mut args: Vec<&str> = vec!["fetch", "--quiet", source];
        args.extend(missing.iter().map(String::as_str));
        let _ = commands::git(workdir, &args);
    }
}

fn commit(repo: &gix::Repository, hex: &str) -> Result<ObjectId> {
    let oid = ObjectId::from_hex(hex.as_bytes())
        .map_err(|e| anyhow!("invalid commit id {hex:?}: {e}"))?;
    repo.find_commit(oid).with_context(|| format!("commit {hex} not present locally"))?;
    Ok(oid)
}

/// `owner/name` from a github.com remote URL, in its ssh, https, or
/// git-protocol spellings.
fn github_slug(url: &str) -> Option<(String, String)> {
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git://github.com/"))?;
    let rest = rest.strip_suffix(".git").unwrap_or(rest).trim_end_matches('/');
    let (owner, name) = rest.split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/'))
        .then(|| (owner.to_string(), name.to_string()))
}

/// A PR spec: a number (`123`, `#123`) or a full PR URL, which also names
/// the repository.
fn parse_spec(spec: &str) -> Option<(Option<(String, String)>, u64)> {
    if let Some(rest) = spec
        .strip_prefix("https://github.com/")
        .or_else(|| spec.strip_prefix("http://github.com/"))
    {
        let mut parts = rest.split('/');
        let owner = parts.next()?.to_string();
        let name = parts.next()?.to_string();
        if parts.next()? != "pull" {
            return None;
        }
        let number = parts.next()?.split(['#', '?']).next()?.parse().ok()?;
        return Some((Some((owner, name)), number));
    }
    spec.strip_prefix('#').unwrap_or(spec).parse().ok().map(|number| (None, number))
}

/// The selections both thread queries share, so the two can never drift.
const THREAD_FIELDS: &str = "\
id isResolved path subjectType
diffSide startDiffSide originalLine originalStartLine
resolvedBy{login databaseId}";

const COMMENT_FIELDS: &str = "\
id url body createdAt
author{login ... on User{databaseId} ... on Bot{databaseId}}
originalCommit{oid}
replyTo{id}";

/// One PR's review threads, paginated: the single-PR path, and the
/// fallback when a PR overflows its slot in the bulk sweep.
fn threads_query() -> String {
    format!(
        "query($owner:String!,$name:String!,$number:Int!,$cursor:String){{
  repository(owner:$owner,name:$name){{
    pullRequest(number:$number){{
      baseRefOid
      reviewThreads(first:50,after:$cursor){{
        pageInfo{{hasNextPage endCursor}}
        nodes{{
          {THREAD_FIELDS}
          comments(first:100){{
            pageInfo{{hasNextPage endCursor}}
            nodes{{ {COMMENT_FIELDS} }}
          }}
        }}
      }}
    }}
  }}
}}"
    )
}

/// The rest of an overlong thread's comments.
fn comments_query() -> String {
    format!(
        "query($id:ID!,$cursor:String){{
  node(id:$id){{
    ... on PullRequestReviewThread{{
      comments(first:100,after:$cursor){{
        pageInfo{{hasNextPage endCursor}}
        nodes{{ {COMMENT_FIELDS} }}
      }}
    }}
  }}
}}"
    )
}

/// The `--all` sweep: review threads for 50 PRs per request, oldest first.
/// The inner page sizes are deliberately modest — they keep the query cheap
/// against GitHub's rate-limit scoring, and a PR or thread that overflows
/// falls back to its own paginated fetch.
fn prs_query() -> String {
    format!(
        "query($owner:String!,$name:String!,$cursor:String){{
  repository(owner:$owner,name:$name){{
    pullRequests(first:50,after:$cursor,orderBy:{{field:CREATED_AT,direction:ASC}}){{
      totalCount
      pageInfo{{hasNextPage endCursor}}
      nodes{{
        number baseRefOid
        reviewThreads(first:20){{
          pageInfo{{hasNextPage endCursor}}
          nodes{{
            {THREAD_FIELDS}
            comments(first:50){{
              pageInfo{{hasNextPage endCursor}}
              nodes{{ {COMMENT_FIELDS} }}
            }}
          }}
        }}
      }}
    }}
  }}
}}"
    )
}

#[derive(Deserialize)]
struct PrsResponse {
    data: Option<PrsData>,
}
#[derive(Deserialize)]
struct PrsData {
    repository: Option<PrsRepo>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrsRepo {
    pull_requests: Option<PrConnection>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrConnection {
    total_count: u64,
    page_info: PageInfo,
    nodes: Vec<PrNode>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrNode {
    number: u64,
    base_ref_oid: String,
    review_threads: Page<ReviewThread>,
}

/// Review threads of every PR in the repository, swept in bulk with
/// progress on stderr — the scan is the run's network-bound phase.
fn fetch_all_threads(slug: &(String, String)) -> Result<Vec<PrThreads>> {
    let mut out: Vec<PrThreads> = Vec::new();
    let mut scanned = 0usize;
    let mut cursor: Option<String> = None;
    loop {
        let query = format!("query={}", prs_query());
        let owner = format!("owner={}", slug.0);
        let name = format!("name={}", slug.1);
        let mut args = vec!["api", "graphql", "-f", &query, "-f", &owner, "-f", &name];
        let cursor_field = cursor.as_ref().map(|c| format!("cursor={c}"));
        if let Some(field) = &cursor_field {
            args.extend(["-f", field]);
        }
        let response: PrsResponse =
            serde_json::from_str(&gh(&args)?).context("unexpected gh api graphql output")?;
        let page = response
            .data
            .and_then(|d| d.repository)
            .and_then(|r| r.pull_requests)
            .context("repository not found (or not visible to gh's login)")?;
        scanned += page.nodes.len();
        for pr in page.nodes {
            let threads = if pr.review_threads.page_info.has_next_page {
                // More threads than the sweep's slot: refetch this PR whole.
                match fetch_threads(slug, pr.number)? {
                    Some((_, threads)) => threads,
                    None => continue,
                }
            } else {
                let mut threads = pr.review_threads.nodes;
                for thread in &mut threads {
                    fetch_remaining_comments(thread)?;
                }
                threads
            };
            if !threads.is_empty() {
                out.push(PrThreads { number: pr.number, base_ref_oid: pr.base_ref_oid, threads });
            }
        }
        eprintln!(
            "scanned {scanned}/{} PRs ({} with review threads)",
            page.total_count,
            out.len()
        );
        if !page.page_info.has_next_page {
            break;
        }
        cursor = page.page_info.end_cursor;
    }
    Ok(out)
}

#[derive(Deserialize)]
struct ThreadsResponse {
    data: Option<ThreadsData>,
}
#[derive(Deserialize)]
struct ThreadsData {
    repository: Option<ThreadsRepo>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadsRepo {
    pull_request: Option<PullRequestData>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestData {
    base_ref_oid: String,
    review_threads: Page<ReviewThread>,
}
#[derive(Deserialize)]
struct CommentsResponse {
    data: Option<CommentsData>,
}
#[derive(Deserialize)]
struct CommentsData {
    node: Option<CommentsNode>,
}
#[derive(Deserialize)]
struct CommentsNode {
    comments: Option<Page<ReviewComment>>,
}

/// All review threads of a PR, both pagination levels walked. `None` when
/// the PR does not exist.
fn fetch_threads(
    slug: &(String, String),
    number: u64,
) -> Result<Option<(String, Vec<ReviewThread>)>> {
    let mut threads: Vec<ReviewThread> = Vec::new();
    let mut base_ref_oid: Option<String> = None;
    let mut cursor: Option<String> = None;
    loop {
        let query = format!("query={}", threads_query());
        let owner = format!("owner={}", slug.0);
        let name = format!("name={}", slug.1);
        let number_field = format!("number={number}");
        let mut args =
            vec!["api", "graphql", "-f", &query, "-f", &owner, "-f", &name, "-F", &number_field];
        let cursor_field = cursor.as_ref().map(|c| format!("cursor={c}"));
        if let Some(field) = &cursor_field {
            args.extend(["-f", field]);
        }
        let out = gh(&args)?;
        let response: ThreadsResponse =
            serde_json::from_str(&out).context("unexpected gh api graphql output")?;
        let Some(pr) = response
            .data
            .and_then(|d| d.repository)
            .and_then(|r| r.pull_request)
        else {
            return Ok(None);
        };
        base_ref_oid.get_or_insert(pr.base_ref_oid);
        let page = pr.review_threads;
        for mut thread in page.nodes {
            fetch_remaining_comments(&mut thread)?;
            threads.push(thread);
        }
        if !page.page_info.has_next_page {
            break;
        }
        cursor = page.page_info.end_cursor;
    }
    Ok(base_ref_oid.map(|base| (base, threads)))
}

/// Rarely needed: a thread with more than 100 comments pages the rest in.
fn fetch_remaining_comments(thread: &mut ReviewThread) -> Result<()> {
    while thread.comments.page_info.has_next_page {
        let Some(cursor) = thread.comments.page_info.end_cursor.clone() else {
            break;
        };
        let id = format!("id={}", thread.id);
        let query = format!("query={}", comments_query());
        let cursor_field = format!("cursor={cursor}");
        let out = gh(&["api", "graphql", "-f", &query, "-f", &id, "-f", &cursor_field])?;
        let response: CommentsResponse =
            serde_json::from_str(&out).context("unexpected gh api graphql output")?;
        let Some(page) = response.data.and_then(|d| d.node).and_then(|n| n.comments) else {
            break;
        };
        thread.comments.nodes.extend(page.nodes);
        thread.comments.page_info = page.page_info;
    }
    Ok(())
}

fn gh(args: &[&str]) -> Result<String> {
    let output = Command::new("gh")
        .args(args)
        .output()
        .context("failed to run gh (is the GitHub CLI installed?)")?;
    if !output.status.success() {
        bail!(
            "gh {} failed: {}",
            args.iter().take(2).copied().collect::<Vec<_>>().join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::{github_slug, parse_spec};

    #[test]
    fn slugs_parse_from_all_url_forms() {
        for url in [
            "git@github.com:o/r.git",
            "https://github.com/o/r",
            "https://github.com/o/r.git",
            "ssh://git@github.com/o/r.git",
        ] {
            assert_eq!(github_slug(url), Some(("o".into(), "r".into())), "{url}");
        }
        assert_eq!(github_slug("https://gitlab.com/o/r"), None);
    }

    #[test]
    fn specs_parse_as_numbers_or_urls() {
        assert_eq!(parse_spec("123"), Some((None, 123)));
        assert_eq!(parse_spec("#123"), Some((None, 123)));
        assert_eq!(
            parse_spec("https://github.com/o/r/pull/9#discussion_r1"),
            Some((Some(("o".into(), "r".into())), 9))
        );
        assert_eq!(parse_spec("https://github.com/o/r/issues/9"), None);
        assert_eq!(parse_spec("nope"), None);
    }
}
