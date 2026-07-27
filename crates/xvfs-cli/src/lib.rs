//! Shared pieces of the `xvfs` command-line surface.
//!
//! `xvfs` and `xvfs-rg` are two binaries that must agree exactly on one thing:
//! how a search result is rendered and what exit code it produces. ADR 0004's
//! table is the agent-facing contract, and two copies of it would eventually
//! disagree — so it lives here, once, and both binaries call it.

pub mod search_output;
pub mod workspace;
