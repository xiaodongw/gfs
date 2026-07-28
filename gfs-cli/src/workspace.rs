//! Finding a workspace's daemon, and talking to it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use gfs_mount::control::{self, Request, Response};
use gfs_mount::state::MountState;

/// Where a workspace keeps its state.
///
/// The default is `<workspace>.gfs`, which is what lets every workspace command
/// find a running daemon from the workspace path alone — the only thing a job
/// reliably knows about itself.
pub fn state_dir_for(workspace: &Path, explicit: &Option<PathBuf>) -> PathBuf {
  explicit.clone().unwrap_or_else(|| {
    let mut name = workspace.as_os_str().to_os_string();
    name.push(".gfs");
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

/// The suffix a state directory carries, relative to its workspace.
const STATE_SUFFIX: &str = ".gfs";

/// Find the workspace an agent is standing in.
///
/// `gfs rg` is invoked the way `rg` is — from inside the tree, with no
/// `--workspace` — so it has to work the path upward looking for the state
/// directory a mount publishes beside its workspace. Failing loudly when there
/// is none is the point: `rg`'s behaviour outside a repository is to search the
/// current directory, and silently doing that here would hydrate the mount.
///
/// # Two shapes, because the workspace is a symlink
///
/// A mount publishes `<workspace>` as a symlink to
/// `<workspace>.gfs/generations/<n>`, so an agent that has merely `cd`-ed into
/// its own workspace has a current directory whose *resolved* form contains the
/// state directory rather than sitting beside it. Searching only for a sibling
/// `<current>.gfs` therefore walks straight past the state directory it is
/// standing inside and reports "not inside a GFS workspace" from within the
/// mount — the one place the tool is documented to work.
///
/// Both shapes are accepted at every level:
///
/// * `<current>.gfs/control.sock` — `current` is the workspace, unresolved;
/// * `<current>/control.sock` — `current` *is* the state directory, which is
///   what an ancestor of the resolved publication path looks like.
pub fn discover(start: &Path) -> Option<(PathBuf, PathBuf)> {
  let mut current = start.canonicalize().ok()?;
  loop {
    let beside = {
      let mut name = current.as_os_str().to_os_string();
      name.push(STATE_SUFFIX);
      PathBuf::from(name)
    };
    if MountState::control_socket(&beside).exists() {
      return Some((current, beside));
    }
    if MountState::control_socket(&current).exists() {
      // Standing in (or under) the state directory itself. The workspace is its
      // name without the suffix; if it does not carry one this is a state
      // directory that was placed explicitly with `--state-dir`, and the
      // publication path is the best answer available.
      let workspace = strip_state_suffix(&current).unwrap_or_else(|| current.clone());
      return Some((workspace, current));
    }
    if !current.pop() {
      return None;
    }
  }
}

/// Find the workspace a standalone tool should talk to.
///
/// The rule every `gfs-*` tool shares: an explicit `--workspace` is an answer
/// and is used directly, and its absence means "discover from where I am
/// standing". Routing the explicit case through [`discover`] made the flag depend
/// on the upward walk it exists to bypass.
pub fn resolve(explicit: &Option<PathBuf>) -> Result<(PathBuf, PathBuf)> {
  match explicit {
    Some(path) => {
      let state_dir = state_dir_for(path, &None);
      if !MountState::control_socket(&state_dir).exists() {
        anyhow::bail!(
          "no GFS daemon state at {} (expected the control socket of the \
           workspace given with --workspace). Is the workspace mounted?",
          state_dir.display()
        );
      }
      Ok((path.clone(), state_dir))
    }
    None => {
      let start = std::env::current_dir().context("reading the current directory")?;
      discover(&start).ok_or_else(|| {
        anyhow::anyhow!(
          "not inside a GFS workspace (looked upward from {}). \
           Run this inside a mount, or pass --workspace.",
          start.display()
        )
      })
    }
  }
}

/// Find the workspace a mount subcommand should act on, honouring `--state-dir`.
///
/// [`resolve`] is the standalone-tool rule and knows nothing about
/// `--state-dir`, because the tools that use it do not offer one. The mount
/// subcommands do, and a state directory placed explicitly at `gfs mount` time
/// has to be nameable again afterwards -- otherwise `--state-dir` would work
/// once and then strand the mount.
///
/// The three cases, in the order they are checked:
///
/// * an explicit state directory is an answer on its own, and the workspace is
///   its name without the `.gfs` suffix (or, for a directory placed somewhere
///   unrelated, the state directory itself);
/// * an explicit `--workspace` gives the default `<workspace>.gfs`;
/// * neither means "discover from where I am standing", which is what makes
///   `cd flask && gfs status` work the way `git status` does.
pub fn locate(
  workspace: &Option<PathBuf>,
  state_dir: &Option<PathBuf>,
) -> Result<(PathBuf, PathBuf)> {
  match (workspace, state_dir) {
    (_, Some(explicit)) => {
      if !MountState::control_socket(explicit).exists() {
        anyhow::bail!(
          "no GFS daemon state at {} (given with --state-dir). Is the workspace mounted?",
          explicit.display()
        );
      }
      let workspace = workspace
        .clone()
        .or_else(|| strip_state_suffix(explicit))
        .unwrap_or_else(|| explicit.clone());
      Ok((workspace, explicit.clone()))
    }
    (Some(_), None) => resolve(workspace),
    (None, None) => resolve(&None),
  }
}

/// `/jobs/42/ws.gfs` -> `/jobs/42/ws`, and `None` when the suffix is absent.
///
/// Operates on the whole path rather than the file name because the result has
/// to stay absolute, and on `OsStr` bytes because a workspace path is not
/// required to be UTF-8.
fn strip_state_suffix(state_dir: &Path) -> Option<PathBuf> {
  let text = state_dir.as_os_str().to_str()?;
  text.strip_suffix(STATE_SUFFIX).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A state directory that looks live enough for discovery: only the socket's
  /// existence is checked, so a plain file stands in for a bound socket.
  fn fake_state_dir(state_dir: &Path) {
    std::fs::create_dir_all(state_dir).unwrap();
    std::fs::write(MountState::control_socket(state_dir), b"").unwrap();
  }

  #[test]
  fn a_workspace_is_found_from_the_workspace_path_itself() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    fake_state_dir(&tmp.path().join("ws.gfs"));

    let (workspace, state_dir) = discover(&ws).expect("the workspace was not found");
    assert_eq!(workspace, ws.canonicalize().unwrap());
    assert_eq!(state_dir, tmp.path().join("ws.gfs"));
  }

  #[test]
  fn a_workspace_is_found_through_its_published_symlink() {
    // The shape a real mount publishes, and the one that made `gfs rg` report
    // "not inside a GFS workspace" from inside the mount: canonicalizing the
    // workspace yields a path *under* the state directory, so the sibling
    // `<current>.gfs` the walk looks for never appears.
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("ws.gfs");
    let generation = state.join("generations/1");
    std::fs::create_dir_all(generation.join("django/utils")).unwrap();
    fake_state_dir(&state);
    std::os::unix::fs::symlink(&generation, tmp.path().join("ws")).unwrap();

    let (workspace, state_dir) =
      discover(&tmp.path().join("ws")).expect("the workspace was not found through the symlink");
    assert_eq!(state_dir, state);
    assert_eq!(workspace, tmp.path().join("ws"));

    // And from a subdirectory, which is where an agent actually stands.
    let (_, from_below) = discover(&tmp.path().join("ws/django/utils"))
      .expect("the workspace was not found from a subdirectory");
    assert_eq!(from_below, state);
  }

  #[test]
  fn a_directory_outside_any_workspace_is_not_discovered() {
    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().join("somewhere/else");
    std::fs::create_dir_all(&plain).unwrap();
    assert!(discover(&plain).is_none());
  }

  #[test]
  fn the_state_directory_is_derivable_from_the_workspace_alone() {
    assert_eq!(
      state_dir_for(Path::new("/jobs/42/ws"), &None),
      PathBuf::from("/jobs/42/ws.gfs")
    );
    assert_eq!(
      state_dir_for(Path::new("/jobs/42/ws"), &Some(PathBuf::from("/elsewhere"))),
      PathBuf::from("/elsewhere")
    );
  }
}
