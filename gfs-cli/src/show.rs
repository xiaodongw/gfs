//! `gfs show`: one commit, and what it changed.
//!
//! # The gap this closes
//!
//! Until this existed, GFS could read any commit and could not say what one
//! *did*. `gfs log` refused `-p` and `--stat`, `gfs diff` took no revisions, and
//! there was no `show` — so "review the last three commits" had no first-class
//! answer at all. The 2026-07-29 agent report worked around it with a Python
//! script that walked tree OIDs out of `gfs ls --rev` and diffed
//! `gfs cat --rev` output, which is a second implementation of `git diff` built
//! on forty round trips.
//!
//! The gateway holds the object database and libgit2 renders the patch, so this
//! is one round trip and the mount hydrates nothing.
//!
//! # Merges
//!
//! `git show` on a merge prints the header and *no diff*, which is correct and
//! unhelpful. This defaults to the first parent and says so in the header, which
//! is the question a reviewer actually has — "what did merging bring in" —
//! with `--parent <n>` for the other side and `--all-parents` for both.

use std::path::PathBuf;

use anyhow::{bail, Result};
use gfs_mount::control::{Request, Response};

use crate::history::{self, DiffFlags};
use crate::workspace;

#[derive(Debug, Default)]
struct Args {
  /// The commit to show. Defaults to the workspace's pin.
  rev: String,
  /// Which parent to diff against, 1-based, as `git show -m` numbers them.
  parent: Option<u32>,
  all_parents: bool,
  format: Option<String>,
  /// Suppress the commit header, leaving only the diff.
  no_header: bool,
  workspace: Option<PathBuf>,
  diff: DiffFlags,
}

pub fn run(argv: &[String]) -> Result<i32> {
  if argv.iter().any(|a| a == "-h" || a == "--help") {
    print_help();
    return Ok(0);
  }
  let args = parse(argv)?;
  let (_workspace_root, state_dir) = workspace::resolve(&args.workspace)?;

  // One commit, fetched through the log walk so the header comes from the same
  // place `gfs log` gets it and the two cannot disagree about a field.
  let Response::Log(report) = workspace::call(
    &state_dir,
    &Request::Log {
      skip: 0,
      limit: 1,
      from: Some(args.rev.clone()),
      first_parent: false,
      paths_b64url: Vec::new(),
    },
  )?
  else {
    bail!("the daemon answered a log request with something else");
  };
  let Some(entry) = report.commits.first() else {
    bail!("no such commit: {}", args.rev);
  };

  use std::io::Write;
  let mut out = std::io::stdout().lock();
  if !args.no_header {
    let now = gfs_types::Timestamp::now().secs;
    match &args.format {
      Some(format) => history::write_formatted(&mut out, entry, format, now)?,
      None => history::write_default(&mut out, entry)?,
    }
    writeln!(out)?;
    out.flush()?;
  }

  let commit = history::hex(&entry.commit).to_owned();
  // Which parents to diff against. A non-merge has exactly one and the loop runs
  // once; `--all-parents` on a merge is what answers "what did the side branch
  // do" and "what did the merge bring in" in one command, which otherwise takes
  // two invocations and manual bookkeeping.
  let parents: Vec<Option<u32>> = match (args.all_parents, args.parent) {
    (true, _) => (1..=entry.parents.len().max(1) as u32).map(Some).collect(),
    (false, chosen) => vec![chosen],
  };
  let multiple = parents.len() > 1;
  for parent in parents {
    if multiple {
      let index = parent.unwrap_or(1) as usize;
      writeln!(
        out,
        "--- against parent {index} ({}) ---",
        entry
          .parents
          .get(index - 1)
          .map(|p| history::short(p))
          .unwrap_or_else(|| "the empty tree".to_owned())
      )?;
      out.flush()?;
    }
    let diff = history::request_diff(&state_dir, None, commit.clone(), parent, &args.diff)?;
    history::print_diff(&mut out, &diff)?;
  }
  out.flush()?;
  Ok(0)
}

