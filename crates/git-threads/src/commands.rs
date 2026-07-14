use crate::reanchor::{self, Reanchor};
use crate::store::{Append, Batch, Integration, NewThread, Store, ThreadRecord};
use crate::ui::{self, Ui};
use anyhow::{Context, Result, anyhow, bail};
use git_threads_core::{
    Anchor, AnchorKind, Author, DiffRef, Event, EventId, EventKind, FoldedEvent, FoldedThread,
    GitOid, LineRange, ReanchorStatus, Side, SnippetTarget, ThreadId, Timestamp, derive_snippet,
    fold_thread,
};
use gix::ObjectId;
use std::path::Path;
use std::process::Command;

/// The fetch refspec `init` configures (SPEC.md §7.1): remote state lands in
/// the tracking ref, never directly on `refs/threads/data` — a direct mapping
/// would let any fetch clobber the local ref, orphaning unpublished events.
fn fetch_refspec(remote: &str) -> String {
    format!("+refs/threads/data:{}", Store::tracking_ref(remote))
}

/// Configure this clone (SPEC.md §7.1): add the additive fetch refspec, then
/// attempt an initial fetch. No push refspec is written — publishing pushes
/// explicitly to avoid replacing git's default push behavior.
pub fn init(store: &Store, remote: &str) -> Result<()> {
    let workdir = store
        .repo()
        .workdir()
        .context("git-threads init requires a non-bare repository")?
        .to_owned();
    let remotes = git(&workdir, &["remote"])?;
    if !remotes.lines().any(|line| line == remote) {
        bail!("remote {remote:?} not found (git remote add it first, or pass --remote)");
    }
    let refspec = fetch_refspec(remote);
    let key = format!("remote.{remote}.fetch");
    let existing = git(&workdir, &["config", "--get-all", &key]).unwrap_or_default();
    if existing.lines().any(|line| line == refspec) {
        println!("{key} already includes {refspec}");
    } else {
        git(&workdir, &["config", "--add", &key, &refspec])?;
        println!("configured {key} += {refspec}");
    }
    match git(&workdir, &["fetch", remote]) {
        Ok(_) => {
            println!("fetched from {remote}");
            if let Some(tip) = store.tracking_tip(remote)? {
                report_integration(store.integrate(tip)?, remote);
            }
        }
        Err(err) => eprintln!("warning: initial fetch from {remote} failed: {err:#}"),
    }
    Ok(())
}

/// Remove git-threads from this clone: the fetch refspecs on every remote and
/// everything under `refs/threads/` (data, drafts, tracking refs). The
/// remote's data is untouched — `init` starts over from it. Refuses to orphan
/// unshared work (drafts, or local events on no remote) without `force`.
pub fn deinit(store: &Store, force: bool) -> Result<()> {
    let workdir = workdir(store)?;
    if !force {
        if store.drafts_tip()?.is_some() {
            bail!(
                "there are drafted events; `commit` and `push` to share them, \
                 `discard --all` to drop them, or pass --force"
            );
        }
        if let Some(local) = store.tip()? {
            let mut published = false;
            for remote in git(&workdir, &["remote"])?.lines() {
                if let Some(tracking) = store.tracking_tip(remote)?
                    && store.is_ancestor(local, tracking)?
                {
                    published = true;
                    break;
                }
            }
            if !published {
                bail!(
                    "local threads history has events on no remote; \
                     `push` to share them, or pass --force"
                );
            }
        }
    }
    for remote in git(&workdir, &["remote"])?.lines() {
        let key = format!("remote.{remote}.fetch");
        let refspec = fetch_refspec(remote);
        let existing = git(&workdir, &["config", "--get-all", &key]).unwrap_or_default();
        if existing.lines().any(|line| line == refspec) {
            git(&workdir, &["config", "--fixed-value", "--unset-all", &key, &refspec])?;
            println!("removed {refspec} from {key}");
        }
    }
    let refs = git(&workdir, &["for-each-ref", "--format=%(refname)", "refs/threads/"])?;
    let mut deleted = 0;
    for name in refs.lines() {
        git(&workdir, &["update-ref", "-d", name])?;
        deleted += 1;
    }
    println!(
        "deleted {deleted} ref{} under refs/threads/; the remote's data is untouched \
         (git threads init to start again)",
        if deleted == 1 { "" } else { "s" }
    );
    Ok(())
}

fn report_integration(integration: Integration, remote: &str) {
    match integration {
        Integration::UpToDate => println!("already up to date"),
        Integration::Initialized => println!("initialized from {remote}"),
        Integration::FastForwarded => println!("fast-forwarded to {remote}"),
        Integration::Merged => println!("merged threads from {remote}"),
    }
}

/// Fold already-fetched threads data into the local ref: integrate every
/// `refs/threads/remotes/*/data` tracking ref (SPEC.md §7.2 step 2, run
/// opportunistically before every command). Purely local — after `init`,
/// plain `git fetch` delivers the data and the next command picks it up
/// here. Safe by construction: integration never conflicts and never
/// discards local events.
pub fn integrate_fetched(store: &Store) -> Result<()> {
    const PREFIX: &str = "refs/threads/remotes/";
    let repo = store.repo();
    for reference in repo.references()?.prefixed(PREFIX)?.flatten() {
        let name = reference.name().as_bstr().to_string();
        let Some(remote) = name.strip_prefix(PREFIX).and_then(|r| r.strip_suffix("/data")) else {
            continue;
        };
        let Some(tip) = reference.try_id().map(|id| id.detach()) else { continue };
        match store.integrate(tip)? {
            Integration::UpToDate => {}
            // On stderr: a side note to whatever command is running.
            Integration::Initialized => eprintln!("threads: initialized from {remote}"),
            Integration::FastForwarded => eprintln!("threads: fast-forwarded to {remote}"),
            Integration::Merged => eprintln!("threads: merged data from {remote}"),
        }
    }
    Ok(())
}

pub struct CommentOpts {
    /// What is being discussed: a commit-ish, a range (`A..B`, `A...B`), or —
    /// alone — a file path or `path:lines` of HEAD's change.
    pub target: Option<String>,
    /// File within the target diff, optionally carrying lines (`path:120-128`).
    pub file: Option<String>,
    pub message: String,
    pub side: Side,
}

