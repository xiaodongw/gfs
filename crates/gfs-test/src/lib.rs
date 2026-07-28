//! Fixtures, oracles, and fault injection for the GFS test suite.
//!
//! PLAN.md M1.1 lists this crate as a deliverable rather than a convenience,
//! because every later milestone's conformance work depends on it. Two principles
//! shape it:
//!
//! * **fixtures are built with stock Git**, never with libgit2, so libgit2 is
//!   always the thing under test and never also the thing producing the input;
//! * **oracles are independent implementations**, not GFS calling itself. The
//!   raw-tree materializer uses `git ls-tree` and `git cat-file` so that a bug
//!   shared between the API and its check cannot hide.
//!
//! `mount` is a harness rather than a fixture, and it is here because two
//! crates need it: the mount tests that live beside the library in
//! `gfs-mount`, and the two tests that had to stay with the `gfs-git-shim`
//! binary in the root `gfs-fuse` crate. A `tests/` module can only be shared
//! within one crate, so it became part of the test-support crate instead.

pub mod bigtree;
pub mod fixtures;
pub mod mount;
pub mod oracle;

pub use bigtree::{big_tree, expected_entries};
pub use fixtures::{
  bare, fixture, git, git_bytes, git_env, git_raw, scratch_clone, worktree, Fixture, FIXTURES,
};
pub use oracle::{
  diff_trees, materialize_checkout, materialize_raw, snapshot_tree, EntrySnapshot, TreeSnapshot,
};
