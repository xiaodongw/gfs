//! The mount-publication seam.
//!
//! ADR 0003's amendment asks M2 for exactly one thing in exchange for deferring
//! the Kubernetes measurement: *keep mount publication behind one replaceable
//! step*. A CSI node plugin and a host daemon differ only in how a mount becomes
//! visible to the job — not in the inode model, the blob cache, or the `.git`
//! surface — so the later answer must not ripple into the filesystem code.
//!
//! This module is that step, and it is deliberately small. Everything above it
//! knows only two things: where the daemon should create its FUSE session, and
//! how that session becomes reachable at the workspace path.
//!
//! # The workspace is the mount point
//!
//! ADR 0003's second amendment removed the generation model, and with it the
//! reason this module ever moved anything. A job's workspace is now the FUSE
//! mount itself, so [`DirectMountPublisher::mountpoint`] *is* the workspace and
//! [`MountPublisher::publish`] has nothing left to do. `gfs switch` re-points
//! the filesystem underneath rather than swapping what the workspace resolves
//! to, which is what makes a workspace behave like a Git working tree: the path
//! a job is standing in stays the path it was standing in.
//!
//! The seam survives the simplification because the reason for it was never the
//! symlink. A privileged deployment still publishes differently:
//!
//! * `DirectMountPublisher` — mount at the workspace. Needs no capability, which
//!   is ADR 0003's whole argument, and is what the local daemon uses.
//! * a bind-mount or CSI publisher (M6.1, M7.4) — mount somewhere private, then
//!   `move_mount(2)` it onto the workspace. Needs `CAP_SYS_ADMIN` where it runs;
//!   the job still needs none.
//!
//! Those differ in exactly the two methods below, which is the seam doing its
//! job.

use std::path::{Path, PathBuf};

use gfs_types::error::{ErrorCode, GfsError};

/// How a mount becomes visible to the job.
pub trait MountPublisher: Send + Sync + std::fmt::Debug {
  /// Where the daemon should create its FUSE session.
  ///
  /// Not necessarily the workspace: a privileged publisher mounts somewhere
  /// private and moves it. The daemon creates this directory if it is absent.
  fn mountpoint(&self) -> &Path;

  /// Make the mount reachable at the workspace. Called once, after the session
  /// is serving.
  fn publish(&self) -> Result<(), GfsError>;

  /// Make it unreachable again. Idempotent, and called before the unmount so a
  /// read in flight sees a missing workspace rather than `ENOTCONN`.
  fn unpublish(&self) -> Result<(), GfsError>;

  /// The path the job sees.
  fn workspace(&self) -> &Path;

  /// A one-line description for `gfs inspect`, so an operator can tell which
  /// publication mechanism a mount is using without reading configuration.
  fn describe(&self) -> String;
}

/// Mounts at the workspace itself. The unprivileged local form.
#[derive(Clone, Debug)]
pub struct DirectMountPublisher {
  workspace: PathBuf,
}

