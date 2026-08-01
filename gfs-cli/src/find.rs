//! `gfs find`: the `find` substitute an agent can safely be given.
//!
//! Real `find` walks the projected tree. Name and path tests only cost FUSE
//! lookups, but on a monorepo that is the 100k-readdir sweep the untracked
//! cache exists to kill, and `-exec`/content tests hydrate what they touch.
//! `gfs find` answers the same name-shaped questions without visiting the tree
//! at all: the tracked list comes from the workspace's own git index (one
//! local file), and the overlay journal supplies what the workspace created,
//! deleted, or renamed on top. No directory is read, no blob is fetched.
//!
//! # The subset, and failing closed
//!
//! The grammar is `find`'s, deliberately small: start paths, `-name`/`-iname`,
//! `-path`/`-ipath`, `-type f|d`, `-maxdepth`/`-mindepth`, `-print`. Every
//! other predicate is **refused by name** — `-mtime` answered from an index
//! would be a wrong answer that looks right, and `-exec` needs the real files
//! — with `--hydrate` as the documented escape hatch: run real `find` over the
//! mount and accept the cost. The parser is hand-rolled for the reason
//! `gfs rg`'s is: the informative rejection is part of the contract, and
//! `clap` cannot refuse a flag it was never told about.
//!
//! # What the answer is, exactly
//!
//! The merged worktree: index entries, plus overlay additions and rename
//! targets, minus overlay deletions and rename sources. Directories are
//! derived from file paths, so a directory that is empty in Git's model (Git
//! tracks no empty directories) does not appear — same as `git ls-files`,
//! documented rather than papered over. Output order is depth-first by sorted
//! components, deterministic across runs; real `find` prints readdir order,
//! which is not deterministic to begin with. Paths are treated as UTF-8 (lossy
//! for display), which matches every path `gfs ls` prints.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use gfs_mount::control::{Request, Response};

use crate::workspace;

/// The predicates `gfs find` implements. A table, not `clap`, so the refusal
/// of everything else can name the flag and the way out.
const SUPPORTED: &[(&str, &str)] = &[
  ("-name PATTERN", "basename glob, as find matches it"),
  ("-iname PATTERN", "case-insensitive -name"),
  ("-path PATTERN", "glob against the whole printed path"),
  ("-ipath PATTERN", "case-insensitive -path"),
  ("-type f|d", "files or directories (Git tracks no other kind here)"),
  ("-maxdepth N", "descend at most N levels below a start point"),
  ("-mindepth N", "print nothing shallower than N levels"),
  ("-print", "accepted for compatibility; printing is the default"),
  ("--hydrate", "run real find over the mount instead"),
];

#[derive(Debug, Default)]
struct Args {
  starts: Vec<String>,
  names: Vec<(String, bool)>,
  path_patterns: Vec<(String, bool)>,
  file_type: Option<char>,
  maxdepth: Option<usize>,
  mindepth: Option<usize>,
  hydrate: bool,
  workspace: Option<PathBuf>,
}

