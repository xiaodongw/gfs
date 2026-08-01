//! Finding a workspace's daemon, and talking to it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use gfs_mount::control::{self, HostRequest, HostResponse, Request, Response};
use gfs_mount::state::MountState;

/// Where a workspace keeps its state (ADR 0011): inside the folder, at
/// `<workspace>/.git/gfs`. This is what lets every workspace command find a
/// running daemon from the workspace path alone — the only thing a job
/// reliably knows about itself.
pub fn state_dir_for(workspace: &Path, explicit: &Option<PathBuf>) -> PathBuf {
  explicit
    .clone()
    .unwrap_or_else(|| gfs_mount::state::state_dir_for_workspace(workspace))
}

/// The control socket a state directory's daemon answers on.
///
/// The socket cannot live in the state directory any more — the state sits
/// under the mount, and a Unix socket cannot be connected to through the
/// passthrough — so the daemon binds in the runtime directory and records the
/// path in `.git/gfs.json`. That file is the authority; the derived runtime
/// path and the legacy in-state-dir socket are fallbacks, in that order, so a
/// new CLI keeps working against an old daemon.
pub fn control_socket_of(state_dir: &Path) -> PathBuf {
  if let Some(git_dir) = state_dir.parent() {
    if let Ok(bytes) = std::fs::read(git_dir.join("gfs.json")) {
      if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
        if let Some(path) = value.get("control_socket").and_then(|v| v.as_str()) {
          return PathBuf::from(path);
        }
      }
    }
    if let Some(workspace) = git_dir.parent() {
      let derived = gfs_mount::state::workspace_control_socket(workspace);
      if derived.exists() {
        return derived;
      }
    }
  }
  MountState::control_socket(state_dir)
}

/// One request, one response, over the control socket.
pub fn call(state_dir: &Path, request: &Request) -> Result<Response> {
  let socket = control_socket_of(state_dir);
  let response = control::call(&socket, request)
    .map_err(|e| anyhow::anyhow!("{}: {}", e.code.as_str(), e.message))?;
  response
    .into_result()
    .map_err(|e| anyhow::anyhow!("{}: {}", e.code.as_str(), e.message))
}

/// One request, one response, over a host socket.
///
/// Separate from [`call`] because the two speak different vocabularies over the
/// same framing: [`call`] asks a workspace about itself, this asks the `gfs-fuse`
/// process about the mounts it is serving.
pub fn call_host(socket: &Path, request: &HostRequest) -> Result<HostResponse> {
  let response = control::call_host(socket, request)
    .map_err(|e| anyhow::anyhow!("{}: {}", e.code.as_str(), e.message))?;
  response
    .into_result()
    .map_err(|e| anyhow::anyhow!("{}: {}", e.code.as_str(), e.message))
}

/// The suffix a pre-ADR-0011 state directory carried, relative to its
/// workspace. Kept so discovery still answers against an old daemon.
const STATE_SUFFIX: &str = ".gfs";

/// Whether a state directory has a reachable daemon behind it.
fn is_workspace_state(state_dir: &Path) -> bool {
  control_socket_of(state_dir).exists()
}

