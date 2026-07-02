//! Storage layer (SPEC.md §5): the snapshot tree on `refs/threads/data`.
//!
//! All data lives on a single ref whose tip tree is a full snapshot:
//!
//! ```text
//! threads/<shard>/<thread-id>/
//!     anchor.json
//!     events/<event-id>.json
//! ```
//!
//! Writers only ever add content-addressed files, so concurrent histories
//! merge as a conflict-free tree union (§7.2, future slice). Ref updates use
//! compare-and-swap on the expected tip, which is what the publish retry loop
//! relies on.

use anyhow::{Context, Result, anyhow, bail};
use git_threads_core::{Anchor, Event, EventId, EventKind, ThreadId, to_canonical_json};
use gix::ObjectId;
use gix::objs::tree::EntryKind;
use gix::refs::transaction::PreviousValue;
use std::collections::BTreeSet;
use std::path::Path;

pub const DATA_REF: &str = "refs/threads/data";

pub struct Store {
    repo: gix::Repository,
}

/// A thread as read from the snapshot tree.
pub struct ThreadRecord {
    pub id: ThreadId,
    pub anchor: Anchor,
    pub events: Vec<(EventId, Event)>,
}

/// A thread to create: its immutable anchor, the root `comment`, and any
/// further events published in the same batch.
pub struct NewThread {
    pub anchor: Anchor,
    pub root: Event,
    pub events: Vec<Event>,
}

/// Events to append to an existing thread.
pub struct Append {
    pub thread: ThreadId,
    pub events: Vec<Event>,
}

/// One publish operation — batched into a single commit (SPEC.md §5.2).
#[derive(Default)]
pub struct Batch {
    pub new_threads: Vec<NewThread>,
    pub appends: Vec<Append>,
}

impl Batch {
    pub fn is_empty(&self) -> bool {
        self.new_threads.is_empty() && self.appends.is_empty()
    }
}

impl Store {
    pub fn discover() -> Result<Self> {
        let repo = gix::discover(".").context("not inside a git repository")?;
        Ok(Store { repo })
    }

    pub fn open(path: &Path) -> Result<Self> {
        let repo = gix::open(path).with_context(|| format!("failed to open repo at {}", path.display()))?;
        Ok(Store { repo })
    }

    /// Current tip of `refs/threads/data`, if the ref exists.
    pub fn tip(&self) -> Result<Option<ObjectId>> {
        match self.repo.try_find_reference(DATA_REF)? {
            Some(mut reference) => Ok(Some(reference.peel_to_id()?.detach())),
            None => Ok(None),
        }
    }

    /// All threads in the current snapshot.
    pub fn threads(&self) -> Result<Vec<ThreadRecord>> {
        let Some(tree) = self.tip_tree()? else {
            return Ok(Vec::new());
        };
        let Some(threads_tree) = self.subtree(&tree, "threads")? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for shard_entry in threads_tree.iter() {
            let shard_entry = shard_entry?;
            let shard_tree = self.repo.find_tree(shard_entry.object_id())?;
            for thread_entry in shard_tree.iter() {
                let thread_entry = thread_entry?;
                let name = std::str::from_utf8(thread_entry.filename())
                    .context("non-UTF-8 thread directory name")?;
                let id = EventId::from_hex(name)?;
                out.push(self.read_thread_tree(id, thread_entry.object_id())?);
            }
        }
        Ok(out)
    }

    /// A single thread by ID, if present in the snapshot.
    pub fn read_thread(&self, id: &ThreadId) -> Result<Option<ThreadRecord>> {
        let Some(tree) = self.tip_tree()? else {
            return Ok(None);
        };
        match tree.lookup_entry_by_path(thread_dir(id))? {
            Some(entry) => Ok(Some(self.read_thread_tree(id.clone(), entry.object_id())?)),
            None => Ok(None),
        }
    }

    /// Write a batch as one commit on `refs/threads/data` (SPEC.md §5.2) and
    /// return the new tip. Returns the current tip unchanged if the batch adds
    /// nothing new (content addressing makes duplicate publishes no-ops).
    pub fn write(&self, batch: &Batch) -> Result<ObjectId> {
        if batch.is_empty() {
            bail!("empty batch");
        }
        let tip = self.tip()?;
        let base_tree_id = match tip {
            Some(tip) => self.repo.find_commit(tip)?.tree_id()?.detach(),
            None => self.repo.empty_tree().id().detach(),
        };
        let base_tree = self.repo.find_tree(base_tree_id)?;
        let mut editor = self.repo.edit_tree(base_tree_id)?;
        let mut anchored_heads: BTreeSet<ObjectId> = BTreeSet::new();
        let mut touched: BTreeSet<ThreadId> = BTreeSet::new();
        let mut event_count = 0usize;

        for new_thread in &batch.new_threads {
            new_thread.anchor.validate()?;
            new_thread.root.validate()?;
            if new_thread.root.kind != EventKind::Comment {
                bail!("a thread root must be a comment event (SPEC.md §2.1)");
            }
            let thread_id = new_thread.root.id()?;
            let anchor_blob = self.repo.write_blob(to_canonical_json(&new_thread.anchor)?)?;
            editor.upsert(
                format!("{}/anchor.json", thread_dir(&thread_id)),
                EntryKind::Blob,
                anchor_blob,
            )?;
            for event in std::iter::once(&new_thread.root).chain(&new_thread.events) {
                self.put_event(&mut editor, &thread_id, event)?;
                event_count += 1;
            }
            // Anchored-commit retention (SPEC.md §5.2): the discussed commit
            // becomes an extra parent so reachability keeps it alive.
            let head = git_oid(new_thread.anchor.diff.head.as_str())?;
            self.repo
                .find_commit(head)
                .with_context(|| format!("anchored commit {head} not present locally"))?;
            anchored_heads.insert(head);
            touched.insert(thread_id);
        }

        for append in &batch.appends {
            if base_tree
                .lookup_entry_by_path(format!("{}/anchor.json", thread_dir(&append.thread)))?
                .is_none()
                && !touched.contains(&append.thread)
            {
                bail!("thread {} not found in {DATA_REF}", append.thread);
            }
            for event in &append.events {
                event.validate()?;
                if event.kind == EventKind::Comment {
                    bail!("only a thread root may be a comment event; use a reply");
                }
                self.put_event(&mut editor, &append.thread, event)?;
                event_count += 1;
            }
            touched.insert(append.thread.clone());
        }

        let new_tree = editor.write()?.detach();
        if let Some(tip) = tip
            && new_tree == base_tree_id
        {
            return Ok(tip);
        }

        let mut parents: Vec<ObjectId> = Vec::new();
        parents.extend(tip);
        parents.extend(anchored_heads.into_iter().filter(|head| Some(*head) != tip));

        let message = format!("threads: {event_count} events in {} threads", touched.len());
        self.commit(&message, new_tree, parents, tip)
    }

