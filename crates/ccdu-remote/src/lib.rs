//! Scanning a tree on another machine.
//!
//! `ccdu ssh://host/path` runs `ccdu --agent` over ssh and talks to it on stdin and stdout. The
//! remote does the walking, where the files are; the local side gets the finished tree and browses
//! it. The protocol is deliberately narrow — a handshake, a scan, and saving a plan — because
//! every message is a thing that has to keep working across versions.
//!
//! The transport is just a `Command`, so the protocol can be exercised end to end by spawning the
//! agent directly, with no ssh, no keys and no second machine.

pub mod agent;
pub mod client;
pub mod protocol;

pub use client::{Remote, Target};
pub use protocol::{Request, Response, VERSION};
