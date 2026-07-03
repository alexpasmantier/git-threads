use crate::store::{Append, Batch, NewThread, Store, ThreadRecord};
use anyhow::{Context, Result, anyhow, bail};
use git_threads_core::{
    Anchor, AnchorKind, Author, DiffRef, Event, EventId, EventKind, GitOid, LineRange, Side,
    ThreadId, Timestamp, fold_thread,
};
use gix::ObjectId;
use std::path::Path;
use std::process::Command;

const FETCH_REFSPEC: &str = "+refs/threads/*:refs/threads/*";

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
    let key = format!("remote.{remote}.fetch");
    let existing = git(&workdir, &["config", "--get-all", &key]).unwrap_or_default();
    if existing.lines().any(|line| line == FETCH_REFSPEC) {
        println!("{key} already includes {FETCH_REFSPEC}");
    } else {
        git(&workdir, &["config", "--add", &key, FETCH_REFSPEC])?;
        println!("configured {key} += {FETCH_REFSPEC}");
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

/// List threads in the current snapshot with their folded state.
pub fn list(store: &Store) -> Result<()> {
    let mut threads = store.threads()?;
    if threads.is_empty() {
        println!("no threads");
        return Ok(());
    }
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
        let title = folded
            .events
            .first()
            .and_then(|root| root.effective_body.as_deref())
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("");
        println!(
            "{}  [{status}] {location}  ({} message{})  {title}",
            short(&thread.id),
            folded.events.len(),
            if folded.events.len() == 1 { "" } else { "s" },
        );
    }
    Ok(())
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

fn git(dir: &Path, args: &[&str]) -> Result<String> {
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