/// Find the workspace an agent is standing in.
///
/// `gfs rg` is invoked the way `rg` is — from inside the tree, with no
/// `--workspace` — so it has to work the path upward looking for the state a
/// mount leaves inside its workspace. Failing loudly when there is none is
/// the point: `rg`'s behaviour outside a repository is to search the current
/// directory, and silently doing that here would hydrate the mount.
///
/// # Three shapes
///
/// The single-mount layout (ADR 0011) puts everything at
/// `<workspace>/.git/gfs`, and `.git/gfs.json` names the control socket; the
/// walk finds it the way `git` finds its own root. Two legacy shapes are kept
/// because each costs one `exists` call and keeps a new CLI working against
/// an old daemon:
///
/// * `<current>/.git/gfs/gfs.json`-adjacent state — `current` is the workspace;
/// * `<current>.gfs/control.sock` — the pre-ADR-0011 sibling state directory;
/// * `<current>/control.sock` — `current` *is* an old state directory.
pub fn discover(start: &Path) -> Option<(PathBuf, PathBuf)> {
  let mut current = start.canonicalize().ok()?;
  loop {
    let inside = gfs_mount::state::state_dir_for_workspace(&current);
    if is_workspace_state(&inside) {
      return Some((current, inside));
    }
    let beside = {
      let mut name = current.as_os_str().to_os_string();
      name.push(STATE_SUFFIX);
      PathBuf::from(name)
    };
    if MountState::control_socket(&beside).exists() {
      return Some((current, beside));
    }
    if MountState::control_socket(&current).exists() {
      // Standing in (or under) an old-layout state directory itself. The
      // workspace is its name without the suffix; if it does not carry one
      // this is a state directory that was placed explicitly with
      // `--state-dir`, and the publication path is the best answer available.
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
      if !is_workspace_state(&state_dir) {
        anyhow::bail!(
          "no GFS daemon state at {} (expected the state of the workspace \
           given with --workspace). Is the workspace mounted?",
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
      if !is_workspace_state(explicit) {
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

/// Turn a path as a caller typed it into one relative to the snapshot root.
///
/// An agent stands in a subdirectory and names a file the way `git blame` takes
/// one — relative to *there*. The snapshot only knows root-relative paths, so
/// the translation happens here rather than by requiring the caller to know
/// where the workspace root is.
///
/// Resolved **lexically** after canonicalizing the current directory, not with
/// `canonicalize` on the whole path: the named file need not exist in the
/// workspace at all. `gfs blame old.rs --rev HEAD~5` is about a file that was
/// deleted, and demanding it exist now would refuse the most useful case.
pub fn repo_relative(workspace_root: &Path, path: &str) -> Result<Vec<u8>> {
  use std::os::unix::ffi::OsStrExt;

  let root = workspace_root
    .canonicalize()
    .unwrap_or_else(|_| workspace_root.to_path_buf());
  let candidate = Path::new(path);
  let absolute = if candidate.is_absolute() {
    candidate.to_path_buf()
  } else {
    let cwd = std::env::current_dir().context("reading the current directory")?;
    cwd.canonicalize().unwrap_or(cwd).join(candidate)
  };

  let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
  for component in absolute.components() {
    match component {
      std::path::Component::CurDir => {}
      std::path::Component::ParentDir => {
        parts.pop();
      }
      std::path::Component::Normal(name) => parts.push(name),
      // A root or prefix component resets the accumulation, so an absolute path
      // does not inherit anything from the current directory.
      _ => parts.clear(),
    }
  }
  let normalized: PathBuf = std::iter::once(std::path::Component::RootDir.as_os_str())
    .chain(parts)
    .collect();

  let relative = normalized.strip_prefix(&root).map_err(|_| {
    anyhow::anyhow!(
      "{} is outside the workspace at {}",
      normalized.display(),
      root.display()
    )
  })?;
  Ok(relative.as_os_str().as_bytes().to_vec())
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
  fn a_workspace_is_found_from_a_subdirectory_of_the_mount() {
    // Where an agent actually stands. The workspace is the mount point itself,
    // so canonicalizing a path inside it stays inside it and the sibling
    // `<current>.gfs` is reached on the way up.
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("ws.gfs");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(ws.join("django/utils")).unwrap();
    fake_state_dir(&state);

    let (workspace, state_dir) =
      discover(&ws.join("django/utils")).expect("the workspace was not found from a subdirectory");
    assert_eq!(state_dir, state);
    assert_eq!(workspace, ws.canonicalize().unwrap());
  }

  #[test]
  fn a_caller_standing_in_the_state_directory_is_still_located() {
    // The second shape. Cheap to keep, and it is what a resolved current
    // directory looked like under the symlink publication ADR 0003's second
    // amendment replaced -- so a new CLI keeps working against an old daemon.
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("ws.gfs");
    std::fs::create_dir_all(state.join("generations/1")).unwrap();
    fake_state_dir(&state);

    let (workspace, state_dir) =
      discover(&state.join("generations/1")).expect("the state directory was not recognized");
    assert_eq!(state_dir, state);
    assert_eq!(workspace, tmp.path().join("ws"));
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
    // ADR 0011: inside the folder, under the real git dir.
    assert_eq!(
      state_dir_for(Path::new("/jobs/42/ws"), &None),
      PathBuf::from("/jobs/42/ws/.git/gfs")
    );
    assert_eq!(
      state_dir_for(Path::new("/jobs/42/ws"), &Some(PathBuf::from("/elsewhere"))),
      PathBuf::from("/elsewhere")
    );
  }
}
