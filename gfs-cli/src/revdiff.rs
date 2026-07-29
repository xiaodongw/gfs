//! `gfs diff`: the workspace's own changes, or any two revisions.
//!
//! # Two questions, one command
//!
//! With no revision this is what it always was — the overlay journal's change
//! set as a Git-compatible patch, which is what `gfs export` bundles and what
//! `gfs commit` sends. That patch is built by the daemon from the journal, so it
//! costs the edit set and not the repository.
//!
//! With revisions it is a *history* question, answered by the gateway: two
//! commits in, one rendered patch out, nothing hydrated. Before this existed
//! there was no way to ask it at all — `gfs diff` took `--workspace` and
//! `--state-dir` and nothing else, so comparing two commits meant walking trees
//! by hand.
//!
//! # Why one revision means "against the pin"
//!
//! `git diff <commit>` compares that commit with the *working tree*. Answering
//! that exactly would mean merging the overlay's edits into a server-side diff,
//! which is two sources of truth for one patch. So one revision compares it with
//! the pinned commit, and a dirty workspace gets a note on stderr naming the
//! command that shows the rest. That is a narrower answer than Git's, said out
//! loud, rather than a wrong one said quietly.

use std::path::PathBuf;

use anyhow::{bail, Result};
use gfs_mount::control::{Request, Response};

use crate::history::{self, DiffFlags};
use crate::workspace;

#[derive(Debug, Default)]
struct Args {
  /// Zero, one, or two revisions. A `a..b` range counts as two.
  revs: Vec<String>,
  workspace: Option<PathBuf>,
  state_dir: Option<PathBuf>,
  diff: DiffFlags,
}

pub fn run(argv: &[String]) -> Result<i32> {
  if argv.iter().any(|a| a == "-h" || a == "--help") {
    print_help();
    return Ok(0);
  }
  let args = parse(argv)?;
  let (_workspace_root, state_dir) = workspace::locate(&args.workspace, &args.state_dir)?;

  use std::io::Write;
  let mut out = std::io::stdout().lock();

  match args.revs.len() {
    0 => {
      if args.diff.format != gfs_types::DiffFormat::Patch || !args.diff.paths.is_empty() {
        bail!(
          "the workspace's own diff is a whole patch: --stat, --name-only, \
           --name-status and path limiting apply only when two revisions are \
           named. `gfs status` lists what the workspace changed."
        );
      }
      let Response::Diff { patch_b64url } = workspace::call(&state_dir, &Request::Diff)? else {
        bail!("the daemon answered a diff request with something else");
      };
      let patch = gfs_types::path::b64url_decode(&patch_b64url)
        .map_err(|e| anyhow::anyhow!("the daemon returned an undecodable patch: {}", e.message))?;
      out.write_all(&patch)?;
    }
    1 => {
      // One revision compares it with the pin. See the module header for why
      // this is not `git diff <commit>`'s exact meaning.
      let report = history::request_diff(
        &state_dir,
        Some(args.revs[0].clone()),
        "HEAD".to_owned(),
        None,
        &args.diff,
      )?;
      history::print_diff(&mut out, &report)?;
      warn_if_dirty(&state_dir);
    }
    _ => {
      let report = history::request_diff(
        &state_dir,
        Some(args.revs[0].clone()),
        args.revs[1].clone(),
        None,
        &args.diff,
      )?;
      history::print_diff(&mut out, &report)?;
    }
  }
  out.flush()?;
  Ok(0)
}

/// Say once, on stderr, that uncommitted work is not in this patch.
///
/// Best-effort: a status call that fails must not turn a diff that succeeded
/// into an error, so this is silent about its own failures.
fn warn_if_dirty(state_dir: &std::path::Path) {
  if let Ok(Response::Status(report)) = workspace::call(state_dir, &Request::Status) {
    if !report.status.is_clean() {
      eprintln!(
        "gfs: this compares two commits; the workspace's own uncommitted \
         changes are not included. `gfs diff` with no revision shows those."
      );
    }
  }
}