/// Create a new thread anchored to a commit, file, or line range (SPEC.md §3).
pub fn comment(store: &Store, opts: &CommentOpts) -> Result<ThreadId> {
    let repo = store.repo();
    let Target { base, head, file } =
        resolve_target(repo, opts.target.as_deref(), opts.file.as_deref(), opts.side)?;

    let (kind, path, lines, blob) = match &file {
        None => (AnchorKind::Commit, None, None, None),
        Some(spec) => {
            let (file, suffix) = split_line_suffix(spec);
            // The blob is resolved on the anchor's side: `new` reads the head
            // tree, `old` the base tree (e.g. comments on deleted lines).
            let side_commit = match opts.side {
                Side::New => head,
                Side::Old => base,
            };
            let (blob_id, line_count) = blob_at(repo, side_commit, file)?.with_context(|| {
                format!(
                    "{file:?} not found in the {} version ({})",
                    match opts.side {
                        Side::New => "new",
                        Side::Old => "old",
                    },
                    side_commit
                )
            })?;
            let lines = suffix
                .map(|spec| {
                    let range = parse_lines(spec)?;
                    if range.end as usize > line_count {
                        bail!("lines {spec} are out of range: {file:?} has {line_count} lines");
                    }
                    Ok(range)
                })
                .transpose()?;
            // A comment on a diff must be about that diff; an empty diff
            // (base == head) is the deliberate snapshot-annotation spelling.
            if base != head {
                ensure_in_diff(repo, base, head, file, opts.side, lines)?;
            }
            let kind = if lines.is_some() { AnchorKind::Range } else { AnchorKind::File };
            let blob = GitOid::from_hex(blob_id.to_string())?;
            (kind, Some(file.to_string()), lines, Some(blob))
        }
    };

    let anchor = Anchor {
        v: 1,
        kind,
        diff: DiffRef {
            base: GitOid::from_hex(base.to_string())?,
            head: GitOid::from_hex(head.to_string())?,
        },
        path,
        old_path: None,
        side: file.as_ref().map(|_| opts.side),
        lines,
        blob,
        cols: None,
        extra: Default::default(),
    };
    let root = new_event(repo, EventKind::Comment, |e| {
        e.body = Some(wrap_message(&opts.message));
    })?;
    let thread_id = root.id()?;

    store.draft(&Batch {
        new_threads: vec![NewThread { anchor, root, events: vec![] }],
        appends: vec![],
    })?;
    let ui = Ui::auto();
    println!(
        "drafted thread {} {}",
        ui.yellow(short(&thread_id)),
        ui.dim("(commit and push to share)")
    );
    Ok(thread_id)
}

/// The diff a comment targets, and the file within it, if any.
pub struct Target {
    pub base: ObjectId,
    pub head: ObjectId,
    pub file: Option<String>,
}

/// Sort out what `comment`'s positionals refer to, the way git disambiguates
/// revs from paths. The first positional names the diff — a commit (its
/// first-parent change) or a range — and, when it's the only one, may instead
/// be a file (path or path:lines) of HEAD's change. The second positional is
/// always a file.
pub fn resolve_target(
    repo: &gix::Repository,
    target: Option<&str>,
    file: Option<&str>,
    side: Side,
) -> Result<Target> {
    let Some(spec) = target else {
        let (base, head) = resolve_diff(repo, "HEAD")?;
        return Ok(Target { base, head, file: file.map(String::from) });
    };
    if file.is_some() {
        // The second positional names the file, so the first can only be the diff.
        let (base, head) = resolve_diff(repo, spec)?;
        return Ok(Target { base, head, file: file.map(String::from) });
    }
    if spec.contains("..") {
        // Anything that looks like a range is one, as in git.
        let (base, head) = resolve_diff(repo, spec)?;
        return Ok(Target { base, head, file: None });
    }
    let is_commit = resolve_commit(repo, spec).is_ok();
    let (path, _) = split_line_suffix(spec);
    // A lone file positional means HEAD's change, so its blob lives in the
    // tree of the anchor's side: HEAD for `new`, HEAD's parent for `old`.
    let side_rev = match side {
        Side::New => "HEAD",
        Side::Old => "HEAD^",
    };
    let is_file = match resolve_commit(repo, side_rev) {
        Ok(commit) => blob_at(repo, commit, path)?.is_some(),
        Err(_) => false,
    };
    match (is_commit, is_file) {
        (true, true) => {
            bail!("{spec:?} is both a commit and a file; write `comment HEAD {spec}` to mean the file")
        }
        (true, false) => {
            let (base, head) = resolve_diff(repo, spec)?;
            Ok(Target { base, head, file: None })
        }
        (false, true) => {
            let (base, head) = resolve_diff(repo, "HEAD")?;
            Ok(Target { base, head, file: Some(spec.to_string()) })
        }
        (false, false) => bail!("{spec:?} is neither a commit nor a file in HEAD"),
    }
}

/// How far outside a hunk a comment may still sit — the unified-diff context
/// a reviewer sees, and the same width snippets carry.
const HUNK_CONTEXT: u32 = 3;

/// Reject comments that don't touch their diff: the file must be changed
/// between `base` and `head`, and `lines` (if any) must overlap a hunk on
/// `side`, give or take [`HUNK_CONTEXT`] lines.
fn ensure_in_diff(
    repo: &gix::Repository,
    base: ObjectId,
    head: ObjectId,
    path: &str,
    side: Side,
    lines: Option<LineRange>,
) -> Result<()> {
    let (base_hex, head_hex) = (base.to_string(), head.to_string());
    let diff = git(
        repo.git_dir(),
        &["diff", "--unified=0", &base_hex, &head_hex, "--", path],
    )?;
    let shown = format!("{}..{}", &base_hex[..12], &head_hex[..12]);
    let hint = format!(
        "comment on {0}..{0} (an empty diff) to annotate the file as it stands",
        &head_hex[..12]
    );
    if diff.is_empty() {
        bail!("{path:?} is unchanged in {shown}; {hint}");
    }
    let Some(lines) = lines else { return Ok(()) };
    let spans = hunk_spans(&diff, side);
    let in_context = |&(start, len): &(u32, u32)| {
        let lo = start.saturating_sub(HUNK_CONTEXT).max(1);
        let hi = start + len.max(1) - 1 + HUNK_CONTEXT;
        lines.start <= hi && lines.end >= lo
    };
    if !spans.iter().any(in_context) {
        let changed = spans
            .iter()
            .map(|&(start, len)| match len {
                0 | 1 => start.to_string(),
                _ => format!("{start}-{}", start + len - 1),
            })
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "lines {}-{} of {path:?} are outside the change {shown} (changed: {changed}); {hint}",
            lines.start,
            lines.end
        );
    }
    Ok(())
}

