//! Status: what changed, derived from the journal alone.
//!
//! DESIGN.md section 8.5 is specific about the cost model — "status is derived
//! from the overlay journal and does not scan the base tree" — and that is what
//! makes the `git` shim's `status` both cheaper and more accurate than real Git
//! against this workspace. ADR 0005 measured stock `git status` against a partial
//! clone of the Linux kernel stat'ing all 94 850 index entries; inside a mount
//! that is a full metadata sweep of the monorepo, per invocation, and an agent
//! runs `git status` constantly.
//!
//! # The journal is enough because of what the mutations leave behind
//!
//! Three shapes could have needed a base walk, and none of them does:
//!
//! * `rm -rf src/` — the kernel unlinks each child before it removes the
//!   directory, so the journal holds one whiteout per file. The overlay keeps
//!   those whiteouts when the directory itself goes.
//! * `mv src source` — every base descendant is materialized as a row carrying
//!   `renamed_from`, so each moved file is a rename rather than an add.
//! * an edit that was undone — the local content is hashed as a Git blob and
//!   compared with the base object ID, so a file written and written back is
//!   reported as unchanged rather than as a spurious modification.
//!
//! What remains is [`Status::directory_deletions`]: a whiteout on a directory
//! whose per-file whiteouts are *not* in the journal, which happens when a base
//! directory is replaced wholesale. It is reported separately rather than
//! silently omitted, and the caller — which has base access and the overlay does
//! not — expands it. In an ordinary edit set it is empty.

use gfs_types::{BytePath, HashAlgorithm, ObjectId};

use crate::error::Result;
use crate::state::{Content, OverlayEntry, OverlayKind};
use crate::{Overlay, Resolution};

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
  Added,
  Modified,
  Deleted,
  /// A file became a symlink, or a symlink a directory, and so on. Reported
  /// separately from a modification because a patch has to delete and re-add.
  TypeChanged,
  /// Only the executable bit moved.
  ModeChanged,
  Renamed,
}

