//! `gfs rg`: the `rg` substitute an agent can safely be given.
//!
//! PLAN.md M4.5: "Implement `gfs rg` for the selected safe flag subset and fail
//! closed on unsupported flags unless explicit hydration is requested."
//!
//! # Why the argv parser is hand-rolled
//!
//! This is a subcommand of `gfs`, but it does not use `clap` like the rest of
//! the CLI does. The whole contract is that an *unrecognized* `rg` flag is
//! refused by name with an explanation, and `clap` cannot express that: it
//! knows only what it was told about, so `-w` would be an unknown-argument
//! error rather than "`-w` changes what counts as a match, and here is the way
//! out". `gfs` therefore hands this module the raw argv and lets it parse.
//!
//! # Why a substitute is needed at all
//!
//! Real `rg` inside a GFS mount walks every directory and reads every file.
//! On the M0.1 worst-case repository that is 94 751 first-time FUSE lookups and
//! a full download of the tree — it turns the one operation an agent performs
//! most into the most expensive thing it can possibly do, and destroys the
//! property the project exists for. `gfs rg` answers the same question from the
//! server's index and the overlay journal.
//!
//! # Failing closed
//!
//! Every `rg` flag this does not implement is **rejected**, with a message
//! naming what to do instead. The alternative — ignoring an unknown flag — is
//! how `gfs rg -w authorize` silently returns non-word-boundary matches and an
//! agent acts on a wrong answer. `--hydrate` is the documented escape hatch: it
//! says "run real `rg` over the mount, and accept the cost", so a flag GFS
//! cannot honour still has an answer, just an expensive and explicit one.
//!
//! # The exit codes are `rg`'s, extended
//!
//! 0 for matches and 1 for none are ripgrep's own. 2, 3, and 4 are ADR 0004's
//! additions and are the reason this tool exists as more than a performance
//! optimization: `rg` has no way to say "I did not finish", and an agent that
//! cannot hear that concludes a symbol does not exist.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use gfs_mount::control::{Request, Response};
use gfs_mount::search::SearchRequest;

use crate::search_output;
use crate::workspace;

/// The flags `gfs rg` implements, and how they map.
///
/// Written as a table rather than derived with `clap` because the *rejection*
/// message matters as much as the acceptance: a flag has to be recognized as a
/// real `rg` flag in order to be refused informatively.
const SUPPORTED: &[(&str, &str)] = &[
  ("-e/--regexp", "the pattern"),
  ("-F/--fixed-strings", "literal search"),
  ("-i/--ignore-case", "case-insensitive search"),
  ("-g/--glob", "include glob, repeatable"),
  ("--iglob", "not supported; use -g with explicit cases"),
  ("-A/--after-context", "trailing context lines"),
  ("-B/--before-context", "leading context lines"),
  ("-C/--context", "context lines on both sides"),
  ("-m/--max-count", "result limit"),
  ("-M/--max-columns", "cut a returned line to this many bytes"),
  ("--json", "machine-readable output"),
  ("--no-ignore", "search ignored files"),
  ("--require-exhaustive", "a coverage gap becomes exit 4"),
  ("--hydrate", "run real rg over the mount instead"),
];

#[derive(Debug, Default)]
struct Args {
  pattern: Option<String>,
  paths: Vec<String>,
  literal: bool,
  ignore_case: bool,
  globs: Vec<String>,
  excludes: Vec<String>,
  before: u32,
  after: u32,
  max_results: u32,
  max_columns: u32,
  json: bool,
  no_ignore: bool,
  require_exhaustive: bool,
  hydrate: bool,
  workspace: Option<PathBuf>,
}

