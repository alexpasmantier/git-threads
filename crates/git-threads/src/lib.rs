//! Library target of git-threads: the git-facing storage and synchronization
//! layers (SPEC.md §5, §7), plus the command implementations. Pure format
//! logic lives in `git-threads-core`.
//!
//! Commands compute; they don't print. Operations in [`commands`] return
//! typed values ([`view`]), and the CLI's text output ([`render`]) and
//! `--json` (serde on the same structs) are two consumers of them — clients
//! linking this crate get the same data the CLI shows.

pub mod commands;
pub mod editor;
pub mod pager;
pub mod reanchor;
pub mod render;
pub mod store;
pub mod ui;
pub mod view;
