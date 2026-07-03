//! Core format logic for git-threads: anchored, threaded discussions stored in git.
//!
//! This crate is deliberately I/O-free and git-free — it implements the pure
//! parts of the spec (SPEC.md): event and anchor schemas, canonical JSON
//! serialization and content-addressed IDs, and the thread state fold.
//! Keeping it pure makes it embeddable anywhere, including WASM targets.
//!
//! Layer map (spec section → module):
//! - §2 events, IDs, state fold → [`event`], [`id`], [`fold`]
//! - §3 anchors → [`anchor`]
//! - §6 canonical JSON → [`canonical`]
//! - §4.1 snippet derivation → [`snippet`]
//! - §4.2 re-anchoring ladder → not yet implemented
//!
//! Storage (§5) and synchronization (§7) are git-facing and live in the CLI
//! crate, not here.

pub mod anchor;
pub mod canonical;
pub mod event;
pub mod fold;
pub mod id;
pub mod snippet;

pub use anchor::{Anchor, AnchorKind, ColRange, DiffRef, LineRange, Side};
pub use canonical::to_canonical_json;
pub use event::{Author, Event, EventKind, Timestamp};
pub use fold::{FoldedEvent, FoldedThread, fold_thread};
pub use id::{EventId, GitOid, ThreadId};
pub use snippet::{Snippet, SnippetTarget, derive_snippet};