/// Run `gfs find` over the raw arguments that followed the subcommand.
///
/// Returns the exit code rather than exiting, so the caller decides. `find`'s
/// own contract: 0 when every start point was visited, 1 when one was not.
pub fn run(argv: &[String]) -> Result<i32> {
  if argv.iter().any(|a| a == "-h" || a == "--help") {
    print_help();
    return Ok(0);
  }
  let args = parse(argv)?;
  let (workspace_root, state_dir) = workspace::resolve(&args.workspace)?;

  if args.hydrate {
    let forwarded: Vec<&String> = argv.iter().filter(|a| *a != "--hydrate").collect();
    eprintln!(
      "gfs find: --hydrate: running real find over the mount. Name tests cost a \
       full readdir sweep; -exec and content tests download what they touch."
    );
    let status = std::process::Command::new("find")
      .args(forwarded)
      // Real `find` resolved from PATH may be the scan shim itself; the
      // variable tells it to stand aside instead of delegating back here.
      .env("GFS_SHIM_BYPASS", "1")
      .status()
      .context("running find; is it installed?")?;
    return Ok(status.code().unwrap_or(2));
  }

  // Where the caller stands, workspace-relative: `find .` in a subdirectory
  // means that subtree, exactly as it does for real find.
  let cwd_prefix = std::env::current_dir()
    .ok()
    .and_then(|cwd| {
      cwd
        .strip_prefix(&workspace_root)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
    })
    .unwrap_or_default();

  let files = worktree_files(&workspace_root, &state_dir)?;

  let starts: Vec<String> = if args.starts.is_empty() {
    vec![".".to_owned()]
  } else {
    args.starts.clone()
  };

  let mut out = std::io::stdout().lock();
  let mut missing = false;
  for start in &starts {
    let display_base = if start == "." {
      ".".to_owned()
    } else {
      start.trim_end_matches('/').to_owned()
    };
    let Some(root_rel) = root_relative(&cwd_prefix, &display_base) else {
      eprintln!("gfs find: {start:?}: leaves the workspace");
      missing = true;
      continue;
    };

    // What the start names: a file, a directory (something is under it, or it
    // is the root), or nothing.
    let is_file = files.contains(&root_rel);
    let dir_prefix = if root_rel.is_empty() {
      String::new()
    } else {
      format!("{root_rel}/")
    };
    let is_dir = root_rel.is_empty() || files.iter().any(|f| f.starts_with(&dir_prefix));
    if !is_file && !is_dir {
      eprintln!("gfs find: {start:?}: No such file or directory");
      missing = true;
      continue;
    }

    // Everything visible from this start, as (depth, printed path, is_dir),
    // in depth-first order: sorted component-wise, a directory always sorting
    // before its own children and after no unrelated sibling's subtree.
    let mut entries: BTreeSet<Vec<String>> = BTreeSet::new();
    if is_file {
      entries.insert(Vec::new());
    } else {
      entries.insert(Vec::new());
      for file in &files {
        let Some(rest) = file.strip_prefix(&dir_prefix) else {
          continue;
        };
        if !root_rel.is_empty() && rest.is_empty() {
          continue;
        }
        let rel = if root_rel.is_empty() { file.as_str() } else { rest };
        let components: Vec<String> = rel.split('/').map(str::to_owned).collect();
        for len in 1..=components.len() {
          if args.maxdepth.is_some_and(|m| len > m) {
            break;
          }
          entries.insert(components[..len].to_vec());
        }
      }
    }

    for components in &entries {
      let depth = components.len();
      if args.maxdepth.is_some_and(|m| depth > m) {
        continue;
      }
      if args.mindepth.is_some_and(|m| depth < m) {
        continue;
      }
      let printed = if components.is_empty() {
        display_base.clone()
      } else {
        format!("{display_base}/{}", components.join("/"))
      };
      let entry_rel = if components.is_empty() {
        root_rel.clone()
      } else if root_rel.is_empty() {
        components.join("/")
      } else {
        format!("{root_rel}/{}", components.join("/"))
      };
      let entry_is_file = files.contains(&entry_rel);
      match args.file_type {
        Some('f') if !entry_is_file => continue,
        Some('d') if entry_is_file => continue,
        _ => {}
      }
      let basename = components
        .last()
        .map(String::as_str)
        .unwrap_or_else(|| basename_of(&display_base));
      if !args
        .names
        .iter()
        .all(|(pat, ci)| glob_match(pat, basename, *ci))
      {
        continue;
      }
      if !args
        .path_patterns
        .iter()
        .all(|(pat, ci)| glob_match(pat, &printed, *ci))
      {
        continue;
      }
      use std::io::Write;
      writeln!(out, "{printed}")?;
    }
  }
  use std::io::Write;
  out.flush()?;
  Ok(if missing { 1 } else { 0 })
}

/// The merged worktree's files: the git index, adjusted by the overlay journal.
///
/// The index is read by real `git ls-files` against the workspace's own
/// `.git` — one local file, no tree walk. The journal supplies what never
/// reached the index: files created through the mount, deletions, renames.
fn worktree_files(
  workspace_root: &std::path::Path,
  state_dir: &std::path::Path,
) -> Result<BTreeSet<String>> {
  let out = std::process::Command::new("git")
    .arg("-C")
    .arg(workspace_root)
    .args(["ls-files", "-z"])
    .env("GFS_SHIM_BYPASS", "1")
    .output()
    .context("running git ls-files; is git installed?")?;
  if !out.status.success() {
    bail!(
      "git ls-files failed: {}",
      String::from_utf8_lossy(&out.stderr).trim()
    );
  }
  let mut files: BTreeSet<String> = out
    .stdout
    .split(|&b| b == 0)
    .filter(|p| !p.is_empty())
    .map(|p| String::from_utf8_lossy(p).into_owned())
    .collect();

  let Response::Status(report) = workspace::call(state_dir, &Request::Status)? else {
    bail!("the daemon answered a status request with something else");
  };
  for change in &report.status.changes {
    use gfs_overlay::ChangeKind;
    let path = String::from_utf8_lossy(change.path.as_bytes()).into_owned();
    match change.kind {
      ChangeKind::Deleted => {
        files.remove(&path);
      }
      ChangeKind::Renamed => {
        files.insert(path);
        if let Some(from) = &change.from {
          files.remove(&String::from_utf8_lossy(from.as_bytes()).into_owned());
        }
      }
      _ => {
        files.insert(path);
      }
    }
  }
  for dir in &report.status.directory_deletions {
    let prefix = format!("{}/", String::from_utf8_lossy(dir.as_bytes()));
    files.retain(|f| !f.starts_with(&prefix));
  }
  Ok(files)
}

