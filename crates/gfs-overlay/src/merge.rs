//! Resolution and directory merging, as pure functions.
//!
//! These are the rules the whole overlay hangs on, so they are written without
//! any state of their own: a `BTreeMap` of rows in, an answer out. That is what
//! lets the property tests in [`crate::model`] drive the real rules against a
//! reference model rather than against a re-implementation of them.
//!
//! # The rule
//!
//! ```text
//! for each proper ancestor A of P, root first:
//!     A is a whiteout        -> P is absent
//!     A is not a directory   -> P is absent
//!     A is an opaque dir     -> the base is masked from here down
//! at P:
//!     an overlay row exists  -> that row is the answer (whiteout -> absent)
//!     the base is masked     -> P is absent
//!     otherwise              -> the base's own entry at P is the answer
//! ```
//!
//! The ancestor walk is what makes a single whiteout on a directory hide an
//! entire base subtree without writing a row per path underneath it. Deleting
//! `vendor/` in a monorepo has to cost one row, not four hundred thousand.

use std::collections::BTreeMap;

use gfs_types::BytePath;

use crate::state::{ancestors_of, OverlayEntry};

pub type Rows = BTreeMap<Vec<u8>, OverlayEntry>;

/// What the overlay says about a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
  /// The overlay supplies this path.
  ///
  /// Boxed because `Base` and `Absent` are the overwhelmingly common answers and
  /// an unboxed row would make every one of them 208 bytes wide.
  Overlay(Box<OverlayEntry>),
  /// The overlay says nothing; the base's own entry, if any, applies.
  Base,
  /// Deleted or masked. The base must not be consulted.
  Absent,
}

impl Resolution {
  pub fn overlay(&self) -> Option<&OverlayEntry> {
    match self {
      Resolution::Overlay(entry) => Some(entry),
      _ => None,
    }
  }
}

pub fn resolve(rows: &Rows, path: &BytePath) -> Resolution {
  let mut masked = false;
  for ancestor in ancestors_of(path) {
    match rows.get(ancestor.as_bytes()) {
      Some(entry) if !entry.present => return Resolution::Absent,
      Some(entry) if !entry.kind.is_dir() => return Resolution::Absent,
      Some(entry) => masked |= entry.opaque,
      None => {}
    }
  }
  match rows.get(path.as_bytes()) {
    Some(entry) if entry.present => Resolution::Overlay(Box::new(entry.clone())),
    Some(_) => Resolution::Absent,
    None if masked => Resolution::Absent,
    None => Resolution::Base,
  }
}

/// Whether the base's children of a directory are hidden.
///
/// True when the directory itself is gone, is not a directory, or is an overlay
/// directory that shadows the base — and true when any ancestor is.
pub fn masks_base(rows: &Rows, dir: &BytePath) -> bool {
  for ancestor in ancestors_of(dir) {
    match rows.get(ancestor.as_bytes()) {
      Some(entry) if !entry.present || !entry.kind.is_dir() => return true,
      Some(entry) if entry.opaque => return true,
      _ => {}
    }
  }
  match rows.get(dir.as_bytes()) {
    Some(entry) => !entry.present || !entry.kind.is_dir() || entry.opaque,
    None => false,
  }
}

/// What `readdir` should do with one name the base listing produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BaseChild {
  /// Emit the base entry unchanged.
  Keep,
  /// Emit the overlay's row instead: the path was copied up, renamed onto, or
  /// had its mode changed.
  Replace(Box<OverlayEntry>),
  /// Do not emit it: a whiteout, or the directory is opaque.
  Hide,
}

/// Decide the fate of a base child. `dir` is the directory being listed.
pub fn base_child(rows: &Rows, dir: &BytePath, name: &[u8]) -> BaseChild {
  if masks_base(rows, dir) {
    return BaseChild::Hide;
  }
  let path = dir.join(name);
  match rows.get(path.as_bytes()) {
    Some(entry) if entry.present => BaseChild::Replace(Box::new(entry.clone())),
    Some(_) => BaseChild::Hide,
    None => BaseChild::Keep,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::state::{Content, OverlayKind};
  use gfs_types::Timestamp;

  fn path(s: &str) -> BytePath {
    BytePath::new(s.as_bytes().to_vec())
  }

  fn row(p: &str, present: bool, kind: OverlayKind, opaque: bool) -> OverlayEntry {
    OverlayEntry {
      path: path(p),
      present,
      kind,
      opaque,
      ino: 1,
      content: Content::None,
      symlink_target: None,
      size: 0,
      mtime: Timestamp::from_secs(1),
      ctime: Timestamp::from_secs(1),
      renamed_from: None,
      base: None,
    }
  }

  fn rows(entries: Vec<OverlayEntry>) -> Rows {
    entries
      .into_iter()
      .map(|e| (e.path.as_bytes().to_vec(), e))
      .collect()
  }

  #[test]
  fn an_untouched_path_defers_to_the_base() {
    assert_eq!(
      resolve(&rows(Vec::new()), &path("src/main.rs")),
      Resolution::Base
    );
  }

  #[test]
  fn one_whiteout_on_a_directory_hides_its_whole_base_subtree() {
    // The property the ancestor walk exists for: deleting `vendor/` costs one
    // row, not one per path underneath it.
    let rows = rows(vec![row("vendor", false, OverlayKind::Directory, false)]);
    assert_eq!(resolve(&rows, &path("vendor")), Resolution::Absent);
    assert_eq!(resolve(&rows, &path("vendor/a/b/c.rs")), Resolution::Absent);
    assert_eq!(resolve(&rows, &path("vendors/a.rs")), Resolution::Base);
  }

  #[test]
  fn a_created_directory_is_opaque_and_a_adopted_one_is_not() {
    let created = rows(vec![row("build", true, OverlayKind::Directory, true)]);
    assert_eq!(resolve(&created, &path("build/old.o")), Resolution::Absent);
    assert!(masks_base(&created, &path("build")));

    let adopted = rows(vec![row("build", true, OverlayKind::Directory, false)]);
    assert_eq!(resolve(&adopted, &path("build/old.o")), Resolution::Base);
    assert!(!masks_base(&adopted, &path("build")));
  }

  #[test]
  fn a_file_where_the_base_has_a_directory_hides_what_is_under_it() {
    let rows = rows(vec![row("src", true, OverlayKind::Regular, false)]);
    assert_eq!(resolve(&rows, &path("src/main.rs")), Resolution::Absent);
  }

  #[test]
  fn a_child_of_an_opaque_directory_still_resolves_to_its_own_row() {
    let rows = rows(vec![
      row("build", true, OverlayKind::Directory, true),
      row("build/new.o", true, OverlayKind::Regular, false),
    ]);
    assert!(matches!(
      resolve(&rows, &path("build/new.o")),
      Resolution::Overlay(_)
    ));
  }

  #[test]
  fn base_children_are_kept_replaced_or_hidden() {
    let rows = rows(vec![
      row("src/main.rs", true, OverlayKind::Regular, false),
      row("src/gone.rs", false, OverlayKind::Regular, false),
    ]);
    assert_eq!(
      base_child(&rows, &path("src"), b"other.rs"),
      BaseChild::Keep
    );
    assert_eq!(base_child(&rows, &path("src"), b"gone.rs"), BaseChild::Hide);
    assert!(matches!(
      base_child(&rows, &path("src"), b"main.rs"),
      BaseChild::Replace(_)
    ));
  }
}
