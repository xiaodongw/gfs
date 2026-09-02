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
//! | delegate | `clone` | `gfs clone`, when the flags translate — the mount stands in for the checkout, and in a shim-configured environment `git clone <url>` is *the* way a workspace comes to exist. A failure (no `gfs`, no gateway, a flag with no translation) falls back to real `git clone`, with a note |
//! | fail | `gc`, `repack`, `prune`, `fsck`, `maintenance` | exit 2 with the reason: each one walks or rewrites the whole object database, which through a projection is a monorepo download (`repack -a` measured 6.6 GiB on linux) |
//! | advise | `blame` | run it, after a stderr note that `gfs blame` answers server-side (blame measured 196 MiB cold; the output format of real `git blame` is preserved because tools parse it) |
//! | pass | everything else | `exec` the real `git` |
//!
//! Outside a GFS workspace the shim always passes through, which is what makes
//! a `PATH`-wide install safe — the limitation ADR 0005 recorded (its shim
//! exited 128 outside a workspace) is gone, and with it ADR 0007's sharpest
//! objection. `clone` is the one delegation that happens *outside* a
//! workspace, necessarily — the workspace is what it creates — and its
//! fall-back-on-any-failure rule is what keeps that safe: with no gateway
//! within reach, `git clone` is real `git clone`. `GFS_SHIM_BYPASS` disables
//! delegation outright.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

/// Object-database maintenance: each of these enumerates or rewrites the whole
/// store, which through a projection means downloading it. Refused with the
/// reason rather than allowed to run for an hour and fail on quota.
const REFUSED: &[&str] = &["gc", "repack", "prune", "fsck", "maintenance"];