/// Run `gfs rg` over the raw arguments that followed the subcommand.
///
/// Returns the ADR 0004 exit code rather than exiting, so the caller decides.
pub fn run(argv: &[String]) -> Result<i32> {
  if argv.is_empty() || argv.iter().any(|a| a == "-h" || a == "--help") {
    print_help();
    return Ok(0);
  }
  let args = parse(argv)?;

  let pattern = args
    .pattern
    .clone()
    .ok_or_else(|| anyhow::anyhow!("no pattern given"))?;

  let (workspace_root, state_dir) = workspace::resolve(&args.workspace)?;

  if args.hydrate {
    return hydrate(&workspace_root, argv);
  }

  // Paths on the command line become the scope. More than one would need the
  // protocol to carry a set rather than a prefix; refused rather than silently
  // searching only the first, which would be a wrong answer that looks right.
  let scope = match args.paths.len() {
    0 => Vec::new(),
    1 => args.paths[0].as_bytes().to_vec(),
    _ => bail!(
      "`gfs rg` takes at most one path argument; got {}. \
       Use -g globs to select more than one subtree.",
      args.paths.len()
    ),
  };

  let request = SearchRequest {
    pattern,
    literal: args.literal,
    case_insensitive: args.ignore_case,
    scope,
    include_globs: args.globs.clone(),
    exclude_globs: args.excludes.clone(),
    context_before: args.before,
    context_after: args.after,
    max_results: args.max_results,
    max_line_bytes: args.max_columns,
    search_ignored: args.no_ignore,
  };

  let Response::Search(report) = workspace::call(&state_dir, &Request::Search(Box::new(request)))?
  else {
    bail!("the daemon answered a search request with something else");
  };

  if args.json {
    println!("{}", search_output::to_json(&report)?);
  } else {
    let mut out = std::io::stdout().lock();
    search_output::print_text(&report, &mut out)?;
    std::io::Write::flush(&mut out)?;
    search_output::print_diagnostics(&report, &mut std::io::stderr())?;
  }
  Ok(search_output::exit_code(&report, args.require_exhaustive))
}

/// Run real `rg` over the mount, having been told to.
fn hydrate(workspace_root: &std::path::Path, argv: &[String]) -> Result<i32> {
  let forwarded: Vec<&String> = argv.iter().filter(|a| *a != "--hydrate").collect();
  eprintln!(
    "gfs rg: --hydrate: running real rg over {}. This walks the mount and \
     downloads every file it reads.",
    workspace_root.display()
  );
  let status = std::process::Command::new("rg")
    .args(forwarded)
    .current_dir(workspace_root)
    // Real `rg` resolved from PATH may be the scan shim itself; the variable
    // tells it to stand aside instead of delegating back here.
    .env("GFS_SHIM_BYPASS", "1")
    .status()
    .context("running rg; is it installed?")?;
  Ok(status.code().unwrap_or(2))
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
      "-e" | "--regexp" => args.pattern = Some(take_value("-e")?),
      "-F" | "--fixed-strings" => args.literal = true,
      "-i" | "--ignore-case" => args.ignore_case = true,
      "-g" | "--glob" => args.globs.push(take_value("-g")?),
      "--exclude" => args.excludes.push(take_value("--exclude")?),
      "-A" | "--after-context" => args.after = take_value("-A")?.parse()?,
      "-B" | "--before-context" => args.before = take_value("-B")?.parse()?,
      "-C" | "--context" => {
        let n: u32 = take_value("-C")?.parse()?;
        args.before = n;
        args.after = n;
      }
      "-m" | "--max-count" => args.max_results = take_value("-m")?.parse()?,
      // `rg` suppresses a line this wide and prints a note in its place; GFS
      // keeps the first bytes and marks them. The flag is spelled the same
      // because the intent is the same, and a caller porting an `rg` invocation
      // should not have to look this one up.
      "-M" | "--max-columns" => args.max_columns = take_value("-M")?.parse()?,
      "--json" => args.json = true,
      "--no-ignore" => args.no_ignore = true,
      "--require-exhaustive" => args.require_exhaustive = true,
      "--hydrate" => args.hydrate = true,
      "--workspace" => args.workspace = Some(PathBuf::from(take_value("--workspace")?)),
      "--" => {
        i += 1;
        while i < argv.len() {
          absorb(&mut args, &argv[i]);
          i += 1;
        }
        break;
      }
      other if other.starts_with('-') && other != "-" => {
        return Err(unsupported(other));
      }
      other => absorb(&mut args, other),
    }
    i += 1;
  }
  Ok(args)
}

