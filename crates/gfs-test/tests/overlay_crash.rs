//! M3.4: kill the daemon at every journal/file transaction boundary.
//!
//! The third exit criterion — "fault injection meets the no-lost-acknowledged-
//! mutation goal" — is what these cases are for, and it needs the guarantee
//! stated precisely before it can be tested:
//!
//! * a mutation the overlay **acknowledged** (its journal transaction committed)
//!   must be recoverable after the process dies;
//! * a mutation that was still in flight must be recoverable as *either* fully
//!   present or fully absent — never half-applied, never pointing at content
//!   that is not there;
//! * recovery must be idempotent: repeating it changes nothing, and repeating the
//!   interrupted operation succeeds.
//!
//! The process really dies. `gfs-overlay-crash` calls `std::process::abort` at
//! the armed boundary, so no `Drop` runs, the SQLite connection is not closed,
//! and the staging file is not cleaned up — which is the whole point, because
//! every one of those is what a recovery path has to survive.

use std::path::{Path, PathBuf};
use std::process::Command;

use gfs_overlay::fault::point;

fn harness() -> &'static Path {
  Path::new(env!("CARGO_BIN_EXE_gfs-overlay-crash"))
}

struct Run {
  status: Option<i32>,
  stdout: String,
}

impl Run {
  fn crashed(&self) -> bool {
    // `abort` raises SIGABRT, so the child has no exit code at all -- which is
    // the strongest available evidence that it did not unwind.
    self.status.is_none() || self.status == Some(gfs_overlay::fault::CRASH_STATUS)
  }

  fn succeeded(&self) -> bool {
    self.status == Some(0) && self.stdout.contains("\nok\n")
  }

  fn entries(&self) -> Vec<&str> {
    self
      .stdout
      .lines()
      .filter_map(|line| line.strip_prefix("entry "))
      .collect()
  }

  fn recovery(&self) -> serde_json::Value {
    let line = self
      .stdout
      .lines()
      .find_map(|line| line.strip_prefix("recovery "))
      .expect("the harness always reports recovery first");
    serde_json::from_str(line).expect("a recovery report")
  }
}

fn run(state: &PathBuf, fault: Option<&str>, args: &[&str]) -> Run {
  let mut command = Command::new(harness());
  command.arg(state).args(args);
  match fault {
    Some(name) => {
      command.env("GFS_OVERLAY_FAULT", name);
    }
    None => {
      command.env_remove("GFS_OVERLAY_FAULT");
    }
  }
  let output = command.output().expect("running the crash harness");
  Run {
    status: output.status.code(),
    stdout: String::from_utf8_lossy(&output.stdout).into_owned() + "\n",
  }
}

/// The whole matrix: every boundary, for every shape of mutation.
#[test]
fn a_crash_at_any_transaction_boundary_leaves_a_recoverable_overlay() {
  let operations: &[(&str, &[&str])] = &[
    ("create", &["create", "new.txt", "hello"]),
    ("materialize", &["materialize", "copied.txt", "base bytes"]),
    ("remove", &["remove", "seed.txt"]),
  ];

  // Not every operation passes through every boundary -- a `remove` writes no
  // content, and creating an empty file stages no bytes. A boundary an operation
  // never reaches is skipped here and accounted for below, so the matrix cannot
  // quietly become vacuous.
  let mut reached: std::collections::HashSet<&str> = std::collections::HashSet::new();

  for (label, args) in operations {
    for boundary in point::ALL {
      let tmp = tempfile::tempdir().unwrap();
      let state = tmp.path().join("overlay");

      // One acknowledged mutation before the crash, so recovery has something it
      // is not allowed to lose.
      let seeded = run(&state, None, &["create", "seed.txt", "acknowledged"]);
      assert!(seeded.succeeded(), "seeding failed: {}", seeded.stdout);

      let crashed = run(&state, Some(boundary), args);
      if !crashed.crashed() {
        continue;
      }
      reached.insert(boundary);

      // Recovery, and the acknowledged mutation must have survived it.
      let after = run(&state, None, &["inspect"]);
      assert!(
        after.succeeded(),
        "{label} at {boundary}: the overlay did not reopen: {}",
        after.stdout
      );
      let report = after.recovery();
      assert!(
        report["missing_content"]
          .as_array()
          .is_some_and(|ids| ids.is_empty()),
        "{label} at {boundary}: a journal row points at content that is gone: {report}"
      );

      let entries = after.entries();
      let seed_intact = entries
        .iter()
        .any(|line| line.starts_with("seed.txt 12 acknowledged"));
      assert!(
        seed_intact || *args.first().unwrap() == "remove",
        "{label} at {boundary}: the acknowledged edit was lost: {entries:?}"
      );

      // Recovery is idempotent: a second open finds nothing left to do.
      let again = run(&state, None, &["inspect"]);
      let second = again.recovery();
      assert_eq!(
        second["orphan_files_removed"], 0,
        "{label} at {boundary}: recovery is not idempotent: {second}"
      );
      assert_eq!(second["temporary_files_removed"], 0);
      assert_eq!(
        again.entries(),
        entries,
        "the view changed on a second open"
      );

      // And the interrupted operation can simply be repeated.
      let retried = run(&state, None, args);
      assert!(
        retried.succeeded() || retried.status == Some(101),
        "{label} at {boundary}: the retry neither succeeded nor failed cleanly: {}",
        retried.stdout
      );
    }
  }

  let mut missed: Vec<&str> = point::ALL
    .iter()
    .copied()
    .filter(|boundary| !reached.contains(boundary))
    .collect();
  missed.sort_unstable();
  assert!(
    missed.is_empty(),
    "no operation in the matrix reaches {missed:?}; the crash coverage has a hole"
  );
}