/// (start, length) of each hunk on `side`, in 1-based file coordinates, from
/// unified-diff `@@ -a,b +c,d @@` headers. A zero length marks the point next
/// to lines added or removed on the other side.
fn hunk_spans(diff: &str, side: Side) -> Vec<(u32, u32)> {
    diff.lines()
        .filter(|line| line.starts_with("@@"))
        .filter_map(|line| {
            let mut fields = line.split_whitespace().skip(1);
            let token = match side {
                Side::Old => fields.next()?.strip_prefix('-')?,
                Side::New => fields.nth(1)?.strip_prefix('+')?,
            };
            let (start, len) = match token.split_once(',') {
                Some((start, len)) => (start.parse().ok()?, len.parse().ok()?),
                None => (token.parse().ok()?, 1),
            };
            Some((start, len))
        })
        .collect()
}

/// Resolve a diff spec into (base, head): `A..B` diffs the two commits,
/// `A...B` diffs B against merge-base(A, B) — both as in `git diff` — and a
/// bare commit means its first-parent change. Empty range sides mean HEAD.
fn resolve_diff(repo: &gix::Repository, spec: &str) -> Result<(ObjectId, ObjectId)> {
    let end = |s: &str| resolve_commit(repo, if s.is_empty() { "HEAD" } else { s });
    if let Some((a, b)) = spec.split_once("...") {
        let (a, b) = (end(a)?, end(b)?);
        let base = repo
            .merge_base(a, b)
            .with_context(|| format!("no merge base between {a} and {b}"))?
            .detach();
        Ok((base, b))
    } else if let Some((a, b)) = spec.split_once("..") {
        Ok((end(a)?, end(b)?))
    } else {
        let head = resolve_commit(repo, spec)?;
        let base = repo
            .find_commit(head)?
            .parent_ids()
            .next()
            .map(|id| id.detach())
            .with_context(|| {
                format!("{spec} has no parent; name a range instead (<base>..{spec})")
            })?;
        Ok((base, head))
    }
}

/// Reply to a thread, or to a specific message in one: the prefix may name
/// the thread or any comment/reply in it, and `in_reply_to` records the
/// named event (SPEC.md §2.1 allows replying to any event in the thread).
pub fn reply(store: &Store, prefix: &str, message: &str) -> Result<EventId> {
    let (thread, target) = find_message(store, prefix)?;
    let event = new_event(store.repo(), EventKind::Reply, |e| {
        e.body = Some(wrap_message(message));
        e.in_reply_to = Some(target.id.clone());
    })?;
    let event_id = event.id()?;
    store.draft(&Batch {
        new_threads: vec![],
        appends: vec![Append { thread: thread.id.clone(), events: vec![event] }],
    })?;
    let ui = Ui::auto();
    println!(
        "drafted reply {} to thread {}",
        ui.yellow(short(&event_id)),
        ui.yellow(short(&thread.id))
    );
    Ok(event_id)
}

/// Edit a comment or reply: append an `edit` event superseding the current
/// tip of the target's edit chain (SPEC.md §2.1). Only the author's edits
/// take effect in the fold, so anyone else's are rejected here.
pub fn edit(store: &Store, event_prefix: &str, message: &str) -> Result<EventId> {
    let (thread, target) = find_editable(store, event_prefix)?;
    let event = new_event(store.repo(), EventKind::Edit, |e| {
        e.body = Some(wrap_message(message));
        e.supersedes = Some(target.chain_tip.clone());
    })?;
    let event_id = event.id()?;
    store.draft(&Batch {
        new_threads: vec![],
        appends: vec![Append { thread: thread.id.clone(), events: vec![event] }],
    })?;
    let ui = Ui::auto();
    println!(
        "drafted edit of {} {}",
        ui.yellow(short(&target.id)),
        ui.dim(format_args!("(edit event {})", short(&event_id)))
    );
    Ok(event_id)
}

/// The current text of a comment or reply, for seeding the editor when
/// `edit` runs without --message. Validates like `edit` does, so the editor
/// never opens for a message that can't be edited.
pub fn current_body(store: &Store, event_prefix: &str) -> Result<String> {
    let (_, target) = find_editable(store, event_prefix)?;
    Ok(target.effective_body.unwrap_or_default())
}

/// Find a message and check it's editable: written by us and not retracted.
fn find_editable(store: &Store, event_prefix: &str) -> Result<(ThreadRecord, FoldedEvent)> {
    let (thread, target) = find_message(store, event_prefix)?;
    let me = identity(store.repo())?;
    if target.event.author.email != me.email {
        bail!(
            "{} was written by {} <{}>; only the author can edit it",
            short(&target.id),
            target.event.author.name,
            target.event.author.email
        );
    }
    if target.retracted {
        bail!("{} is retracted; nothing to edit", short(&target.id));
    }
    Ok((thread, target))
}

