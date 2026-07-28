//! Fault injection at the transaction boundaries.
//!
//! PLAN.md M3.4 asks to "kill the daemon at every journal/file transaction
//! boundary". Those boundaries are not observable from outside — a test that
//! killed a process at a random moment would exercise the same two or three of
//! them over and over and never the interesting ones — so they are named here
//! and the process is aborted at exactly one of them per run.
//!
//! # `abort`, not a panic and not an error
//!
//! A panic unwinds, and unwinding runs `Drop`: the SQLite connection closes
//! cleanly, the staged file removes itself, and the recovery path being tested
//! never sees the state a real crash leaves. `std::process::abort` runs no
//! destructors and no `atexit` handlers, which is what `kill -9` looks like from
//! inside. It is also safe code, so this needs no `unsafe` opt-out.
//!
//! # It is compiled in, not feature-gated
//!
//! A fault point behind a `#[cfg(test)]` would not exist in the binary the crash
//! harness runs, and one behind a Cargo feature would produce a *different*
//! binary from the one that ships. The cost of leaving them in is one relaxed
//! atomic load per boundary, and the benefit is that the recovery path is
//! measured on the same code path that runs in production.
//!
//! Arming requires setting `GFS_OVERLAY_FAULT` in the environment, which no
//! deployment does and which a process cannot be tricked into by any input it
//! reads.

use std::sync::OnceLock;

/// The boundaries a crash can land on. Each is a real ordering step, not a
/// convenient place to put a hook.
pub mod point {
  /// Bytes written into the staging file, not yet fsynced.
  pub const CONTENT_STAGED: &str = "content-staged";
  /// Staging file fsynced, not yet renamed into place.
  pub const CONTENT_SYNCED: &str = "content-synced";
  /// Content published and its directory fsynced; the journal does not yet
  /// reference it. This is the orphan case.
  pub const CONTENT_PUBLISHED: &str = "content-published";
  /// Inside the journal transaction, before the commit.
  pub const JOURNAL_UNCOMMITTED: &str = "journal-uncommitted";
  /// After the commit, before the in-memory index is updated and before any
  /// superseded content file is removed.
  pub const JOURNAL_COMMITTED: &str = "journal-committed";

  pub const ALL: &[&str] = &[
    CONTENT_STAGED,
    CONTENT_SYNCED,
    CONTENT_PUBLISHED,
    JOURNAL_UNCOMMITTED,
    JOURNAL_COMMITTED,
  ];
}

/// The exit status an injected crash produces. Distinct from any status the
/// program could reach on its own, so a test can tell a crash from a clean exit
/// that happened to fail.
pub const CRASH_STATUS: i32 = 134;

fn armed() -> Option<&'static str> {
  static ARMED: OnceLock<Option<String>> = OnceLock::new();
  ARMED
    .get_or_init(|| std::env::var("GFS_OVERLAY_FAULT").ok())
    .as_deref()
}

/// Abort the process if this boundary is the armed one.
pub fn trip(name: &str) {
  if armed() == Some(name) {
    // Written to stderr before aborting so a harness that sees an unexpected
    // status can tell which boundary it stopped at.
    eprintln!("gfs-overlay: injected crash at {name}");
    std::process::abort();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn nothing_is_armed_without_the_environment_variable() {
    // The whole safety argument: the fault points are compiled in, and they do
    // nothing at all unless something outside the process asks them to.
    assert!(armed().is_none());
    for name in point::ALL {
      trip(name);
    }
  }

  #[test]
  fn every_boundary_has_a_distinct_name() {
    let mut names = point::ALL.to_vec();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), point::ALL.len());
  }
}