fn parse(argv: &[String]) -> Result<Args> {
  let mut args = Args {
    // The pin, which is what `git show` with no argument means.
    rev: "HEAD".to_owned(),
    ..Default::default()
  };
  let mut saw_rev = false;
  let mut in_paths = false;
  let mut i = 0;
  while i < argv.len() {
    let arg = argv[i].as_str();
    if in_paths {
      args.diff.paths.push(arg.as_bytes().to_vec());
      i += 1;
      continue;
    }
    let mut take_value = |name: &str| -> Result<String> {
      i += 1;
      argv
        .get(i)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
    };
    match arg {
      "--" => in_paths = true,
      "--all-parents" | "-m" => args.all_parents = true,
      "--first-parent" => args.parent = Some(1),
      "--parent" => args.parent = Some(take_value("--parent")?.parse()?),
      "--no-header" | "--no-commit-id" => args.no_header = true,
      "--workspace" => args.workspace = Some(PathBuf::from(take_value("--workspace")?)),
      other if other.starts_with("--workspace=") => {
        args.workspace = Some(PathBuf::from(other.split_once('=').expect("checked").1));
      }
      other if other.starts_with("--format=") || other.starts_with("--pretty=") => {
        args.format = Some(other.split_once('=').expect("checked").1.to_owned());
      }
      "--format" | "--pretty" => args.format = Some(take_value("--format")?),
      other if args.diff.accept(other) => {}
      other if !other.starts_with('-') && !saw_rev => {
        args.rev = other.to_owned();
        saw_rev = true;
      }
      other => bail!(
        "gfs show does not support `{other}`. Supported: --parent <n>, \
         --all-parents/-m, --first-parent, -p/--stat/--name-only/--name-status, \
         -U<n>, --format=/--pretty=, --no-header, --workspace, and \
         `-- <path>`. Run with --help."
      ),
    }
    i += 1;
  }
  Ok(args)
}

fn print_help() {
  println!(
    "gfs show: one commit and what it changed, answered by the server.\n\
     \n\
     USAGE:\n    \
       gfs show [<revision>] [--stat] [-- <path>...]\n    \
       gfs show HEAD~2               the commit two back from the pin\n    \
       gfs show abc1234 --stat       which files it touched, and by how much\n\
     \n\
     OPTIONS:\n    \
           --parent <N>     diff against parent N instead of the first\n    \
       -m, --all-parents    diff against every parent, one section each\n    \
           --first-parent   the default; here so it can be said explicitly\n    \
       -p, --patch          full patch (the default)\n    \
           --stat           changed-file histogram\n    \
           --name-only      just the paths\n    \
           --name-status    those paths with a status letter\n    \
       -U<n>, --unified=<n> context lines (default 3)\n    \
           --format=<FMT>   the header's format (%H %h %s %an %ae %ad %P)\n    \
           --no-header      the diff alone, for piping to `git apply`\n    \
           --workspace <P>  the workspace, when not standing in it\n\
     \n\
     REVISIONS:\n    \
       A branch, tag, commit id, or an ancestry expression: HEAD~3, main^,\n    \
       abc1234^2. `HEAD` is this workspace's pinned commit.\n\
     \n\
     MERGES:\n    \
       `git show` prints no diff for a merge. This one defaults to the first\n    \
       parent -- what the merge brought in -- and `--parent 2` gives the other\n    \
       side. `-m` shows every parent in one run.\n\
     \n\
     A root commit is diffed against the empty tree, so its whole content is\n\
     the patch.\n\
     \n\
     EXIT: 0 shown, 2 no answer."
  );
}

#[cfg(test)]
mod tests {
  use super::*;

  fn parsed(argv: &[&str]) -> Args {
    parse(&argv.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
  }

  #[test]
  fn no_argument_means_the_pin() {
    assert_eq!(parsed(&[]).rev, "HEAD");
    assert_eq!(parsed(&["HEAD~2"]).rev, "HEAD~2");
  }

  #[test]
  fn a_path_is_not_mistaken_for_the_revision() {
    let args = parsed(&["HEAD", "--", "src/main.rs"]);
    assert_eq!(args.rev, "HEAD");
    assert_eq!(args.diff.paths, vec![b"src/main.rs".to_vec()]);
  }

  #[test]
  fn the_merge_flags_select_a_parent() {
    assert_eq!(parsed(&["--parent", "2"]).parent, Some(2));
    assert!(parsed(&["-m"]).all_parents);
    assert_eq!(parsed(&["--first-parent"]).parent, Some(1));
  }
}