/// Retract a comment or reply with a `delete` tombstone (SPEC.md §2.1). The
/// content stays in history; the fold marks the event retracted.
pub fn delete(store: &Store, event_prefix: &str) -> Result<EventId> {
    let (thread, target) = find_message(store, event_prefix)?;
    let me = identity(store.repo())?;
    if target.event.author.email != me.email {
        bail!(
            "{} was written by {} <{}>; only the author can retract it",
            short(&target.id),
            target.event.author.name,
            target.event.author.email
        );
    }
    if target.retracted {
        bail!("{} is already retracted", short(&target.id));
    }
    let event = new_event(store.repo(), EventKind::Delete, |e| {
        e.supersedes = Some(target.id.clone());
    })?;
    let event_id = event.id()?;
    store.draft(&Batch {
        new_threads: vec![],
        appends: vec![Append { thread: thread.id.clone(), events: vec![event] }],
    })?;
    println!("drafted retraction of {}", Ui::auto().yellow(short(&target.id)));
    Ok(event_id)
}

/// Mark a thread resolved (or reopen it). The prefix may name the thread or
/// any comment/reply in it.
pub fn resolve(store: &Store, prefix: &str, resolved: bool) -> Result<()> {
    let (thread, _) = find_message(store, prefix)?;
    let event = new_event(store.repo(), EventKind::Resolve, |e| {
        e.resolved = Some(resolved);
    })?;
    store.draft(&Batch {
        new_threads: vec![],
        appends: vec![Append { thread: thread.id.clone(), events: vec![event] }],
    })?;
    let ui = Ui::auto();
    println!(
        "thread {} {} {}",
        ui.yellow(short(&thread.id)),
        if resolved { "resolved" } else { "reopened" },
        ui.dim("(draft)")
    );
    Ok(())
}

pub struct ListOpts {
    /// Only threads on this change — or, alone, on this path (grammar
    /// mirrors `comment`).
    pub target: Option<String>,
    pub file: Option<String>,
    /// Commit to re-anchor threads against.
    pub at: String,
    /// Keep only this resolution state.
    pub resolved: Option<bool>,
    pub oneline: bool,
    pub patch: bool,
    /// git log's -n: stop after this many threads.
    pub max_count: Option<usize>,
    /// Substring of the root author's name or email, case-insensitive.
    pub author: Option<String>,
    /// Boundaries on the root comment's date, git log style.
    pub since: Option<String>,
    pub until: Option<String>,
}

/// List threads in the current snapshot with their folded state and their
/// re-anchor status against `at` (SPEC.md §4.2). `target`/`file` narrow to
/// one change and one path (grammar mirrors `comment`, except a lone file
/// filters across all changes).
pub fn list(store: &Store, opts: &ListOpts) -> Result<()> {
    let repo = store.repo();
    let mut threads = store.threads()?;
    let (target, file) = match (opts.target.as_deref(), opts.file.as_deref()) {
        (Some(spec), None) => resolve_list_filters(repo, &threads, spec)?,
        (target, file) => (target.map(String::from), file.map(String::from)),
    };
    if let Some(spec) = &target {
        let (base, head) = resolve_diff(repo, spec)?;
        let commits = range_commits(repo, base, head)?;
        threads.retain(|t| commits.contains(t.anchor.diff.head.as_str()));
    }
    if let Some(spec) = &file {
        let (path, lines) = split_line_suffix(spec);
        let lines = lines.map(parse_lines).transpose()?;
        threads.retain(|t| anchor_matches(&t.anchor, path, lines));
    }
    let at_commit = resolve_commit(repo, &opts.at)?;
    let since = opts.since.as_deref().map(parse_date).transpose()?;
    let until = opts.until.as_deref().map(parse_date).transpose()?;
    // Newest first, by earliest event timestamp.
    threads.sort_by_key(|t| std::cmp::Reverse(t.events.iter().map(|(_, e)| e.ts.clone()).min()));
    let ui = Ui::auto();
    let mut shown = 0;
    for thread in threads {
        let folded = fold_thread(thread.events.clone());
        if opts.resolved.is_some_and(|want| folded.resolved != want) {
            continue;
        }
        let root = folded.events.first();
        if let Some(pattern) = &opts.author {
            let author = root
                .map(|r| format!("{} <{}>", r.event.author.name, r.event.author.email))
                .unwrap_or_default();
            if !author.to_lowercase().contains(&pattern.to_lowercase()) {
                continue;
            }
        }
        if since.is_some() || until.is_some() {
            let Some(ts) = root.and_then(|r| r.event.ts.as_str().parse::<jiff::Timestamp>().ok())
            else {
                continue;
            };
            if since.is_some_and(|s| ts < s) || until.is_some_and(|u| ts > u) {
                continue;
            }
        }
        if opts.max_count.is_some_and(|n| shown >= n) {
            break;
        }
        let placement = reanchor::reanchor(store, &thread.anchor, at_commit)?;
        let mut deco =
            vec![if folded.resolved { ui.magenta("resolved") } else { ui.green("open") }];
        if matches!(placement, Reanchor::Outdated) {
            deco.push(ui.red("outdated"));
        }
        if folded.events.len() > 1 {
            deco.push(ui.dim(format_args!("{} messages", folded.events.len())));
        }
        if !thread.drafts.is_empty() {
            let n = thread.drafts.len();
            deco.push(ui.yellow(format_args!("{n} draft{}", if n == 1 { "" } else { "s" })));
        }
        let decoration = decorate(ui, &deco);
        let location = match (&thread.anchor.path, &thread.anchor.lines) {
            (Some(path), Some(lines)) => format!("{path}:{}-{}", lines.start, lines.end),
            (Some(path), None) => path.clone(),
            _ if opts.oneline => format!("commit {}", &thread.anchor.diff.head.as_str()[..12]),
            _ => "whole change".to_string(),
        };
        if opts.oneline {
            let drift = match &placement {
                Reanchor::WholeCommit
                | Reanchor::Located { status: ReanchorStatus::Exact, .. }
                | Reanchor::Outdated => String::new(),
                Reanchor::Located { path, lines, status } => {
                    let lines =
                        lines.map(|l| format!(":{}-{}", l.start, l.end)).unwrap_or_default();
                    ui.yellow(format_args!(" → {path}{lines} ({status})"))
                }
            };
            let title = root
                .and_then(|r| r.effective_body.as_deref())
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("");
            println!("{} {decoration} {location}{drift}  {title}", ui.yellow(short(&thread.id)));
            if opts.patch {
                print!("{}", placement_snippet(ui, store, &thread.anchor, &placement, at_commit)?);
            }
        } else {
            if shown > 0 {
                println!();
            }
            println!("{} {decoration}", ui.yellow(format_args!("thread {}", thread.id)));
            if let Some(root) = root {
                println!(
                    "Author: {} {}",
                    ui.bold(&root.event.author.name),
                    ui.dim(format_args!("<{}>", root.event.author.email))
                );
                println!("Date:   {}", ui::date(&root.event.ts));
            }
            println!(
                "Anchor: {} {}",
                ui.bold(&location),
                ui.dim(format_args!(
                    "of {}..{}",
                    &thread.anchor.diff.base.as_str()[..12],
                    &thread.anchor.diff.head.as_str()[..12]
                ))
            );
            match &placement {
                Reanchor::WholeCommit
                | Reanchor::Located { status: ReanchorStatus::Exact, .. }
                | Reanchor::Outdated => {}
                Reanchor::Located { path, lines, status } => {
                    let lines =
                        lines.map(|l| format!(":{}-{}", l.start, l.end)).unwrap_or_default();
                    println!(
                        "Now:    {} {}",
                        ui.bold(format_args!("{path}{lines}")),
                        ui.yellow(format_args!("({status})"))
                    );
                }
            }
            let body = match root {
                Some(root) if root.retracted => ui.dim("[retracted]"),
                Some(root) => root.effective_body.clone().unwrap_or_default(),
                None => String::new(),
            };
            if !body.is_empty() {
                println!();
                for line in body.lines() {
                    println!("    {line}");
                }
            }
            if opts.patch {
                print!("{}", placement_snippet(ui, store, &thread.anchor, &placement, at_commit)?);
            }
        }
        shown += 1;
    }
    if shown == 0 {
        println!("{}", ui.dim("no threads"));
    }
    Ok(())
}

