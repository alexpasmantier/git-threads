//! Typed views of thread state: what `list`, `show`, and `status` compute,
//! decoupled from how it prints. The text renderers (`render`) and `--json`
//! (plain serde serialization of these structs) consume the same values, so
//! the linked API and the JSON interface cannot drift apart.

use crate::reanchor::Reanchor;
use git_threads_core::{
    Anchor, Author, EventId, EventKind, GitOid, LineRange, Side, ThreadId, Timestamp,
};
use serde::Serialize;

/// One thread as readers see it: the folded conversation plus its re-anchor
/// placement against `at`. Serializes to the object `--json` emits; `anchor`
/// (and `moved_to`) are anchor.json documents verbatim (SPEC.md §3).
#[derive(Clone, Debug, Serialize)]
pub struct ThreadView {
    pub id: ThreadId,
    pub resolved: bool,
    pub anchor: Anchor,
    /// The latest move's anchor when the thread was re-pinned (SPEC.md §2.4
    /// rule 5); `None` when the thread never moved.
    pub moved_to: Option<Anchor>,
    /// Who re-pinned it; `None` when the thread never moved.
    pub moved_by: Option<Author>,
    /// The commit the thread was re-anchored against.
    pub at: GitOid,
    pub placement: Reanchor,
    /// Comments and replies in display order, folded state applied.
    pub messages: Vec<MessageView>,
}

impl ThreadView {
    /// The anchor re-anchoring starts from: the latest move's, else the
    /// thread's own. The original stays the record of what was discussed;
    /// the move records where that code lives now.
    pub fn effective_anchor(&self) -> &Anchor {
        self.moved_to.as_ref().unwrap_or(&self.anchor)
    }

    /// The root comment. A same-second reply can tie-break ahead of it in
    /// display order, so this is not always the first message.
    pub fn root(&self) -> Option<&MessageView> {
        self.messages.iter().find(|m| m.id == self.id).or_else(|| self.messages.first())
    }
}

/// One comment or reply with its folded state applied.
#[derive(Clone, Debug, Serialize)]
pub struct MessageView {
    pub id: EventId,
    #[serde(rename = "type")]
    pub kind: EventKind,
    pub author: Author,
    pub ts: Timestamp,
    /// Current (folded) text; `None` when retracted.
    pub body: Option<String>,
    pub in_reply_to: Option<EventId>,
    pub edited: bool,
    pub retracted: bool,
    /// A local draft, not yet published.
    pub draft: bool,
    /// Published by someone else and not seen here yet.
    pub new: bool,
}

/// What `status` reports: drafts awaiting `commit`, sealed events not yet on
/// each remote (awaiting `push`), threads with unseen activity, and threads
/// a rewrite stranded that `move --orphans` could re-pin at the checkout.
#[derive(Clone, Debug, Serialize)]
pub struct StatusView {
    pub drafted: Vec<ThreadDrafts>,
    pub remotes: Vec<RemoteStatus>,
    pub threads_with_news: usize,
    pub repins: usize,
}

impl StatusView {
    /// Total drafted events across all threads.
    pub fn drafted_events(&self) -> usize {
        self.drafted.iter().map(|t| t.events.len()).sum()
    }
}

/// One thread's drafted events, in (ts, id) order.
#[derive(Clone, Debug, Serialize)]
pub struct ThreadDrafts {
    pub thread: ThreadId,
    pub anchor: Anchor,
    pub events: Vec<DraftedEvent>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DraftedEvent {
    pub id: EventId,
    #[serde(rename = "type")]
    pub kind: EventKind,
    pub body: Option<String>,
}

/// Sealed local events not yet known to be on a remote.
#[derive(Clone, Debug, Serialize)]
pub struct RemoteStatus {
    pub remote: String,
    pub unpushed: usize,
}

/// The code context a thread renders with: the anchored change as git shows
/// it, or a file excerpt when there is no change to show. Computed by
/// [`crate::commands::anchor_context`]; how it looks is the renderer's
/// business, and clients are free to render it their own way.
#[derive(Clone, Debug)]
pub enum AnchorContext {
    /// `git diff --stat` output of the anchored change.
    Stat(String),
    /// Unified diff of the anchored change.
    Diff {
        text: String,
        /// The side of the diff the anchored lines live on.
        side: Side,
        lines: Option<LineRange>,
        /// Whole-change diff: file headers separate the files.
        headers: bool,
        /// Keep only hunks overlapping the anchored lines.
        clip: bool,
    },
    /// A file excerpt at `lines`: snapshot annotations, or the fallback when
    /// the change cannot be shown (outdated threads render the original).
    Excerpt { content: String, lines: LineRange },
}
