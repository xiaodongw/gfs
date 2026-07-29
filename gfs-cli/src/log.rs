//! `gfs log`: `git log` for a workspace that has no object database.
//!
//! # Why this exists
//!
//! ADR 0005 chose a synthesized `.git` over a real partial clone, which means the
//! workspace has no packfile to walk. The shim's `log` was frozen at `-1` as a
//! result: the pinned commit's metadata is known, and nothing could produce its
//! parent. An agent orienting itself by recent history had no answer, and a
//! `--depth 1` clone — the raw-git equivalent — has the same single commit.
//!
//! The server has the object database. `gfs log` asks it to walk, which costs
//! one round trip and downloads nothing.
//!
//! # `-p` and `--stat` are here now, and the reason they were not
//!
//! They used to be refused by name, on the grounds that each "needs a tree or
//! blob per commit, which is the unbounded download ADR 0005 exists to avoid".
//! That reasoning was about the **client**: the partial clone was rejected
//! because the *workspace* would hydrate itself a piece at a time. The gateway
//! holds the object database and pays no such cost, so a diff per commit is one
//! more server-side render and the mount still reads nothing — which
//! `gfs inspect | grep hydration` will confirm.
//!
//! What that costs instead is one round trip per commit printed, bounded by
//! `-n` (default 20). That is why the diff is fetched per commit rather than
//! folded into the log response: a log page would otherwise carry a body whose
//! size no paging contract bounds.
//!
//! # What is still deliberately not here
//!
//! `-S` and `--follow`. Both are searches *across* commits — a pickaxe over
//! every blob, a similarity score per commit — rather than one comparison per
//! commit, and neither is bounded by the page size. `--graph` is also refused:
//! it needs the topological order, and the default here is date order for the
//! reason below.
//!
//! No `--topo-order` either, and the default is not topological. `git log -10`
//! on the M0.1 worst case is 0.007 s in date order and 10.383 s with
//! `--topo-order`, because topological sorting buffers the reachable graph
//! before emitting anything. The visible cost is that two commits sharing a
//! commit timestamp may appear in the opposite order to `git log`; the set and
//! the time ordering are the same.

use std::path::PathBuf;

use anyhow::{bail, Result};
use gfs_mount::control::{Request, Response};

use crate::history::{self, DiffFlags};
use crate::workspace;

#[derive(Debug)]
struct Args {
  limit: u32,
  skip: u32,
  oneline: bool,
  format: Option<String>,
  json: bool,
  workspace: Option<PathBuf>,
  /// Where the walk starts. `None` is the pin; `HEAD` inside an expression also
  /// means the pin, which the daemon substitutes.
  from: Option<String>,
  first_parent: bool,
  /// Set once any of `-p`, `--stat`, `--name-only`, `--name-status` appeared.
  with_diff: bool,
  diff: DiffFlags,
}

impl Default for Args {
  fn default() -> Self {
    // `git log` with no limit pages the whole history. A default is used instead
    // because there is no pager here and an agent reading 1.4 million commits
    // into a context window is a worse failure than a short answer.
    Args {
      limit: 20,
      skip: 0,
      oneline: false,
      format: None,
      json: false,
      workspace: None,
      from: None,
      first_parent: false,
      with_diff: false,
      diff: DiffFlags::default(),
    }
  }
}