/// A git-log-style decoration list — `(open, 2 messages, 1 draft)` — with
/// dim punctuation around already-colored parts.
fn decorate(ui: Ui, parts: &[String]) -> String {
    format!("{}{}{}", ui.dim("("), parts.join(&ui.dim(", ")), ui.dim(")"))
}

/// Parse a --since/--until boundary, covering the shapes git dates are
/// usually written in: an ISO timestamp, a date or datetime (local time),
/// "yesterday", or "<n> <unit> ago" (seconds through years).
fn parse_date(spec: &str) -> Result<jiff::Timestamp> {
    let spec = spec.trim();
    if let Ok(ts) = spec.parse::<jiff::Timestamp>() {
        return Ok(ts);
    }
    let tz = jiff::tz::TimeZone::system();
    if let Ok(dt) = spec.parse::<jiff::civil::DateTime>() {
        return Ok(dt.to_zoned(tz)?.timestamp());
    }
    if let Ok(date) = spec.parse::<jiff::civil::Date>() {
        return Ok(date.to_zoned(tz)?.timestamp());
    }
    let now = jiff::Zoned::now();
    if spec == "yesterday" {
        return Ok(now.checked_sub(jiff::Span::new().days(1))?.timestamp());
    }
    let words: Vec<&str> = spec.split_whitespace().collect();
    if let [n, unit, rest @ ..] = words.as_slice()
        && (rest.is_empty() || rest == ["ago"])
        && let Ok(n) = n.parse::<i64>()
    {
        let span = match unit.trim_end_matches('s') {
            "second" => jiff::Span::new().seconds(n),
            "minute" => jiff::Span::new().minutes(n),
            "hour" => jiff::Span::new().hours(n),
            "day" => jiff::Span::new().days(n),
            "week" => jiff::Span::new().weeks(n),
            "month" => jiff::Span::new().months(n),
            "year" => jiff::Span::new().years(n),
            _ => bail!("cannot parse date {spec:?}"),
        };
        return Ok(now.checked_sub(span)?.timestamp());
    }
    bail!("cannot parse date {spec:?} (try ISO like 2026-07-01, or \"2 weeks ago\")")
}

/// The code snippet for a thread at `target`: from the re-anchored location
/// when there is one, from the anchor's own blob when outdated (SPEC.md §4.2
/// step 4). Empty for whole-change and whole-file anchors.
fn placement_snippet(
    ui: Ui,
    store: &Store,
    anchor: &Anchor,
    placement: &Reanchor,
    target: ObjectId,
) -> Result<String> {
    match placement {
        Reanchor::Located { path, lines: Some(lines), .. } => {
            match reanchor::blob_at(store.repo(), target, path)? {
                Some(blob) => {
                    Ok(render_snippet(ui, &reanchor::blob_content(store.repo(), blob)?, *lines))
                }
                None => Ok(String::new()),
            }
        }
        _ => match (anchor.lines, &anchor.blob) {
            (Some(lines), Some(blob)) => {
                let blob_id = ObjectId::from_hex(blob.as_str().as_bytes())?;
                Ok(render_snippet(ui, &reanchor::blob_content(store.repo(), blob_id)?, lines))
            }
            _ => Ok(String::new()),
        },
    }
}

/// Sort out `list`'s lone positional: a change (commit or range) or a path
/// filter. Same rev-vs-path disambiguation as `comment`, but a lone path
/// means "across all changes" — so anchored paths count as paths even when
/// the file no longer exists — and `./` forces the path reading, as in git.
fn resolve_list_filters(
    repo: &gix::Repository,
    threads: &[ThreadRecord],
    spec: &str,
) -> Result<(Option<String>, Option<String>)> {
    if spec.contains("..") {
        return Ok((Some(spec.to_string()), None));
    }
    if let Some(path) = spec.strip_prefix("./") {
        return Ok((None, Some(path.to_string())));
    }
    let is_commit = resolve_commit(repo, spec).is_ok();
    let (path, _) = split_line_suffix(spec);
    let head = resolve_commit(repo, "HEAD")?;
    // A file or directory in HEAD's tree, or any anchored path.
    let is_file = repo.find_commit(head)?.tree()?.lookup_entry_by_path(path)?.is_some()
        || threads.iter().any(|t| anchor_matches(&t.anchor, path, None));
    match (is_commit, is_file) {
        (true, true) => {
            bail!("{spec:?} is both a commit and a path; write ./{spec} to mean the path")
        }
        (true, false) => Ok((Some(spec.to_string()), None)),
        (false, true) => Ok((None, Some(spec.to_string()))),
        (false, false) => bail!("{spec:?} is neither a commit nor a path with threads"),
    }
}

