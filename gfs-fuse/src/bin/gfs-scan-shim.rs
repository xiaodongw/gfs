//! The `grep`/`find`/`rg` shim: the server-side answer first, the real tool
//! as the fallback (ADR 0009, amended).
//!
//! Installed early in `PATH` inside an agent image under the names `grep`,
//! `find`, and `rg`, dispatching on the name it was invoked as.
//!
//! The first version of this shim only *advised*: a stderr note naming the
//! cheap route, then the real tool. In practice the note is read by nobody a
//! sweep is about to hurt — an agent mid-plan does not change tools because
//! stderr suggested one — so the shim now takes the cheap route itself:
//!
//! | invoked as | inside a workspace | when `gfs` refuses a flag |
//! | --- | --- | --- |
//! | `rg` | `gfs rg` with the same argv (flag-compatible) | real `rg`, with a note |
//! | `find` | `gfs find` with the same argv (find's grammar) | real `find`, with a note |
//! | `grep`, recursive | translated to `gfs rg` | real `grep`, with a note |
//! | `grep`, non-recursive | real `grep` (reads only the files it was given) | — |
//!
//! The fallback is what keeps the old contract — unsupported invocations
//! *work slowly* instead of failing: `gfs rg`/`gfs find` refuse an
//! unimplemented flag by name (their contract), the shim hears the refusal on
//! stderr before any output exists, and runs the real tool over the mount.
//! The hydration budget prices that sweep, as it always did. Delegation never
//! starts once output could have: the refusal happens at parse time, so
//! stdout is untouched when the fallback runs.
//!
//! Output fidelity is the reason `grep` is translated conservatively: a BRE
//! pattern that leans on backslash escapes goes to real `grep` rather than to
//! a regex engine that reads them differently — a wrong match set would look
//! exactly like a right one.
//!
//! Outside a GFS workspace the shim always passes through silently, the same
//! property that makes the `git` shim safe to install `PATH`-wide. The
//! `GFS_SHIM_BYPASS` variable forces the same pass-through inside one: it is
//! how `--hydrate` runs the real tool without the shim delegating straight
//! back.
//!
//! The workspace-detection and PATH-resolution helpers are duplicated from
//! `gfs-git-shim.rs` rather than shared: this crate is deliberately entry
//! points only, and each shim stays one self-contained file.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

