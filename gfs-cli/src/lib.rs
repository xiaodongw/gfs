//! Shared pieces of the `gfs` command-line surface.
//!
//! `rg`, `find` and `log` were binaries of their own and are subcommands now,
//! but they keep their own hand-rolled argv parsers rather than being folded
//! into the `clap` derive the rest of the CLI uses. Each mimics a tool an agent
//! already knows, and the informative *rejection* of an unsupported flag is as
//! much a part of that contract as the flags that work. See each module header.
//!
//! `search_output` stays shared for the reason it always was. `gfs search` and
//! `gfs rg` ask the daemon the same question through two flag syntaxes, and
//! ADR 0004's exit-code table is the agent-facing contract — two copies of it
//! would eventually disagree, so there is one.

pub mod blame;
pub mod find;
pub mod gitdate;
pub mod history;
pub mod revdiff;
pub mod rg;
pub mod search_output;
pub mod show;
pub mod workspace;
