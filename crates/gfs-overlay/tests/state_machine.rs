//! M3's first exit criterion: random mutation sequences match the reference
//! in-memory filesystem model.
//!
//! The sequences run against a real [`Overlay`] — a real SQLite journal and real
//! content files in a temporary directory — and against
//! [`gfs_overlay::model::Model`], which is a `BTreeMap` and nothing else. After
//! every operation the test compares two things, and both matter:
//!
//! * **the outcome**, so that a refusal is refused for the same reason. An
//!   overlay that accepted `mkdir` over an existing file would still produce a
//!   matching listing on the next comparison if the model rejected it and the
//!   overlay's write happened to be a no-op.
//! * **the whole merged tree**, path by path, including file bytes, kind, and
//!   symlink targets.
//!
//! A failing seed is printed with the operation that diverged and reproduces
//! exactly: the generator is a seeded xorshift, not a thread-local RNG.

use gfs_overlay::model::{
  generate, merged_view, path_pool, test_binding, test_snapshot_time, BaseTree, Model, Rng,
};
use gfs_overlay::{Overlay, OverlayConfig};
use gfs_types::EntryKind;

fn base() -> BaseTree {
  BaseTree::new(&[
    ("README.md", EntryKind::Regular, b"readme\n"),
    ("run.sh", EntryKind::Executable, b"#!/bin/sh\necho hi\n"),
    ("link", EntryKind::Symlink, b"README.md"),
    ("src/main.rs", EntryKind::Regular, b"fn main() {}\n"),
    ("src/lib.rs", EntryKind::Regular, b"pub fn lib() {}\n"),
    ("src/util/mod.rs", EntryKind::Regular, b"mod util;\n"),
    ("docs/guide.md", EntryKind::Regular, b"# guide\n"),
    ("empty.txt", EntryKind::Regular, b""),
  ])
}

fn open(dir: &std::path::Path) -> Overlay {
  Overlay::open(
    dir,
    &test_binding(),
    test_snapshot_time(),
    OverlayConfig::default(),
  )
  .unwrap()
}

/// Names outside the base, so `create` sometimes lands on virgin ground and
/// sometimes collides.
const EXTRA: &[&str] = &[
  "new.txt",
  "src/new.rs",
  "src",
  "docs",
  "build",
  "build/out.o",
  "src/util",
  "vendor/lib.rs",
];

#[test]
fn random_mutation_sequences_match_the_reference_model() {
  let base = base();
  let pool = path_pool(&base, EXTRA);

  for seed in 1..=64u64 {
    let tmp = tempfile::tempdir().unwrap();
    let overlay = open(tmp.path());
    let mut model = Model::new(&base);
    let mut rng = Rng::new(seed);
    let mut log = Vec::new();

    for step in 0..200 {
      let op = generate(&mut rng, &model, &pool);
      log.push(format!("{op:?}"));

      let mut candidate = model.clone();
      let expected = op.apply_to_model(&mut candidate);
      let actual = op.apply_to_overlay(&overlay, &base);

      assert_eq!(
        expected,
        actual,
        "seed {seed} step {step}: outcomes differ for {op:?}\nlog:\n  {}",
        log.join("\n  ")
      );
      if expected.is_ok() {
        model = candidate;
      }

      let got = merged_view(&overlay, &base);
      if got != model.live {
        panic!(
          "seed {seed} step {step}: the merged view diverged after {op:?}\n{}\nlog:\n  {}",
          describe(&model.live, &got),
          log.join("\n  ")
        );
      }
    }
  }
}

/// The same sequences, but the overlay is closed and reopened partway through.
///
/// Recovery is not a separate feature to be tested separately: an overlay that
/// reopens into a different state than it had is wrong in exactly the way the
/// model comparison is designed to catch, so the cheapest place to catch it is
/// here.
#[test]
fn a_reopened_overlay_resumes_the_same_merged_view() {
  let base = base();
  let pool = path_pool(&base, EXTRA);

  for seed in 1..=12u64 {
    let tmp = tempfile::tempdir().unwrap();
    let mut overlay = open(tmp.path());
    let mut model = Model::new(&base);
    let mut rng = Rng::new(seed.wrapping_mul(7919));

    for step in 0..60 {
      if step % 17 == 16 {
        drop(overlay);
        overlay = open(tmp.path());
        assert!(
          overlay.recovery().is_clean(),
          "seed {seed}: a clean drop left work for recovery: {:?}",
          overlay.recovery()
        );
        assert_eq!(
          merged_view(&overlay, &base),
          model.live,
          "seed {seed} step {step}: the view changed across a reopen"
        );
      }
      let op = generate(&mut rng, &model, &pool);
      let mut candidate = model.clone();
      let expected = op.apply_to_model(&mut candidate);
      let actual = op.apply_to_overlay(&overlay, &base);
      assert_eq!(expected, actual, "seed {seed} step {step}: {op:?}");
      if expected.is_ok() {
        model = candidate;
      }
    }
  }
}

/// Report the first few differences rather than two whole trees.
fn describe(
  expected: &std::collections::BTreeMap<Vec<u8>, gfs_overlay::model::Node>,
  got: &std::collections::BTreeMap<Vec<u8>, gfs_overlay::model::Node>,
) -> String {
  let mut lines = Vec::new();
  let mut keys: Vec<&Vec<u8>> = expected.keys().chain(got.keys()).collect();
  keys.sort();
  keys.dedup();
  for key in keys {
    let (left, right) = (expected.get(key), got.get(key));
    if left != right {
      lines.push(format!(
        "  {}: model={:?} overlay={:?}",
        String::from_utf8_lossy(key),
        left.map(short),
        right.map(short)
      ));
    }
    if lines.len() >= 8 {
      break;
    }
  }
  lines.join("\n")
}

fn short(node: &gfs_overlay::model::Node) -> String {
  format!(
    "{:?}({})",
    node.kind,
    String::from_utf8_lossy(&node.content).escape_debug()
  )
}
