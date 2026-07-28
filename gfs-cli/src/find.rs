//! `gfs find`: the `find -name` substitute an agent can safely be given.
//!
//! # Why a substitute is needed at all
//!
//! Real `find` inside a GFS mount walks every directory, which on the M0.1
//! worst case is 94 751 first-time FUSE lookups — the same objection ADR 0005
//! raised against letting `git status` sweep the tree. The shim's `git ls-files`
//! avoided the walk but replaced it with one snapshot-API round trip per
//! directory, measured at 56 seconds on a 7 000-file repository for a question
//! the server can answer in one request.
//!
//! `gfs find` asks the server for the pinned commit's matching paths and merges
//! the workspace's own changes locally. It downloads nothing.
//!
//! # Named entries, not directories
//!
//! The result set is `git ls-files`'s: every file, symlink, gitlink, and
//! unmodelled mode, with directories recursed into but not listed. Symlinks are
//! included deliberately — the *search* corpus drops them to agree with `rg`, and
//! reusing that corpus here silently omitted four paths in django and would omit
//! 99 in the Linux kernel. A filename search that answers "no such file" about a
//! file that is right there is the wrong answer that looks like a right one.
//!
//! # Exit codes
//!
//! ripgrep's, and the same ones `gfs rg` uses, because an agent should not have
//! to learn a second convention: 0 for matches, 1 for none, 2 for "no answer",
//! 3 when a limit truncated the result. 1 and 3 are the pair that matters — both
//! may print nothing, and only 1 means "there is no such file".

use std::path::PathBuf;

use anyhow::{bail, Result};
use gfs_mount::control::{Request, Response};
use gfs_mount::find::FindRequest;

use crate::workspace;

#[derive(Debug, Default)]
struct Args {
  globs: Vec<String>,
  excludes: Vec<String>,
  scope: Option<String>,
  max_results: u32,
  json: bool,
  null: bool,
  workspace: Option<PathBuf>,
}

/// Run `gfs find` over the raw arguments that followed the subcommand.
///
/// Returns the exit code rather than exiting, so the caller decides.
pub fn run(argv: &[String]) -> Result<i32> {
  if argv.iter().any(|a| a == "-h" || a == "--help") {
    print_help();
    return Ok(0);
  }
  let args = parse(argv)?;

  let (_workspace_root, state_dir) = workspace::resolve(&args.workspace)?;

  let request = FindRequest {
    scope: args.scope.clone().unwrap_or_default().into_bytes(),
    include_globs: args.globs.clone(),
    exclude_globs: args.excludes.clone(),
    max_results: args.max_results,
  };

  let Response::Find(report) = workspace::call(&state_dir, &Request::Find(Box::new(request)))?
  else {
    bail!("the daemon answered a find request with something else");
  };

  if args.json {
    println!("{}", serde_json::to_string(&report)?);
  } else {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let terminator = if args.null { b"\0" } else { b"\n" };
    for path in &report.paths {
      // Written as bytes: a repository path is not required to be UTF-8, and
      // lossily converting one would print a name that cannot be opened.
      out.write_all(path.as_bytes())?;
      out.write_all(terminator)?;
    }
    out.flush()?;
    if report.truncated {
      eprintln!(
        "gfs find: truncated at {} paths; narrow the glob or raise --max-results",
        report.paths.len()
      );
    }
  }

  Ok(match (report.paths.is_empty(), report.truncated) {
    (_, true) => 3,
    (true, false) => 1,
    (false, false) => 0,
  })
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
      "-g" | "--glob" => args.globs.push(take_value("--glob")?),
      "--exclude" => args.excludes.push(take_value("--exclude")?),
      // `--max-results` and ripgrep's `-m/--max-count` both, so an agent that
      // learned the limit flag from `gfs rg` does not get a refusal here.
      "--max-results" | "-m" | "--max-count" => {
        args.max_results = take_value("--max-results")?.parse()?
      }
      "--workspace" => args.workspace = Some(PathBuf::from(take_value("--workspace")?)),
      "--json" => args.json = true,
      "-0" | "--null" => args.null = true,
      // A bare word is the name pattern, so `gfs find '*.py'` works the way an
      // agent would guess. A second one is a scope, matching `find <path>`
      // reversed — the glob is the common argument, so it comes first.
      other if !other.starts_with('-') => {
        if args.globs.is_empty() {
          args.globs.push(other.to_owned());
        } else if args.scope.is_none() {
          args.scope = Some(other.to_owned());
        } else {
          bail!("unexpected argument `{other}`");
        }
      }
      other => bail!(
        "gfs find does not support `{other}`. Supported: -g/--glob, --exclude, \
         --max-results, --workspace, --json, -0/--null. Run with --help."
      ),
    }
    i += 1;
  }
  Ok(args)
}

fn print_help() {
  println!(
    "gfs find: filename search over a GFS workspace, answered by the server.\n\
     \n\
     USAGE:\n    \
       gfs find <glob> [scope]\n    \
       gfs find -g '*.py' -g '*.pyi' --exclude '*/tests/*'\n\
     \n\
     OPTIONS:\n    \
       -g, --glob <G>       include glob, repeatable\n    \
           --exclude <G>    exclude glob, repeatable\n    \
           --max-results <N>\n    \
           --workspace <P>  the workspace, when not standing in it\n    \
           --json           machine-readable output\n    \
       -0, --null           NUL-terminate paths, for xargs -0\n\
     \n\
     Lists files, symlinks and gitlinks, as `git ls-files` does. Local edits are\n\
     included: a created file is found, a deleted one is not, a renamed one is\n     found at its new path.\n\
     \n\
     EXIT: 0 matches, 1 none, 2 no answer, 3 truncated."
  );
}
