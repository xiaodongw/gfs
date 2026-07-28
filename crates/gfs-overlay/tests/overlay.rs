//! Smoke tests for the overlay's entry points.
//!
//! The state machine in `state_machine.rs` covers *what the filesystem looks
//! like* after arbitrary mutation sequences. These cover the properties it
//! cannot see: what got written to disk, what a restart makes of it, and what the
//! quota does — the parts of M3.1 that are about durability rather than
//! semantics.

use std::io::Read;

use gfs_overlay::model::{test_binding, test_snapshot_time, BaseTree};
use gfs_overlay::{BaseFacts, Condition, Content, Overlay, OverlayConfig, Resolution, Source};
use gfs_types::{BytePath, EntryKind, HashAlgorithm, ObjectId, Timestamp};

fn path(s: &str) -> BytePath {
  BytePath::new(s.as_bytes().to_vec())
}

fn open(dir: &std::path::Path) -> Overlay {
  open_with(dir, OverlayConfig::default())
}

fn open_with(dir: &std::path::Path, config: OverlayConfig) -> Overlay {
  Overlay::open(dir, &test_binding(), test_snapshot_time(), config).unwrap()
}

fn base() -> BaseTree {
  BaseTree::new(&[
    ("README.md", EntryKind::Regular, b"readme\n"),
    ("src/main.rs", EntryKind::Regular, b"fn main() {}\n"),
    ("big.bin", EntryKind::Regular, b"0123456789"),
  ])
}

#[test]
fn a_created_file_survives_a_restart_with_its_bytes_and_its_inode() {
  let tmp = tempfile::tempdir().unwrap();
  let (ino, size) = {
    let overlay = open(tmp.path());
    overlay
      .create_file(&path("new.txt"), None, None, 100, false)
      .unwrap();
    overlay.write_at(&path("new.txt"), 0, b"hello").unwrap();
    let entry = overlay.get(&path("new.txt")).unwrap();
    assert_eq!(entry.size, 5);
    (entry.ino, entry.size)
  };

  let overlay = open(tmp.path());
  assert!(
    overlay.recovery().is_clean(),
    "a clean close leaves nothing to recover: {:?}",
    overlay.recovery()
  );
  let entry = overlay.get(&path("new.txt")).unwrap();
  assert_eq!(entry.ino, ino, "an overlay inode number is persistent");
  assert_eq!(entry.size, size);
  let mut bytes = Vec::new();
  overlay
    .open_content(&entry)
    .unwrap()
    .read_to_end(&mut bytes)
    .unwrap();
  assert_eq!(bytes, b"hello");
}

#[test]
fn a_mode_change_diverges_metadata_without_fetching_any_bytes() {
  // The reason `Content::Base` exists. A `chmod +x` on a 100 MiB file must not
  // download it, and the only way to be sure is to assert that no local content
  // was created at all.
  let tmp = tempfile::tempdir().unwrap();
  let overlay = open(tmp.path());
  let base = base();
  let target = path("src/main.rs");

  let entry = overlay
    .set_executable(&target, base.facts(&target), 4242, true)
    .unwrap();
  assert_eq!(entry.kind, gfs_overlay::OverlayKind::Executable);
  assert!(matches!(entry.content, Content::Base(_)));
  assert_eq!(entry.ino, 4242, "copy-up must not change identity");
  assert_eq!(overlay.stats().local_bytes, 0, "nothing was hydrated");
}

#[test]
fn deleting_a_base_directory_costs_one_row_and_hides_the_whole_subtree() {
  let tmp = tempfile::tempdir().unwrap();
  let overlay = open(tmp.path());
  let base = base();
  overlay
    .remove(&path("src"), base.facts(&path("src")), true, true)
    .unwrap();
  assert_eq!(overlay.stats().entries, 1);
  assert_eq!(overlay.stats().whiteouts, 1);
  assert_eq!(overlay.resolve(&path("src/main.rs")), Resolution::Absent);
  assert_eq!(overlay.resolve(&path("README.md")), Resolution::Base);
}

#[test]
fn o_trunc_replaces_a_base_file_without_reading_it() {
  let tmp = tempfile::tempdir().unwrap();
  let overlay = open(tmp.path());
  let base = base();
  let target = path("big.bin");
  let entry = overlay
    .materialize(&target, base.facts(&target), 77, Source::Empty)
    .unwrap();
  assert_eq!(entry.size, 0, "the old content was never fetched");
  assert_eq!(overlay.stats().local_bytes, 0);
}

