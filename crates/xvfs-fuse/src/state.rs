//! `mount.json`: what a mount is, durably.
//!
//! DESIGN.md section 8.3 lists it as the first thing in a job's state directory,
//! and PLAN.md M2.1 fixes the contents: pinned repository, commit, snapshot time,
//! lease state, API version, and format version. It exists so that a restarted
//! daemon can resume a live mount rather than abandoning a job, and so that an
//! orchestrator cleaning up after a crash can tell what to release.
//!
//! # It holds a credential, and that is why it is 0600
//!
//! The mount capability is in this file. It has to be: a daemon that restarts
//! cannot renew a lease it cannot prove it holds, and the alternative — asking
//! the control plane to re-issue one — would make lease renewal depend on the
//! very authentication round trip that a control-plane outage removes. The file
//! is written 0600 in a directory the daemon owns, which is the same boundary
//! ADR 0006 puts around the blob cache.
//!
//! # Writes are atomic
//!
//! Temporary file, `fsync`, rename. A `mount.json` truncated by a crash is worse
//! than no `mount.json` at all: the orchestrator would read a valid-looking file
//! naming a lease it cannot release.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use xvfs_types::error::{ErrorCode, XvfsError};
use xvfs_types::{LeaseState, Timestamp};

pub const MOUNT_STATE_FILE: &str = "mount.json";
pub const CONTROL_SOCKET_FILE: &str = "control.sock";
pub const GENERATIONS_DIR: &str = "generations";

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LeaseRecord {
  pub state: LeaseState,
  pub expires_at: Timestamp,
  pub heartbeat_interval_secs: u64,
  /// The capability. See the module docs: this is why the file is 0600.
  pub capability: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MountState {
  /// Refused rather than misread when it does not match this build.
  pub state_format_version: u32,
  pub api_version: String,
  pub mount_id: String,
  pub repository_id: String,
  /// The selector the job asked for. Kept because `xvfs refresh` re-resolves it,
  /// and because an operator reading the file wants to know what was requested,
  /// not only what it resolved to.
  pub revision_selector: String,
  pub commit: String,
  pub tree: String,
  pub ref_name: Option<String>,
  pub snapshot_time: Timestamp,
  pub grpc_endpoint: String,
  pub http_endpoint: String,
  pub workspace: PathBuf,
  /// The generation currently published. Incremented by `xvfs refresh`.
  pub generation: u64,
  pub lease: LeaseRecord,
  pub daemon_pid: u32,
}

impl MountState {
  pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join(MOUNT_STATE_FILE)
  }

  pub fn load(state_dir: &Path) -> Result<Self, XvfsError> {
    let path = MountState::path(state_dir);
    let bytes = std::fs::read(&path).map_err(|e| {
      XvfsError::new(
        ErrorCode::NotFound,
        format!("no mount state at {}: {}", path.display(), e.kind()),
      )
    })?;
    let state: MountState = serde_json::from_slice(&bytes)
      .map_err(|e| XvfsError::internal(format!("mount state is unreadable: {e}")))?;
    if state.state_format_version != xvfs_types::STATE_FORMAT_VERSION {
      return Err(XvfsError::new(
        ErrorCode::FailedPrecondition,
        format!(
          "mount state format {} was written by a different XVFS version (this build speaks {})",
          state.state_format_version,
          xvfs_types::STATE_FORMAT_VERSION
        ),
      ));
    }
    Ok(state)
  }

  pub fn store(&self, state_dir: &Path) -> Result<(), XvfsError> {
    let path = MountState::path(state_dir);
    let temporary = state_dir.join(".mount.json.tmp");
    let mut bytes = serde_json::to_vec_pretty(self)
      .map_err(|e| XvfsError::internal(format!("serializing mount state: {e}")))?;
    bytes.push(b'\n');
    {
      let mut file = std::fs::File::create(&temporary)
        .map_err(|e| XvfsError::internal(format!("creating the mount state temporary: {e}")))?;
      file
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|e| XvfsError::internal(format!("restricting mount state permissions: {e}")))?;
      file
        .write_all(&bytes)
        .map_err(|e| XvfsError::internal(format!("writing mount state: {e}")))?;
      file
        .sync_all()
        .map_err(|e| XvfsError::internal(format!("syncing mount state: {e}")))?;
    }
    std::fs::rename(&temporary, &path)
      .map_err(|e| XvfsError::internal(format!("publishing mount state: {e}")))?;
    Ok(())
  }

  pub fn generation_dir(state_dir: &Path, generation: u64) -> PathBuf {
    state_dir.join(GENERATIONS_DIR).join(generation.to_string())
  }

  pub fn control_socket(state_dir: &Path) -> PathBuf {
    state_dir.join(CONTROL_SOCKET_FILE)
  }
}

