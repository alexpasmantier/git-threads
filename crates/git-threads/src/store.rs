//! Storage layer (SPEC.md §5): the snapshot tree on `refs/threads/data`,
//! plus the client-local drafts staging area.
//!
//! All shared data lives on a single ref whose tip tree is a full snapshot:
//!
//! ```text
//! threads/<shard>/<thread-id>/
//!     anchor.json
//!     events/<event-id>.json
//! ```
//!
//! New events are first staged on `refs/threads/drafts` — a local-only ref
//! with the same tree layout, holding exactly the unpublished delta. Publish
//! promotes the whole drafts tree into the data ref as one commit (SPEC.md
//! §5.2: one commit per publish session), so shared history stays coarse
//! no matter how many commands produced the events.
//!
//! Writers only ever add content-addressed files, so concurrent histories
//! merge as a conflict-free tree union (§7.2). Ref updates use
//! compare-and-swap on the expected tip, which is what the publish retry
//! loop relies on.

use anyhow::{Context, Result, anyhow, bail};
use git_threads_core::{Anchor, Event, EventId, EventKind, ThreadId, to_canonical_json};
use gix::ObjectId;
use gix::objs::tree::EntryKind;
use gix::refs::transaction::PreviousValue;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const DATA_REF: &str = "refs/threads/data";
/// Local-only staging ref for drafted events. Same tree layout as
/// [`DATA_REF`], but holding only the unpublished delta; its commits carry no
/// history parent, only anchored-commit retention pins. Never fetched or
/// pushed (the configured refspec maps only `data`).
pub const DRAFTS_REF: &str = "refs/threads/drafts";

pub struct Store {
    repo: gix::Repository,
}

/// A thread as read from the snapshot tree, with unpublished drafts overlaid.
#[derive(Clone)]
pub struct ThreadRecord {
    pub id: ThreadId,
    pub anchor: Anchor,
    pub events: Vec<(EventId, Event)>,
    /// IDs of events (and possibly the root itself) that exist only as
    /// local drafts, not yet published.
    pub drafts: BTreeSet<EventId>,
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

/// One write operation: new threads and/or appends.
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

/// What a drafts promotion put on the data ref.
pub struct PromotedDrafts {
    pub events: usize,
    pub threads: usize,
}

/// Tallies from applying a batch to a tree.
struct BatchStats {
    anchored_heads: BTreeSet<ObjectId>,
    touched: BTreeSet<ThreadId>,
    events: usize,
}

impl Store {
    pub fn repo(&self) -> &gix::Repository {
        &self.repo
    }

    pub fn discover() -> Result<Self> {
        let repo = gix::discover(".").context("not inside a git repository")?;
        Ok(Store { repo })
    }

    pub fn open(path: &Path) -> Result<Self> {
        let repo = gix::open(path).with_context(|| format!("failed to open repo at {}", path.display()))?;
        Ok(Store { repo })
    }

    fn ref_tip(&self, name: &str) -> Result<Option<ObjectId>> {
        match self.repo.try_find_reference(name)? {
            Some(mut reference) => Ok(Some(reference.peel_to_id()?.detach())),
            None => Ok(None),
        }
    }

    /// Current tip of `refs/threads/data`, if the ref exists.
    pub fn tip(&self) -> Result<Option<ObjectId>> {
        self.ref_tip(DATA_REF)
    }

    /// Current tip of the local drafts ref, if any drafts exist.
    pub fn drafts_tip(&self) -> Result<Option<ObjectId>> {
        self.ref_tip(DRAFTS_REF)
    }

    /// All threads: the published snapshot with local drafts overlaid.
    pub fn threads(&self) -> Result<Vec<ThreadRecord>> {
        let mut out: BTreeMap<ThreadId, ThreadRecord> = BTreeMap::new();
        if let Some(tree) = self.ref_tree(DATA_REF)? {
            self.collect_threads(&tree, &mut out, false)?;
        }
        if let Some(tree) = self.ref_tree(DRAFTS_REF)? {
            self.collect_threads(&tree, &mut out, true)?;
        }
        Ok(out.into_values().collect())
    }

    /// A single thread by ID (published state plus drafts), if present.
    pub fn read_thread(&self, id: &ThreadId) -> Result<Option<ThreadRecord>> {
        Ok(self
            .threads()?
            .into_iter()
            .find(|thread| thread.id == *id))
    }

