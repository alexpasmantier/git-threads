use crate::reanchor::{self, Reanchor};
use crate::store::{Append, Batch, Integration, NewThread, PromotedDrafts, Store, ThreadRecord};
use crate::ui::short;
use crate::view::{
    AnchorContext, DraftedEvent, MessageView, RemoteStatus, StatusView, ThreadDrafts, ThreadView,
};
use anyhow::{Context, Result, anyhow, bail};
use git_threads_core::{
    Anchor, AnchorKind, Author, DiffRef, Event, EventId, EventKind, FoldedEvent, FoldedThread,
    GitOid, LineRange, ReanchorStatus, Side, ThreadId, Timestamp, fold_thread,
};
use gix::ObjectId;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};

/// The fetch refspec `init` configures (SPEC.md §7.1): remote state lands in
/// the tracking ref, never directly on `refs/threads/data` — a direct mapping
/// would let any fetch clobber the local ref, orphaning unpublished events.
/// The glob matters: git errors on a configured exact refspec whose ref is
/// missing, so the exact form breaks plain `git fetch` until the remote has
/// threads data. A glob that matches nothing is silently skipped.
fn fetch_refspec(remote: &str) -> String {
    format!("+refs/threads/data*:{}*", Store::tracking_ref(remote))
}

/// The exact-form refspec older versions of `init` wrote (see above for why
/// it was replaced). `init` and `deinit` still remove it.
fn legacy_fetch_refspec(remote: &str) -> String {
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
    let legacy = legacy_fetch_refspec(remote);
    let key = format!("remote.{remote}.fetch");
    let existing = git(&workdir, &["config", "--get-all", &key]).unwrap_or_default();
    if existing.lines().any(|line| line == legacy) {
        git(&workdir, &["config", "--fixed-value", "--unset-all", &key, &legacy])?;
        println!("removed legacy refspec {legacy}");
    }
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
                println!("{}", store.integrate(tip)?.describe(remote));
            }
        }
        Err(err) => eprintln!("warning: initial fetch from {remote} failed: {err:#}"),
    }
    // First setup seeds the read mark: the history you just imported is not
    // news. Re-running init must not touch an inbox already in use.
    if !store.has_seen_mark()? {
        store.mark_all_seen()?;
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
        let existing = git(&workdir, &["config", "--get-all", &key]).unwrap_or_default();
        for refspec in [fetch_refspec(remote), legacy_fetch_refspec(remote)] {
            if existing.lines().any(|line| line == refspec) {
                git(&workdir, &["config", "--fixed-value", "--unset-all", &key, &refspec])?;
                println!("removed {refspec} from {key}");
            }
        }
    }
    let refs = git(&workdir, &["for-each-ref", "--format=%(refname)", "refs/threads/"])?;
    let mut deleted = 0;
    for name in refs.lines() {
        git(&workdir, &["update-ref", "-d", name])?;
        deleted += 1;
    }
    // Client-local derived data (the re-anchor cache) goes with the refs.
    let _ = std::fs::remove_dir_all(store.repo().git_dir().join("threads"));
    println!(
        "deleted {deleted} ref{} under refs/threads/; the remote's data is untouched \
         (git threads init to start again)",
        if deleted == 1 { "" } else { "s" }
    );
    Ok(())
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
    let anchor = resolve_anchor(repo, opts.target.as_deref(), opts.file.as_deref(), opts.side)?;
    let root = new_event(repo, EventKind::Comment, |e| {
        e.body = Some(wrap_message(&opts.message));
    })?;
    let thread_id = root.id()?;

    store.draft(&Batch {
        new_threads: vec![NewThread { anchor, root, events: vec![] }],
        appends: vec![],
    })?;
    Ok(thread_id)
}

