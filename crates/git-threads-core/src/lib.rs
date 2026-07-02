//! Core format logic for git-threads: anchored, threaded discussions stored in git.
//!
//! This crate is deliberately I/O-free and git-free — it implements the pure
//! parts of the spec (SPEC.md): event and anchor schemas, canonical JSON
//! serialization and content-addressed IDs, the thread state fold, and the
//! re-anchoring ladder. Keeping it pure makes it embeddable anywhere,
//! including WASM targets.
