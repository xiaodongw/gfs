//! `gfs blame`: who last changed each line, answered by the server.
//!
//! The normal next step after `gfs rg` finds a suspicious line, and until this
//! existed there was no way to take it: the workspace has no object database, so
//! stock `git blame` inside a mount has nothing to walk.
//!
//! libgit2 runs the blame where the objects are. The file's bytes come back with
//! the hunks in the same response, because a blame without the lines it
//! attributes is not an answer and fetching them separately would cost a second
//! round trip and a blob ticket for one bounded file.

use std::path::PathBuf;

use anyhow::{bail, Result};
use gfs_mount::control::{BlameHunkEntry, BlameReport, Request, Response};

use crate::{gitdate, history, workspace};

#[derive(Debug, Default)]
struct Args {
  path: Option<String>,
  rev: Option<String>,
  /// Restrict output to these 1-based lines, as `git blame -L` does.
  range: Option<(u32, u32)>,
  json: bool,
  /// Show the whole author name and an ISO date rather than the compact form.
  long: bool,
  workspace: Option<PathBuf>,
}

pub fn run(argv: &[String]) -> Result<i32> {
  if argv.iter().any(|a| a == "-h" || a == "--help") {
    print_help();
    return Ok(0);
  }
  let args = parse(argv)?;
  let Some(path) = args.path.clone() else {
    bail!("gfs blame needs a path. Run with --help.");
  };
  let (workspace_root, state_dir) = workspace::resolve(&args.workspace)?;

  // A path is given the way it is typed, which for an agent standing inside a
  // subdirectory is relative to *there*. The snapshot only knows root-relative
  // paths, so the two are reconciled here rather than by asking the caller to
  // know where the workspace root is.
  let path = workspace::repo_relative(&workspace_root, &path)?;

  let Response::Blame(report) = workspace::call(
    &state_dir,
    &Request::Blame {
      rev: args.rev.clone(),
      path_b64url: gfs_types::path::b64url_encode(&path),
    },
  )?
  else {
    bail!("the daemon answered a blame request with something else");
  };

  if args.json {
    println!("{}", serde_json::to_string(&report)?);
    return Ok(0);
  }
  print_text(&report, &args)?;
  Ok(0)
}

fn print_text(report: &BlameReport, args: &Args) -> Result<()> {
  use std::io::Write;
  if report.truncated {
    bail!(
      "that file is too large to blame; the server returns attribution only for \
       files under its searchable-blob limit"
    );
  }
  let content = gfs_types::path::b64url_decode(&report.content_b64url)
    .map_err(|e| anyhow::anyhow!("the daemon returned undecodable content: {}", e.message))?;
  // `split` rather than `lines`, and the trailing empty piece after a final
  // newline is dropped: a file ending in `\n` has N lines, not N+1, which is
  // what the hunk line numbers count.
  let mut lines: Vec<&[u8]> = content.split(|b| *b == b'\n').collect();
  if lines.last() == Some(&&b""[..]) {
    lines.pop();
  }

  // The widths are computed over what will actually be printed, so a file whose
  // authors have short names does not get a column of padding.
  let width = |f: fn(&BlameHunkEntry) -> usize| report.hunks.iter().map(f).max().unwrap_or(0);
  let name_width = width(|h| String::from_utf8_lossy(&h.author_name).chars().count());
  let line_width = lines.len().to_string().len();

  let stdout = std::io::stdout();
  let mut out = stdout.lock();
  for hunk in &report.hunks {
    for offset in 0..hunk.lines {
      let number = hunk.final_start_line + offset;
      if let Some((first, last)) = args.range {
        if number < first || number > last {
          continue;
        }
      }
      let date = if args.long {
        gitdate::iso(hunk.author_time, hunk.author_tz_offset_minutes)
      } else {
        // The date alone, which is what `git blame` shows: the time of day is
        // rarely what a reader is after and it costs eight columns.
        gitdate::iso(hunk.author_time, hunk.author_tz_offset_minutes)
          .chars()
          .take(10)
          .collect()
      };
      write!(out, "{} (", history::short(&hunk.commit))?;
      let name = String::from_utf8_lossy(&hunk.author_name);
      write!(out, "{name:<name_width$} {date} {number:>line_width$}) ")?;
      // The line itself, as bytes: source is not required to be UTF-8.
      match lines.get(number as usize - 1) {
        Some(line) => out.write_all(line)?,
        None => out.write_all(b"")?,
      }
      out.write_all(b"\n")?;
    }
  }
  out.flush()?;
  Ok(())
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
      "--json" => args.json = true,
      "-l" | "--long" => args.long = true,
      "--rev" => args.rev = Some(take_value("--rev")?),
      "-L" => args.range = Some(parse_range(&take_value("-L")?)?),
      "--workspace" => args.workspace = Some(PathBuf::from(take_value("--workspace")?)),
      other if other.starts_with("--workspace=") => {
        args.workspace = Some(PathBuf::from(other.split_once('=').expect("checked").1));
      }
      other if other.starts_with("--rev=") => {
        args.rev = Some(other.split_once('=').expect("checked").1.to_owned());
      }
      other if !other.starts_with('-') && args.path.is_none() => {
        args.path = Some(other.to_owned());
      }
      // A second bare argument is a revision, the way `git blame <rev> -- <path>`
      // allows -- except spelled `--rev`, so the two cannot be confused.
      other => bail!(
        "gfs blame does not support `{other}`. Supported: <path>, --rev <r>, \
         -L <first>,<last>, -l/--long, --json, --workspace. Run with --help."
      ),
    }
    i += 1;
  }
  Ok(args)
}