impl DirectMountPublisher {
  /// Refuses a workspace that exists and holds anything besides its own
  /// `.git`.
  ///
  /// Mounting over a populated directory succeeds and *hides* what was there,
  /// which is a data-loss shape rather than an error: the files come back when
  /// the job ends, having been invisible for its whole run. The one exception
  /// is the layout's own: ADR 0011 puts the real `.git` inside the workspace
  /// precisely so the mount can shadow it and serve it back through the
  /// retained handle. A symlink left by an older build is removed rather than
  /// refused, so upgrading over an existing lab directory does not need manual
  /// cleanup.
  pub fn new(workspace: PathBuf) -> Result<Self, GfsError> {
    match std::fs::symlink_metadata(&workspace) {
      Ok(meta) if meta.file_type().is_symlink() => {
        std::fs::remove_file(&workspace).map_err(|e| {
          GfsError::internal(format!(
            "removing the workspace symlink left by an older GFS: {}",
            e.kind()
          ))
        })?;
      }
      Ok(meta) if meta.is_dir() => {
        let stray = std::fs::read_dir(&workspace)
          .map_err(|e| {
            GfsError::internal(format!("reading the workspace directory: {}", e.kind()))
          })?
          .filter_map(Result::ok)
          .find(|entry| entry.file_name() != crate::passthrough::GIT_DIR_NAME);
        if let Some(stray) = stray {
          return Err(GfsError::new(
            ErrorCode::FailedPrecondition,
            format!(
              "{} contains {:?} besides .git; mounting over it would hide it for the \
               life of the job (unmounted workspaces hold only their .git — if this \
               folder was copied while mounted, copy it again after unmounting)",
              workspace.display(),
              stray.file_name(),
            ),
          ));
        }
      }
      Ok(_) => {
        return Err(GfsError::new(
          ErrorCode::FailedPrecondition,
          format!(
            "{} exists and is not a directory; a workspace is a mount point",
            workspace.display()
          ),
        ));
      }
      Err(_) => {}
    }
    Ok(DirectMountPublisher { workspace })
  }
}

impl MountPublisher for DirectMountPublisher {
  fn mountpoint(&self) -> &Path {
    &self.workspace
  }

  /// Nothing to do: the session is already serving the workspace path.
  fn publish(&self) -> Result<(), GfsError> {
    Ok(())
  }

  /// Also nothing. The unmount is what makes the workspace unreachable, and
  /// removing the directory here would race the session that is still on it.
  fn unpublish(&self) -> Result<(), GfsError> {
    Ok(())
  }

  fn workspace(&self) -> &Path {
    &self.workspace
  }

  fn describe(&self) -> String {
    format!("direct({})", self.workspace.display())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_workspace_is_the_mount_point() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("ws");
    let publisher = DirectMountPublisher::new(workspace.clone()).unwrap();
    assert_eq!(publisher.mountpoint(), workspace);
    assert_eq!(publisher.workspace(), workspace);
    // Both are no-ops, and both are idempotent, because job cleanup runs more
    // than once and startup must not depend on ordering that no longer exists.
    publisher.publish().unwrap();
    publisher.publish().unwrap();
    publisher.unpublish().unwrap();
    publisher.unpublish().unwrap();
  }

  #[test]
  fn an_empty_or_git_only_directory_is_accepted_and_a_populated_one_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let empty = tmp.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    DirectMountPublisher::new(empty).unwrap();

    // The single-mount layout: a workspace holding exactly its own `.git` is
    // what the mount is *supposed* to shadow (ADR 0011).
    let adopted = tmp.path().join("adopted");
    std::fs::create_dir_all(adopted.join(".git/gfs")).unwrap();
    DirectMountPublisher::new(adopted).unwrap();

    let populated = tmp.path().join("populated");
    std::fs::create_dir(&populated).unwrap();
    std::fs::write(populated.join("work.txt"), b"not mine to hide").unwrap();
    let e = DirectMountPublisher::new(populated).unwrap_err();
    assert_eq!(e.code, ErrorCode::FailedPrecondition);
  }

  #[test]
  fn a_symlink_from_an_older_build_is_cleaned_up_rather_than_refused() {
    // Upgrading over a lab directory created by the generation-model build must
    // not require the operator to know that the layout changed.
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("generations/1");
    std::fs::create_dir_all(&target).unwrap();
    let workspace = tmp.path().join("ws");
    std::os::unix::fs::symlink(&target, &workspace).unwrap();

    DirectMountPublisher::new(workspace.clone()).unwrap();
    assert!(
      std::fs::symlink_metadata(&workspace).is_err(),
      "the stale symlink is gone, leaving the path free to be a mount point"
    );
  }

  #[test]
  fn a_regular_file_at_the_workspace_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("ws");
    std::fs::write(&workspace, b"").unwrap();
    let e = DirectMountPublisher::new(workspace).unwrap_err();
    assert_eq!(e.code, ErrorCode::FailedPrecondition);
  }
}