/// Create the state directory tree with the permissions the daemon owns.
pub fn prepare_state_dir(state_dir: &Path) -> Result<(), XvfsError> {
  std::fs::create_dir_all(state_dir.join(GENERATIONS_DIR))
    .map_err(|e| XvfsError::internal(format!("creating the state directory: {e}")))?;
  // 0700: the state directory holds the capability and, from M3, the overlay.
  // Jobs reach content through the mount, never by reading the daemon's state.
  std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700))
    .map_err(|e| XvfsError::internal(format!("restricting the state directory: {e}")))?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn sample(dir: &Path) -> MountState {
    MountState {
      state_format_version: xvfs_types::STATE_FORMAT_VERSION,
      api_version: xvfs_types::API_VERSION.to_owned(),
      mount_id: "m-1".to_owned(),
      repository_id: "r-1".to_owned(),
      revision_selector: "main".to_owned(),
      commit: "sha1:".to_owned() + &"ab".repeat(20),
      tree: "sha1:".to_owned() + &"cd".repeat(20),
      ref_name: Some("refs/heads/main".to_owned()),
      snapshot_time: Timestamp::from_secs(1_600_000_000),
      grpc_endpoint: "http://127.0.0.1:8431".to_owned(),
      http_endpoint: "http://127.0.0.1:8430".to_owned(),
      workspace: dir.join("ws"),
      generation: 1,
      lease: LeaseRecord {
        state: LeaseState::Active,
        expires_at: Timestamp::from_secs(1_600_001_800),
        heartbeat_interval_secs: 300,
        capability: "xvfs1.secret".to_owned(),
      },
      daemon_pid: 4242,
    }
  }

  #[test]
  fn mount_state_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    prepare_state_dir(tmp.path()).unwrap();
    let state = sample(tmp.path());
    state.store(tmp.path()).unwrap();
    let loaded = MountState::load(tmp.path()).unwrap();
    assert_eq!(loaded.commit, state.commit);
    assert_eq!(loaded.generation, 1);
    assert_eq!(loaded.lease.capability, "xvfs1.secret");
  }

  #[test]
  fn the_capability_is_never_world_readable() {
    let tmp = tempfile::tempdir().unwrap();
    prepare_state_dir(tmp.path()).unwrap();
    sample(tmp.path()).store(tmp.path()).unwrap();
    let mode = std::fs::metadata(MountState::path(tmp.path()))
      .unwrap()
      .permissions()
      .mode();
    assert_eq!(mode & 0o777, 0o600, "mount.json holds a credential");
    assert_eq!(
      std::fs::metadata(tmp.path()).unwrap().permissions().mode() & 0o777,
      0o700
    );
  }

  #[test]
  fn a_state_file_from_another_format_version_is_refused_not_misread() {
    let tmp = tempfile::tempdir().unwrap();
    prepare_state_dir(tmp.path()).unwrap();
    let mut state = sample(tmp.path());
    state.state_format_version = xvfs_types::STATE_FORMAT_VERSION + 1;
    state.store(tmp.path()).unwrap();
    let e = MountState::load(tmp.path()).unwrap_err();
    assert_eq!(e.code, ErrorCode::FailedPrecondition);
  }

  #[test]
  fn a_missing_state_file_is_not_found_rather_than_internal() {
    // The orchestrator distinguishes "no mount here" from "this mount is broken",
    // and cleanup retries only the second.
    let tmp = tempfile::tempdir().unwrap();
    let e = MountState::load(tmp.path()).unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
  }
}