/// Resolve a start path against where the caller stands, workspace-relative.
///
/// Returns the root-relative path (empty for the root), or `None` when `..`
/// climbs out of the workspace — which real find would happily follow, but
/// out there is not a snapshot this command can answer for.
fn root_relative(cwd_prefix: &str, start: &str) -> Option<String> {
  let mut components: Vec<&str> = cwd_prefix.split('/').filter(|c| !c.is_empty()).collect();
  for part in start.split('/') {
    match part {
      "" | "." => {}
      ".." => {
        components.pop()?;
      }
      other => components.push(other),
    }
  }
  Some(components.join("/"))
}

fn basename_of(path: &str) -> &str {
  path.rsplit('/').next().unwrap_or(path)
}

/// `fnmatch`-style globbing, the way find's `-name`/`-path` do it: `*` and `?`
/// match across `/` (find leaves slashes to the caller's choice of predicate),
/// `[...]` is a class with ranges and `!`/`^` negation.
fn glob_match(pattern: &str, text: &str, case_insensitive: bool) -> bool {
  fn matches(pat: &[char], text: &[char]) -> bool {
    match pat.first() {
      None => text.is_empty(),
      Some('*') => {
        (0..=text.len()).any(|skip| matches(&pat[1..], &text[skip..]))
      }
      Some('?') => !text.is_empty() && matches(&pat[1..], &text[1..]),
      Some('[') => {
        let Some(end) = pat.iter().skip(1).position(|&c| c == ']').map(|p| p + 1) else {
          // An unterminated class is a literal `[`, as fnmatch treats it.
          return !text.is_empty() && text[0] == '[' && matches(&pat[1..], &text[1..]);
        };
        let Some(&first) = text.first() else {
          return false;
        };
        let mut class = &pat[1..end];
        let negated = class.first().is_some_and(|&c| c == '!' || c == '^');
        if negated {
          class = &class[1..];
        }
        let mut hit = false;
        let mut i = 0;
        while i < class.len() {
          if i + 2 < class.len() && class[i + 1] == '-' {
            if class[i] <= first && first <= class[i + 2] {
              hit = true;
            }
            i += 3;
          } else {
            if class[i] == first {
              hit = true;
            }
            i += 1;
          }
        }
        hit != negated && matches(&pat[end + 1..], &text[1..])
      }
      Some(&c) => !text.is_empty() && text[0] == c && matches(&pat[1..], &text[1..]),
    }
  }
  let (pattern, text) = if case_insensitive {
    (pattern.to_lowercase(), text.to_lowercase())
  } else {
    (pattern.to_owned(), text.to_owned())
  };
  matches(
    &pattern.chars().collect::<Vec<_>>(),
    &text.chars().collect::<Vec<_>>(),
  )
}

fn parse(argv: &[String]) -> Result<Args> {
  let mut args = Args::default();
  let mut i = 0;
  while i < argv.len() {
    let arg = argv[i].as_str();
    let mut take_value = |name: &str| -> Result<String> {
      i += 1;
      argv
        .get(i)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
    };
    match arg {
      "-name" => {
        let v = take_value("-name")?;
        args.names.push((v, false));
      }
      "-iname" => {
        let v = take_value("-iname")?;
        args.names.push((v, true));
      }
      "-path" | "-wholename" => {
        let v = take_value("-path")?;
        args.path_patterns.push((v, false));
      }
      "-ipath" => {
        let v = take_value("-ipath")?;
        args.path_patterns.push((v, true));
      }
      "-type" => {
        let v = take_value("-type")?;
        match v.as_str() {
          "f" | "d" => args.file_type = Some(v.chars().next().expect("checked")),
          other => bail!(
            "-type {other} cannot be answered from the index (Git tracks files \
             and directories here); use --hydrate to run real find over the mount"
          ),
        }
      }
      "-maxdepth" => args.maxdepth = Some(take_value("-maxdepth")?.parse()?),
      "-mindepth" => args.mindepth = Some(take_value("-mindepth")?.parse()?),
      "-print" => {}
      "--hydrate" => args.hydrate = true,
      "--workspace" => args.workspace = Some(PathBuf::from(take_value("--workspace")?)),
      other if other.starts_with('-') || other == "(" || other == "!" => {
        return Err(unsupported(other));
      }
      other => {
        // find's grammar: start points come before the first predicate.
        if args.names.is_empty()
          && args.path_patterns.is_empty()
          && args.file_type.is_none()
          && args.maxdepth.is_none()
          && args.mindepth.is_none()
        {
          args.starts.push(other.to_owned());
        } else {
          bail!("start point {other:?} must come before the predicates, as find requires");
        }
      }
    }
    i += 1;
  }
  Ok(args)
}