fn absorb(args: &mut Args, value: &str) {
  if args.pattern.is_none() {
    args.pattern = Some(value.to_owned());
  } else {
    args.paths.push(value.to_owned());
  }
}

/// Refuse an unimplemented flag, with the reason and the way out.
fn unsupported(flag: &str) -> anyhow::Error {
  anyhow::anyhow!(
    "unsupported flag {flag}.\n\
     `gfs rg` implements a deliberately small subset of ripgrep and refuses \
     the rest rather than ignoring it -- a flag that changes what counts as a \
     match must not be silently dropped, because the wrong answer would look \
     like a right one.\n\
     Supported: {}\n\
     To run real ripgrep over the mount anyway, add --hydrate. That walks the \
     workspace and downloads every file it reads.",
    SUPPORTED
      .iter()
      .map(|(flag, _)| *flag)
      .collect::<Vec<_>>()
      .join(", ")
  )
}

fn print_help() {
  println!("gfs rg -- search a GFS workspace without hydrating it\n");
  println!("USAGE:\n  gfs rg [OPTIONS] PATTERN [PATH]\n");
  println!("OPTIONS:");
  for (flag, meaning) in SUPPORTED {
    println!("  {flag:<24} {meaning}");
  }
  println!("\nEXIT CODES (ADR 0004):");
  println!("  0  complete, matches found");
  println!("  1  complete, no matches");
  println!("  2  the search did not complete; the results are not an answer");
  println!("  3  execution truncated by a budget; not exhaustive");
  println!("  4  a coverage gap, under --require-exhaustive");
}

#[cfg(test)]
mod tests {
  use super::*;

  fn parsed(argv: &[&str]) -> Result<Args> {
    parse(&argv.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>())
  }

  #[test]
  fn the_first_positional_is_the_pattern_and_the_second_is_a_path() {
    let args = parsed(&["needle", "src"]).unwrap();
    assert_eq!(args.pattern.as_deref(), Some("needle"));
    assert_eq!(args.paths, vec!["src".to_owned()]);
  }

  #[test]
  fn an_unsupported_flag_is_refused_rather_than_ignored() {
    // The failure this exists to prevent: `-w` changes what counts as a match,
    // and dropping it returns matches the caller did not ask for.
    let err = parsed(&["-w", "needle"]).unwrap_err();
    let text = format!("{err}");
    assert!(text.contains("unsupported flag -w"), "{text}");
    assert!(text.contains("--hydrate"), "{text}");
  }

  #[test]
  fn the_supported_subset_parses() {
    let args = parsed(&[
      "-F", "-i", "-g", "*.rs", "-C", "2", "-m", "10", "-M", "200", "--json", "needle",
    ])
    .unwrap();
    assert!(args.literal);
    assert!(args.ignore_case);
    assert_eq!(args.globs, vec!["*.rs".to_owned()]);
    assert_eq!(args.before, 2);
    assert_eq!(args.after, 2);
    assert_eq!(args.max_results, 10);
    assert_eq!(args.max_columns, 200);
    assert!(args.json);
    assert_eq!(args.pattern.as_deref(), Some("needle"));
  }

  #[test]
  fn a_pattern_that_looks_like_a_flag_survives_after_a_double_dash() {
    let args = parsed(&["--", "-not-a-flag"]).unwrap();
    assert_eq!(args.pattern.as_deref(), Some("-not-a-flag"));
  }

  #[test]
  fn an_explicit_pattern_flag_wins_over_a_positional() {
    let args = parsed(&["-e", "needle", "src"]).unwrap();
    assert_eq!(args.pattern.as_deref(), Some("needle"));
    assert_eq!(args.paths, vec!["src".to_owned()]);
  }
}
