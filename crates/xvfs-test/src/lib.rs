//! Fixtures, oracles, and fault injection for the XVFS test suite.
//!
//! PLAN.md M1.1 lists this crate as a deliverable rather than a convenience,
//! because every later milestone's conformance work depends on it. Two principles
//! shape it:
//!
//! * **fixtures are built with stock Git**, never with libgit2, so libgit2 is
//!   always the thing under test and never also the thing producing the input;
//! * **oracles are independent implementations**, not XVFS calling itself. The
//!   raw-tree materializer uses `git ls-tree` and `git cat-file` so that a bug
//!   shared between the API and its check cannot hide.

pub mod bigtree;
pub mod fixtures;
pub mod oracle;

pub use bigtree::{big_tree, expected_entries};
pub use fixtures::{bare, fixture, git, git_raw, scratch_clone, worktree, Fixture, FIXTURES};
pub use oracle::{
  diff_trees, materialize_checkout, materialize_raw, snapshot_tree, EntrySnapshot, TreeSnapshot,
};
