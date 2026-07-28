//! The mount-publication seam.
//!
//! ADR 0003's amendment asks M2 for exactly one thing in exchange for deferring
//! the Kubernetes measurement: *keep mount publication behind one replaceable
//! step*. A CSI node plugin and a host daemon differ only in how a mount becomes
//! visible to the job — not in the inode model, the blob cache, or the `.git`
//! surface — so the later answer must not ripple into the filesystem code.
//!
//! This module is that step, and it is deliberately small. Everything above it
//! knows only "make generation N visible at the workspace path".
//!
//! # Why the local implementation is a symlink and not a bind mount
//!
//! `mount --bind` needs `CAP_SYS_ADMIN`. ADR 0003's whole argument is the
//! privilege asymmetry: the daemon needs no capability where it runs, and the job
//! needs none to use what the daemon produced. An unprivileged daemon therefore
//! cannot bind-mount, and a symlink replaced by `rename(2)` gives the property
//! `gfs refresh` actually needs:
//!
//! * the swap is atomic, so no reader ever resolves a half-published path;
//! * a path resolved *after* the swap reaches the new generation;
//! * a descriptor opened *before* it keeps referring to the old one until closed.
//!
//! That is precisely PLAN.md M2.1's requirement that refresh expose only the old
//! or the new generation and never a mixture.
//!
//! The bind-mount and CSI publishers replace this one implementation when M6.1
//! and M7.4 need them.

use std::path::{Path, PathBuf};

use gfs_types::error::{ErrorCode, GfsError};

/// How a mount generation becomes visible to the job.
pub trait MountPublisher: Send + Sync + std::fmt::Debug {
  /// Make `generation` the workspace, replacing whatever was there atomically.
  fn publish(&self, generation: &Path) -> Result<(), GfsError>;
  /// Remove the workspace. Idempotent.
  fn unpublish(&self) -> Result<(), GfsError>;
  /// The path the job sees.
  fn workspace(&self) -> &Path;
  /// A one-line description for `gfs inspect`, so an operator can tell which
  /// publication mechanism a mount is using without reading configuration.
  fn describe(&self) -> String;
}

/// Publishes by atomically replacing a symlink. The unprivileged local form.
#[derive(Clone, Debug)]
pub struct SymlinkPublisher {
  workspace: PathBuf,
}

impl SymlinkPublisher {
  /// Refuses a workspace path that is a real directory.
  ///
  /// A `rename(2)` over a directory fails, so publication would break on the
  /// *second* generation rather than the first — a failure that appears only
  /// during a refresh, which is the worst possible time to discover it.
  pub fn new(workspace: PathBuf) -> Result<Self, GfsError> {
    if let Ok(meta) = std::fs::symlink_metadata(&workspace) {
      if !meta.file_type().is_symlink() {
        return Err(GfsError::new(
          ErrorCode::FailedPrecondition,
          format!(
            "{} already exists and is not a symlink; the local publisher \
             replaces a symlink atomically and cannot replace a directory",
            workspace.display()
          ),
        ));
      }
    }
    Ok(SymlinkPublisher { workspace })
  }
}

impl MountPublisher for SymlinkPublisher {
  fn publish(&self, generation: &Path) -> Result<(), GfsError> {
    let temporary = self
      .workspace
      .with_extension(format!("gfs-publish-{}", std::process::id()));
    let _ = std::fs::remove_file(&temporary);
    std::os::unix::fs::symlink(generation, &temporary)
      .map_err(|e| GfsError::internal(format!("staging the workspace symlink: {}", e.kind())))?;
    // The atomic step. A reader either resolves the old target or the new one.
    std::fs::rename(&temporary, &self.workspace).map_err(|e| {
      let _ = std::fs::remove_file(&temporary);
      GfsError::internal(format!("publishing the workspace: {}", e.kind()))
    })?;
    Ok(())
  }

  fn unpublish(&self) -> Result<(), GfsError> {
    match std::fs::remove_file(&self.workspace) {
      Ok(()) => Ok(()),
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
      Err(e) => Err(GfsError::internal(format!(
        "removing the workspace: {}",
        e.kind()
      ))),
    }
  }

  fn workspace(&self) -> &Path {
    &self.workspace
  }

  fn describe(&self) -> String {
    format!("symlink({})", self.workspace.display())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn publication_is_atomic_and_swaps_generations() {
    let tmp = tempfile::tempdir().unwrap();
    let one = tmp.path().join("generations/1");
    let two = tmp.path().join("generations/2");
    std::fs::create_dir_all(&one).unwrap();
    std::fs::create_dir_all(&two).unwrap();
    std::fs::write(one.join("marker"), b"one").unwrap();
    std::fs::write(two.join("marker"), b"two").unwrap();

    let workspace = tmp.path().join("ws");
    let publisher = SymlinkPublisher::new(workspace.clone()).unwrap();

    publisher.publish(&one).unwrap();
    assert_eq!(std::fs::read(workspace.join("marker")).unwrap(), b"one");

    // Replacing an existing publication must work, not fail with EEXIST.
    publisher.publish(&two).unwrap();
    assert_eq!(std::fs::read(workspace.join("marker")).unwrap(), b"two");

    publisher.unpublish().unwrap();
    assert!(!workspace.exists());
    // Idempotent: job cleanup runs more than once.
    publisher.unpublish().unwrap();
  }

  #[test]
  fn a_descriptor_opened_before_the_swap_still_reads_the_old_generation() {
    // The property `gfs refresh` depends on: open handles keep their generation.
    use std::io::Read;
    let tmp = tempfile::tempdir().unwrap();
    let one = tmp.path().join("generations/1");
    let two = tmp.path().join("generations/2");
    std::fs::create_dir_all(&one).unwrap();
    std::fs::create_dir_all(&two).unwrap();
    std::fs::write(one.join("f"), b"old").unwrap();
    std::fs::write(two.join("f"), b"new").unwrap();

    let workspace = tmp.path().join("ws");
    let publisher = SymlinkPublisher::new(workspace.clone()).unwrap();
    publisher.publish(&one).unwrap();

    let mut open = std::fs::File::open(workspace.join("f")).unwrap();
    publisher.publish(&two).unwrap();

    let mut old = String::new();
    open.read_to_string(&mut old).unwrap();
    assert_eq!(old, "old", "an open descriptor keeps its generation");
    assert_eq!(std::fs::read(workspace.join("f")).unwrap(), b"new");
  }

  #[test]
  fn a_real_directory_is_refused_at_construction_not_at_the_first_refresh() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("ws");
    std::fs::create_dir(&workspace).unwrap();
    let e = SymlinkPublisher::new(workspace).unwrap_err();
    assert_eq!(e.code, ErrorCode::FailedPrecondition);
  }
}
