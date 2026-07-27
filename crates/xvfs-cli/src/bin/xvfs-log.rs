//! `xvfs-log`: `git log` for a workspace that has no object database.
//!
//! # Why this exists
//!
//! ADR 0005 chose a synthesized `.git` over a real partial clone, which means the
//! workspace has no packfile to walk. The shim's `log` was frozen at `-1` as a
//! result: the pinned commit's metadata is known, and nothing could produce its
//! parent. An agent orienting itself by recent history had no answer, and a
//! `--depth 1` clone — the raw-git equivalent — has the same single commit.
//!
//! The server has the object database. `xvfs-log` asks it to walk, which costs
//! one round trip and downloads nothing.
//!
//! # What is deliberately not here
//!
//! No `-p`, no `-S`, no `--follow`, no path limiting. Each needs trees or blobs
//! per commit, which is the unbounded hydration ADR 0005 rejected the partial
//! clone for. They are refused by name rather than approximated, the same way the
//! `git` shim refuses what it cannot answer.
//!
//! No `--topo-order` either, and the default is not topological. `git log -10`
//! on the M0.1 worst case is 0.007 s in date order and 10.383 s with
//! `--topo-order`, because topological sorting buffers the reachable graph
//! before emitting anything. The visible cost is that two commits sharing a
//! commit timestamp may appear in the opposite order to `git log`; the set and
//! the time ordering are the same.
//!
//! `%h` abbreviates to 7 characters. Git scales its abbreviation with the
//! repository's object count — 10 for django — so `%h` here is stable rather
//! than identical to Git's. `%H` is the full ID and always matches.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Result};
use xvfs_cli::workspace;
use xvfs_fuse::control::{LogEntry, Request, Response};

fn main() -> ExitCode {
  match run() {
    Ok(code) => ExitCode::from(code as u8),
    Err(e) => {
      eprintln!("xvfs-log: {e:#}");
      ExitCode::from(2)
    }
  }
}

#[derive(Debug)]
struct Args {
  limit: u32,
  skip: u32,
  oneline: bool,
  format: Option<String>,
  json: bool,
  workspace: Option<PathBuf>,
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
    }
  }
}

fn run() -> Result<i32> {
  let argv: Vec<String> = std::env::args().skip(1).collect();
  if argv.iter().any(|a| a == "-h" || a == "--help") {
    print_help();
    return Ok(0);
  }
  let args = parse(&argv)?;
  let (_workspace_root, state_dir) = workspace::resolve(&args.workspace)?;

  let Response::Log(report) = workspace::call(
    &state_dir,
    &Request::Log {
      skip: args.skip,
      limit: args.limit,
    },
  )?
  else {
    bail!("the daemon answered a log request with something else");
  };

  if args.json {
    println!("{}", serde_json::to_string(&report)?);
    return Ok(0);
  }

  use std::io::Write;
  let mut out = std::io::stdout().lock();
  for entry in &report.commits {
    match (&args.format, args.oneline) {
      (Some(format), _) => write_formatted(&mut out, entry, format)?,
      (None, true) => {
        writeln!(out, "{} {}", short(&entry.commit), subject(&entry.message))?;
      }
      (None, false) => {
        writeln!(out, "commit {}", hex(&entry.commit))?;
        out.write_all(b"Author: ")?;
        out.write_all(&entry.author_name)?;
        out.write_all(b" <")?;
        out.write_all(&entry.author_email)?;
        out.write_all(b">\n")?;
        writeln!(out, "Date:   {}", entry.author_time)?;
        writeln!(out)?;
        for line in entry.message.split(|b| *b == b'\n') {
          out.write_all(b"    ")?;
          out.write_all(line)?;
          out.write_all(b"\n")?;
        }
      }
    }
  }
  out.flush()?;

  if report.has_more {
    eprintln!(
      "xvfs-log: more history follows; continue with --skip {}",
      args.skip as usize + report.commits.len()
    );
  }
  Ok(if report.commits.is_empty() { 1 } else { 0 })
}

