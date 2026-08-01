//! The `grep`/`find` shim: a degrade rule, not a refusal (ADR 0009).
//!
//! Installed early in `PATH` inside an agent image under the names `grep`,
//! `find`, and `rg`, dispatching on the name it was invoked as.
//!
//! The compatibility boundary ADR 0009 bought is that unsupported commands
//! *work slowly* instead of failing or lying, with the hydration budget as the
//! enforcement. So this shim never substitutes output and never refuses: it
//! says the cheap route once on stderr — where it reaches the agent without
//! contaminating stdout for tools — and then `exec`s the real binary. The
//! sweep the note warns about is priced by the budget, not policed here.
//!
//! | invoked as | when a note is printed | the cheap route it names |
//! | --- | --- | --- |
//! | `grep` | a recursive flag is present | `gfs rg` (server-side, zero hydration) |
//! | `rg` | always (recursion is its default) | `gfs rg` |
//! | `find` | always (walking is its purpose) | `git ls-files` (free) |
//!
//! Outside a GFS workspace the shim always passes through silently, the same
//! property that makes the `git` shim safe to install `PATH`-wide.
//!
//! Detection is by working directory, like the `git` shim: a sweep pointed at
//! a workspace from outside it is not noted, which is accepted — the budget
//! still prices it, and a cwd rule is one an agent can predict.
//!
//! The workspace-detection and PATH-resolution helpers are duplicated from
//! `gfs-git-shim.rs` rather than shared: this crate is deliberately entry
//! points only, and each shim stays one self-contained file.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

fn main() {
  let args: Vec<String> = std::env::args().skip(1).collect();
  let invoked_as = std::env::args()
    .next()
    .map(PathBuf::from)
    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    .unwrap_or_default();

  let tool = match invoked_as.as_str() {
    "grep" | "find" | "rg" => invoked_as.clone(),
    _ => {
      eprintln!(
        "gfs-scan-shim: run me as `grep`, `find`, or `rg` (a symlink named for \
         the tool); `gfs install-shim` sets this up"
      );
      std::process::exit(2);
    }
  };

  if in_gfs_workspace() {
    match tool.as_str() {
      "grep" if wants_recursion(&args) => eprintln!(
        "gfs: note: recursive grep reads every file it visits, and this tree is a \
         lazy projection — a full sweep hydrates it and can hit the hydration \
         budget (EDQUOT, \"Disk quota exceeded\"). `gfs rg <pattern>` searches \
         server-side without hydrating. Continuing with real grep."
      ),
      "rg" => eprintln!(
        "gfs: note: rg sweeps the tree by default, and this tree is a lazy \
         projection — the sweep hydrates it and can hit the hydration budget \
         (EDQUOT). `gfs rg` takes the same flags and searches server-side without \
         hydrating. Continuing with real rg."
      ),
      "find" => eprintln!(
        "gfs: note: find walks a lazily projected tree; name/path tests are cheap \
         but `-exec` and content tests hydrate what they touch. `git ls-files` \
         lists tracked paths for free. Continuing with real find."
      ),
      _ => {}
    }
  }

  let Some(real) = real_tool(&tool) else {
    eprintln!("gfs: no real `{tool}` found on PATH beyond this shim");
    std::process::exit(127);
  };
  let error = std::process::Command::new(real).args(&args).exec();
  eprintln!("gfs: cannot exec real {tool}: {error}");
  std::process::exit(127);
}

/// Whether this grep invocation will walk a tree rather than read named files.
///
/// The flags GNU grep and BSD grep spell recursion with. `--` ends flag
/// parsing; nothing after it can request recursion.
fn wants_recursion(args: &[String]) -> bool {
  for arg in args {
    if arg == "--" {
      return false;
    }
    if arg == "-r" || arg == "-R" || arg == "--recursive" || arg == "--dereference-recursive" {
      return true;
    }
    // Grouped short flags (`-rn`), and `-d recurse` / `--directories=recurse`.
    if arg.starts_with('-')
      && !arg.starts_with("--")
      && arg.chars().skip(1).any(|c| c == 'r' || c == 'R')
    {
      return true;
    }
    if arg == "--directories=recurse" {
      return true;
    }
  }
  false
}

/// Whether the working directory is inside a GFS workspace.
///
/// The workspace's `.git` is a real directory holding `gfs.json` (ADR 0011);
/// the older gitfile shape is still recognized. Detection must be exact —
/// noting on an ordinary repository would teach agents to ignore the note —
/// so anything else is "no".
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

/// The first binary named `tool` on PATH that is not this shim.
///
/// Compared by canonical path rather than by directory, so a symlink installed
/// under another name still cannot exec itself in a loop.
fn real_tool(tool: &str) -> Option<PathBuf> {
  let own = std::env::current_exe()
    .ok()
    .and_then(|p| p.canonicalize().ok());
  let path = std::env::var_os("PATH")?;
  for dir in std::env::split_paths(&path) {
    let candidate = dir.join(tool);
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
