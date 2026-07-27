//! Finding a workspace's daemon, and talking to it.

use std::path::{Path, PathBuf};

use anyhow::Result;
use xvfs_fuse::control::{self, Request, Response};
use xvfs_fuse::state::MountState;

/// Where a workspace keeps its state.
///
/// The default is `<workspace>.xvfs`, which is what lets every workspace command
/// find a running daemon from the workspace path alone — the only thing a job
/// reliably knows about itself.
pub fn state_dir_for(workspace: &Path, explicit: &Option<PathBuf>) -> PathBuf {
  explicit.clone().unwrap_or_else(|| {
    let mut name = workspace.as_os_str().to_os_string();
    name.push(".xvfs");
    PathBuf::from(name)
  })
}

/// One request, one response, over the control socket.
pub fn call(state_dir: &Path, request: &Request) -> Result<Response> {
  let socket = MountState::control_socket(state_dir);
  let response = control::call(&socket, request)
    .map_err(|e| anyhow::anyhow!("{}: {}", e.code.as_str(), e.message))?;
  response
    .into_result()
    .map_err(|e| anyhow::anyhow!("{}: {}", e.code.as_str(), e.message))
}

/// Find the workspace an agent is standing in.
///
/// `xvfs-rg` is invoked the way `rg` is — from inside the tree, with no
/// `--workspace` — so it has to work the path upward looking for the state
/// directory a mount publishes beside its workspace. Failing loudly when there
/// is none is the point: `rg`'s behaviour outside a repository is to search the
/// current directory, and silently doing that here would hydrate the mount.
pub fn discover(start: &Path) -> Option<(PathBuf, PathBuf)> {
  let mut current = start.canonicalize().ok()?;
  loop {
    let candidate = {
      let mut name = current.as_os_str().to_os_string();
      name.push(".xvfs");
      PathBuf::from(name)
    };
    if MountState::control_socket(&candidate).exists() {
      return Some((current, candidate));
    }
    if !current.pop() {
      return None;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_state_directory_is_derivable_from_the_workspace_alone() {
    assert_eq!(
      state_dir_for(Path::new("/jobs/42/ws"), &None),
      PathBuf::from("/jobs/42/ws.xvfs")
    );
    assert_eq!(
      state_dir_for(Path::new("/jobs/42/ws"), &Some(PathBuf::from("/elsewhere"))),
      PathBuf::from("/elsewhere")
    );
  }
}
