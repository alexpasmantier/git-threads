use crate::reanchor::{self, Reanchor};
use crate::store::{Append, Batch, Integration, NewThread, Store, ThreadRecord};
use anyhow::{Context, Result, anyhow, bail};
use git_threads_core::{
    Anchor, AnchorKind, Author, DiffRef, Event, EventId, EventKind, FoldedEvent, GitOid,
    LineRange, ReanchorStatus, Side, SnippetTarget, ThreadId, Timestamp, derive_snippet,
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
        Ok(_) => println!("fetched from {remote}"),
        Err(err) => eprintln!("warning: initial fetch from {remote} failed: {err:#}"),
    }
    Ok(())
}

pub struct CommentOpts {
    /// Commit whose change is being discussed.
    pub commit: String,
    pub message: String,
    pub file: Option<String>,
    /// `"120"` or `"120-128"`; requires `file`.
    pub lines: Option<String>,
    pub side: Side,
    /// Diff base; defaults to the first parent of `commit`.
    pub base: Option<String>,
}

/// Create a new thread anchored to a commit, file, or line range (SPEC.md §3).
pub fn comment(store: &Store, opts: &CommentOpts) -> Result<ThreadId> {
    let repo = store.repo();
    let head = resolve_commit(repo, &opts.commit)?;
    let base = match &opts.base {
        Some(spec) => resolve_commit(repo, spec)?,
        None => repo
            .find_commit(head)?
            .parent_ids()
            .next()
            .map(|id| id.detach())
            .with_context(|| {
                format!("{} has no parent; pass --base to choose a diff base", &opts.commit)
            })?,
    };

    let (kind, path, lines, blob) = match (&opts.file, &opts.lines) {
        (None, None) => (AnchorKind::Commit, None, None, None),
        (None, Some(_)) => bail!("--lines requires --file"),
        (Some(file), lines) => {
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
            let lines = lines
                .as_deref()
                .map(|spec| {
                    let range = parse_lines(spec)?;
                    if range.end as usize > line_count {
                        bail!("--lines {spec} is out of range: {file:?} has {line_count} lines");
                    }
                    Ok(range)
                })
                .transpose()?;
            let kind = if lines.is_some() { AnchorKind::Range } else { AnchorKind::File };
            let blob = GitOid::from_hex(blob_id.to_string())?;
            (kind, Some(file.clone()), lines, Some(blob))
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
        side: opts.file.as_ref().map(|_| opts.side),
        lines,
        blob,
        cols: None,
        extra: Default::default(),
    };
    let root = new_event(repo, EventKind::Comment, |e| {
        e.body = Some(opts.message.clone());
    })?;
    let thread_id = root.id()?;

    store.write(&Batch {
        new_threads: vec![NewThread { anchor, root, events: vec![] }],
        appends: vec![],
    })?;
    println!("created thread {thread_id}");
    Ok(thread_id)
}

/// Reply to an existing thread (found by ID prefix).
pub fn reply(store: &Store, thread_prefix: &str, message: &str) -> Result<EventId> {
    let thread = find_thread(store, thread_prefix)?;
    let event = new_event(store.repo(), EventKind::Reply, |e| {
        e.body = Some(message.to_string());
        e.in_reply_to = Some(thread.id.clone());
    })?;
    let event_id = event.id()?;
    store.write(&Batch {
        new_threads: vec![],
        appends: vec![Append { thread: thread.id.clone(), events: vec![event] }],
    })?;
    println!("added reply {} to thread {}", short(&event_id), short(&thread.id));
    Ok(event_id)
}

/// Edit a comment or reply: append an `edit` event superseding the current
/// tip of the target's edit chain (SPEC.md §2.1). Only the author's edits
/// take effect in the fold, so anyone else's are rejected here.
pub fn edit(store: &Store, event_prefix: &str, message: &str) -> Result<EventId> {
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
    let event = new_event(store.repo(), EventKind::Edit, |e| {
        e.body = Some(message.to_string());
        e.supersedes = Some(target.chain_tip.clone());
    })?;
    let event_id = event.id()?;
    store.write(&Batch {
        new_threads: vec![],
        appends: vec![Append { thread: thread.id.clone(), events: vec![event] }],
    })?;
    println!("edited {} (edit event {})", short(&target.id), short(&event_id));
    Ok(event_id)
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
    store.write(&Batch {
        new_threads: vec![],
        appends: vec![Append { thread: thread.id.clone(), events: vec![event] }],
    })?;
    println!("retracted {}", short(&target.id));
    Ok(event_id)
}

/// Mark a thread resolved (or reopen it).
pub fn resolve(store: &Store, thread_prefix: &str, resolved: bool) -> Result<()> {
    let thread = find_thread(store, thread_prefix)?;
    let event = new_event(store.repo(), EventKind::Resolve, |e| {
        e.resolved = Some(resolved);
    })?;
    store.write(&Batch {
        new_threads: vec![],
        appends: vec![Append { thread: thread.id.clone(), events: vec![event] }],
    })?;
    println!(
        "thread {} {}",
        short(&thread.id),
        if resolved { "resolved" } else { "reopened" }
    );
    Ok(())
}

/// List threads in the current snapshot with their folded state and their
/// re-anchor status against `at` (SPEC.md §4.2).
pub fn list(store: &Store, at: &str) -> Result<()> {
    let mut threads = store.threads()?;
    if threads.is_empty() {
        println!("no threads");
        return Ok(());
    }
    let target = resolve_commit(store.repo(), at)?;
    // Newest first, by earliest event timestamp.
    threads.sort_by_key(|t| std::cmp::Reverse(t.events.iter().map(|(_, e)| e.ts.clone()).min()));
    for thread in threads {
        let folded = fold_thread(thread.events.clone());
        let status = if folded.resolved { "resolved" } else { "open" };
        let location = match (&thread.anchor.path, &thread.anchor.lines) {
            (Some(path), Some(lines)) => format!("{path}:{}-{}", lines.start, lines.end),
            (Some(path), None) => path.clone(),
            _ => format!("commit {}", &thread.anchor.diff.head.as_str()[..12]),
        };
        let placement = match reanchor::reanchor(store, &thread.anchor, target)? {
            Reanchor::WholeCommit | Reanchor::Located { status: ReanchorStatus::Exact, .. } => {
                String::new()
            }
            Reanchor::Located { path, lines, status } => {
                let lines = lines.map(|l| format!(":{}-{}", l.start, l.end)).unwrap_or_default();
                format!("  → {path}{lines} ({status})")
            }
            Reanchor::Outdated => "  (outdated)".to_string(),
        };
        let title = folded
            .events
            .first()
            .and_then(|root| root.effective_body.as_deref())
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("");
        println!(
            "{}  [{status}] {location}{placement}  ({} message{})  {title}",
            short(&thread.id),
            folded.events.len(),
            if folded.events.len() == 1 { "" } else { "s" },
        );
    }
    Ok(())
}

/// Render a thread: anchor location, re-anchor placement on `at` (SPEC.md
/// §4.2), code context, and the folded conversation. The context comes from
/// the re-anchored location when there is one, from the anchor's own diff
/// when outdated (§4.2 step 4).
pub fn show(store: &Store, thread_prefix: &str, at: &str) -> Result<()> {
    let thread = find_thread(store, thread_prefix)?;
    let folded = fold_thread(thread.events.clone());
    let anchor = &thread.anchor;

    let status = if folded.resolved { "resolved" } else { "open" };
    println!("thread {}  [{status}]", thread.id);
    let side = match anchor.side {
        Some(Side::Old) => " (old side)",
        _ => "",
    };
    let location = match (&anchor.path, &anchor.lines) {
        (Some(path), Some(lines)) => format!("{path}:{}-{}{side}", lines.start, lines.end),
        (Some(path), None) => format!("{path}{side}"),
        _ => "whole change".to_string(),
    };
    println!(
        "on {location} of {}..{}",
        &anchor.diff.base.as_str()[..12],
        &anchor.diff.head.as_str()[..12]
    );

    let target = resolve_commit(store.repo(), at)?;
    let target_short = &target.to_string()[..12];
    let placement = reanchor::reanchor(store, anchor, target)?;
    match &placement {
        Reanchor::WholeCommit => {}
        Reanchor::Located { path, lines, status } => {
            let lines = lines.map(|l| format!(":{}-{}", l.start, l.end)).unwrap_or_default();
            println!("now {path}{lines} at {target_short} ({status})");
        }
        Reanchor::Outdated => {
            println!("outdated at {target_short}: showing the anchor's own context");
        }
    }

    match &placement {
        Reanchor::Located { path, lines: Some(lines), .. } => {
            if let Some(blob) = reanchor::blob_at(store.repo(), target, path)? {
                print_snippet(&reanchor::blob_content(store.repo(), blob)?, *lines);
            }
        }
        _ => {
            if let (Some(lines), Some(blob)) = (anchor.lines, &anchor.blob) {
                let blob_id = ObjectId::from_hex(blob.as_str().as_bytes())?;
                print_snippet(&reanchor::blob_content(store.repo(), blob_id)?, lines);
            }
        }
    }

    for event in &folded.events {
        println!();
        let marker = if event.event.kind == EventKind::Reply { "↳ " } else { "● " };
        let edited = if event.edited { " (edited)" } else { "" };
        println!(
            "{marker}{}  {} <{}> {}{edited}",
            short(&event.id),
            event.event.author.name,
            event.event.author.email,
            event.event.ts
        );
        if event.retracted {
            println!("  [retracted]");
        } else if let Some(body) = &event.effective_body {
            for line in body.lines() {
                println!("  {line}");
            }
        }
    }
    Ok(())
}

/// Print a marked, line-numbered snippet of `lines` out of `content`.
fn print_snippet(content: &str, lines: LineRange) {
    let Some(snippet) = derive_snippet(content, lines) else {
        return;
    };
    println!();
    fn print_line(line_no: &mut u32, line: &str, marked: bool) {
        println!("{} {line_no:>5} │ {line}", if marked { ">" } else { " " });
        *line_no += 1;
    }
    let mut line_no = snippet.first_line;
    for line in &snippet.before {
        print_line(&mut line_no, line, false);
    }
    match &snippet.target {
        SnippetTarget::Full(lines) => {
            for line in lines {
                print_line(&mut line_no, line, true);
            }
        }
        SnippetTarget::Truncated { head, tail, omitted, .. } => {
            for line in head {
                print_line(&mut line_no, line, true);
            }
            println!("        ⋮ {omitted} lines omitted");
            line_no += *omitted as u32;
            for line in tail {
                print_line(&mut line_no, line, true);
            }
        }
    }
    for line in &snippet.after {
        print_line(&mut line_no, line, false);
    }
}

/// Fetch the remote's threads data into the tracking ref and integrate it
/// into the local ref (SPEC.md §7.2 steps 1–2).
pub fn pull(store: &Store, remote: &str) -> Result<()> {
    match fetch_and_integrate(store, remote)? {
        None => println!("no threads data on {remote}"),
        Some(Integration::UpToDate) => println!("already up to date"),
        Some(Integration::Initialized) => println!("initialized from {remote}"),
        Some(Integration::FastForwarded) => println!("fast-forwarded to {remote}"),
        Some(Integration::Merged) => println!("merged threads from {remote}"),
    }
    Ok(())
}

/// The publish loop (SPEC.md §7.2): integrate remote state, push, and on a
/// lost race re-integrate and retry.
pub fn publish(store: &Store, remote: &str) -> Result<()> {
    let workdir = workdir(store)?;
    const MAX_ATTEMPTS: usize = 5;
    for attempt in 1..=MAX_ATTEMPTS {
        fetch_and_integrate(store, remote)?;
        if store.tip()?.is_none() {
            println!("nothing to publish");
            return Ok(());
        }
        match git(&workdir, &["push", remote, "refs/threads/data:refs/threads/data"]) {
            Ok(_) => {
                println!("published to {remote}");
                return Ok(());
            }
            Err(err) => {
                let lost_race = err.to_string().contains("[rejected]")
                    || err.to_string().contains("non-fast-forward")
                    || err.to_string().contains("fetch first")
                    || err.to_string().contains("stale info");
                if !lost_race || attempt == MAX_ATTEMPTS {
                    return Err(err.context(format!("publish failed after {attempt} attempt(s)")));
                }
                eprintln!("push rejected (concurrent publish), retrying ({attempt}/{MAX_ATTEMPTS})");
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

fn parse_lines(spec: &str) -> Result<LineRange> {
    let (start, end) = match spec.split_once('-') {
        Some((start, end)) => (start, end),
        None => (spec, spec),
    };
    let parse = |s: &str| {
        s.trim()
            .parse::<u32>()
            .map_err(|_| anyhow!("invalid --lines {spec:?}: expected N or N-M"))
    };
    Ok(LineRange { start: parse(start)?, end: parse(end)? })
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

fn find_thread(store: &Store, prefix: &str) -> Result<ThreadRecord> {
    let mut matches: Vec<ThreadRecord> = store
        .threads()?
        .into_iter()
        .filter(|t| t.id.as_str().starts_with(prefix))
        .collect();
    match matches.len() {
        0 => bail!("no thread matches {prefix:?}"),
        1 => Ok(matches.remove(0)),
        n => bail!(
            "{prefix:?} is ambiguous ({n} matches): {}",
            matches.iter().map(|t| short(&t.id)).collect::<Vec<_>>().join(", ")
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