/// Resolve and fully validate what a comment anchors to — target, file,
/// lines, membership in the diff. Everything that can reject a comment is
/// here, so the CLI can run it before opening the editor and no one writes
/// a message that can't be saved.
pub fn resolve_anchor(
    repo: &gix::Repository,
    target: Option<&str>,
    file: Option<&str>,
    side: Side,
) -> Result<Anchor> {
    let Target { base, head, file } = resolve_target(repo, target, file, side)?;

    let (kind, path, lines, blob) = match &file {
        None => (AnchorKind::Commit, None, None, None),
        Some(spec) => {
            let (file, suffix) = split_line_suffix(spec);
            // The blob is resolved on the anchor's side: `new` reads the head
            // tree, `old` the base tree (e.g. comments on deleted lines).
            let side_commit = match side {
                Side::New => head,
                Side::Old => base,
            };
            let (blob_id, line_count) = blob_at(repo, side_commit, file)?.with_context(|| {
                format!(
                    "{file:?} not found in the {} version ({})",
                    match side {
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
                ensure_in_diff(repo, base, head, file, side, lines)?;
            }
            let kind = if lines.is_some() { AnchorKind::Range } else { AnchorKind::File };
            let blob = GitOid::from_hex(blob_id.to_string())?;
            (kind, Some(file.to_string()), lines, Some(blob))
        }
    };

    Ok(Anchor {
        v: 1,
        kind,
        diff: DiffRef {
            base: GitOid::from_hex(base.to_string())?,
            head: GitOid::from_hex(head.to_string())?,
        },
        path,
        old_path: None,
        side: file.as_ref().map(|_| side),
        lines,
        blob,
        cols: None,
        extra: Default::default(),
    })
}

/// The diff a comment targets, and the file within it, if any.
struct Target {
    base: ObjectId,
    head: ObjectId,
    file: Option<String>,
}

/// Sort out what `comment`'s positionals refer to, the way git disambiguates
/// revs from paths. The first positional names the diff — a commit (its
/// first-parent change) or a range — and, when it's the only one, may instead
/// be a file (path or path:lines) of HEAD's change. The second positional is
/// always a file.
fn resolve_target(
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

/// A drafted event and the thread it lands in.
#[derive(Clone, Debug)]
pub struct Drafted {
    pub thread: ThreadId,
    pub event: EventId,
}

/// A drafted event acting on an earlier message: an edit's replacement, a
/// delete's tombstone.
#[derive(Clone, Debug)]
pub struct Amendment {
    /// The comment or reply acted on.
    pub target: EventId,
    /// The drafted edit or delete event.
    pub event: EventId,
}

/// Reply to a thread, or to a specific message in one: the prefix may name
/// the thread or any comment/reply in it, and `in_reply_to` records the
/// named event (SPEC.md §2.1 allows replying to any event in the thread).
pub fn reply(store: &Store, prefix: &str, message: &str) -> Result<Drafted> {
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
    Ok(Drafted { thread: thread.id, event: event_id })
}

/// Edit a comment or reply: append an `edit` event superseding the current
/// tip of the target's edit chain (SPEC.md §2.1). Only the author's edits
/// take effect in the fold, so anyone else's are rejected here.
pub fn edit(store: &Store, event_prefix: &str, message: &str) -> Result<Amendment> {
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
    Ok(Amendment { target: target.id, event: event_id })
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
pub fn delete(store: &Store, event_prefix: &str) -> Result<Amendment> {
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
    Ok(Amendment { target: target.id, event: event_id })
}

/// The anchor re-anchoring starts from (SPEC.md §2.4 rule 5): the latest
/// move's, else the thread's own. The original stays the record of what was
/// discussed; the move records where that code lives now.
fn effective_anchor<'a>(thread: &'a ThreadRecord, folded: &'a FoldedThread) -> &'a Anchor {
    folded.moved.as_ref().and_then(|(_, e)| e.anchor.as_ref()).unwrap_or(&thread.anchor)
}

/// Re-pin a thread to where its code lives now: an empty-diff anchor on
/// `at`, recorded as a `move` event (SPEC.md §2.1). The escape hatch for
/// outdated threads — when re-anchoring can't follow the code, a person can.
pub fn move_thread(store: &Store, prefix: &str, file_spec: &str, at: &str) -> Result<MoveDraft> {
    let (thread, _) = find_message(store, prefix)?;
    let repo = store.repo();
    let target = resolve_commit(repo, at)?;
    let (path, suffix) = split_line_suffix(file_spec);
    let (blob_id, line_count) = blob_at(repo, target, path)?
        .with_context(|| format!("{path:?} not found at {}", &target.to_string()[..12]))?;
    let lines = suffix
        .map(|spec| {
            let range = parse_lines(spec)?;
            if range.end as usize > line_count {
                bail!("lines {spec} are out of range: {path:?} has {line_count} lines");
            }
            Ok(range)
        })
        .transpose()?;
    let event = move_event(repo, target, path, lines, blob_id)?;
    let event_id = event.id()?;
    store.draft(&Batch {
        new_threads: vec![],
        appends: vec![Append { thread: thread.id.clone(), events: vec![event] }],
    })?;
    Ok(MoveDraft { thread: thread.id, event: event_id, path: path.to_string(), lines })
}

/// The move event itself: an empty-diff anchor pinned at `target` — a
/// statement of where the code is, not of a change (SPEC.md §2.1).
fn move_event(
    repo: &gix::Repository,
    target: ObjectId,
    path: &str,
    lines: Option<LineRange>,
    blob: ObjectId,
) -> Result<Event> {
    let pin = GitOid::from_hex(target.to_string())?;
    let anchor = Anchor {
        v: 1,
        kind: if lines.is_some() { AnchorKind::Range } else { AnchorKind::File },
        diff: DiffRef { base: pin.clone(), head: pin },
        path: Some(path.to_string()),
        old_path: None,
        side: Some(Side::New),
        lines,
        blob: Some(GitOid::from_hex(blob.to_string())?),
        cols: None,
        extra: Default::default(),
    };
    new_event(repo, EventKind::Move, |e| {
        e.anchor = Some(anchor);
    })
}

/// Bulk `move --orphans`: re-pin every actionable orphan at `at`. A thread
/// is orphaned when a rewrite left both its addresses — where it was
/// discussed and where it was last moved — outside `at`'s history with no
/// patch-id twin on it (a twinned thread is already findable; an event
/// would be noise). Actionable means re-anchoring finds its code at `at`
/// verbatim (`exact`/`relocated`): content decides, never a reconstruction
/// of what happened to the commit. Fuzzy and outdated threads are reported
/// untouched — re-pinning those is a judgment call, one `move` per thread.
/// So are orphaned whole-change threads: commit anchors never re-anchor,
/// so a patch-id twin is their only rescue.
pub fn move_orphans(store: &Store, at: &str) -> Result<OrphanMoves> {
    let repo = store.repo();
    let target = resolve_commit(repo, at)?;
    // One verdict per distinct commit: threads share anchored heads.
    let mut verdicts: std::collections::HashMap<String, bool> = Default::default();
    let mut findable_at_target = |head: &str| -> Result<bool> {
        if let Some(&verdict) = verdicts.get(head) {
            return Ok(verdict);
        }
        let verdict = is_ancestor(repo.git_dir(), head, target)? || {
            let commit = ObjectId::from_hex(head.as_bytes())?;
            ChangeMembership::new(repo, commit, target)?.contains(head)
        };
        verdicts.insert(head.to_string(), verdict);
        Ok(verdict)
    };
    let mut cache = reanchor::Cache::open(repo, target);
    let mut moved = Vec::new();
    let mut appends = Vec::new();
    let mut unplaced = Vec::new();
    let mut whole_commit = Vec::new();
    for thread in store.threads()? {
        let folded = fold_thread(thread.events.clone());
        let anchor = effective_anchor(&thread, &folded).clone();
        if findable_at_target(thread.anchor.diff.head.as_str())?
            || (anchor.diff.head != thread.anchor.diff.head
                && findable_at_target(anchor.diff.head.as_str())?)
        {
            continue;
        }
        match cache.placement(store, &anchor)? {
            Reanchor::WholeCommit => whole_commit.push(thread.id),
            Reanchor::Located { path, lines, status }
                if matches!(status, ReanchorStatus::Exact | ReanchorStatus::Relocated) =>
            {
                let (blob, _) = blob_at(repo, target, &path)?
                    .with_context(|| format!("{path:?} vanished from the target"))?;
                let event = move_event(repo, target, &path, lines, blob)?;
                let event_id = event.id()?;
                appends.push(Append { thread: thread.id.clone(), events: vec![event] });
                moved
                    .push((MoveDraft { thread: thread.id, event: event_id, path, lines }, status));
            }
            _ => unplaced.push(thread.id),
        }
    }
    if !appends.is_empty() {
        store.draft(&Batch { new_threads: vec![], appends })?;
    }
    cache.save();
    Ok(OrphanMoves { target, moved, unplaced, whole_commit })
}

/// The outcome of `move --orphans`: what was re-pinned, and what stayed
/// put and why.
pub struct OrphanMoves {
    pub target: ObjectId,
    pub moved: Vec<(MoveDraft, ReanchorStatus)>,
    /// Orphaned, but no verbatim match at the target.
    pub unplaced: Vec<ThreadId>,
    /// Orphaned whole-change threads: commit anchors never re-anchor.
    pub whole_commit: Vec<ThreadId>,
}

/// `git merge-base --is-ancestor`: whether `commit` is in `tip`'s history.
fn is_ancestor(dir: &Path, commit: &str, tip: ObjectId) -> Result<bool> {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["merge-base", "--is-ancestor", commit, &tip.to_string()])
        .stderr(Stdio::null())
        .status()
        .context("failed to run git merge-base")?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("git merge-base --is-ancestor failed for {commit}"),
    }
}

/// A drafted move: the thread re-pinned and where to.
#[derive(Clone, Debug)]
pub struct MoveDraft {
    pub thread: ThreadId,
    pub event: EventId,
    pub path: String,
    pub lines: Option<LineRange>,
}

/// Mark a thread resolved (or reopen it). The prefix may name the thread or
/// any comment/reply in it.
pub fn resolve(store: &Store, prefix: &str, resolved: bool) -> Result<ThreadId> {
    let (thread, _) = find_message(store, prefix)?;
    let event = new_event(store.repo(), EventKind::Resolve, |e| {
        e.resolved = Some(resolved);
    })?;
    store.draft(&Batch {
        new_threads: vec![],
        appends: vec![Append { thread: thread.id.clone(), events: vec![event] }],
    })?;
    Ok(thread.id)
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
    /// Only threads with events you haven't seen.
    pub new: bool,
    /// git log's -n: stop after this many threads.
    pub max_count: Option<usize>,
    /// Substring of the root author's name or email, case-insensitive.
    pub author: Option<String>,
    /// Substring of any message's current text, case-insensitive.
    pub grep: Option<String>,
    /// Boundaries on the root comment's date, git log style.
    pub since: Option<String>,
    pub until: Option<String>,
}

/// List threads in the current snapshot as [`ThreadView`]s, newest first,
/// re-anchored against `at` (SPEC.md §4.2). `target`/`file` narrow to one
/// change and one path (grammar mirrors `comment`, except a lone file
/// filters across all changes).
pub fn list(store: &Store, opts: &ListOpts) -> Result<Vec<ThreadView>> {
    let repo = store.repo();
    let mut threads = store.threads()?;
    let (target, file) = match (opts.target.as_deref(), opts.file.as_deref()) {
        (Some(spec), None) => resolve_list_filters(repo, &threads, spec)?,
        (target, file) => (target.map(String::from), file.map(String::from)),
    };
    if let Some(spec) = &target {
        let (base, head) = resolve_diff(repo, spec)?;
        let mut change = ChangeMembership::new(repo, base, head)?;
        threads.retain(|t| {
            change.contains(t.anchor.diff.head.as_str())
                || moved_anchor(t).is_some_and(|a| change.contains(a.diff.head.as_str()))
        });
    }
    if let Some(spec) = &file {
        let (path, lines) = split_line_suffix(spec);
        let lines = lines.map(parse_lines).transpose()?;
        threads.retain(|t| {
            let here = |a: &Anchor| anchor_matches(a, path, lines);
            here(&t.anchor) || moved_anchor(t).as_ref().is_some_and(here)
        });
    }
    let at_commit = resolve_commit(repo, &opts.at)?;
    let since = opts.since.as_deref().map(parse_date).transpose()?;
    let until = opts.until.as_deref().map(parse_date).transpose()?;
    // Newest first, by earliest event timestamp.
    threads.sort_by_key(|t| std::cmp::Reverse(t.events.iter().map(|(_, e)| e.ts.clone()).min()));
    let seen = store.seen_event_ids()?;
    let me = identity(repo).ok();
    let mut cache = reanchor::Cache::open(repo, at_commit);
    let mut views: Vec<ThreadView> = Vec::new();
    for thread in threads {
        let unseen = unseen_ids(&thread, &seen, me.as_ref());
        if opts.new && unseen.is_empty() {
            continue;
        }
        let folded = fold_thread(thread.events.clone());
        if opts.resolved.is_some_and(|want| folded.resolved != want) {
            continue;
        }
        // The actual root comment: a same-second reply can tie-break ahead
        // of it in display order, so don't just take the first.
        let root =
            folded.events.iter().find(|e| e.id == thread.id).or_else(|| folded.events.first());
        if let Some(pattern) = &opts.author {
            let author = root
                .map(|r| format!("{} <{}>", r.event.author.name, r.event.author.email))
                .unwrap_or_default();
            if !author.to_lowercase().contains(&pattern.to_lowercase()) {
                continue;
            }
        }
        // Like the author filter, --grep is a case-insensitive substring
        // match. It reads what a reader would see: current (folded) bodies,
        // any message of the thread, retracted ones excluded.
        if let Some(pattern) = &opts.grep {
            let pattern = pattern.to_lowercase();
            let matches = folded.events.iter().any(|e| {
                !e.retracted
                    && e.effective_body
                        .as_deref()
                        .is_some_and(|body| body.to_lowercase().contains(&pattern))
            });
            if !matches {
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
        if opts.max_count.is_some_and(|n| views.len() >= n) {
            break;
        }
        views.push(build_view(store, &thread, &folded, &mut cache, &seen, me.as_ref())?);
    }
    cache.save();
    Ok(views)
}

/// A thread's published events the user hasn't looked at: absent from the
/// seen snapshot, not their own writing, not their drafts. What "new" means
/// everywhere it appears (decorations, --new, status, the json flag).
fn unseen_ids(
    thread: &ThreadRecord,
    seen: &BTreeSet<EventId>,
    me: Option<&Author>,
) -> BTreeSet<EventId> {
    thread
        .events
        .iter()
        .filter(|(id, event)| {
            !seen.contains(id)
                && !thread.drafts.contains(id)
                && me.is_none_or(|me| me.email != event.author.email)
        })
        .map(|(id, _)| id.clone())
        .collect()
}

/// Assemble a [`ThreadView`] from a thread's record and folded state: the
/// re-anchor placement against `at`, and every message with its folded body
/// and draft/new flags applied.
fn build_view(
    store: &Store,
    thread: &ThreadRecord,
    folded: &FoldedThread,
    cache: &mut reanchor::Cache,
    seen: &BTreeSet<EventId>,
    me: Option<&Author>,
) -> Result<ThreadView> {
    let at = cache.target();
    let unseen = unseen_ids(thread, seen, me);
    let placement = cache.placement(store, effective_anchor(thread, folded))?;
    let (moved_to, moved_by) = match &folded.moved {
        Some((_, event)) => (event.anchor.clone(), Some(event.author.clone())),
        None => (None, None),
    };
    let messages = folded
        .events
        .iter()
        .map(|e| MessageView {
            id: e.id.clone(),
            kind: e.event.kind.clone(),
            author: e.event.author.clone(),
            ts: e.event.ts.clone(),
            body: if e.retracted { None } else { e.effective_body.clone() },
            in_reply_to: e.event.in_reply_to.clone(),
            edited: e.edited,
            retracted: e.retracted,
            draft: thread.drafts.contains(&e.id),
            new: unseen.contains(&e.id),
        })
        .collect();
    Ok(ThreadView {
        id: thread.id.clone(),
        resolved: folded.resolved,
        anchor: thread.anchor.clone(),
        moved_to,
        moved_by,
        at: GitOid::from_hex(at.to_string())?,
        placement,
        messages,
    })
}

/// One thread as a [`ThreadView`], re-anchored against `at`. The prefix may
/// name the thread or any comment/reply in it.
pub fn thread_view(store: &Store, prefix: &str, at: &str) -> Result<ThreadView> {
    let (thread, _) = find_message(store, prefix)?;
    let folded = fold_thread(thread.events.clone());
    let at = resolve_commit(store.repo(), at)?;
    let seen = store.seen_event_ids()?;
    let me = identity(store.repo()).ok();
    let mut cache = reanchor::Cache::open(store.repo(), at);
    let view = build_view(store, &thread, &folded, &mut cache, &seen, me.as_ref())?;
    cache.save();
    Ok(view)
}

/// The thread a prefix names: the thread ID, or the ID of any comment or
/// reply in it.
pub fn resolve_thread(store: &Store, prefix: &str) -> Result<ThreadId> {
    Ok(find_message(store, prefix)?.0.id)
}

/// How many threads have messages the user hasn't seen.
pub fn threads_with_news(store: &Store) -> Result<usize> {
    let seen = store.seen_event_ids()?;
    let me = identity(store.repo()).ok();
    Ok(news_count(&store.threads()?, &seen, me.as_ref()))
}

fn news_count(threads: &[ThreadRecord], seen: &BTreeSet<EventId>, me: Option<&Author>) -> usize {
    threads.iter().filter(|t| !unseen_ids(t, seen, me).is_empty()).count()
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

/// How much of the anchored change to render, following git log's levels.
#[derive(Clone, Copy, PartialEq)]
pub enum SnippetMode {
    /// Bounded by default: clipped hunks for line anchors, a diffstat when
    /// the anchor covers a whole file or change.
    Auto,
    /// git log -p: the full patch (line anchors keep their marks).
    Patch,
    /// git log --stat: the diffstat, whatever the anchor.
    Stat,
}

/// The context a thread is about. Comments target diffs, so this is the
/// anchored change itself, as git reports it. When there is no change to
/// show — snapshot annotations (an empty diff), or anchors whose lines
/// predate the diff-intersection rule — the file stands in: at the
/// placement's location when the code was found on the target commit (the
/// excerpt then agrees with the `Current:`/`Anchor:` line above it), the
/// original blob only when it wasn't. `None` when there is nothing to show.
pub fn anchor_context(
    store: &Store,
    view: &ThreadView,
    mode: SnippetMode,
) -> Result<Option<AnchorContext>> {
    let anchor = view.effective_anchor();
    let target = ObjectId::from_hex(view.at.as_str().as_bytes())?;
    if anchor.diff.base != anchor.diff.head {
        let (base, head) = (anchor.diff.base.as_str(), anchor.diff.head.as_str());
        let stat = mode == SnippetMode::Stat
            || (mode == SnippetMode::Auto && anchor.lines.is_none());
        let mut args = if stat { vec!["diff", "--stat", base, head] } else { vec!["diff", base, head] };
        if let Some(path) = &anchor.path {
            args.extend(["--", path]);
        }
        let text = git(store.repo().git_dir(), &args)?;
        if stat {
            if !text.is_empty() {
                return Ok(Some(AnchorContext::Stat(text)));
            }
        } else {
            let side = anchor.side.unwrap_or(Side::New);
            let clip = mode != SnippetMode::Patch;
            let headers = anchor.path.is_none();
            // Only a diff the renderer would show something of counts —
            // same hunk-overlap rule the renderer clips by. An empty one
            // falls through to the file excerpt.
            let spans = hunk_spans(&text, side);
            let overlaps = |&(start, len): &(u32, u32)| {
                anchor.lines.is_none_or(|want| {
                    let end = start + len.max(1) - 1;
                    want.start <= end && want.end >= start
                })
            };
            let usable = if headers {
                !text.is_empty()
            } else if clip {
                spans.iter().any(overlaps)
            } else {
                !spans.is_empty()
            };
            if usable {
                return Ok(Some(AnchorContext::Diff {
                    text,
                    side,
                    lines: anchor.lines,
                    headers,
                    clip,
                }));
            }
        }
    }
    if let Reanchor::Located { path, lines: Some(lines), .. } = &view.placement
        && let Some(blob_id) = reanchor::blob_at(store.repo(), target, path)?
    {
        let content = reanchor::blob_content(store.repo(), blob_id)?;
        return Ok(Some(AnchorContext::Excerpt { content, lines: *lines }));
    }
    if let (Some(lines), Some(blob)) = (anchor.lines, &anchor.blob) {
        let blob_id = ObjectId::from_hex(blob.as_str().as_bytes())?;
        let content = reanchor::blob_content(store.repo(), blob_id)?;
        return Ok(Some(AnchorContext::Excerpt { content, lines }));
    }
    Ok(None)
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

/// Where a thread was re-pinned to, if it ever was (SPEC.md §2.4 rule 5).
/// A moved thread is findable by both addresses: where it was discussed
/// (its immutable anchor) and where it lives now.
fn moved_anchor(thread: &ThreadRecord) -> Option<Anchor> {
    fold_thread(thread.events.clone()).moved.and_then(|(_, e)| e.anchor)
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

/// Whether an anchored head belongs to `base..head`. Identity first; when
/// the commit isn't one of the range's, a patch-id twin counts instead:
/// `git patch-id --stable` is unchanged by a rebase, a rebased merge, or a
/// one-commit squash, and retention (SPEC.md §5.2) keeps the original
/// commit readable, so the same diff re-committed under a new SHA is found
/// even by a reader who never saw the rewrite. Merge commits (no usable
/// patch) and unreadable commits simply never twin-match. Patch-ids are
/// computed lazily: a listing where nothing was rewritten never pays for
/// them.
struct ChangeMembership<'a> {
    dir: &'a Path,
    base: ObjectId,
    head: ObjectId,
    commits: std::collections::HashSet<String>,
    /// Patch-ids of the range's own commits, filled on first identity miss.
    twins: Option<std::collections::HashSet<String>>,
    /// Twin verdicts for heads that missed on identity.
    checked: std::collections::HashMap<String, bool>,
}

impl<'a> ChangeMembership<'a> {
    fn new(repo: &'a gix::Repository, base: ObjectId, head: ObjectId) -> Result<Self> {
        Ok(Self {
            dir: repo.git_dir(),
            base,
            head,
            commits: range_commits(repo, base, head)?,
            twins: None,
            checked: Default::default(),
        })
    }

    fn contains(&mut self, head: &str) -> bool {
        if self.commits.contains(head) {
            return true;
        }
        if let Some(&hit) = self.checked.get(head) {
            return hit;
        }
        let hit = patch_ids(self.dir, &["log", "-1", "-p", head])
            .ok()
            .and_then(|ids| ids.into_iter().next())
            .is_some_and(|(id, _)| self.twins().contains(&id));
        self.checked.insert(head.to_string(), hit);
        hit
    }

    fn twins(&mut self) -> &std::collections::HashSet<String> {
        if self.twins.is_none() {
            let head = self.head.to_string();
            let range = format!("{}..{}", self.base, self.head);
            // Same commit set as range_commits: an empty diff is its own commit.
            let args: Vec<&str> = if self.base == self.head {
                vec!["log", "-1", "-p", &head]
            } else {
                vec!["log", "-p", &range]
            };
            let ids = patch_ids(self.dir, &args).unwrap_or_default();
            self.twins = Some(ids.into_iter().map(|(id, _)| id).collect());
        }
        self.twins.as_ref().expect("just filled")
    }
}

/// `git patch-id --stable` over `git log <args>`: (patch-id, commit) pairs,
/// one per commit that has a patch. The two processes share a real pipe —
/// nothing is buffered here, so a range of any size streams.
fn patch_ids(dir: &Path, log_args: &[&str]) -> Result<Vec<(String, String)>> {
    let mut log = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(log_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to run git log")?;
    let ids = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["patch-id", "--stable"])
        .stdin(Stdio::from(log.stdout.take().expect("stdout is piped")))
        .stderr(Stdio::null())
        .output()
        .context("failed to run git patch-id")?;
    if !log.wait()?.success() || !ids.status.success() {
        bail!("git log -p | git patch-id failed for {log_args:?}");
    }
    Ok(String::from_utf8_lossy(&ids.stdout)
        .lines()
        .filter_map(|line| {
            let (id, commit) = line.split_once(' ')?;
            Some((id.to_string(), commit.to_string()))
        })
        .collect())
}

/// The commits making up `base..head` — [`ChangeMembership`]'s identity
/// layer. An empty diff is just its own commit.
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

/// Discard one drafted event before publishing. Discarding a drafted
/// thread's root discards the whole draft thread.
pub fn discard(store: &Store, prefix: &str) -> Result<Discarded> {
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
    Ok(if removed > 1 || event_id == thread_id {
        Discarded::Thread { thread: thread_id, events: removed }
    } else {
        Discarded::Event(event_id)
    })
}

/// What `discard` dropped.
#[derive(Clone, Debug)]
pub enum Discarded {
    /// The whole draft thread went with its discarded root.
    Thread { thread: ThreadId, events: usize },
    Event(EventId),
}

/// Discard every unpublished draft. Returns how many events were dropped.
pub fn discard_all(store: &Store) -> Result<usize> {
    store.discard_all_drafts()
}

/// The threads counterpart of `git status`: what's drafted (awaiting
/// `commit`) and what's sealed locally but not yet on each remote (awaiting
/// `push`). Unpushed counts compare snapshots, not commit graphs — content
/// addressing makes event sets directly comparable across refs.
pub fn status(store: &Store) -> Result<StatusView> {
    let threads = store.threads()?;

    let mut drafted: Vec<ThreadDrafts> = Vec::new();
    for thread in &threads {
        if thread.drafts.is_empty() {
            continue;
        }
        let mut events: Vec<&(EventId, Event)> =
            thread.events.iter().filter(|(id, _)| thread.drafts.contains(id)).collect();
        events.sort_by_key(|(id, event)| (event.ts.clone(), id.clone()));
        drafted.push(ThreadDrafts {
            thread: thread.id.clone(),
            anchor: thread.anchor.clone(),
            events: events
                .into_iter()
                .map(|(id, event)| DraftedEvent {
                    id: id.clone(),
                    kind: event.kind.clone(),
                    body: event.body.clone(),
                })
                .collect(),
        });
    }

    let local = match store.tip()? {
        Some(tip) => store.event_ids(tip)?,
        None => Default::default(),
    };
    let workdir = workdir(store)?;
    let mut remotes: Vec<RemoteStatus> = Vec::new();
    for remote in git(&workdir, &["remote"])?.lines() {
        let unpushed = match store.tracking_tip(remote)? {
            Some(tracking) => {
                let known = store.event_ids(tracking)?;
                local.difference(&known).count()
            }
            // Never fetched from this remote: everything sealed is unshared.
            None => local.len(),
        };
        remotes.push(RemoteStatus { remote: remote.to_string(), unpushed });
    }

    let seen = store.seen_event_ids()?;
    let me = identity(store.repo()).ok();
    let threads_with_news = news_count(&threads, &seen, me.as_ref());
    Ok(StatusView { drafted, remotes, threads_with_news })
}

/// Fetch the remote's threads data into the tracking ref and integrate it
/// into the local ref (SPEC.md §7.2 steps 1–2). `None` when the remote has
/// no threads data yet.
pub fn pull(store: &Store, remote: &str) -> Result<Option<Integration>> {
    fetch_and_integrate(store, remote)
}

/// Seal all drafts into the local data ref as one commit (SPEC.md §5.2
/// session batching). Local only — `push` shares it. `None` when there was
/// nothing to commit.
pub fn commit(store: &Store) -> Result<Option<PromotedDrafts>> {
    store.commit_drafts()
}

/// Outcome of the publish loop.
#[derive(Clone, Copy, Debug)]
pub enum PushOutcome {
    Pushed,
    /// No local threads data exists yet.
    NothingToPush,
}

/// The publish loop (SPEC.md §7.2): integrate remote state, push the local
/// data ref, and on a lost race re-integrate and retry. Drafts are not
/// included — `commit` seals them first.
pub fn push(store: &Store, remote: &str) -> Result<PushOutcome> {
    let workdir = workdir(store)?;
    const MAX_ATTEMPTS: usize = 5;
    for attempt in 1..=MAX_ATTEMPTS {
        fetch_and_integrate(store, remote)?;
        let Some(tip) = store.tip()? else {
            return Ok(PushOutcome::NothingToPush);
        };
        match git(&workdir, &["push", remote, "refs/threads/data:refs/threads/data"]) {
            Ok(_) => {
                store.record_pushed_tip(remote, tip)?;
                return Ok(PushOutcome::Pushed);
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
/// threads data yet (the glob refspec matches nothing and the fetch is a
/// no-op). The explicit refspec makes this work even without the configured
/// refspec from `init`.
fn fetch_and_integrate(store: &Store, remote: &str) -> Result<Option<Integration>> {
    let workdir = workdir(store)?;
    let refspec = fetch_refspec(remote);
    git(&workdir, &["fetch", remote, &refspec])?;
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
        anchor: None,
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