/// Whether a thread's anchor is on `path` (the file itself, or under it as a
/// directory) and, when a line range is given, overlaps it. Whole-file
/// anchors overlap any lines.
fn anchor_matches(anchor: &Anchor, path: &str, lines: Option<LineRange>) -> bool {
    let path = path.trim_end_matches('/');
    let on_path = anchor
        .path
        .as_deref()
        .is_some_and(|p| p == path || p.strip_prefix(path).is_some_and(|r| r.starts_with('/')));
    on_path
        && lines.is_none_or(|want| {
            anchor.lines.is_none_or(|have| want.start <= have.end && want.end >= have.start)
        })
}

/// The commits making up `base..head` — the set a thread's anchored head must
/// fall in to belong to that change. An empty diff is just its own commit.
fn range_commits(
    repo: &gix::Repository,
    base: ObjectId,
    head: ObjectId,
) -> Result<std::collections::HashSet<String>> {
    if base == head {
        return Ok(std::iter::once(head.to_string()).collect());
    }
    let out = git(repo.git_dir(), &["rev-list", &format!("{base}..{head}")])?;
    Ok(out.lines().map(str::to_string).collect())
}

/// Render a thread: anchor location, re-anchor placement on `at` (SPEC.md
/// §4.2), code context, and the folded conversation. The context comes from
/// the re-anchored location when there is one, from the anchor's own diff
/// when outdated (§4.2 step 4). The prefix may name the thread or any
/// comment/reply in it.
pub fn show(store: &Store, prefix: &str, at: &str) -> Result<()> {
    let (thread, _) = find_message(store, prefix)?;
    print!("{}", render_thread(Ui::auto(), store, &thread, at)?);
    Ok(())
}

/// The full thread as `show` prints it: header, re-anchor placement, code
/// context, conversation.
fn render_thread(ui: Ui, store: &Store, thread: &ThreadRecord, at: &str) -> Result<String> {
    use std::fmt::Write;
    let folded = fold_thread(thread.events.clone());
    let anchor = &thread.anchor;
    let mut out = String::new();

    let target = resolve_commit(store.repo(), at)?;
    let target_short = &target.to_string()[..12];
    let placement = reanchor::reanchor(store, anchor, target)?;

    let mut deco = vec![if folded.resolved { ui.magenta("resolved") } else { ui.green("open") }];
    if matches!(placement, Reanchor::Outdated) {
        deco.push(ui.red("outdated"));
    }
    writeln!(
        out,
        "{} {}",
        ui.yellow(format_args!("thread {}", thread.id)),
        decorate(ui, &deco)
    )
    .unwrap();
    let side = match anchor.side {
        Some(Side::Old) => ui.dim(" (old side)"),
        _ => String::new(),
    };
    let location = match (&anchor.path, &anchor.lines) {
        (Some(path), Some(lines)) => format!("{path}:{}-{}", lines.start, lines.end),
        (Some(path), None) => path.clone(),
        _ => "whole change".to_string(),
    };
    writeln!(
        out,
        "Anchor: {}{side} {}",
        ui.bold(location),
        ui.dim(format_args!(
            "of {}..{}",
            &anchor.diff.base.as_str()[..12],
            &anchor.diff.head.as_str()[..12]
        ))
    )
    .unwrap();

    match &placement {
        Reanchor::WholeCommit => {}
        Reanchor::Located { path, lines, status } => {
            // Only news when the thread drifted: an exact hit at the anchor's
            // own location goes without saying.
            let exact = matches!(status, ReanchorStatus::Exact);
            if !exact || Some(path) != anchor.path.as_ref() || *lines != anchor.lines {
                let lines = lines.map(|l| format!(":{}-{}", l.start, l.end)).unwrap_or_default();
                let status = format!("({status})");
                let status = if exact { ui.green(status) } else { ui.yellow(status) };
                writeln!(
                    out,
                    "Now:    {} at {target_short} {status}",
                    ui.bold(format_args!("{path}{lines}"))
                )
                .unwrap();
            }
        }
        Reanchor::Outdated => {
            writeln!(
                out,
                "Now:    {} at {target_short} {}",
                ui.red("no match"),
                ui.dim("— showing the anchor's own context")
            )
            .unwrap();
        }
    }

    out.push_str(&placement_snippet(ui, store, anchor, &placement, target)?);
    out.push_str(&render_conversation(ui, thread, &folded));
    Ok(out)
}

/// The conversation as `show` prints it: one block per message, blank-line
/// separated, starting with a blank line.
fn render_conversation(ui: Ui, thread: &ThreadRecord, folded: &FoldedThread) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for event in &folded.events {
        let kind = if event.event.kind == EventKind::Reply { "reply" } else { "comment" };
        let edited = if event.edited { format!(" {}", ui.dim("(edited)")) } else { String::new() };
        let draft = if thread.drafts.contains(&event.id) {
            format!(" {}", ui.yellow("(draft)"))
        } else {
            String::new()
        };
        writeln!(
            out,
            "\n{}  {} {}  {}{edited}{draft}",
            ui.yellow(format_args!("{kind:<7} {}", short(&event.id))),
            ui.bold(&event.event.author.name),
            ui.dim(format_args!("<{}>", event.event.author.email)),
            ui.dim(ui::date(&event.event.ts)),
        )
        .unwrap();
        if event.retracted {
            writeln!(out, "    {}", ui.dim("[retracted]")).unwrap();
        } else if let Some(body) = &event.effective_body {
            for line in body.lines() {
                writeln!(out, "    {line}").unwrap();
            }
        }
    }
    out
}

/// The full thread as `show` renders it, uncolored, for the editor hint
/// when `reply` runs without --message.
pub fn thread_preview(store: &Store, prefix: &str) -> Result<String> {
    let (thread, _) = find_message(store, prefix)?;
    Ok(format!("replying to:\n\n{}", render_thread(Ui::plain(), store, &thread, "HEAD")?))
}

