//! Mounting: turning a pinned commit into a live FUSE session.
//!
//! # Both remedies, not either
//!
//! ADR 0003's dispatch rule has two halves and this module supplies one of them:
//! `n_threads` greater than one. The other half — never blocking a callback — is
//! [`crate::fs`]'s. `n_threads` alone caps concurrency at the thread count, and
//! non-blocking dispatch alone leaves a single reader thread to demultiplex every
//! request; the ADR adopts both because the measurement showed each fixing a
//! different half of the 1321 ms serial case.
//!
//! # `allow_other` is a host decision, not a client one
//!
//! ADR 0003 section 2 measured that a bind mount into a container with a different
//! UID needs `allow_other`, that `allow_other` for a non-root mounter needs
//! `user_allow_other` in `/etc/fuse.conf`, and that without it even `dockerd` —
//! running as root — cannot prepare the bind mount and fails with an opaque
//! `error while creating mount source path`. That makes it a documented
//! deployment prerequisite rather than something to enable hopefully. The default
//! here is `Owner`, so an ordinary `cargo test` never needs a privileged host
//! action; the daemon opts in when the deployment has been prepared.

use std::path::Path;

use fuser::{Config, MountOption, SessionACL};
use xvfs_types::error::{ErrorCode, XvfsError};

/// How a mount is presented to the kernel.
#[derive(Clone, Debug)]
pub struct MountConfig {
  /// FUSE event-loop threads. See the module docs: this is one of the two halves
  /// of ADR 0003's rule.
  pub threads: usize,
  /// Whether a different UID may read the mount. Requires `user_allow_other`.
  pub allow_other: bool,
  /// Ask the kernel to unmount if the daemon dies. Requires `allow_other`, and
  /// therefore the same host prerequisite; when unavailable, orphan cleanup is
  /// the orchestrator's job and a leaked mount is visible and inert (`ENOTCONN`)
  /// rather than silently wrong.
  pub auto_unmount: bool,
}

impl Default for MountConfig {
  fn default() -> Self {
    MountConfig {
      // Four rather than one because the ADR requires more than one, and rather
      // than the core count because these threads demultiplex rather than
      // compute: the work itself runs on the tokio runtime.
      threads: 4,
      allow_other: false,
      auto_unmount: false,
    }
  }
}

impl MountConfig {
  pub fn to_fuser_config(&self) -> Config {
    let mut mount_options = vec![
      MountOption::FSName("xvfs".to_owned()),
      MountOption::Subtype("xvfs".to_owned()),
      // Deliberately **not** `MountOption::RO`. M2 set it because the base is
      // immutable and there was no overlay; the kernel then refused every write
      // before it became an upcall, which was both faster and clearer. From M3
      // the overlay makes the workspace writable, and leaving `RO` on would make
      // every mutation fail with `EROFS` without the filesystem ever seeing it --
      // including the ones that are supposed to fail differently, like the
      // `EPERM` a hard link gets.
      MountOption::NoSuid,
      MountOption::NoDev,
      // Nothing here has a meaningful access time: every entry reports the
      // snapshot time, so recording reads would only generate write traffic for
      // a value that never changes.
      MountOption::NoAtime,
      // Let the kernel enforce the mode bits reported by `getattr`. Without it
      // an unsupported-mode entry reported as mode 0 could still be opened, and
      // the refusal would have to be reimplemented in every callback.
      MountOption::DefaultPermissions,
    ];
    if self.auto_unmount {
      mount_options.push(MountOption::AutoUnmount);
    }
    // `Config` is `#[non_exhaustive]`, so it is built from its default and then
    // adjusted. That is the right shape anyway: a new option added upstream picks
    // up its default here rather than failing to compile.
    let mut config = Config::default();
    config.mount_options = mount_options;
    config.acl = if self.allow_other {
      SessionACL::All
    } else {
      SessionACL::Owner
    };
    config.n_threads = Some(self.threads);
    config.clone_fd = true;
    config
  }
}

/// Mount, returning the background session that owns the FUSE connection.
///
/// Dropping the returned session unmounts.
pub fn spawn_mount(
  filesystem: crate::fs::XvfsFilesystem,
  mountpoint: &Path,
  config: &MountConfig,
) -> Result<fuser::BackgroundSession, XvfsError> {
  fuser::spawn_mount(filesystem, mountpoint, &config.to_fuser_config()).map_err(|e| {
    XvfsError::new(
      ErrorCode::FailedPrecondition,
      format!(
        "mounting {}: {e}. A host daemon needs /dev/fuse and fuse3; allow_other \
         additionally needs user_allow_other in /etc/fuse.conf (ADR 0003).",
        mountpoint.display()
      ),
    )
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_default_mount_needs_no_privileged_host_configuration() {
    // ADR 0003's amendment: requiring `user_allow_other` to run `cargo test`
    // would make a privileged host action a prerequisite of the ordinary suite.
    let config = MountConfig::default();
    assert!(!config.allow_other);
    assert_eq!(config.to_fuser_config().acl, SessionACL::Owner);
  }

  #[test]
  fn the_event_loop_has_more_than_one_thread() {
    // Half of ADR 0003's dispatch rule, and the half that is a configuration
    // value rather than a code structure, so it is the half that can regress
    // silently.
    assert!(MountConfig::default().threads > 1);
    assert_eq!(
      MountConfig::default().to_fuser_config().n_threads,
      Some(MountConfig::default().threads)
    );
  }

  #[test]
  fn the_mount_is_writable_and_still_enforces_permissions() {
    // `RO` would make the kernel refuse every mutation before the overlay saw
    // it, which is exactly how M3's write path could look implemented and be
    // unreachable. `DefaultPermissions` stays: an entry reported as mode 0 must
    // still be unopenable without every callback re-checking.
    let options = MountConfig::default().to_fuser_config().mount_options;
    assert!(!options.contains(&MountOption::RO));
    assert!(options.contains(&MountOption::DefaultPermissions));
    assert!(options.contains(&MountOption::NoSuid));
  }
}