/// Run `gfs log` over the raw arguments that followed the subcommand.
///
/// Returns the exit code rather than exiting, so the caller decides.
pub fn run(argv: &[String]) -> Result<i32> {
  if argv.iter().any(|a| a == "-h" || a == "--help") {
    print_help();
    return Ok(0);
  }
  let args = parse(argv)?;
  let (_workspace_root, state_dir) = workspace::resolve(&args.workspace)?;

  let Response::Log(report) = workspace::call(
    &state_dir,
    &Request::Log {
      skip: args.skip,
      limit: args.limit,
      from: args.from.clone(),
      first_parent: args.first_parent,
      paths_b64url: args
        .diff
        .paths
        .iter()
        .map(|p| gfs_types::path::b64url_encode(p))
        .collect(),
    },
  )?
  else {
    bail!("the daemon answered a log request with something else");
  };

  if args.json {
    println!("{}", serde_json::to_string(&report)?);
    return Ok(0);
  }

  // One clock reading for the whole page, so `%ar` cannot say "2 minutes ago"
  // on one line and "3 minutes ago" on the next of the same output.
  let now = gfs_types::Timestamp::now().secs;
  use std::io::Write;
  let mut out = std::io::stdout().lock();
  for entry in &report.commits {
    match (&args.format, args.oneline) {
      (Some(format), _) => history::write_formatted(&mut out, entry, format, now)?,
      (None, true) => {
        writeln!(
          out,
          "{} {}",
          history::short(&entry.commit),
          history::subject(&entry.message)
        )?;
      }
      (None, false) => history::write_default(&mut out, entry)?,
    }
    if args.with_diff {
      writeln!(out)?;
      out.flush()?;
      // One request per commit, against its first parent -- which is what
      // `git log -p` shows. See the module header for why this is not folded
      // into the log response.
      let diff = history::request_diff(
        &state_dir,
        None,
        history::hex(&entry.commit).to_owned(),
        None,
        &args.diff,
      )?;
      history::print_diff(&mut out, &diff)?;
      writeln!(out)?;
    }
  }
  out.flush()?;

  if report.has_more {
    eprintln!(
      "gfs log: more history follows; continue with --skip {}",
      args.skip as usize + report.commits.len()
    );
  }
  Ok(if report.commits.is_empty() { 1 } else { 0 })
}

fn parse(argv: &[String]) -> Result<Args> {
  let mut args = Args::default();
  let mut i = 0;
  // Everything after `--` is a path, exactly as `git log -- <path>` reads it.
  let mut in_paths = false;
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
      "--oneline" => args.oneline = true,
      "--json" => args.json = true,
      "--first-parent" => args.first_parent = true,
      "-n" | "--max-count" => args.limit = take_value("--max-count")?.parse()?,
      "--skip" => args.skip = take_value("--skip")?.parse()?,
      "--workspace" => args.workspace = Some(PathBuf::from(take_value("--workspace")?)),
      other if other.starts_with("--workspace=") => {
        args.workspace = Some(PathBuf::from(other.split_once('=').expect("checked").1));
      }
      other if other.starts_with("--format=") || other.starts_with("--pretty=") => {
        args.format = Some(other.split_once('=').expect("checked").1.to_owned());
      }
      "--format" | "--pretty" => args.format = Some(take_value("--format")?),
      // `-10`, the way `git log -10` is written. Matched on the stripped
      // remainder rather than by slicing, which would panic on `-é`.
      other
        if other
          .strip_prefix('-')
          .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())) =>
      {
        args.limit = other.strip_prefix('-').expect("checked").parse()?;
      }
      "-S" | "--follow" | "--graph" | "--topo-order" => bail!(
        "gfs log does not support `{arg}`: `-S` and `--follow` search across \
         commits rather than comparing each to its parent, and neither is \
         bounded by the page size; `--graph` and `--topo-order` need the \
         reachable graph buffered before anything can be printed, which is \
         10.383 s against 0.007 s on the M0.1 worst case. Use `gfs rg` to \
         search content and `gfs log -p` to see what each commit changed."
      ),
      other if args.diff.accept(other) => args.with_diff = true,
      // A bare revision, the way `git log <rev>` takes one. Last, so a flag is
      // never mistaken for one.
      other if !other.starts_with('-') && args.from.is_none() => {
        args.from = Some(other.to_owned());
      }
      other => bail!(
        "gfs log does not support `{other}`. Supported: -n/--max-count, -<n>, \
         --skip, --oneline, --format=/--pretty=, --first-parent, -p/--stat/\
         --name-only/--name-status, -U<n>, --json, --workspace, a revision, \
         and `-- <path>`. Run with --help."
      ),
    }
    i += 1;
  }
  Ok(args)
}

