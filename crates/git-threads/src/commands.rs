use crate::reanchor::{self, Reanchor};
use crate::store::{Append, Batch, Integration, NewThread, Store, ThreadRecord};
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
        Ok(_) => println!("fetched from {remote}"),
        Err(err) => eprintln!("warning: initial fetch from {remote} failed: {err:#}"),
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
        e.body = Some(opts.message.clone());
    })?;
    let thread_id = root.id()?;

    store.draft(&Batch {
        new_threads: vec![NewThread { anchor, root, events: vec![] }],
        appends: vec![],
    })?;
    println!("drafted thread {thread_id} (commit and push to share)");
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
                format!("{spec} has no parent; comment on a range instead (<base>..{spec})")
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
        e.body = Some(message.to_string());
        e.in_reply_to = Some(target.id.clone());
    })?;
    let event_id = event.id()?;
    store.draft(&Batch {
        new_threads: vec![],
        appends: vec![Append { thread: thread.id.clone(), events: vec![event] }],
    })?;
    println!("drafted reply {} to thread {}", short(&event_id), short(&thread.id));
    Ok(event_id)
}

/// Edit a comment or reply: append an `edit` event superseding the current
/// tip of the target's edit chain (SPEC.md §2.1). Only the author's edits
/// take effect in the fold, so anyone else's are rejected here.
pub fn edit(store: &Store, event_prefix: &str, message: &str) -> Result<EventId> {
    let (thread, target) = find_editable(store, event_prefix)?;
    let event = new_event(store.repo(), EventKind::Edit, |e| {
        e.body = Some(message.to_string());
        e.supersedes = Some(target.chain_tip.clone());
    })?;
    let event_id = event.id()?;
    store.draft(&Batch {
        new_threads: vec![],
        appends: vec![Append { thread: thread.id.clone(), events: vec![event] }],
    })?;
    println!("drafted edit of {} (edit event {})", short(&target.id), short(&event_id));
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
    println!("drafted retraction of {}", short(&target.id));
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
    println!(
        "thread {} {} (draft)",
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
        let drafts = match thread.drafts.len() {
            0 => String::new(),
            n => format!(", {n} draft{}", if n == 1 { "" } else { "s" }),
        };
        println!(
            "{}  [{status}] {location}{placement}  ({} message{}{drafts})  {title}",
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
/// when outdated (§4.2 step 4). The prefix may name the thread or any
/// comment/reply in it.
pub fn show(store: &Store, prefix: &str, at: &str) -> Result<()> {
    let (thread, _) = find_message(store, prefix)?;
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

    print!("{}", render_conversation(&thread, &folded));
    Ok(())
}

/// The conversation as `show` prints it: one block per message, blank-line
/// separated, starting with a blank line.
fn render_conversation(thread: &ThreadRecord, folded: &FoldedThread) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for event in &folded.events {
        let marker = if event.event.kind == EventKind::Reply { "↳ " } else { "● " };
        let edited = if event.edited { " (edited)" } else { "" };
        let draft = if thread.drafts.contains(&event.id) { " (draft)" } else { "" };
        writeln!(
            out,
            "\n{marker}{}  {} <{}> {}{edited}{draft}",
            short(&event.id),
            event.event.author.name,
            event.event.author.email,
            event.event.ts
        )
        .unwrap();
        if event.retracted {
            out.push_str("  [retracted]\n");
        } else if let Some(body) = &event.effective_body {
            for line in body.lines() {
                writeln!(out, "  {line}").unwrap();
            }
        }
    }
    out
}

/// A compact rendering of a thread — id, status, location, conversation —
/// for the editor hint when `reply` runs without --message.
pub fn thread_preview(store: &Store, prefix: &str) -> Result<String> {
    let (thread, _) = find_message(store, prefix)?;
    let folded = fold_thread(thread.events.clone());
    let status = if folded.resolved { "resolved" } else { "open" };
    let location = match (&thread.anchor.path, &thread.anchor.lines) {
        (Some(path), Some(lines)) => format!("{path}:{}-{}", lines.start, lines.end),
        (Some(path), None) => path.clone(),
        _ => format!("commit {}", &thread.anchor.diff.head.as_str()[..12]),
    };
    Ok(format!(
        "replying to thread {}  [{status}]  on {location}{}",
        short(&thread.id),
        render_conversation(&thread, &folded)
    ))
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
    if removed > 1 || event_id == thread_id {
        println!(
            "discarded draft thread {} ({removed} event{})",
            short(&thread_id),
            if removed == 1 { "" } else { "s" },
        );
    } else {
        println!("discarded draft {}", short(&event_id));
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

/// Seal all drafts into the local data ref as one commit (SPEC.md §5.2
/// session batching). Local only — `push` shares it.
pub fn commit(store: &Store) -> Result<()> {
    match store.commit_drafts()? {
        Some(promoted) => println!(
            "committed {} event{} in {} thread{} (git threads push to share)",
            promoted.events,
            if promoted.events == 1 { "" } else { "s" },
            promoted.threads,
            if promoted.threads == 1 { "" } else { "s" },
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