pub(crate) fn main() {
  let args: Vec<String> = std::env::args().skip(1).collect();
  let subcommand = args
    .iter()
    .find(|a| !a.starts_with('-'))
    .map(String::as_str)
    .unwrap_or("");

  // The plain spelling only: `git clone <url>`. Globals before the subcommand
  // (`git -c x=y clone`) mean a caller with opinions the translation cannot
  // carry, and they go to real git.
  if args.first().map(String::as_str) == Some("clone")
    && std::env::var_os("GFS_SHIM_BYPASS").is_none()
  {
    // Returns only to fall back.
    gfs_clone(&args[1..]);
  }

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

/// Serve `git clone` through the gateway. Returns only to fall back.
///
/// Any failure — flags with no translation, no `gfs` binary, no reachable
/// gateway, a refused clone — falls through to real `git clone`, so a shell
/// with this shim on PATH never loses the ability to clone; it only gains the
/// mounted kind when the gateway is there to serve one.
fn gfs_clone(clone_args: &[String]) {
  let Some(translated) = translate_clone(clone_args) else {
    return;
  };
  let Some(gfs) = gfs_binary() else {
    return;
  };
  eprintln!("gfs: serving `git clone` from the gateway (gfs clone)");
  match std::process::Command::new(gfs)
    .arg("clone")
    .args(&translated)
    .status()
  {
    Ok(status) if status.success() => std::process::exit(0),
    Ok(status) => {
      eprintln!("gfs: note: gfs clone did not succeed ({status}); falling back to real git clone")
    }
    Err(error) => {
      eprintln!("gfs: note: cannot run gfs clone ({error}); falling back to real git clone")
    }
  }
}

/// Translate `git clone` arguments into `gfs clone` arguments, or decline.
///
/// The URL and target directory carry over as they are; `-b`/`--branch`
/// becomes `--rev`. Flags that only *tune the download* — depth, filters,
/// tags, progress — are dropped, because the mount supersedes what they
/// optimize: there is no download to shallow or filter. Anything else is a
/// caller with intentions this cannot honour, and declining sends the whole
/// invocation to real git.
fn translate_clone(args: &[String]) -> Option<Vec<String>> {
  let mut rev: Option<String> = None;
  let mut positionals: Vec<String> = Vec::new();
  let mut i = 0;
  while i < args.len() {
    let arg = args[i].as_str();
    match arg {
      "-b" | "--branch" => {
        i += 1;
        rev = Some(args.get(i)?.clone());
      }
      "--depth" | "--shallow-since" | "--shallow-exclude" | "--filter" | "--jobs" | "-j" => {
        i += 1;
        args.get(i)?;
      }
      "--single-branch" | "--no-single-branch" | "--no-tags" | "--tags" | "--quiet" | "-q"
      | "--verbose" | "-v" | "--progress" | "--no-progress" | "--no-checkout" => {}
      other => {
        if let Some(branch) = other.strip_prefix("--branch=") {
          rev = Some(branch.to_owned());
        } else if other.starts_with("--depth=")
          || other.starts_with("--filter=")
          || other.starts_with("--shallow-since=")
          || other.starts_with("--shallow-exclude=")
          || other.starts_with("--jobs=")
        {
          // Download tuning; the mount has no download to tune.
        } else if other.starts_with('-') {
          return None;
        } else {
          // The URL, then the directory, exactly git's positional order.
          if positionals.len() == 2 {
            return None;
          }
          positionals.push(other.to_owned());
        }
      }
    }
    i += 1;
  }
  if positionals.is_empty() {
    return None;
  }
  let mut out = positionals;
  if let Some(rev) = rev {
    out.push("--rev".to_owned());
    out.push(rev);
  }
  Some(out)
}

/// Where the `gfs` binary is: an explicit override, this shim's own sibling
/// (the deployment layout, and `target/debug`), or PATH.
fn gfs_binary() -> Option<PathBuf> {
  if let Some(explicit) = std::env::var_os("GFS_BIN") {
    return Some(PathBuf::from(explicit));
  }
  if let Some(sibling) = std::env::current_exe()
    .ok()
    .and_then(|p| p.canonicalize().ok())
    .and_then(|p| p.parent().map(|d| d.join("gfs")))
    .filter(|p| p.is_file())
  {
    return Some(sibling);
  }
  let path = std::env::var_os("PATH")?;
  std::env::split_paths(&path)
    .map(|dir| dir.join("gfs"))
    .find(|candidate| candidate.is_file())
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

#[cfg(test)]
mod tests {
  use super::*;

  fn translated(args: &[&str]) -> Option<Vec<String>> {
    translate_clone(&args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>())
  }

  #[test]
  fn a_plain_clone_carries_the_url_and_directory_over() {
    assert_eq!(
      translated(&["https://example.com/r.git", "dir"]).unwrap(),
      vec!["https://example.com/r.git", "dir"]
    );
  }

  #[test]
  fn branch_becomes_rev_wherever_it_appears() {
    assert_eq!(
      translated(&["-b", "main", "https://example.com/r.git"]).unwrap(),
      vec!["https://example.com/r.git", "--rev", "main"]
    );
    assert_eq!(
      translated(&["https://example.com/r.git", "--branch=v2"]).unwrap(),
      vec!["https://example.com/r.git", "--rev", "v2"]
    );
  }

  #[test]
  fn download_tuning_flags_are_dropped_because_there_is_no_download() {
    assert_eq!(
      translated(&[
        "--depth",
        "1",
        "--filter=blob:none",
        "-q",
        "https://example.com/r.git"
      ])
      .unwrap(),
      vec!["https://example.com/r.git"]
    );
  }

  #[test]
  fn intentions_the_mount_cannot_honour_decline() {
    assert!(translated(&["--mirror", "https://example.com/r.git"]).is_none());
    assert!(translated(&["--bare", "https://example.com/r.git"]).is_none());
    assert!(translated(&["--recurse-submodules", "https://example.com/r.git"]).is_none());
    assert!(translated(&[]).is_none(), "no URL, nothing to serve");
  }
}
