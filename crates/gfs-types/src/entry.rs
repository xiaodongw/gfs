//! Git file modes and the entry kinds GFS commits to representing.

/// The Git file modes DESIGN.md section 8.2 supports.
pub mod mode {
  pub const REGULAR: u32 = 0o100_644;
  pub const EXECUTABLE: u32 = 0o100_755;
  pub const SYMLINK: u32 = 0o120_000;
  pub const DIRECTORY: u32 = 0o040_000;
  pub const GITLINK: u32 = 0o160_000;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
  Regular,
  Executable,
  Symlink,
  Directory,
  /// A submodule reference.
  ///
  /// Presented as an empty, read-only directory with inspectable GFS metadata.
  /// ADR 0006 answers open question 7 with the measurement that submodules are
  /// *present* in the corpus (12 gitlinks in the Rust repository), so this is a
  /// live compatibility case for the pilot rather than a hypothetical.
  Gitlink,
  /// Anything else libgit2 reports.
  ///
  /// Recorded with its mode rather than guessed at or mapped onto the nearest
  /// supported kind. Git has historically written a few other modes, and a
  /// wrong guess here would present a file as something it is not.
  Unsupported(u32),
}

impl EntryKind {
  pub fn from_mode(m: u32) -> Self {
    match m {
      mode::REGULAR => EntryKind::Regular,
      mode::EXECUTABLE => EntryKind::Executable,
      mode::SYMLINK => EntryKind::Symlink,
      mode::DIRECTORY => EntryKind::Directory,
      mode::GITLINK => EntryKind::Gitlink,
      other => EntryKind::Unsupported(other),
    }
  }

  /// The Git mode this kind was made from.
  ///
  /// Exact for every kind, including [`EntryKind::Unsupported`], which carries
  /// its own mode precisely so a round trip through the enum cannot quietly
  /// rewrite a mode Git recorded into the nearest one GFS understands.
  pub fn as_mode(self) -> u32 {
    match self {
      EntryKind::Regular => mode::REGULAR,
      EntryKind::Executable => mode::EXECUTABLE,
      EntryKind::Symlink => mode::SYMLINK,
      EntryKind::Directory => mode::DIRECTORY,
      EntryKind::Gitlink => mode::GITLINK,
      EntryKind::Unsupported(m) => m,
    }
  }

  /// Whether the entry is listable as a directory. A gitlink counts: it is
  /// presented as an empty directory, which is a listing that returns nothing
  /// rather than an error.
  pub fn is_dir_like(self) -> bool {
    matches!(self, EntryKind::Directory | EntryKind::Gitlink)
  }

  /// Whether the entry has blob content the blob API can serve.
  pub fn has_blob_content(self) -> bool {
    matches!(
      self,
      EntryKind::Regular | EntryKind::Executable | EntryKind::Symlink
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn known_modes_map_to_their_kinds() {
    assert_eq!(EntryKind::from_mode(0o100644), EntryKind::Regular);
    assert_eq!(EntryKind::from_mode(0o100755), EntryKind::Executable);
    assert_eq!(EntryKind::from_mode(0o120000), EntryKind::Symlink);
    assert_eq!(EntryKind::from_mode(0o040000), EntryKind::Directory);
    assert_eq!(EntryKind::from_mode(0o160000), EntryKind::Gitlink);
  }

  #[test]
  fn an_unknown_mode_keeps_its_value_instead_of_being_coerced() {
    // 0o100664 is group-writable regular, which Git itself normalizes but which
    // has appeared in trees written by other tools. It must not silently become
    // `Regular`, or the mount would report a mode the commit does not contain.
    assert_eq!(
      EntryKind::from_mode(0o100664),
      EntryKind::Unsupported(0o100664)
    );
    assert!(!EntryKind::from_mode(0o100664).has_blob_content());
  }

  #[test]
  fn a_gitlink_lists_as_a_directory_but_serves_no_content() {
    assert!(EntryKind::Gitlink.is_dir_like());
    assert!(!EntryKind::Gitlink.has_blob_content());
  }

  #[test]
  fn a_symlink_has_blob_content_because_its_target_is_the_blob() {
    assert!(EntryKind::Symlink.has_blob_content());
    assert!(!EntryKind::Symlink.is_dir_like());
  }
}