#[test]
fn the_quota_short_writes_rather_than_endangering_what_is_already_there() {
  // PLAN.md M3.2: "enforce per-job overlay disk quota without endangering
  // existing edits". A caller that hits the limit gets a short write, which is
  // what a POSIX write against a full filesystem returns -- not a failure that
  // discards the bytes already accepted.
  let tmp = tempfile::tempdir().unwrap();
  let overlay = open_with(
    tmp.path(),
    OverlayConfig {
      quota_bytes: 16,
      ..OverlayConfig::default()
    },
  );
  overlay
    .create_file(&path("a.txt"), None, None, 101, false)
    .unwrap();
  assert_eq!(
    overlay.write_at(&path("a.txt"), 0, &[b'x'; 10]).unwrap(),
    10
  );
  assert_eq!(
    overlay.write_at(&path("a.txt"), 10, &[b'y'; 10]).unwrap(),
    6,
    "the write stops at the quota"
  );
  let error = overlay
    .write_at(&path("a.txt"), 16, b"z")
    .expect_err("no headroom left");
  assert_eq!(error.condition, Condition::QuotaExceeded);

  let entry = overlay.get(&path("a.txt")).unwrap();
  assert_eq!(entry.size, 16);
  let mut bytes = Vec::new();
  overlay
    .open_content(&entry)
    .unwrap()
    .read_to_end(&mut bytes)
    .unwrap();
  assert_eq!(bytes, b"xxxxxxxxxxyyyyyy", "the accepted bytes are intact");
}

#[test]
fn content_a_crash_left_unreferenced_is_collected_on_the_next_open() {
  let tmp = tempfile::tempdir().unwrap();
  {
    let overlay = open(tmp.path());
    overlay
      .create_file(&path("keep.txt"), None, None, 102, false)
      .unwrap();
    overlay.write_at(&path("keep.txt"), 0, b"kept").unwrap();
    // What a crash between "content published" and "journal committed" leaves:
    // a content file no row names, plus a torn temporary.
    let store = overlay.content_store();
    let mut staged = store.stage(999).unwrap();
    staged.write_all(b"orphaned").unwrap();
    store.publish(staged, 999).unwrap();
    std::fs::write(store.root().join("tmp").join("stage-torn"), b"torn").unwrap();
  }

  let overlay = open(tmp.path());
  let report = overlay.recovery();
  assert_eq!(report.orphan_files_removed, 1);
  assert_eq!(report.orphan_bytes_removed, 8);
  assert_eq!(report.temporary_files_removed, 1);
  assert!(report.missing_content.is_empty());
  assert_eq!(
    overlay.get(&path("keep.txt")).unwrap().size,
    4,
    "the acknowledged write is untouched"
  );
}

#[test]
fn an_overlay_from_another_commit_is_refused_rather_than_merged() {
  // Reopening against a different base would leave every path resolving and
  // every answer quietly about the wrong tree. `gfs refresh` depends on this.
  let tmp = tempfile::tempdir().unwrap();
  {
    let overlay = open(tmp.path());
    overlay
      .create_file(&path("a.txt"), None, None, 101, false)
      .unwrap();
  }
  let other = gfs_overlay::Binding {
    repository_id: "r-model".to_owned(),
    base_commit: ObjectId::from_raw(HashAlgorithm::Sha1, &[0x22; 20])
      .unwrap()
      .to_qualified(),
  };
  let error = Overlay::open(
    tmp.path(),
    &other,
    test_snapshot_time(),
    OverlayConfig::default(),
  )
  .expect_err("a different base commit must be refused");
  assert_eq!(error.condition, Condition::Invalid);
}

#[test]
fn a_newer_schema_is_refused_rather_than_read() {
  let tmp = tempfile::tempdir().unwrap();
  drop(open(tmp.path()));
  let conn = rusqlite::Connection::open(tmp.path().join("overlay.sqlite")).unwrap();
  conn.execute_batch("PRAGMA user_version = 999;").unwrap();
  drop(conn);
  let error = Overlay::open(
    tmp.path(),
    &test_binding(),
    test_snapshot_time(),
    OverlayConfig::default(),
  )
  .expect_err("a future schema must be refused");
  assert_eq!(error.condition, Condition::Invalid);
}