/// The `%`-placeholders `git log --format` uses that are answerable here.
fn write_formatted(out: &mut impl std::io::Write, entry: &LogEntry, format: &str) -> Result<()> {
  let mut chars = format.chars().peekable();
  while let Some(c) = chars.next() {
    if c != '%' {
      write!(out, "{c}")?;
      continue;
    }
    match chars.next() {
      Some('H') => write!(out, "{}", hex(&entry.commit))?,
      Some('h') => write!(out, "{}", short(&entry.commit))?,
      Some('s') => write!(out, "{}", subject(&entry.message))?,
      Some('a') => match chars.next() {
        Some('n') => out.write_all(&entry.author_name)?,
        Some('e') => out.write_all(&entry.author_email)?,
        Some('t') => write!(out, "{}", entry.author_time)?,
        other => bail!("unsupported format placeholder `%a{}`", other.unwrap_or(' ')),
      },
      Some('c') => match chars.next() {
        Some('n') => out.write_all(&entry.committer_name)?,
        Some('e') => out.write_all(&entry.committer_email)?,
        Some('t') => write!(out, "{}", entry.committer_time)?,
        other => bail!("unsupported format placeholder `%c{}`", other.unwrap_or(' ')),
      },
      Some('P') => write!(
        out,
        "{}",
        entry
          .parents
          .iter()
          .map(|p| hex(p))
          .collect::<Vec<_>>()
          .join(" ")
      )?,
      Some('n') => writeln!(out)?,
      Some('%') => write!(out, "%")?,
      other => bail!(
        "unsupported format placeholder `%{}`. Supported: %H %h %s %an %ae %at \
         %cn %ce %ct %P %n %%",
        other.unwrap_or(' ')
      ),
    }
  }
  writeln!(out)?;
  Ok(())
}

/// Strip the `sha1:` qualifier the wire carries; `git log` prints bare hex.
fn hex(qualified: &str) -> &str {
  qualified.split_once(':').map_or(qualified, |(_, h)| h)
}

fn short(qualified: &str) -> String {
  hex(qualified).chars().take(7).collect()
}

/// The first line, the way `git log --oneline` shows it.
fn subject(message: &[u8]) -> String {
  let first = message.split(|b| *b == b'\n').next().unwrap_or(b"");
  String::from_utf8_lossy(first).into_owned()
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
      "--oneline" => args.oneline = true,
      "--json" => args.json = true,
      "-n" | "--max-count" => args.limit = take_value("--max-count")?.parse()?,
      "--skip" => args.skip = take_value("--skip")?.parse()?,
      "--workspace" => args.workspace = Some(PathBuf::from(take_value("--workspace")?)),
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
      "-p" | "--patch" | "-S" | "--follow" | "--stat" | "--graph" => bail!(
        "xvfs-log does not support `{arg}`: it needs a tree or blob per commit, \
         which is the unbounded download ADR 0005 exists to avoid. Use \
         `xvfs-rg` to search content, or clone through the gateway for full \
         history tooling."
      ),
      other => bail!(
        "xvfs-log does not support `{other}`. Supported: -n/--max-count, -<n>, \
         --skip, --oneline, --format=/--pretty=, --json, --workspace. Run with --help."
      ),
    }
    i += 1;
  }
  Ok(args)
}

fn print_help() {
  println!(
    "xvfs-log: commit history for an XVFS workspace, answered by the server.\n\
     \n\
     USAGE:\n    \
       xvfs-log [-n <count>] [--oneline] [--skip <n>]\n    \
       xvfs-log -10 --format='%h %s'\n\
     \n\
     OPTIONS:\n    \
       -n, --max-count <N>  commits to show (default 20); also `-<n>`\n    \
           --skip <N>       skip this many first, for paging\n    \
           --oneline        abbreviated hash and subject\n    \
           --format=<FMT>   %H %h %s %an %ae %at %cn %ce %ct %P %n %%\n    \
           --json           machine-readable output\n    \
           --workspace <P>  the workspace, when not standing in it\n\
     \n\
     History is the pinned commit's ancestry. Local edits do not create commits,\n\
     so they do not appear here; use `xvfs status` for what the workspace changed.\n\
     \n\
     -p, -S, --follow, --stat and --graph are refused: each needs a tree or blob\n\
     per commit, which would download the repository a piece at a time.\n\
     \n\
     EXIT: 0 commits shown, 1 none, 2 no answer."
  );
}
