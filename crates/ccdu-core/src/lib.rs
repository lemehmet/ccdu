//! Core engine for ccdu: scanning, the in-memory tree, plans, and the journaled executor.
//!
//! This crate is deliberately free of UI and I/O-presentation concerns: it never prints and never
//! blocks on a frontend. Long-running work reports progress over channels, so the TUI, the headless
//! CLI and the remote agent can all drive the exact same engine.

pub mod format;
pub mod model;
pub mod scan;