#[test]
fn every_overlay_timestamp_is_newer_than_the_base_even_with_a_skewed_clock() {
  // ADR 0006's overlay clock, end to end through the journal rather than as a
  // unit test of the formula: the snapshot time here is in 2033, well ahead of
  // any plausible host clock, and every acknowledged mutation must still be
  // strictly newer than it and strictly increasing.
  let tmp = tempfile::tempdir().unwrap();
  let future = Timestamp::from_secs(2_000_000_000);
  let overlay = Overlay::open(
    tmp.path(),
    &test_binding(),
    future,
    OverlayConfig::default(),
  )
  .unwrap();

  let mut previous = future;
  for index in 0..8 {
    let name = format!("f{index}.txt");
    let entry = overlay
      .create_file(&path(&name), None, None, 200 + index, false)
      .unwrap();
    assert!(
      entry.mtime > future,
      "{:?} must be after the base",
      entry.mtime
    );
    assert!(entry.mtime > previous, "the overlay clock must advance");
    previous = entry.mtime;
  }

  // And across a restart, which is where a clock seeded from the host would go
  // backwards.
  drop(overlay);
  let overlay = Overlay::open(
    tmp.path(),
    &test_binding(),
    future,
    OverlayConfig::default(),
  )
  .unwrap();
  let entry = overlay
    .create_file(&path("after-restart.txt"), None, None, 300, false)
    .unwrap();
  assert!(entry.mtime > previous, "the clock survived the restart");
}

#[test]
fn an_explicit_timestamp_below_the_floor_is_clamped() {
  let tmp = tempfile::tempdir().unwrap();
  let overlay = open(tmp.path());
  overlay
    .create_file(&path("a.txt"), None, None, 101, false)
    .unwrap();
  let entry = overlay
    .set_times(
      &path("a.txt"),
      None,
      0,
      Some(Timestamp::from_secs(1_000_000_000)),
    )
    .unwrap();
  assert!(entry.mtime > test_snapshot_time());
}

#[test]
fn renaming_a_base_directory_moves_metadata_and_fetches_nothing() {
  let tmp = tempfile::tempdir().unwrap();
  let overlay = open(tmp.path());
  let base = base();
  overlay
    .rename(
      &path("src"),
      400,
      base.facts(&path("src")),
      &path("lib"),
      None,
      None,
      &base.descendants(&path("src")),
      true,
      false,
    )
    .unwrap();

  assert_eq!(overlay.resolve(&path("src/main.rs")), Resolution::Absent);
  let moved = overlay
    .resolve(&path("lib/main.rs"))
    .overlay()
    .cloned()
    .expect("the descendant moved");
  assert!(
    matches!(moved.content, Content::Base(_)),
    "a moved file still points at the base blob"
  );
  assert_eq!(overlay.stats().local_bytes, 0, "nothing was hydrated");
}

#[test]
fn a_rename_larger_than_the_configured_bound_is_refused() {
  let tmp = tempfile::tempdir().unwrap();
  let overlay = open_with(
    tmp.path(),
    OverlayConfig {
      max_rename_entries: 1,
      ..OverlayConfig::default()
    },
  );
  let base = base();
  let error = overlay
    .rename(
      &path("src"),
      400,
      base.facts(&path("src")),
      &path("lib"),
      None,
      None,
      &[
        base.descendants(&path("src"))[0].clone(),
        base.descendants(&path("src"))[0].clone(),
      ],
      true,
      false,
    )
    .expect_err("the bound must be enforced");
  assert_eq!(error.condition, Condition::QuotaExceeded);
}

#[test]
fn a_symlink_is_never_copied_into_a_content_file() {
  // Its target *is* its content. Copying one up would let `readlink` and `read`
  // disagree about what the link says.
  let tmp = tempfile::tempdir().unwrap();
  let overlay = open(tmp.path());
  overlay
    .symlink(&path("l"), b"README.md", None, None, 103)
    .unwrap();
  let error = overlay
    .materialize(&path("l"), None, 1, Source::Empty)
    .expect_err("a symlink has no content file");
  assert_eq!(error.condition, Condition::Invalid);
}

#[test]
fn a_gitlink_cannot_be_modified_through_the_overlay() {
  let tmp = tempfile::tempdir().unwrap();
  let overlay = open(tmp.path());
  let facts = BaseFacts {
    oid: ObjectId::from_raw(HashAlgorithm::Sha1, &[3; 20]).unwrap(),
    kind: EntryKind::Gitlink,
    size: 0,
  };
  let error = overlay
    .adopt(&path("sub"), Some(facts), 9)
    .expect_err("a submodule is not overlay content");
  assert_eq!(error.condition, Condition::NotPermitted);
}
