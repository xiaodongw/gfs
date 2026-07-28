//! A subprocess that mutates an overlay and can be made to die mid-transaction.
//!
//! PLAN.md M1.1 lists fault injection as part of this crate's job, and M3.4 needs
//! a *process* to kill rather than a function to fail: unwinding runs `Drop`, and
//! `Drop` is exactly what a crash does not do. Everything interesting about
//! overlay recovery lives in the state a process leaves when its destructors
//! never ran.
//!
//! ```text
//! gfs-overlay-crash <state-dir> <op> [args...]
//! ```
//!
//! With `GFS_OVERLAY_FAULT=<boundary>` set, the process aborts at that boundary
//! (see `gfs_overlay::fault`). Without it, the operation completes and the
//! process exits 0 — the same binary is used for both, so "what recovery sees"
//! and "what success looks like" cannot drift apart.

use std::path::PathBuf;

use gfs_overlay::model::{test_binding, test_snapshot_time};
use gfs_overlay::{Overlay, OverlayConfig, Source};
use gfs_types::BytePath;

fn main() {
  let args: Vec<String> = std::env::args().skip(1).collect();
  let [state_dir, op, rest @ ..] = args.as_slice() else {
    eprintln!("usage: gfs-overlay-crash <state-dir> <op> [args...]");
    std::process::exit(2);
  };

  let overlay = Overlay::open(
    &PathBuf::from(state_dir),
    &test_binding(),
    test_snapshot_time(),
    OverlayConfig::default(),
  )
  .expect("opening the overlay");

  // The recovery report is printed before the operation, so a harness can assert
  // on what the *previous* run left behind without a second process.
  println!(
    "recovery {}",
    serde_json::to_string(overlay.recovery()).expect("encoding the recovery report")
  );

  let path = |s: &String| BytePath::new(s.as_bytes().to_vec());
  match op.as_str() {
    // Create a file and write bytes into it: two transactions, so a fault in the
    // second lands on an overlay that already has one acknowledged mutation.
    "create" => {
      let (target, bytes) = (&rest[0], rest[1].as_bytes());
      overlay
        .create_file(&path(target), None, None, 0, false)
        .expect("create");
      overlay.write_at(&path(target), 0, bytes).expect("write");
    }
    // A copy-up, which is the boundary set that moves real content bytes. The
    // base facts are synthesized from the bytes themselves: the overlay only
    // needs to be told what the pinned commit holds there, and a test base is as
    // good as a real one for the ordering being measured.
    "materialize" => {
      let (target, bytes) = (&rest[0], rest[1].as_bytes());
      let facts = gfs_overlay::BaseFacts {
        oid: gfs_overlay::hash::blob_oid(gfs_types::HashAlgorithm::Sha1, bytes)
          .expect("hashing the synthetic base blob"),
        kind: gfs_types::EntryKind::Regular,
        size: bytes.len() as u64,
      };
      let mut reader = std::io::Cursor::new(bytes.to_vec());
      overlay
        .materialize(&path(target), Some(facts), 7, Source::Reader(&mut reader))
        .expect("materialize");
    }
    "truncate" => {
      let (target, size) = (&rest[0], rest[1].parse::<u64>().expect("size"));
      overlay.truncate(&path(target), size).expect("truncate");
    }
    "rename" => {
      overlay
        .rename(
          &path(&rest[0]),
          0,
          None,
          &path(&rest[1]),
          None,
          None,
          &[],
          true,
          false,
        )
        .expect("rename");
    }
    "remove" => {
      overlay
        .remove(&path(&rest[0]), None, false, true)
        .expect("remove");
    }
    // No mutation: just report what recovery found and what the overlay holds.
    "inspect" => {}
    other => {
      eprintln!("unknown operation {other}");
      std::process::exit(2);
    }
  }

  let mut listing: Vec<String> = overlay
    .entries()
    .iter()
    .filter(|entry| entry.present)
    .map(|entry| {
      let bytes = match entry.content.local_id() {
        Some(_) => {
          let mut file = overlay.open_content(entry).expect("content");
          let mut bytes = Vec::new();
          std::io::Read::read_to_end(&mut file, &mut bytes).expect("reading content");
          bytes
        }
        None => Vec::new(),
      };
      format!(
        "{} {} {}",
        entry.path.escaped(),
        entry.size,
        String::from_utf8_lossy(&bytes)
      )
    })
    .collect();
  listing.sort();
  for line in listing {
    println!("entry {line}");
  }
  println!("ok");
}