#[test]
fn a_crash_after_the_commit_keeps_the_mutation_it_acknowledged() {
  // The sharp end of the guarantee. `JOURNAL_COMMITTED` fires *after* the
  // transaction commits and before the in-memory index is updated, so the write
  // was acknowledged by every definition that matters and the process died
  // before it could do anything else with it.
  let tmp = tempfile::tempdir().unwrap();
  let state = tmp.path().join("overlay");

  let crashed = run(
    &state,
    Some(point::JOURNAL_COMMITTED),
    &["create", "durable.txt", "x"],
  );
  assert!(crashed.crashed(), "{:?}", crashed.status);

  let after = run(&state, None, &["inspect"]);
  assert!(after.succeeded(), "{}", after.stdout);
  assert!(
    after
      .entries()
      .iter()
      .any(|line| line.starts_with("durable.txt")),
    "a committed mutation was lost: {:?}",
    after.entries()
  );
}

#[test]
fn a_crash_before_the_commit_leaves_no_trace_of_the_mutation() {
  // The other side of the same guarantee: nothing half-applied. The content file
  // may exist on disk -- it is written before the journal that names it -- but it
  // is unreferenced, so recovery collects it and the path does not appear.
  let tmp = tempfile::tempdir().unwrap();
  let state = tmp.path().join("overlay");

  for boundary in [
    point::CONTENT_STAGED,
    point::CONTENT_SYNCED,
    point::CONTENT_PUBLISHED,
    point::JOURNAL_UNCOMMITTED,
  ] {
    // `materialize` rather than `create`: creating an empty file stages no
    // bytes, so it never reaches the content boundaries at all.
    let crashed = run(
      &state,
      Some(boundary),
      &["materialize", "ghost.txt", "bytes"],
    );
    assert!(crashed.crashed(), "{boundary}: {:?}", crashed.status);

    let after = run(&state, None, &["inspect"]);
    assert!(after.succeeded(), "{boundary}: {}", after.stdout);
    assert!(
      !after
        .entries()
        .iter()
        .any(|line| line.starts_with("ghost.txt")),
      "{boundary}: an uncommitted mutation became visible: {:?}",
      after.entries()
    );
  }
}

#[test]
fn a_crash_after_publishing_content_leaves_an_orphan_that_recovery_collects() {
  // The one boundary where a crash leaves real bytes on disk that nothing names.
  // Leaving them would grow the overlay by a whole file on every interrupted
  // write, and a job that retries a large copy-up in a loop would fill its quota
  // with garbage.
  let tmp = tempfile::tempdir().unwrap();
  let state = tmp.path().join("overlay");

  let crashed = run(
    &state,
    Some(point::CONTENT_PUBLISHED),
    &["materialize", "orphaned.txt", "some bytes"],
  );
  assert!(crashed.crashed());

  let after = run(&state, None, &["inspect"]);
  let report = after.recovery();
  assert_eq!(
    report["orphan_files_removed"], 1,
    "the published-but-unreferenced content was not collected: {report}"
  );
  assert!(report["missing_content"]
    .as_array()
    .is_some_and(|ids| ids.is_empty()));
}

#[test]
fn a_crash_in_a_rename_leaves_the_path_at_exactly_one_end() {
  // A rename is a delete and one or more inserts in one transaction. The failure
  // that matters is the one where they separate: a path that exists twice, or a
  // path that exists nowhere.
  for boundary in point::ALL {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("overlay");
    let seeded = run(&state, None, &["create", "before.txt", "content"]);
    assert!(seeded.succeeded());

    let crashed = run(
      &state,
      Some(boundary),
      &["rename", "before.txt", "after.txt"],
    );
    if !crashed.crashed() {
      // A rename writes no content, so the content boundaries never fire for it.
      continue;
    }

    let after = run(&state, None, &["inspect"]);
    assert!(after.succeeded(), "{boundary}: {}", after.stdout);
    let entries = after.entries();
    let before = entries
      .iter()
      .filter(|l| l.starts_with("before.txt"))
      .count();
    let renamed = entries
      .iter()
      .filter(|l| l.starts_with("after.txt"))
      .count();
    assert_eq!(
      before + renamed,
      1,
      "{boundary}: the file is at {before} old and {renamed} new locations: {entries:?}"
    );
  }
}