    /// Create the commit object and update the ref with compare-and-swap
    /// semantics on the expected previous tip. Done manually rather than via
    /// `Repository::commit` because our first parent is not always the ref's
    /// previous value (an initial write can still carry anchored parents).
    fn commit(
        &self,
        message: &str,
        tree: ObjectId,
        parents: Vec<ObjectId>,
        expected_tip: Option<ObjectId>,
    ) -> Result<ObjectId> {
        let committer = self
            .repo
            .committer()
            .ok_or_else(|| anyhow!("no committer identity configured (set user.name and user.email)"))?
            .map_err(|e| anyhow!("invalid committer identity: {e}"))?
            .to_owned()?;
        let commit = gix::objs::Commit {
            tree,
            parents: parents.into(),
            author: committer.clone(),
            committer,
            encoding: None,
            message: message.into(),
            extra_headers: Vec::new(),
        };
        let commit_id = self.repo.write_object(&commit)?.detach();
        let expected = match expected_tip {
            Some(tip) => PreviousValue::MustExistAndMatch(tip.into()),
            None => PreviousValue::MustNotExist,
        };
        self.repo
            .reference(DATA_REF, commit_id, expected, message)
            .context("failed to update refs/threads/data (concurrent write?)")?;
        Ok(commit_id)
    }

    fn put_event(
        &self,
        editor: &mut gix::object::tree::Editor<'_>,
        thread: &ThreadId,
        event: &Event,
    ) -> Result<()> {
        event.validate()?;
        let bytes = event.canonical_json()?;
        let event_id = EventId::compute(&bytes);
        let blob = self.repo.write_blob(bytes)?;
        editor.upsert(
            format!("{}/events/{}.json", thread_dir(thread), event_id),
            EntryKind::Blob,
            blob,
        )?;
        Ok(())
    }

    fn tip_tree(&self) -> Result<Option<gix::Tree<'_>>> {
        match self.tip()? {
            Some(tip) => Ok(Some(self.repo.find_commit(tip)?.tree()?)),
            None => Ok(None),
        }
    }

    fn subtree<'a>(&'a self, tree: &gix::Tree<'a>, name: &str) -> Result<Option<gix::Tree<'a>>> {
        match tree.lookup_entry_by_path(name)? {
            Some(entry) => Ok(Some(self.repo.find_tree(entry.object_id())?)),
            None => Ok(None),
        }
    }

    fn read_thread_tree(&self, id: ThreadId, tree_id: ObjectId) -> Result<ThreadRecord> {
        let tree = self.repo.find_tree(tree_id)?;
        let anchor_entry = tree
            .lookup_entry_by_path("anchor.json")?
            .ok_or_else(|| anyhow!("thread {id} has no anchor.json"))?;
        let anchor: Anchor = serde_json::from_slice(&self.repo.find_blob(anchor_entry.object_id())?.data)
            .with_context(|| format!("invalid anchor.json in thread {id}"))?;
        let mut events = Vec::new();
        if let Some(events_tree) = self.subtree(&tree, "events")? {
            for entry in events_tree.iter() {
                let entry = entry?;
                let name = std::str::from_utf8(entry.filename()).context("non-UTF-8 event filename")?;
                let Some(stem) = name.strip_suffix(".json") else {
                    continue;
                };
                let event_id = EventId::from_hex(stem)?;
                let event: Event = serde_json::from_slice(&self.repo.find_blob(entry.object_id())?.data)
                    .with_context(|| format!("invalid event {event_id} in thread {id}"))?;
                events.push((event_id, event));
            }
        }
        Ok(ThreadRecord { id, anchor, events })
    }
}

/// `threads/<first-2-hex>/<thread-id>` (SPEC.md §5.1).
fn thread_dir(id: &ThreadId) -> String {
    format!("threads/{}/{}", &id.as_str()[..2], id)
}

fn git_oid(hex: &str) -> Result<ObjectId> {
    ObjectId::from_hex(hex.as_bytes()).with_context(|| format!("invalid git object id {hex:?}"))
}