impl ChangeKind {
  /// The letter `git status --porcelain` uses.
  pub fn porcelain(self) -> u8 {
    match self {
      ChangeKind::Added => b'A',
      ChangeKind::Modified | ChangeKind::ModeChanged => b'M',
      ChangeKind::Deleted => b'D',
      ChangeKind::TypeChanged => b'T',
      ChangeKind::Renamed => b'R',
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Change {
  pub path: BytePath,
  pub kind: ChangeKind,
  /// Where a renamed path came from.
  pub from: Option<BytePath>,
  pub old_mode: Option<u32>,
  pub new_mode: Option<u32>,
  pub old_oid: Option<String>,
  /// The Git object ID of the workspace's content, computed here.
  pub new_oid: Option<String>,
  pub new_size: u64,
}

impl Change {
  pub fn is_content_change(&self) -> bool {
    !matches!(self.kind, ChangeKind::ModeChanged)
  }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Status {
  /// Sorted by path, so two runs over the same overlay produce identical output
  /// and a diff of two exports is a diff of the work rather than of the ordering.
  pub changes: Vec<Change>,
  /// Directories the overlay deleted without a per-file record. See the module
  /// docs: reported rather than omitted, and expanded by a caller with base
  /// access.
  pub directory_deletions: Vec<BytePath>,
}

impl Status {
  pub fn is_clean(&self) -> bool {
    self.changes.is_empty() && self.directory_deletions.is_empty()
  }
}

impl Overlay {
  /// Compute the change set.
  ///
  /// `algorithm` is the repository's hash algorithm, taken from the mount rather
  /// than assumed, so a SHA-256 repository fails loudly instead of producing
  /// object IDs that are wrong in a way nothing would notice.
  pub fn status(&self, algorithm: HashAlgorithm) -> Result<Status> {
    let mut status = Status::default();
    let rows = self.entries();

    // The source of a rename is a whiteout *and* the origin of a present row.
    // Reporting both would tell a downstream applier to rename the file and then
    // delete the file it renamed, and `git apply` refuses the pair outright with
    // "path ... has been renamed/deleted".
    let renamed_away: std::collections::HashSet<Vec<u8>> = rows
      .iter()
      .filter(|row| row.present)
      .filter_map(|row| row.renamed_from.as_ref())
      .filter(|from| self.resolve(from) == Resolution::Absent)
      .map(|from| from.as_bytes().to_vec())
      .collect();

    for entry in &rows {
      if !entry.present {
        // A directory whiteout is not a change in Git's model -- a directory
        // exists exactly as long as a file under it does -- but it may be hiding
        // base files this journal has no row for.
        if entry
          .base
          .as_ref()
          .is_some_and(|base| base.kind.is_dir_like())
        {
          if !rows
            .iter()
            .any(|other| other.path != entry.path && crate::is_within(&other.path, &entry.path))
          {
            status.directory_deletions.push(entry.path.clone());
          }
          continue;
        }
        let Some(base) = &entry.base else { continue };
        if renamed_away.contains(entry.path.as_bytes()) {
          continue;
        }
        status.changes.push(Change {
          path: entry.path.clone(),
          kind: ChangeKind::Deleted,
          from: None,
          old_mode: Some(base.kind.as_mode()),
          new_mode: None,
          old_oid: Some(base.oid.to_qualified()),
          new_oid: None,
          new_size: 0,
        });
        continue;
      }

      if entry.kind.is_dir() {
        // Git records no directories, so a created one is only a change once
        // something is in it -- and that something has its own row.
        continue;
      }

      // Hash under the *base entry's* key form. An expanded LFS entry's base
      // oid is `lfs-sha256:` over the raw bytes (ADR 0012), and hashing the
      // workspace content as a git blob would make a reverted LFS file
      // compare unequal to its own base forever — a phantom modification
      // that blocks `gfs switch` and pads every commit plan.
      let hash_algorithm = match &entry.base {
        Some(base) if base.oid.algorithm() == HashAlgorithm::LfsSha256 => {
          HashAlgorithm::LfsSha256
        }
        _ => algorithm,
      };
      let new_oid = self.workspace_oid(entry, hash_algorithm)?;
      let new_mode = entry.kind.git_mode();

      // A rename is a rename when the place it came from is genuinely gone. A
      // `renamed_from` pointing at a path that still resolves would mean the
      // source was recreated, and reporting that as a rename would tell a
      // downstream applier to delete a file the workspace still has.
      if let Some(from) = &entry.renamed_from {
        if self.resolve(from) == Resolution::Absent {
          status.changes.push(Change {
            path: entry.path.clone(),
            kind: ChangeKind::Renamed,
            from: Some(from.clone()),
            old_mode: entry.base.as_ref().map(|b| b.kind.as_mode()),
            new_mode: Some(new_mode),
            old_oid: entry.base.as_ref().map(|b| b.oid.to_qualified()),
            new_oid: Some(new_oid.to_qualified()),
            new_size: entry.size,
          });
          continue;
        }
      }

      let Some(base) = &entry.base else {
        status.changes.push(Change {
          path: entry.path.clone(),
          kind: ChangeKind::Added,
          from: None,
          old_mode: None,
          new_mode: Some(new_mode),
          old_oid: None,
          new_oid: Some(new_oid.to_qualified()),
          new_size: entry.size,
        });
        continue;
      };

      let kind = if base.kind.is_dir_like()
        || (base.kind == gfs_types::EntryKind::Symlink) != (entry.kind == OverlayKind::Symlink)
      {
        ChangeKind::TypeChanged
      } else if base.oid != new_oid {
        ChangeKind::Modified
      } else if base.kind.as_mode() != new_mode {
        ChangeKind::ModeChanged
      } else {
        // Written and written back. Not a change, and saying otherwise would put
        // a phantom entry in every `git status` after an aborted edit.
        continue;
      };

      status.changes.push(Change {
        path: entry.path.clone(),
        kind,
        from: None,
        old_mode: Some(base.kind.as_mode()),
        new_mode: Some(new_mode),
        old_oid: Some(base.oid.to_qualified()),
        new_oid: Some(new_oid.to_qualified()),
        new_size: entry.size,
      });
    }

    status.changes.sort_by(|a, b| a.path.cmp(&b.path));
    status.directory_deletions.sort();
    Ok(status)
  }

  /// The Git object ID of what the workspace holds at a row.
  fn workspace_oid(&self, entry: &OverlayEntry, algorithm: HashAlgorithm) -> Result<ObjectId> {
    match &entry.content {
      // Unchanged bytes: the base already told us the object ID, so there is
      // nothing to hash and nothing to fetch.
      Content::Base(oid) => Ok(oid.clone()),
      Content::Local(_) => {
        let mut file = self.open_content(entry)?;
        crate::hash::blob_oid_of_file(algorithm, &mut file, entry.size)
      }
      // A symlink's target *is* its blob, and it is in the row.
      Content::None => crate::hash::blob_oid(
        algorithm,
        entry.symlink_target.as_deref().unwrap_or_default(),
      ),
    }
  }
}
