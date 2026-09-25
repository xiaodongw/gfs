//! The path state machine.
//!
//! DESIGN.md section 6.4 names the states an overlay has to represent: base,
//! copied-up, created, deleted, renamed, and type-changed. They are not six
//! independent flags. They are three orthogonal facts about one path, and
//! modelling them as six states is how an overlay ends up unable to express
//! "renamed *and* then modified".
//!
//! ```text
//!   present?      false -> a whiteout: the base entry at this path is deleted
//!                 true  -> the overlay supplies this path
//!   content       Base(oid)  -> bytes still come from the pinned commit
//!                 Local(id)  -> bytes are a file in `files/`
//!                 None       -> a directory
//!   base          Some(..)   -> the pinned commit has (had) an entry here
//!                 None       -> this path is new
//! ```
//!
//! The six named states fall out of the combination:
//!
//! | State | `present` | `content` | `base` |
//! | --- | --- | --- | --- |
//! | base (no row at all) | — | — | — |
//! | created | true | `Local`/`None` | `None` |
//! | copied-up | true | `Local` | `Some` |
//! | deleted | false | — | `Some` |
//! | renamed | true | either | either, plus `renamed_from` |
//! | type-changed | true | differs in `kind` from `base` | `Some` |
//!
//! # Why `Content::Base` exists
//!
//! A `chmod +x` on a 100 MiB file, or an `mv` of it, changes no bytes. An overlay
//! that could only express "local file" would have to download and copy the blob
//! to record a mode change, which is precisely the hydration this project exists
//! to avoid. `Content::Base` is an overlay row whose bytes are still the pinned
//! commit's — metadata diverged, content did not.
//!
//! # Opacity
//!
//! A directory created by the overlay hides whatever the base has at that path.
//! Without that, `rm -rf build && mkdir build` would leave the base's `build/`
//! children showing through the new empty directory. Adopting an *existing* base
//! directory (to change its mode, say) is the opposite case and must not hide
//! anything, so opacity is a property of the row rather than of directories.

use gfs_types::{BytePath, EntryKind, ObjectId, Timestamp};

/// What kind of thing the overlay holds at a path.
///
/// Narrower than [`EntryKind`] on purpose: an overlay can never contain a gitlink
/// or an unsupported Git mode, because nothing in the mount can create one.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayKind {
  Regular,
  Executable,
  Symlink,
  Directory,
}

impl OverlayKind {
  pub fn is_dir(self) -> bool {
    self == OverlayKind::Directory
  }

  pub fn is_file(self) -> bool {
    matches!(self, OverlayKind::Regular | OverlayKind::Executable)
  }

  /// The Git mode this kind records in an exported tree.
  pub fn git_mode(self) -> u32 {
    match self {
      OverlayKind::Regular => gfs_types::mode::REGULAR,
      OverlayKind::Executable => gfs_types::mode::EXECUTABLE,
      OverlayKind::Symlink => gfs_types::mode::SYMLINK,
      OverlayKind::Directory => gfs_types::mode::DIRECTORY,
    }
  }

  pub fn from_entry_kind(kind: EntryKind) -> Option<Self> {
    match kind {
      EntryKind::Regular => Some(OverlayKind::Regular),
      EntryKind::Executable => Some(OverlayKind::Executable),
      EntryKind::Symlink => Some(OverlayKind::Symlink),
      EntryKind::Directory => Some(OverlayKind::Directory),
      // A gitlink is a directory to `readdir` but is not something the overlay
      // can hold: there is nothing to copy up and nothing to export.
      EntryKind::Gitlink | EntryKind::Unsupported(_) => None,
    }
  }

  pub fn to_entry_kind(self) -> EntryKind {
    match self {
      OverlayKind::Regular => EntryKind::Regular,
      OverlayKind::Executable => EntryKind::Executable,
      OverlayKind::Symlink => EntryKind::Symlink,
      OverlayKind::Directory => EntryKind::Directory,
    }
  }

  fn code(self) -> i64 {
    match self {
      OverlayKind::Regular => 0,
      OverlayKind::Executable => 1,
      OverlayKind::Symlink => 2,
      OverlayKind::Directory => 3,
    }
  }

  fn from_code(code: i64) -> Option<Self> {
    match code {
      0 => Some(OverlayKind::Regular),
      1 => Some(OverlayKind::Executable),
      2 => Some(OverlayKind::Symlink),
      3 => Some(OverlayKind::Directory),
      _ => None,
    }
  }
}

/// Where an entry's bytes live.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Content {
  /// A directory, which has no content of its own.
  None,
  /// A file in `files/`, identified by its content id.
  Local(u64),
  /// Still the pinned commit's blob. Metadata diverged; bytes did not.
  Base(ObjectId),
}

impl Content {
  pub fn local_id(&self) -> Option<u64> {
    match self {
      Content::Local(id) => Some(*id),
      _ => None,
    }
  }