/// Refuse an unimplemented predicate, with the reason and the way out.
fn unsupported(flag: &str) -> anyhow::Error {
  anyhow::anyhow!(
    "unsupported predicate {flag}.\n\
     `gfs find` answers from the index and the overlay journal, so it \
     implements the name-shaped subset of find and refuses the rest rather \
     than guessing -- a time, size, or permission test answered without the \
     real files would be a wrong answer that looks right, and -exec needs \
     them outright.\n\
     Supported: {}\n\
     To run real find over the mount anyway, add --hydrate. Name tests then \
     cost a full readdir sweep, and content tests download what they touch.",
    SUPPORTED
      .iter()
      .map(|(flag, _)| *flag)
      .collect::<Vec<_>>()
      .join(", ")
  )
}

fn print_help() {
  println!("gfs find -- find files in a GFS workspace without walking it\n");
  println!("USAGE:\n  gfs find [START...] [PREDICATES]\n");
  println!("PREDICATES:");
  for (flag, meaning) in SUPPORTED {
    println!("  {flag:<18} {meaning}");
  }
  println!("\nAnswered from the git index plus the overlay journal: no directory");
  println!("is read through the mount and no file content is fetched. Directories");
  println!("Git does not track (empty ones) do not appear.");
}

#[cfg(test)]
mod tests {
  use super::*;

  fn parsed(argv: &[&str]) -> Result<Args> {
    parse(&argv.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>())
  }

  #[test]
  fn starts_come_before_predicates() {
    let args = parsed(&["src", "docs", "-name", "*.py"]).unwrap();
    assert_eq!(args.starts, vec!["src".to_owned(), "docs".to_owned()]);
    assert_eq!(args.names, vec![("*.py".to_owned(), false)]);
  }

  #[test]
  fn an_unsupported_predicate_is_refused_rather_than_ignored() {
    let err = parsed(&[".", "-mtime", "-1"]).unwrap_err();
    let text = format!("{err}");
    assert!(text.contains("unsupported predicate -mtime"), "{text}");
    assert!(text.contains("--hydrate"), "{text}");
  }

  #[test]
  fn exec_is_refused() {
    let err = parsed(&[".", "-exec", "rm", "{}", ";"]).unwrap_err();
    assert!(format!("{err}").contains("-exec"));
  }

  #[test]
  fn type_beyond_f_and_d_is_refused_with_the_reason() {
    let err = parsed(&["-type", "l"]).unwrap_err();
    assert!(format!("{err}").contains("--hydrate"));
  }

  #[test]
  fn the_supported_subset_parses() {
    let args = parsed(&[
      ".", "-maxdepth", "2", "-type", "f", "-iname", "*.RS", "-path", "./src/*",
    ])
    .unwrap();
    assert_eq!(args.maxdepth, Some(2));
    assert_eq!(args.file_type, Some('f'));
    assert_eq!(args.names, vec![("*.RS".to_owned(), true)]);
    assert_eq!(args.path_patterns, vec![("./src/*".to_owned(), false)]);
  }

  #[test]
  fn globs_match_the_way_fnmatch_does() {
    assert!(glob_match("*.py", "app.py", false));
    assert!(!glob_match("*.py", "app.pyc", false));
    assert!(glob_match("*.PY", "app.py", true));
    assert!(glob_match("a?c", "abc", false));
    assert!(glob_match("[a-c]pp", "app", false));
    assert!(!glob_match("[!a-c]pp", "app", false));
    // find's -path semantics: `*` crosses slashes.
    assert!(glob_match("./src/*", "./src/flask/app.py", false));
  }

  #[test]
  fn relative_starts_resolve_against_the_callers_subdirectory() {
    assert_eq!(root_relative("", ".").as_deref(), Some(""));
    assert_eq!(root_relative("src", ".").as_deref(), Some("src"));
    assert_eq!(root_relative("src", "flask").as_deref(), Some("src/flask"));
    assert_eq!(root_relative("src", "..").as_deref(), Some(""));
    assert_eq!(root_relative("", ".."), None, "out of the workspace");
    assert_eq!(root_relative("", "src/").as_deref(), Some("src"));
  }
}