    fn collect_threads(
        &self,
        root: &gix::Tree<'_>,
        out: &mut BTreeMap<ThreadId, ThreadRecord>,
        draft: bool,
    ) -> Result<()> {
        let Some(threads_tree) = self.subtree(root, "threads")? else {
            return Ok(());
        };
        for shard_entry in threads_tree.iter() {
            let shard_tree = self.repo.find_tree(shard_entry?.object_id())?;
            for thread_entry in shard_tree.iter() {
                let thread_entry = thread_entry?;
                let name = std::str::from_utf8(thread_entry.filename())
                    .context("non-UTF-8 thread directory name")?;
                let id = EventId::from_hex(name)?;
                let (anchor, events) = self.read_thread_dir(&id, thread_entry.object_id())?;
                match out.get_mut(&id) {
                    Some(record) => {
                        for (event_id, event) in events {
                            if !record.events.iter().any(|(id, _)| *id == event_id) {
                                if draft {
                                    record.drafts.insert(event_id.clone());
                                }
                                record.events.push((event_id, event));
                            }
                        }
                    }
                    None => {
                        let anchor = anchor.ok_or_else(|| {
                            anyhow!("thread {id} has no anchor.json and no published state")
                        })?;
                        let drafts = if draft {
                            events.iter().map(|(id, _)| id.clone()).collect()
                        } else {
                            BTreeSet::new()
                        };
                        out.insert(id.clone(), ThreadRecord { id, anchor, events, drafts });
                    }
                }
            }
        }
        Ok(())
    }

    /// Write a batch directly as one commit on `refs/threads/data` (SPEC.md
    /// §5.2) and return the new tip. Returns the current tip unchanged if the
    /// batch adds nothing new (content addressing makes duplicates no-ops).
    /// The CLI stages through [`Store::draft`] instead; this is the direct
    /// path for tools that batch on their own.
    pub fn write(&self, batch: &Batch) -> Result<ObjectId> {
        if batch.is_empty() {
            bail!("empty batch");
        }
        let tip = self.tip()?;
        let base_tree_id = self.tree_id_of(tip)?;
        let base_tree = self.repo.find_tree(base_tree_id)?;
        let mut editor = self.repo.edit_tree(base_tree_id)?;
        let stats = self.apply_batch(&mut editor, batch, &[&base_tree])?;
        let new_tree = editor.write()?.detach();
        if let Some(tip) = tip
            && new_tree == base_tree_id
        {
            return Ok(tip);
        }

        let mut parents: Vec<ObjectId> = Vec::new();
        parents.extend(tip);
        parents.extend(stats.anchored_heads.into_iter().filter(|head| Some(*head) != tip));

        let message =
            format!("threads: {} events in {} threads", stats.events, stats.touched.len());
        self.commit(DATA_REF, &message, new_tree, parents, tip)
    }

    /// Stage a batch on the local drafts ref. Drafts commits carry no history
    /// parent — their parents are only the anchored-commit retention pins
    /// (accumulated across draft writes so pinned commits stay reachable
    /// while drafted).
    pub fn draft(&self, batch: &Batch) -> Result<ObjectId> {
        if batch.is_empty() {
            bail!("empty batch");
        }
        let drafts_tip = self.drafts_tip()?;
        let base_tree_id = self.tree_id_of(drafts_tip)?;
        let base_tree = self.repo.find_tree(base_tree_id)?;
        let data_tree = self.ref_tree(DATA_REF)?;
        let mut existing: Vec<&gix::Tree<'_>> = vec![&base_tree];
        if let Some(tree) = &data_tree {
            existing.push(tree);
        }
        let mut editor = self.repo.edit_tree(base_tree_id)?;
        let stats = self.apply_batch(&mut editor, batch, &existing)?;
        let new_tree = editor.write()?.detach();
        if let Some(tip) = drafts_tip
            && new_tree == base_tree_id
        {
            return Ok(tip);
        }

        let mut parents: BTreeSet<ObjectId> = stats.anchored_heads;
        if let Some(tip) = drafts_tip {
            parents.extend(self.repo.find_commit(tip)?.parent_ids().map(|id| id.detach()));
        }
        self.commit(DRAFTS_REF, "threads: drafts", new_tree, parents.into_iter().collect(), drafts_tip)
    }

