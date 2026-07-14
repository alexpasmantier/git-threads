//! Library target of the git-threads CLI: the git-facing storage and
//! synchronization layers (SPEC.md §5, §7), plus the command implementations.
//! Pure format logic lives in `git-threads-core`.

pub mod commands;
pub mod editor;
pub mod pager;
pub mod reanchor;
pub mod store;
pub mod ui;