/// `-L 10,40`, or `-L 10` for a single line.
fn parse_range(text: &str) -> Result<(u32, u32)> {
  let (first, last) = match text.split_once(',') {
    Some((a, b)) => (a, b),
    None => (text, text),
  };
  let first: u32 = first
    .trim()
    .parse()
    .map_err(|_| anyhow::anyhow!("`-L {text}`: {first:?} is not a line number"))?;
  let last: u32 = last
    .trim()
    .parse()
    .map_err(|_| anyhow::anyhow!("`-L {text}`: {last:?} is not a line number"))?;
  if first == 0 || last < first {
    bail!("`-L {text}`: lines are numbered from 1 and the range must not run backwards");
  }
  Ok((first, last))
}

fn print_help() {
  println!(
    "gfs blame: who last changed each line, answered by the server.\n\
     \n\
     USAGE:\n    \
       gfs blame <path>\n    \
       gfs blame src/flask/cli.py -L 40,80\n    \
       gfs blame README.md --rev HEAD~5\n\
     \n\
     OPTIONS:\n    \
           --rev <R>        blame as of this revision (default: the pin)\n    \
       -L <first>,<last>    only these lines; `-L <n>` for one\n    \
       -l, --long           full ISO timestamp rather than the date\n    \
           --json           machine-readable output\n    \
           --workspace <P>  the workspace, when not standing in it\n\
     \n\
     The path may be relative to where you are standing, as `git blame` allows.\n\
     Rename following is on: a hunk names the path the file had in the commit\n\
     it is attributed to.\n\
     \n\
     A binary file, a directory, or a submodule is refused rather than\n\
     attributed, and a file over the searchable-blob limit is too large to\n\
     blame.\n\
     \n\
     EXIT: 0 shown, 2 no answer."
  );
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_line_range_accepts_both_spellings() {
    assert_eq!(parse_range("10,40").unwrap(), (10, 40));
    assert_eq!(parse_range("7").unwrap(), (7, 7));
    assert_eq!(parse_range(" 3 , 9 ").unwrap(), (3, 9));
  }

  #[test]
  fn a_backwards_or_zero_range_is_refused() {
    // Silently swapping them would answer a question the caller did not ask, and
    // line 0 does not exist -- both are typos worth reporting.
    assert!(parse_range("40,10").is_err());
    assert!(parse_range("0,10").is_err());
    assert!(parse_range("a,b").is_err());
  }

  #[test]
  fn the_first_bare_argument_is_the_path() {
    let args = parse(&[
      "src/main.rs".to_owned(),
      "--rev".to_owned(),
      "HEAD~1".to_owned(),
    ])
    .unwrap();
    assert_eq!(args.path.as_deref(), Some("src/main.rs"));
    assert_eq!(args.rev.as_deref(), Some("HEAD~1"));
  }
}