    /// Promote all drafts into `refs/threads/data` as one commit (SPEC.md
    /// §5.2: one commit per publish session) and delete the drafts ref.
    /// `None` when there are no drafts or they add nothing new.
    pub fn commit_drafts(&self) -> Result<Option<PromotedDrafts>> {
        let Some(drafts_tip) = self.drafts_tip()? else {
            return Ok(None);
        };
        let drafts_commit = self.repo.find_commit(drafts_tip)?;
        let drafts_tree = drafts_commit.tree_id()?.detach();
        let tip = self.tip()?;
        let base_tree_id = self.tree_id_of(tip)?;
        let merged_tree = self.union_trees(base_tree_id, drafts_tree)?;
        if merged_tree == base_tree_id {
            // Every draft was already published (duplicate content).
            self.delete_ref(DRAFTS_REF)?;
            return Ok(None);
        }

        let (_, events, threads) = self.tree_stats(drafts_tree)?;
        let mut parents: Vec<ObjectId> = Vec::new();
        parents.extend(tip);
        parents.extend(
            drafts_commit
                .parent_ids()
                .map(|id| id.detach())
                .filter(|pin| Some(*pin) != tip),
        );
        let message = format!("threads: {events} events in {threads} threads");
        self.commit(DATA_REF, &message, merged_tree, parents, tip)?;
        self.delete_ref(DRAFTS_REF)?;
        Ok(Some(PromotedDrafts { events, threads }))
    }

    /// Remove one drafted event. Discarding a drafted thread root — or the
    /// last drafted event of a thread that has no drafted anchor — removes
    /// the whole draft directory. Returns how many drafted events were
    /// removed.
    pub fn discard_draft(&self, thread: &ThreadId, event: &EventId) -> Result<usize> {
        let Some(drafts_tip) = self.drafts_tip()? else {
            bail!("no drafts");
        };
        let drafts_tree_id = self.tree_id_of(Some(drafts_tip))?;
        let drafts_tree = self.repo.find_tree(drafts_tree_id)?;
        let dir = thread_dir(thread);
        let Some(dir_entry) = drafts_tree.lookup_entry_by_path(&dir)? else {
            bail!("{event} is not a draft");
        };
        let dir_tree = self.repo.find_tree(dir_entry.object_id())?;
        if dir_tree
            .lookup_entry_by_path(format!("events/{event}.json"))?
            .is_none()
        {
            bail!("{event} is not a draft");
        }
        let has_anchor = dir_tree.lookup_entry_by_path("anchor.json")?.is_some();
        let event_count = match self.subtree(&dir_tree, "events")? {
            Some(events) => events.iter().count(),
            None => 0,
        };

        let mut editor = self.repo.edit_tree(drafts_tree_id)?;
        // A discarded root takes its whole draft thread with it; a lone
        // drafted append leaves no reason to keep the directory.
        let removed = if event.as_str() == thread.as_str() || (event_count == 1 && !has_anchor) {
            editor.remove(dir.as_str())?;
            event_count
        } else {
            editor.remove(format!("{dir}/events/{event}.json").as_str())?;
            1
        };
        let new_tree = editor.write()?.detach();

        let (anchors, events, _) = self.tree_stats(new_tree)?;
        if anchors == 0 && events == 0 {
            self.delete_ref(DRAFTS_REF)?;
        } else {
            // Retention pins are kept as-is; a stale pin is harmless.
            let parents = self
                .repo
                .find_commit(drafts_tip)?
                .parent_ids()
                .map(|id| id.detach())
                .collect();
            self.commit(DRAFTS_REF, "threads: drafts", new_tree, parents, Some(drafts_tip))?;
        }
        Ok(removed)
    }

    /// Drop every draft. Returns how many drafted events were discarded.
    pub fn discard_all_drafts(&self) -> Result<usize> {
        let Some(drafts_tip) = self.drafts_tip()? else {
            return Ok(0);
        };
        let drafts_tree = self.repo.find_commit(drafts_tip)?.tree_id()?.detach();
        let (_, events, _) = self.tree_stats(drafts_tree)?;
        self.delete_ref(DRAFTS_REF)?;
        Ok(events)
    }

    /// Apply a batch's writes to `editor`, validating appends against the
    /// trees in `existing` (plus threads created earlier in the same batch).
    fn apply_batch(
        &self,
        editor: &mut gix::object::tree::Editor<'_>,
        batch: &Batch,
        existing: &[&gix::Tree<'_>],
    ) -> Result<BatchStats> {
        let mut anchored_heads: BTreeSet<ObjectId> = BTreeSet::new();
        let mut touched: BTreeSet<ThreadId> = BTreeSet::new();
        let mut events = 0usize;

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
                self.put_event(editor, &thread_id, event)?;
                events += 1;
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
            let anchor_path = format!("{}/anchor.json", thread_dir(&append.thread));
            let mut known = touched.contains(&append.thread);
            for tree in existing {
                known = known || tree.lookup_entry_by_path(&anchor_path)?.is_some();
            }
            if !known {
                bail!("thread {} not found in {DATA_REF}", append.thread);
            }
            for event in &append.events {
                event.validate()?;
                if event.kind == EventKind::Comment {
                    bail!("only a thread root may be a comment event; use a reply");
                }
                self.put_event(editor, &append.thread, event)?;
                events += 1;
            }
            touched.insert(append.thread.clone());
        }
        Ok(BatchStats { anchored_heads, touched, events })
    }

