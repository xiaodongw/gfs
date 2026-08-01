//! The fsmonitor hook: what makes `git status` O(changes) inside a mount.
//!
//! `core.fsmonitor` names this binary (via a one-line hook script the daemon
//! writes), and Git invokes it before every index refresh with a version and
//! the last token it stored. The answer — which paths changed since that token
//! — is exactly what the overlay journal knows, and the daemon serves it over
//! the workspace control socket.
//!
//! This is the closure of the caveat that decided ADR 0005:
//! `spikes/reports/m05-git-surface.md` set fsmonitor aside as "not available
//! through a FUSE mount without further work". The work is this file plus one
//! control request, and the measured effect is `git status` on the Linux
//! kernel's 94 851 files going from 108 445 FUSE lookups and 18.8 s to 170
//! lookups and 49 ms.
//!
//! # Protocol (hook version 2)
//!
//! Arguments: `<version> <last-token>`. Output: the new token, NUL, then
//! changed paths NUL-separated; the single path `/` means "rescan everything".
//! Any failure here prints `/`, because a hook that answers "nothing changed"
//! when it does not know is a hook that makes `git status` lie.
//!
//! # No credential, like the shim
//!
//! The control socket is a local `SOCK_STREAM` at mode 0600 carrying no token;
//! a process that can open it can already read the workspace. The workspace is
//! found from the hook's own working directory — Git runs hooks at the worktree
//! root — through the `.git` file's `gitdir:` pointer.

use std::io::Write;
use std::path::PathBuf;

fn main() {
  let args: Vec<String> = std::env::args().collect();
  // args: [exe, version, token]. Version 2 is the only one with this wire form;
  // an unknown version gets a full-rescan answer, which every version reads
  // correctly.
  let version_ok = args.get(1).map(String::as_str) == Some("2");
  let token = args.get(2).cloned().unwrap_or_default();

  match answer(version_ok, &token) {
    Some((new_token, paths, full_rescan)) => {
      let mut out = Vec::new();
      out.extend_from_slice(new_token.as_bytes());
      out.push(0);
      if full_rescan {
        out.extend_from_slice(b"/");
        out.push(0);
      } else {
        for path in paths {
          out.extend_from_slice(path.as_bytes());
          out.push(0);
        }
      }
      let _ = std::io::stdout().write_all(&out);
    }
    None => {
      // Unknown workspace or unreachable daemon: tell Git to look for itself.
      let _ = std::io::stdout().write_all(b"gfs:unknown\0/\0");
    }
  }
}

fn answer(version_ok: bool, token: &str) -> Option<(String, Vec<String>, bool)> {
  let socket = find_control_socket()?;
  let response = gfs_mount::control::call(
    &socket,
    &gfs_mount::control::Request::FsMonitorChanges {
      token: token.to_owned(),
    },
  )
  .ok()?;
  let gfs_mount::control::Response::FsMonitorChanges(answer) = response else {
    return None;
  };
  Some((
    answer.token,
    answer.paths,
    answer.full_rescan || !version_ok,
  ))
}

/// Walk from the worktree root to the daemon's control socket.
///
/// cwd is the worktree root (Git sets it for hooks). `.git` there is a real
/// directory (ADR 0011) holding `gfs.json`, which names the socket; the
/// gitfile shape is still read so this hook keeps working against a
/// pre-ADR-0011 daemon. Nothing else is trusted: a workspace this walk does
/// not recognize gets a full-rescan answer, never a guess.
fn find_control_socket() -> Option<PathBuf> {
  let cwd = std::env::current_dir().ok()?;
  let dotgit = cwd.join(".git");
  let git_dir = if dotgit.is_dir() {
    dotgit
  } else {
    let git_file = std::fs::read_to_string(&dotgit).ok()?;
    PathBuf::from(git_file.trim().strip_prefix("gitdir: ")?)
  };
  let facts = std::fs::read_to_string(git_dir.join("gfs.json")).ok()?;
  let facts: serde_json::Value = serde_json::from_str(&facts).ok()?;
  Some(PathBuf::from(facts.get("control_socket")?.as_str()?))
}