fn parse(argv: &[String]) -> Result<Args> {
  let mut args = Args::default();
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
      "--workspace" => args.workspace = Some(PathBuf::from(take_value("--workspace")?)),
      "--state-dir" => args.state_dir = Some(PathBuf::from(take_value("--state-dir")?)),
      // `--flag=value` as well as `--flag value`: both spellings reach these
      // tools from agents and shell scripts, and rejecting one by name is a
      // refusal that teaches nothing.
      other if other.starts_with("--workspace=") => {
        args.workspace = Some(PathBuf::from(other.split_once('=').expect("checked").1));
      }
      other if other.starts_with("--state-dir=") => {
        args.state_dir = Some(PathBuf::from(other.split_once('=').expect("checked").1));
      }
      other if args.diff.accept(other) => {}
      other if !other.starts_with('-') => {
        // `a..b`, the way `git diff` takes a range. Split here rather than sent
        // on, because `..` is exactly what the revision grammar refuses -- it is
        // range syntax, not part of a name.
        match other.split_once("..") {
          Some((from, to)) if !from.is_empty() && !to.is_empty() => {
            args.revs.push(from.to_owned());
            args.revs.push(to.to_owned());
          }
          _ => args.revs.push(other.to_owned()),
        }
        if args.revs.len() > 2 {
          bail!("gfs diff takes at most two revisions, or one `a..b` range");
        }
      }
      other => bail!(
        "gfs diff does not support `{other}`. Supported: -p/--stat/--name-only/\
         --name-status, -U<n>, --workspace, --state-dir, up to two revisions or \
         one `a..b` range, and `-- <path>`. Run with --help."
      ),
    }
    i += 1;
  }
  Ok(args)
}

fn print_help() {
  println!(
    "gfs diff: what the workspace changed, or what happened between two commits.\n\
     \n\
     USAGE:\n    \
       gfs diff                      the workspace's uncommitted changes\n    \
       gfs diff HEAD~3 HEAD          what the last three commits did\n    \
       gfs diff HEAD~3..HEAD         the same, as a range\n    \
       gfs diff main --stat          the pin against main, by file\n\
     \n\
     OPTIONS:\n    \
       -p, --patch          full patch (the default)\n    \
           --stat           changed-file histogram\n    \
           --name-only      just the paths\n    \
           --name-status    those paths with a status letter\n    \
       -U<n>, --unified=<n> context lines (default 3)\n    \
           --workspace <P>  the workspace, when not standing in it\n    \
           --state-dir <P>  its state directory, if placed explicitly\n\
     \n\
     REVISIONS:\n    \
       A branch, tag, commit id, or an ancestry expression: HEAD~3, main^,\n    \
       abc1234^2. `HEAD` is this workspace's pinned commit.\n\
     \n\
     With no revision this is the overlay's own change set, built from the\n\
     journal, and the rendering flags do not apply -- it is one whole patch.\n\
     With one revision it is compared against the pin, and uncommitted work is\n\
     called out on stderr rather than silently folded in.\n\
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
  fn a_range_is_two_revisions() {
    assert_eq!(parsed(&["a..b"]).revs, vec!["a", "b"]);
    assert_eq!(parsed(&["a", "b"]).revs, vec!["a", "b"]);
    assert_eq!(parsed(&["HEAD~3..HEAD"]).revs, vec!["HEAD~3", "HEAD"]);
  }

  #[test]
  fn a_lone_name_stays_one_revision() {
    assert_eq!(parsed(&["main"]).revs, vec!["main"]);
    assert!(parsed(&[]).revs.is_empty());
  }

  #[test]
  fn three_revisions_are_refused_rather_than_silently_dropped() {
    assert!(parse(&["a".into(), "b".into(), "c".into()]).is_err());
  }
}