/// Discard one drafted event before publishing. Discarding a drafted
/// thread's root discards the whole draft thread.
pub fn discard(store: &Store, prefix: &str) -> Result<()> {
    let mut matches: Vec<(ThreadId, EventId)> = Vec::new();
    for thread in store.threads()? {
        for event_id in &thread.drafts {
            if event_id.as_str().starts_with(prefix) {
                matches.push((thread.id.clone(), event_id.clone()));
            }
        }
    }
    let (thread_id, event_id) = match matches.len() {
        0 => bail!("no draft matches {prefix:?}"),
        1 => matches.remove(0),
        n => bail!(
            "{prefix:?} is ambiguous ({n} drafts): {}",
            matches.iter().map(|(_, e)| short(e)).collect::<Vec<_>>().join(", ")
        ),
    };
    let removed = store.discard_draft(&thread_id, &event_id)?;
    let ui = Ui::auto();
    if removed > 1 || event_id == thread_id {
        println!(
            "discarded draft thread {} {}",
            ui.yellow(short(&thread_id)),
            ui.dim(format_args!("({removed} event{})", if removed == 1 { "" } else { "s" })),
        );
    } else {
        println!("discarded draft {}", ui.yellow(short(&event_id)));
    }
    Ok(())
}

/// Discard every unpublished draft.
pub fn discard_all(store: &Store) -> Result<()> {
    let removed = store.discard_all_drafts()?;
    match removed {
        0 => println!("no drafts"),
        n => println!("discarded {n} draft{}", if n == 1 { "" } else { "s" }),
    }
    Ok(())
}

/// A marked, line-numbered snippet of `lines` out of `content`, preceded by
/// a blank line. Context lines are dimmed so the target lines carry the eye.
fn render_snippet(ui: Ui, content: &str, lines: LineRange) -> String {
    use std::fmt::Write;
    let Some(snippet) = derive_snippet(content, lines) else {
        return String::new();
    };
    let mut out = String::from("\n");
    let push_line = |out: &mut String, line_no: &mut u32, line: &str, marked: bool| {
        let gutter = ui.dim(format_args!("{line_no:>5} │"));
        if marked {
            writeln!(out, "{} {gutter} {line}", ui.cyan(">")).unwrap();
        } else {
            writeln!(out, "  {gutter} {}", ui.dim(line)).unwrap();
        }
        *line_no += 1;
    };
    let mut line_no = snippet.first_line;
    for line in &snippet.before {
        push_line(&mut out, &mut line_no, line, false);
    }
    match &snippet.target {
        SnippetTarget::Full(lines) => {
            for line in lines {
                push_line(&mut out, &mut line_no, line, true);
            }
        }
        SnippetTarget::Truncated { head, tail, omitted, .. } => {
            for line in head {
                push_line(&mut out, &mut line_no, line, true);
            }
            writeln!(out, "        {}", ui.dim(format_args!("⋮ {omitted} lines omitted"))).unwrap();
            line_no += *omitted as u32;
            for line in tail {
                push_line(&mut out, &mut line_no, line, true);
            }
        }
    }
    for line in &snippet.after {
        push_line(&mut out, &mut line_no, line, false);
    }
    out
}

/// Fetch the remote's threads data into the tracking ref and integrate it
/// into the local ref (SPEC.md §7.2 steps 1–2).
pub fn pull(store: &Store, remote: &str) -> Result<()> {
    match fetch_and_integrate(store, remote)? {
        None => println!("no threads data on {remote}"),
        Some(integration) => report_integration(integration, remote),
    }
    Ok(())
}

/// Seal all drafts into the local data ref as one commit (SPEC.md §5.2
/// session batching). Local only — `push` shares it.
pub fn commit(store: &Store) -> Result<()> {
    match store.commit_drafts()? {
        Some(promoted) => println!(
            "committed {} event{} in {} thread{} {}",
            promoted.events,
            if promoted.events == 1 { "" } else { "s" },
            promoted.threads,
            if promoted.threads == 1 { "" } else { "s" },
            Ui::auto().dim("(git threads push to share)"),
        ),
        None => println!("nothing to commit (no drafts)"),
    }
    Ok(())
}

/// The publish loop (SPEC.md §7.2): integrate remote state, push the local
/// data ref, and on a lost race re-integrate and retry. Drafts are not
/// included — `commit` seals them first.
pub fn push(store: &Store, remote: &str) -> Result<()> {
    let workdir = workdir(store)?;
    if store.drafts_tip()?.is_some() {
        eprintln!("note: you have drafted events; git threads commit to include them");
    }
    const MAX_ATTEMPTS: usize = 5;
    for attempt in 1..=MAX_ATTEMPTS {
        fetch_and_integrate(store, remote)?;
        if store.tip()?.is_none() {
            println!("nothing to push");
            return Ok(());
        }
        match git(&workdir, &["push", remote, "refs/threads/data:refs/threads/data"]) {
            Ok(_) => {
                println!("pushed to {remote}");
                return Ok(());
            }
            Err(err) => {
                let lost_race = err.to_string().contains("[rejected]")
                    || err.to_string().contains("non-fast-forward")
                    || err.to_string().contains("fetch first")
                    || err.to_string().contains("stale info");
                if !lost_race || attempt == MAX_ATTEMPTS {
                    return Err(err.context(format!("push failed after {attempt} attempt(s)")));
                }
                eprintln!("push rejected (concurrent push), retrying ({attempt}/{MAX_ATTEMPTS})");
            }
        }
    }
    unreachable!("loop returns on success or error");
}

/// Fetch into the tracking ref and integrate. `None` means the remote has no
/// threads data yet. The explicit refspec makes this work (and self-heal)
/// even without the configured refspec from `init`.
fn fetch_and_integrate(store: &Store, remote: &str) -> Result<Option<Integration>> {
    let workdir = workdir(store)?;
    let refspec = fetch_refspec(remote);
    if let Err(err) = git(&workdir, &["fetch", remote, &refspec]) {
        if err.to_string().contains("couldn't find remote ref") {
            return Ok(None);
        }
        return Err(err);
    }
    match store.tracking_tip(remote)? {
        Some(remote_tip) => Ok(Some(store.integrate(remote_tip)?)),
        None => Ok(None),
    }
}