pub(crate) fn main() {
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

  if in_gfs_workspace() && std::env::var_os("GFS_SHIM_BYPASS").is_none() {
    match tool.as_str() {
      "rg" => delegate(&tool, "rg", args.clone()),
      "find" => delegate(&tool, "find", args.clone()),
      "grep" if wants_recursion(&args) => {
        if let Some(translated) = translate_grep(&args) {
          delegate(&tool, "rg", translated);
        } else {
          eprintln!(
            "gfs: note: this grep invocation uses flags `gfs rg` cannot honour, \
             so it runs over the mount — a recursive sweep hydrates what it \
             reads and is priced by the hydration budget."
          );
        }
      }
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

/// Run the `gfs` subcommand in place of the tool. Returns only to fall back.
///
/// stdout is inherited — results stream exactly as the tool's would — and
/// stderr is captured so a flag refusal can be recognized. The refusal
/// happens at parse time, before any output exists, so returning to run the
/// real tool never splices two tools' output together. `gfs` missing from
/// the image entirely also falls through, so a half-installed shim degrades
/// to the stock behaviour instead of breaking the tool it shadows.
fn delegate(tool: &str, subcommand: &str, args: Vec<String>) {
  let Some(gfs) = gfs_binary() else {
    eprintln!("gfs: note: no `gfs` binary found (set GFS_BIN); running real {tool}");
    return;
  };
  let child = std::process::Command::new(gfs)
    .arg(subcommand)
    .args(&args)
    .stdin(std::process::Stdio::inherit())
    .stdout(std::process::Stdio::inherit())
    .stderr(std::process::Stdio::piped())
    .output();
  let output = match child {
    Ok(output) => output,
    Err(error) => {
      eprintln!("gfs: note: cannot run `gfs {subcommand}` ({error}); running real {tool}");
      return;
    }
  };
  let stderr = String::from_utf8_lossy(&output.stderr);
  if !output.status.success() && stderr.contains("unsupported") {
    eprintln!(
      "gfs: note: `gfs {subcommand}` does not implement a flag this invocation \
       uses; running real {tool} over the mount instead. The sweep is priced \
       by the hydration budget."
    );
    return;
  }
  eprint!("{stderr}");
  std::process::exit(output.status.code().unwrap_or(2));
}

/// Where the `gfs` binary is: an explicit override, this shim's own sibling
/// (the deployment layout), or PATH.
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

/// Translate a recursive grep invocation into `gfs rg` argv, or decline.
///
/// Only flags whose meaning survives the translation exactly are mapped; the
/// rest — and any pattern whose BRE reading could differ from a regex
/// engine's — return `None`, which sends the invocation to real grep. `-E`
/// and `-F` patterns translate faithfully (`gfs rg` is regex by default and
/// literal under `-F`); a default-BRE pattern is translated only when it uses
/// no backslash escapes, the place the two grammars diverge.
fn translate_grep(args: &[String]) -> Option<Vec<String>> {
  let mut out: Vec<String> = Vec::new();
  let mut pattern: Option<String> = None;
  let mut paths: Vec<String> = Vec::new();
  let mut fixed = false;
  let mut extended = false;
  let mut after_dashdash = false;

  let mut i = 0;
  while i < args.len() {
    let arg = args[i].as_str();
    let take_value = |i: &mut usize| -> Option<String> {
      *i += 1;
      args.get(*i).cloned()
    };
    if after_dashdash {
      if pattern.is_none() {
        pattern = Some(arg.to_owned());
      } else {
        paths.push(arg.to_owned());
      }
      i += 1;
      continue;
    }
    match arg {
      "--" => after_dashdash = true,
      "-r" | "-R" | "--recursive" | "--dereference-recursive" => {}
      "-n" | "--line-number" => {} // always on in gfs rg's output
      "-i" | "--ignore-case" => out.push("-i".to_owned()),
      "-F" | "--fixed-strings" => {
        fixed = true;
        out.push("-F".to_owned());
      }
      "-E" | "--extended-regexp" => extended = true, // gfs rg's default dialect
      "-e" | "--regexp" => {
        // A second -e means grep-side alternation; not translated.
        if pattern.is_some() {
          return None;
        }
        pattern = Some(take_value(&mut i)?);
      }
      "--include" => {
        out.push("-g".to_owned());
        out.push(take_value(&mut i)?);
      }
      "--exclude" => {
        out.push("--exclude".to_owned());
        out.push(take_value(&mut i)?);
      }
      "-A" | "--after-context" | "-B" | "--before-context" | "-C" | "--context" => {
        out.push(arg[..2].to_owned());
        out.push(take_value(&mut i)?);
      }
      other => {
        if let Some(glob) = other.strip_prefix("--include=") {
          out.push("-g".to_owned());
          out.push(glob.to_owned());
        } else if let Some(glob) = other.strip_prefix("--exclude=") {
          out.push("--exclude".to_owned());
          out.push(glob.to_owned());
        } else if let Some((flag, value)) = attached_context(other) {
          out.push(flag);
          out.push(value);
        } else if let Some(compact) = grouped_shorts(other) {
          for flag in compact {
            match flag.as_str() {
              "-F" => {
                fixed = true;
                out.push(flag);
              }
              "-E" => extended = true,
              _ => out.push(flag),
            }
          }
        } else if other.starts_with('-') && other != "-" {
          return None;
        } else if pattern.is_none() {
          pattern = Some(other.to_owned());
        } else {
          paths.push(other.to_owned());
        }
      }
    }
    i += 1;
  }

  let pattern = pattern?;
  if !fixed && !extended && pattern.contains('\\') {
    // Default grep is BRE, where backslashes are exactly where the dialects
    // part ways. Refusing the translation keeps the match set grep's.
    return None;
  }
  out.push("-e".to_owned());
  out.push(pattern);
  out.extend(paths);
  Some(out)
}

/// `-A3` / `-B2` / `-C1`, the attached-value spelling.
fn attached_context(arg: &str) -> Option<(String, String)> {
  let rest = arg.strip_prefix("-A").or_else(|| arg.strip_prefix("-B")).or_else(|| arg.strip_prefix("-C"))?;
  if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
    return None;
  }
  Some((arg[..2].to_owned(), rest.to_owned()))
}

/// Grouped short flags whose members all translate: `-rn` → nothing,
/// `-rin` → `-i`. Any unknown member declines the whole group. `-E` is
/// returned as a marker for the caller's dialect tracking, not forwarded.
fn grouped_shorts(arg: &str) -> Option<Vec<String>> {
  let body = arg.strip_prefix('-')?;
  if body.is_empty() || arg.starts_with("--") {
    return None;
  }
  let mut out = Vec::new();
  for c in body.chars() {
    match c {
      'r' | 'R' | 'n' => {}
      'i' => out.push("-i".to_owned()),
      'F' => out.push("-F".to_owned()),
      'E' => out.push("-E".to_owned()),
      _ => return None,
    }
  }
  Some(out)
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
/// delegating on an ordinary repository would answer from an index that is
/// not there — so anything else is "no".
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

#[cfg(test)]
mod tests {
  use super::*;

  fn translated(args: &[&str]) -> Option<Vec<String>> {
    translate_grep(&args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>())
  }

  #[test]
  fn a_plain_recursive_grep_translates_to_a_pattern_and_a_path() {
    let out = translated(&["-rn", "needle", "src"]).unwrap();
    assert_eq!(out, vec!["-e", "needle", "src"]);
  }

  #[test]
  fn include_and_case_flags_map_to_their_rg_spellings() {
    let out = translated(&["-ri", "--include=*.rs", "needle"]).unwrap();
    assert_eq!(out, vec!["-i", "-g", "*.rs", "-e", "needle"]);
  }

  #[test]
  fn attached_context_values_survive() {
    let out = translated(&["-r", "-A3", "needle"]).unwrap();
    assert_eq!(out, vec!["-A", "3", "-e", "needle"]);
  }

  #[test]
  fn a_bre_pattern_with_escapes_is_not_translated() {
    // `\(` groups in BRE and matches a literal parenthesis in a regex engine;
    // translating would change the match set silently.
    assert!(translated(&["-r", r"foo\(bar\)"]).is_none());
    // The same bytes under -F are a literal, which translates exactly.
    assert!(translated(&["-rF", r"foo\(bar\)"]).is_some());
  }

  #[test]
  fn word_regexp_and_files_with_matches_decline() {
    assert!(translated(&["-rw", "needle"]).is_none());
    assert!(translated(&["-rl", "needle"]).is_none());
  }

  #[test]
  fn a_second_pattern_flag_declines() {
    assert!(translated(&["-r", "-e", "a", "-e", "b"]).is_none());
  }

  #[test]
  fn recursion_detection_reads_grouped_flags_but_stops_at_dashdash() {
    let owned = |args: &[&str]| args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
    assert!(wants_recursion(&owned(&["-rn", "x"])));
    assert!(wants_recursion(&owned(&["--recursive", "x"])));
    assert!(!wants_recursion(&owned(&["x", "--", "-r"])));
    assert!(!wants_recursion(&owned(&["-n", "x", "file.txt"])));
  }
}