  pub fn base_oid(&self) -> Option<&ObjectId> {
    match self {
      Content::Base(oid) => Some(oid),
      _ => None,
    }
  }
}

/// What the pinned commit has, or had, at a path.
///
/// Recorded at the moment the overlay first diverges from the base, and never
/// updated afterwards, because the base cannot change: the mount is pinned. It is
/// what status and diff compare against, and it is what a whiteout is a whiteout
/// *of*.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BaseFacts {
  pub oid: ObjectId,
  pub kind: EntryKind,
  pub size: u64,
}

/// One row of the journal.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OverlayEntry {
  pub path: BytePath,
  /// False for a whiteout. A whiteout always has `base: Some(..)`; deleting a
  /// path the base does not have simply removes the row.
  pub present: bool,
  pub kind: OverlayKind,
  /// A created directory hides the base's children at the same path.
  pub opaque: bool,
  pub ino: u64,
  pub content: Content,
  pub symlink_target: Option<Vec<u8>>,
  pub size: u64,
  pub mtime: Timestamp,
  pub ctime: Timestamp,
  /// Where this path came from, when it was produced by a rename. Kept so export
  /// can emit a rename record rather than a delete plus an add.
  pub renamed_from: Option<BytePath>,
  pub base: Option<BaseFacts>,
}

impl OverlayEntry {
  pub fn parent(&self) -> BytePath {
    parent_of(&self.path)
  }

  pub fn name(&self) -> Vec<u8> {
    self.path.file_name().unwrap_or_default().to_vec()
  }

  /// Whether the base listing of the parent directory will also produce this
  /// name, in which case `readdir` must replace rather than append it.
  pub fn shadows_base(&self) -> bool {
    self.base.is_some()
  }
}

/// The parent of a byte path. The root's parent is the root.
pub fn parent_of(path: &BytePath) -> BytePath {
  let bytes = path.as_bytes();
  match bytes.iter().rposition(|b| *b == b'/') {
    Some(index) => BytePath::new(bytes[..index].to_vec()),
    None => BytePath::root(),
  }
}

/// Every proper ancestor of a path, root first, excluding the path itself.
pub fn ancestors_of(path: &BytePath) -> Vec<BytePath> {
  let bytes = path.as_bytes();
  if bytes.is_empty() {
    return Vec::new();
  }
  let mut out = vec![BytePath::root()];
  for (index, byte) in bytes.iter().enumerate() {
    if *byte == b'/' {
      out.push(BytePath::new(bytes[..index].to_vec()));
    }
  }
  out
}

/// Whether `path` is `dir` itself or lives underneath it.
pub fn is_within(path: &BytePath, dir: &BytePath) -> bool {
  if dir.is_empty() {
    return true;
  }
  let (path, dir) = (path.as_bytes(), dir.as_bytes());
  path == dir || (path.len() > dir.len() && path[dir.len()] == b'/' && path.starts_with(dir))
}

// ---------------------------------------------------------------------------
// Row encoding. Kept beside the types it encodes so a new field cannot be added
// to one without the other refusing to compile.
// ---------------------------------------------------------------------------

/// The stored form of an entry: the exact column tuple, in schema order.
pub(crate) struct Row {
  pub path: Vec<u8>,
  pub parent: Vec<u8>,
  pub present: i64,
  pub kind: i64,
  pub opaque: i64,
  pub ino: i64,
  pub content_kind: i64,
  pub content_id: Option<i64>,
  pub content_oid: Option<String>,
  pub symlink_target: Option<Vec<u8>>,
  pub size: i64,
  pub mtime_secs: i64,
  pub mtime_nanos: i64,
  pub ctime_secs: i64,
  pub ctime_nanos: i64,
  pub renamed_from: Option<Vec<u8>>,
  pub base_oid: Option<String>,
  pub base_mode: Option<i64>,
  pub base_size: Option<i64>,
}

pub(crate) const CONTENT_NONE: i64 = 0;
pub(crate) const CONTENT_LOCAL: i64 = 1;
pub(crate) const CONTENT_BASE: i64 = 2;

impl Row {
  pub fn of(entry: &OverlayEntry) -> Row {
    let (content_kind, content_id, content_oid) = match &entry.content {
      Content::None => (CONTENT_NONE, None, None),
      Content::Local(id) => (CONTENT_LOCAL, Some(*id as i64), None),
      Content::Base(oid) => (CONTENT_BASE, None, Some(oid.to_qualified())),
    };
    Row {
      path: entry.path.as_bytes().to_vec(),
      parent: entry.parent().as_bytes().to_vec(),
      present: i64::from(entry.present),
      kind: entry.kind.code(),
      opaque: i64::from(entry.opaque),
      ino: entry.ino as i64,
      content_kind,
      content_id,
      content_oid,
      symlink_target: entry.symlink_target.clone(),
      size: entry.size as i64,
      mtime_secs: entry.mtime.secs,
      mtime_nanos: i64::from(entry.mtime.nanos),
      ctime_secs: entry.ctime.secs,
      ctime_nanos: i64::from(entry.ctime.nanos),
      renamed_from: entry.renamed_from.as_ref().map(|p| p.as_bytes().to_vec()),
      base_oid: entry.base.as_ref().map(|b| b.oid.to_qualified()),
      base_mode: entry.base.as_ref().map(|b| i64::from(b.kind.as_mode())),
      base_size: entry.base.as_ref().map(|b| b.size as i64),
    }
  }