    /// Create the commit object and update `refname` with compare-and-swap
    /// semantics on the expected previous tip. Done manually rather than via
    /// `Repository::commit` because our first parent is not always the ref's
    /// previous value (drafts commits have no history parent at all).
    fn commit(
        &self,
        refname: &str,
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
        self.update_ref(refname, expected_tip, commit_id, message)?;
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

    /// The tree of `tip`, or the empty tree when there is no tip yet.
    fn tree_id_of(&self, tip: Option<ObjectId>) -> Result<ObjectId> {
        Ok(match tip {
            Some(tip) => self.repo.find_commit(tip)?.tree_id()?.detach(),
            None => self.repo.empty_tree().id().detach(),
        })
    }

    fn ref_tree(&self, name: &str) -> Result<Option<gix::Tree<'_>>> {
        match self.ref_tip(name)? {
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

    /// `(anchor files, event files, thread dirs with events)` under `threads/`.
    fn tree_stats(&self, tree_id: ObjectId) -> Result<(usize, usize, usize)> {
        let tree = self.repo.find_tree(tree_id)?;
        let Some(threads_tree) = self.subtree(&tree, "threads")? else {
            return Ok((0, 0, 0));
        };
        let (mut anchors, mut events, mut threads) = (0, 0, 0);
        for shard_entry in threads_tree.iter() {
            let shard_tree = self.repo.find_tree(shard_entry?.object_id())?;
            for thread_entry in shard_tree.iter() {
                let dir = self.repo.find_tree(thread_entry?.object_id())?;
                if dir.lookup_entry_by_path("anchor.json")?.is_some() {
                    anchors += 1;
                }
                let count = match self.subtree(&dir, "events")? {
                    Some(events_tree) => events_tree.iter().count(),
                    None => 0,
                };
                if count > 0 {
                    threads += 1;
                    events += count;
                }
            }
        }
        Ok((anchors, events, threads))
    }

    /// Anchor (if present) and events of one thread directory.
    fn read_thread_dir(
        &self,
        id: &ThreadId,
        tree_id: ObjectId,
    ) -> Result<(Option<Anchor>, Vec<(EventId, Event)>)> {
        let tree = self.repo.find_tree(tree_id)?;
        let anchor = match tree.lookup_entry_by_path("anchor.json")? {
            Some(entry) => Some(
                serde_json::from_slice(&self.repo.find_blob(entry.object_id())?.data)
                    .with_context(|| format!("invalid anchor.json in thread {id}"))?,
            ),
            None => None,
        };
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
        Ok((anchor, events))
    }

    fn delete_ref(&self, name: &str) -> Result<()> {
        if let Some(reference) = self.repo.try_find_reference(name)? {
            reference.delete().with_context(|| format!("failed to delete {name}"))?;
        }
        Ok(())
    }
}

/// Outcome of integrating a remote tip into the local ref (SPEC.md §7.2).
#[derive(Debug, PartialEq, Eq)]
pub enum Integration {
    UpToDate,
    /// Local ref did not exist; it now points at the remote tip.
    Initialized,
    FastForwarded,
    /// Histories diverged; a tree-union merge commit was created.
    Merged,
}

impl Store {
    /// The local tracking ref for `remote`'s threads data (SPEC.md §7.1).
    pub fn tracking_ref(remote: &str) -> String {
        format!("refs/threads/remotes/{remote}/data")
    }

    /// Tip of the tracking ref for `remote`, if present.
    pub fn tracking_tip(&self, remote: &str) -> Result<Option<ObjectId>> {
        self.ref_tip(&Self::tracking_ref(remote))
    }

    /// Integrate a fetched remote tip into `refs/threads/data` (SPEC.md §7.2
    /// step 2): no-op if already contained, fast-forward if possible,
    /// otherwise a tree-union merge with both tips as parents. Local drafts
    /// are untouched — they live on their own ref.
    pub fn integrate(&self, remote_tip: ObjectId) -> Result<Integration> {
        let Some(local) = self.tip()? else {
            self.update_ref(DATA_REF, None, remote_tip, "git-threads: initialize from remote")?;
            return Ok(Integration::Initialized);
        };
        if local == remote_tip || self.is_ancestor(remote_tip, local)? {
            return Ok(Integration::UpToDate);
        }
        if self.is_ancestor(local, remote_tip)? {
            self.update_ref(DATA_REF, Some(local), remote_tip, "git-threads: fast-forward")?;
            return Ok(Integration::FastForwarded);
        }
        let local_tree = self.repo.find_commit(local)?.tree_id()?.detach();
        let remote_tree = self.repo.find_commit(remote_tip)?.tree_id()?.detach();
        let merged_tree = self.union_trees(local_tree, remote_tree)?;
        self.commit(
            DATA_REF,
            "threads: merge remote",
            merged_tree,
            vec![local, remote_tip],
            Some(local),
        )?;
        Ok(Integration::Merged)
    }

    fn update_ref(
        &self,
        refname: &str,
        expected: Option<ObjectId>,
        to: ObjectId,
        log: &str,
    ) -> Result<()> {
        let previous = match expected {
            Some(tip) => PreviousValue::MustExistAndMatch(tip.into()),
            None => PreviousValue::MustNotExist,
        };
        self.repo
            .reference(refname, to, previous, log)
            .with_context(|| format!("failed to update {refname} (concurrent write?)"))?;
        Ok(())
    }

    fn is_ancestor(&self, ancestor: ObjectId, descendant: ObjectId) -> Result<bool> {
        if ancestor == descendant {
            return Ok(true);
        }
        for info in self.repo.rev_walk([descendant]).all()? {
            if info?.id == ancestor {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Recursive tree union (SPEC.md §7.2). Writers only add uniquely named
    /// content-addressed files, so identical paths carry identical blobs and
    /// there is no conflict case. Divergence at a path is only possible with
    /// malformed data; it resolves deterministically (trees over blobs, then
    /// larger object ID) so all replicas converge on the same result.
    fn union_trees(&self, ours: ObjectId, theirs: ObjectId) -> Result<ObjectId> {
        use gix::objs::tree::{Entry, EntryMode};

        if ours == theirs {
            return Ok(ours);
        }
        let read = |id: ObjectId| -> Result<BTreeMap<gix::bstr::BString, (EntryMode, ObjectId)>> {
            let tree = self.repo.find_tree(id)?;
            let mut entries = BTreeMap::new();
            for entry in tree.iter() {
                let entry = entry?;
                entries.insert(
                    entry.filename().to_owned(),
                    (entry.mode(), entry.object_id()),
                );
            }
            Ok(entries)
        };
        let our_entries = read(ours)?;
        let their_entries = read(theirs)?;

        let mut merged: Vec<Entry> = Vec::new();
        let names: BTreeSet<&gix::bstr::BString> =
            our_entries.keys().chain(their_entries.keys()).collect();
        for name in names {
            let (mode, oid) = match (our_entries.get(name), their_entries.get(name)) {
                (Some(entry), None) | (None, Some(entry)) => *entry,
                (Some(&(our_mode, our_oid)), Some(&(their_mode, their_oid))) => {
                    if our_oid == their_oid && our_mode == their_mode {
                        (our_mode, our_oid)
                    } else if our_mode.is_tree() && their_mode.is_tree() {
                        (our_mode, self.union_trees(our_oid, their_oid)?)
                    } else {
                        // Malformed data: same path, different content.
                        // Deterministic pick so every replica converges.
                        let ours_wins = (our_mode.is_tree(), our_oid) > (their_mode.is_tree(), their_oid);
                        if ours_wins { (our_mode, our_oid) } else { (their_mode, their_oid) }
                    }
                }
                (None, None) => unreachable!(),
            };
            merged.push(Entry { mode, filename: name.clone(), oid });
        }
        merged.sort();
        Ok(self.repo.write_object(&gix::objs::Tree { entries: merged })?.detach())
    }
}

/// `threads/<first-2-hex>/<thread-id>` (SPEC.md §5.1).
fn thread_dir(id: &ThreadId) -> String {
    format!("threads/{}/{}", &id.as_str()[..2], id)
}

fn git_oid(hex: &str) -> Result<ObjectId> {
    ObjectId::from_hex(hex.as_bytes()).with_context(|| format!("invalid git object id {hex:?}"))
}