fn print_help() {
  println!(
    "gfs log: commit history for a GFS workspace, answered by the server.\n\
     \n\
     USAGE:\n    \
       gfs log [<revision>] [-n <count>] [--oneline] [-- <path>...]\n    \
       gfs log -3 -p                 what the last three commits changed\n    \
       gfs log -10 --format='%h %ad %s'\n\
     \n\
     OPTIONS:\n    \
       -n, --max-count <N>  commits to show (default 20); also `-<n>`\n    \
           --skip <N>       skip this many first, for paging\n    \
           --oneline        abbreviated hash and subject\n    \
           --first-parent   do not walk into a merge's side branch\n    \
       -p, --patch          each commit's diff against its first parent\n    \
           --stat           each commit's changed-file histogram\n    \
           --name-only      just the paths each commit touched\n    \
           --name-status    those paths with a status letter\n    \
       -U<n>, --unified=<n> context lines in a patch (default 3)\n    \
           --format=<FMT>   %H %h %T %t %P %p %s %b %B\n    \
                            %an %ae %at %ad %ai %aI %ar\n    \
                            %cn %ce %ct %cd %ci %cI %cr  %n %%\n    \
           --json           machine-readable output\n    \
           --workspace <P>  the workspace, when not standing in it\n\
     \n\
     REVISIONS:\n    \
       A branch, tag, commit id, or an ancestry expression: HEAD~3, main^,\n    \
       abc1234^2. `HEAD` is this workspace's pinned commit -- after\n    \
       `gfs switch -c` that is not the repository's default branch.\n\
     \n\
     PATHS:\n    \
       `-- <path>...` shows only commits that changed those paths, and limits\n    \
       any patch to them. Rename following (--follow) is not implied.\n\
     \n\
     History is the pinned commit's ancestry. Local edits do not create commits,\n\
     so they do not appear here; use `gfs status` for what the workspace changed.\n\
     \n\
     -S, --follow, --graph and --topo-order are refused; `gfs log --help` above\n\
     says why for each.\n\
     \n\
     EXIT: 0 commits shown, 1 none, 2 no answer."
  );
}

#[cfg(test)]
mod tests {
  use super::*;

  fn parsed(argv: &[&str]) -> Args {
    parse(&argv.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
  }

  #[test]
  fn a_bare_argument_is_a_revision_and_after_the_dashes_it_is_a_path() {
    let args = parsed(&["HEAD~3", "--", "src/flask/cli.py"]);
    assert_eq!(args.from.as_deref(), Some("HEAD~3"));
    assert_eq!(args.diff.paths, vec![b"src/flask/cli.py".to_vec()]);
  }

  #[test]
  fn a_path_after_the_dashes_is_never_read_as_a_flag() {
    // The case that makes `--` load-bearing: a file whose name starts with a
    // dash, or one that happens to spell a flag.
    let args = parsed(&["--", "--stat"]);
    assert_eq!(args.diff.paths, vec![b"--stat".to_vec()]);
    assert!(!args.with_diff);
  }

  #[test]
  fn the_diff_flags_turn_the_per_commit_diff_on() {
    assert!(parsed(&["-p"]).with_diff);
    assert!(parsed(&["--stat"]).with_diff);
    assert!(!parsed(&["--oneline"]).with_diff);
  }

  #[test]
  fn the_refusals_still_name_what_to_use_instead() {
    for flag in ["-S", "--follow", "--graph"] {
      let e = parse(&[flag.to_owned()]).unwrap_err().to_string();
      assert!(e.contains(flag), "{e}");
      assert!(e.contains("gfs rg") || e.contains("gfs log -p"), "{e}");
    }
  }
}
