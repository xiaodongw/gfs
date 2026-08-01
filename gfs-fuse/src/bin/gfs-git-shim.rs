//! The `git` shim: a hint layer, not a grammar (ADR 0009).
//!
//! Installed early in `PATH` inside an agent image, under the name `git`.
//!
//! ADR 0005's shim was load-bearing for correctness: against the synthesized,
//! object-free `.git`, `ls-files` and `diff` exited 0 with empty output, so the
//! shim had to intercept them or they lied. Its frozen grammar was amended
//! twice in four days, which is what a grammar does when the world asks
//! questions it froze out.
//!
//! With a real `.git` over the projected object store, stock Git answers
//! everything truthfully, so this shim routes *cost*, not correctness — and is
//! therefore a fixed short list with a default of **pass through to real
//! Git**. Pressure to grow the list into a grammar is the signal that ADR
//! 0005's mistake is being repeated; the answer to that pressure is "no".
//!
//! | class | commands | behaviour |
//! | --- | --- | --- |
//! | fail | `gc`, `repack`, `prune`, `fsck`, `maintenance` | exit 2 with the reason: each one walks or rewrites the whole object database, which through a projection is a monorepo download (`repack -a` measured 6.6 GiB on linux) |
//! | advise | `blame` | run it, after a stderr note that `gfs blame` answers server-side (blame measured 196 MiB cold; the output format of real `git blame` is preserved because tools parse it) |
//! | pass | everything else | `exec` the real `git` |
//!
//! Outside a GFS workspace the shim always passes through, which is what makes
//! a `PATH`-wide install safe — the limitation ADR 0005 recorded (its shim
//! exited 128 outside a workspace) is gone, and with it ADR 0007's sharpest
//! objection.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

/// Object-database maintenance: each of these enumerates or rewrites the whole
/// store, which through a projection means downloading it. Refused with the
/// reason rather than allowed to run for an hour and fail on quota.
const REFUSED: &[&str] = &["gc", "repack", "prune", "fsck", "maintenance"];

fn main() {
  let args: Vec<String> = std::env::args().skip(1).collect();
  let subcommand = args
    .iter()
    .find(|a| !a.starts_with('-'))
    .map(String::as_str)
    .unwrap_or("");

  if in_gfs_workspace() {
    if REFUSED.contains(&subcommand) {
      eprintln!(
        "gfs: `git {subcommand}` is refused in a GFS workspace: it walks the entire \
         object database, which is projected from the gateway and would be downloaded \
         wholesale (`repack -a` costs the full pack -- 6.6 GiB on a kernel-sized \
         repository). The gateway maintains the object store; nothing here needs it."
      );
      // 2, not 1: several git subcommands use exit 1 as a data-bearing answer
      // ("there were differences"), and a refusal must not be readable as one.
      std::process::exit(2);
    }
    if subcommand == "blame" {
      eprintln!(
        "gfs: note: `git blame` reads history through the projection (measured up to \
         196 MiB cold on a monorepo); `gfs blame <path>` answers on the server. \
         Continuing with real git so the output format is exact."
      );
    }
  }

  // Everything else: the real git, found on PATH past this shim's own
  // directory. `exec` so exit codes, signals, and streams are git's own.
  let Some(git) = real_git() else {
    eprintln!("gfs: no real `git` found on PATH beyond this shim");
    std::process::exit(127);
  };
  let error = std::process::Command::new(git).args(&args).exec();
  eprintln!("gfs: cannot exec real git: {error}");
  std::process::exit(127);
}

/// Whether the working directory is inside a GFS workspace.
///
/// The single-mount shape (ADR 0011): `.git` is a real directory holding
/// `gfs.json`, seeded by the daemon and served back through the workspace
/// mount. Detection must be exact — answering for an ordinary repository
/// would wrap a working `git` in refusals it does not deserve — so anything
/// else is "no". The gitfile shape (`gitdir: <path>` naming a directory with
/// `gfs.json`) is still recognized, so this shim keeps working against a
/// pre-ADR-0011 daemon.
fn in_gfs_workspace() -> bool {
  let Ok(start) = std::env::current_dir() else {
    return false;
  };
  let mut current: Option<&Path> = Some(&start);
  while let Some(directory) = current {
    let dotgit = directory.join(".git");
    if dotgit.is_dir() {
      return dotgit.join("gfs.json").is_file();
    }
    if dotgit.exists() {
      let Ok(content) = std::fs::read_to_string(&dotgit) else {
        return false;
      };
      let Some(git_dir) = content.trim().strip_prefix("gitdir: ") else {
        return false;
      };
      return PathBuf::from(git_dir).join("gfs.json").is_file();
    }
    current = directory.parent();
  }
  false
}

/// The first `git` on PATH that is not this shim.
///
/// Compared by canonical path rather than by directory, so a symlink installed
/// under another name still cannot exec itself in a loop.
fn real_git() -> Option<PathBuf> {
  let own = std::env::current_exe()
    .ok()
    .and_then(|p| p.canonicalize().ok());
  let path = std::env::var_os("PATH")?;
  for dir in std::env::split_paths(&path) {
    let candidate = dir.join("git");
    if !candidate.is_file() {
      continue;
    }
    if let (Some(own), Ok(resolved)) = (&own, candidate.canonicalize()) {
      if &resolved == own {
        continue;
      }
    }
    return Some(candidate);
  }
  None
}