fn workdir(store: &Store) -> Result<std::path::PathBuf> {
    Ok(store
        .repo()
        .workdir()
        .context("this operation requires a non-bare repository")?
        .to_owned())
}

fn new_event(
    repo: &gix::Repository,
    kind: EventKind,
    fill: impl FnOnce(&mut Event),
) -> Result<Event> {
    let mut event = Event {
        v: 1,
        kind,
        author: identity(repo)?,
        ts: now()?,
        body: None,
        in_reply_to: None,
        supersedes: None,
        resolved: None,
        extra: Default::default(),
    };
    fill(&mut event);
    event.validate()?;
    Ok(event)
}

/// Author identity from git config / environment, same sources as commits.
fn identity(repo: &gix::Repository) -> Result<Author> {
    let signature = repo
        .author()
        .or_else(|| repo.committer())
        .ok_or_else(|| anyhow!("no identity configured (set user.name and user.email)"))?
        .map_err(|e| anyhow!("invalid identity: {e}"))?;
    Ok(Author {
        name: String::from_utf8_lossy(signature.name).into_owned(),
        email: String::from_utf8_lossy(signature.email).into_owned(),
    })
}

fn now() -> Result<Timestamp> {
    let formatted = jiff::Timestamp::now().strftime("%Y-%m-%dT%H:%M:%SZ").to_string();
    Ok(Timestamp::parse(formatted)?)
}

fn resolve_commit(repo: &gix::Repository, spec: &str) -> Result<ObjectId> {
    let object = repo
        .rev_parse_single(spec)
        .with_context(|| format!("cannot resolve {spec:?}"))?
        .object()?
        .peel_to_kind(gix::object::Kind::Commit)
        .with_context(|| format!("{spec:?} is not a commit"))?;
    Ok(object.id)
}

/// Blob ID and line count of `path` in `commit`'s tree, if present.
fn blob_at(
    repo: &gix::Repository,
    commit: ObjectId,
    path: &str,
) -> Result<Option<(ObjectId, usize)>> {
    let tree = repo.find_commit(commit)?.tree()?;
    let Some(entry) = tree.lookup_entry_by_path(path)? else {
        return Ok(None);
    };
    if !entry.mode().is_blob() {
        bail!("{path:?} is not a file in {commit}");
    }
    let data = &repo.find_blob(entry.object_id())?.data;
    let newlines = data.iter().filter(|b| **b == b'\n').count();
    let count = if data.is_empty() || data.ends_with(b"\n") { newlines } else { newlines + 1 };
    Ok(Some((entry.object_id(), count)))
}

/// Split a trailing `:N` / `:N-M` off a file spec (`src/lib.rs:120-128`),
/// the same shape `list` and `show` print. The suffix only counts as lines
/// when it parses as one, so a path that merely contains colons stays intact.
fn split_line_suffix(spec: &str) -> (&str, Option<&str>) {
    match spec.rsplit_once(':') {
        Some((path, suffix)) if !path.is_empty() && parse_lines(suffix).is_ok() => {
            (path, Some(suffix))
        }
        _ => (spec, None),
    }
}

fn parse_lines(spec: &str) -> Result<LineRange> {
    let (start, end) = match spec.split_once('-') {
        Some((start, end)) => (start, end),
        None => (spec, spec),
    };
    let parse = |s: &str| {
        s.trim()
            .parse::<u32>()
            .map_err(|_| anyhow!("invalid lines {spec:?}: expected N or N-M"))
    };
    Ok(LineRange { start: parse(start)?, end: parse(end)? })
}

/// Hard-wrap a new message to the 72-column convention git messages follow,
/// once, at write time — stored text is displayed verbatim. Only unindented
/// prose is re-flowed: indented lines are deliberate formatting (code,
/// tables, quotes) and pass through untouched, as do words longer than the
/// width (URLs).
fn wrap_message(text: &str) -> String {
    const WIDTH: usize = 72;
    let mut out = Vec::new();
    for line in text.lines() {
        if line.chars().count() <= WIDTH || line.starts_with([' ', '\t']) {
            out.push(line.to_string());
            continue;
        }
        let mut current = String::new();
        for word in line.split_whitespace() {
            if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > WIDTH {
                out.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        out.push(current);
    }
    out.join("\n")
}

/// Find a comment or reply by event-ID prefix across all threads, returning
/// its thread and its folded state (chain tip, retraction).
fn find_message(store: &Store, prefix: &str) -> Result<(ThreadRecord, FoldedEvent)> {
    let mut matches: Vec<(ThreadRecord, FoldedEvent)> = Vec::new();
    for thread in store.threads()? {
        let folded = fold_thread(thread.events.clone());
        for event in folded.events {
            if event.id.as_str().starts_with(prefix) {
                matches.push((thread.clone(), event));
            }
        }
    }
    match matches.len() {
        0 => bail!("no comment or reply matches {prefix:?}"),
        1 => Ok(matches.remove(0)),
        n => bail!(
            "{prefix:?} is ambiguous ({n} matches): {}",
            matches.iter().map(|(_, e)| short(&e.id)).collect::<Vec<_>>().join(", ")
        ),
    }
}

fn short(id: &EventId) -> &str {
    &id.as_str()[..12]
}

pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {args:?}"))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::wrap_message;

    #[test]
    fn wrap_message_reflows_prose_and_keeps_formatting() {
        let long = "word ".repeat(20); // 99 chars, prose
        let wrapped = wrap_message(long.trim_end());
        assert!(wrapped.lines().count() == 2 && wrapped.lines().all(|l| l.len() <= 72));

        let formatted = format!("    {}", "x".repeat(80)); // indented: verbatim
        assert_eq!(wrap_message(&formatted), formatted);

        let url = "x".repeat(80); // one long word: verbatim
        assert_eq!(wrap_message(&url), url);
    }
}