  pub fn decode(self) -> Result<OverlayEntry, String> {
    let kind = OverlayKind::from_code(self.kind)
      .ok_or_else(|| format!("unknown overlay kind {}", self.kind))?;
    let content = match self.content_kind {
      CONTENT_NONE => Content::None,
      CONTENT_LOCAL => Content::Local(
        self
          .content_id
          .ok_or_else(|| "a local content row has no content id".to_owned())? as u64,
      ),
      CONTENT_BASE => Content::Base(
        ObjectId::parse_qualified(
          self
            .content_oid
            .as_deref()
            .ok_or_else(|| "a base content row has no object id".to_owned())?,
        )
        .map_err(|e| e.to_string())?,
      ),
      other => return Err(format!("unknown content kind {other}")),
    };
    let base = match (self.base_oid, self.base_mode, self.base_size) {
      (Some(oid), Some(mode), Some(size)) => Some(BaseFacts {
        oid: ObjectId::parse_qualified(&oid).map_err(|e| e.to_string())?,
        kind: EntryKind::from_mode(mode as u32),
        size: size as u64,
      }),
      _ => None,
    };
    Ok(OverlayEntry {
      path: BytePath::new(self.path),
      present: self.present != 0,
      kind,
      opaque: self.opaque != 0,
      ino: self.ino as u64,
      content,
      symlink_target: self.symlink_target,
      size: self.size as u64,
      mtime: Timestamp::new(self.mtime_secs, self.mtime_nanos as u32),
      ctime: Timestamp::new(self.ctime_secs, self.ctime_nanos as u32),
      renamed_from: self.renamed_from.map(BytePath::new),
      base,
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use gfs_types::HashAlgorithm;

  fn oid(byte: u8) -> ObjectId {
    ObjectId::from_raw(HashAlgorithm::Sha1, &[byte; 20]).unwrap()
  }

  fn path(s: &str) -> BytePath {
    BytePath::new(s.as_bytes().to_vec())
  }

  #[test]
  fn ancestors_run_from_the_root_and_exclude_the_path() {
    let got: Vec<String> = ancestors_of(&path("a/b/c"))
      .iter()
      .map(|p| String::from_utf8_lossy(p.as_bytes()).into_owned())
      .collect();
    assert_eq!(got, vec!["", "a", "a/b"]);
    assert!(ancestors_of(&BytePath::root()).is_empty());
  }

  #[test]
  fn containment_does_not_match_a_sibling_with_a_shared_prefix() {
    // The bug this exists to prevent: `src2/x` counted as living under `src`,
    // so deleting `src` would whiteout an unrelated tree.
    assert!(is_within(&path("src/a"), &path("src")));
    assert!(is_within(&path("src"), &path("src")));
    assert!(!is_within(&path("src2/a"), &path("src")));
    assert!(is_within(&path("anything"), &BytePath::root()));
  }

  #[test]
  fn a_row_round_trips_every_field() {
    let entry = OverlayEntry {
      path: path("src/main.rs"),
      present: true,
      kind: OverlayKind::Executable,
      opaque: false,
      ino: 1 << 48,
      content: Content::Base(oid(7)),
      symlink_target: None,
      size: 42,
      mtime: Timestamp::new(1_700_000_000, 5),
      ctime: Timestamp::new(1_700_000_001, 6),
      renamed_from: Some(path("src/old.rs")),
      base: Some(BaseFacts {
        oid: oid(9),
        kind: EntryKind::Regular,
        size: 41,
      }),
    };
    let decoded = Row::of(&entry).decode().unwrap();
    assert_eq!(decoded, entry);
  }

  #[test]
  fn a_whiteout_row_round_trips_without_content() {
    let entry = OverlayEntry {
      path: path("gone"),
      present: false,
      kind: OverlayKind::Regular,
      opaque: false,
      ino: 5,
      content: Content::None,
      symlink_target: None,
      size: 0,
      mtime: Timestamp::from_secs(1),
      ctime: Timestamp::from_secs(1),
      renamed_from: None,
      base: Some(BaseFacts {
        oid: oid(3),
        kind: EntryKind::Regular,
        size: 10,
      }),
    };
    assert_eq!(Row::of(&entry).decode().unwrap(), entry);
  }
}
