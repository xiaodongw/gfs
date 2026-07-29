//! What `log`, `show`, `diff` and `blame` share.
//!
//! All four read *history* rather than the mounted tree, and all four are
//! answered by the gateway over the workspace's control socket. What they have
//! in common is the vocabulary: the `%` placeholders `--format` accepts, the
//! flags that select a diff rendering, and the printing of a rendered diff. One
//! copy, because `gfs log -p` and `gfs show` must not disagree about what `-U2`
//! means or about how a truncated diff is reported.

use anyhow::{bail, Result};
use gfs_mount::control::{LogEntry, Request, Response, RevDiffReport};
use gfs_types::DiffFormat;

use crate::gitdate;
use crate::workspace;

/// Strip the `sha1:` qualifier the wire carries; Git prints bare hex.
pub fn hex(qualified: &str) -> &str {
  qualified.split_once(':').map_or(qualified, |(_, h)| h)
}

/// `%h`'s seven characters.
///
/// Git scales its abbreviation with the repository's object count — 10 for
/// django — so this is stable rather than identical to Git's. `%H` is the full
/// ID and always matches.
pub fn short(qualified: &str) -> String {
  hex(qualified).chars().take(7).collect()
}

/// The first line of a commit message, the way `--oneline` shows it.
pub fn subject(message: &[u8]) -> String {
  let first = message.split(|b| *b == b'\n').next().unwrap_or(b"");
  String::from_utf8_lossy(first).into_owned()
}

/// Everything after the subject and the blank line that follows it — `%b`.
///
/// Git's rule, and the reason a body is worth having at all: the subject says
/// what changed and the body says why, which for a review is usually the whole
/// point.
pub fn body(message: &[u8]) -> &[u8] {
  let mut rest = match message.iter().position(|b| *b == b'\n') {
    Some(i) => &message[i + 1..],
    None => return b"",
  };
  // Skip the blank line separating subject from body. Only one, so a body that
  // deliberately starts with a blank line keeps it.
  if rest.first() == Some(&b'\n') {
    rest = &rest[1..];
  }
  rest
}

/// Write one commit through a `--format` string.
///
/// The supported verbs and the reason each is here:
///
/// * `%H %h %T %t %P %p` — identity and shape;
/// * `%s %b %B` — subject, body, and the raw message. `%b` was the gap that
///   mattered: reviewing a commit without its body is reviewing what changed
///   with no access to why;
/// * `%an %ae %at %ad %ai %aI %ar` and the `%c…` committer equivalents. `%at`
///   alone forced every caller to convert epochs by hand.
///
/// `now` is passed in rather than read here so `%ar` is stable within one
/// invocation and testable outside one.
pub fn write_formatted(
  out: &mut impl std::io::Write,
  entry: &LogEntry,
  format: &str,
  now: i64,
) -> Result<()> {
  let mut chars = format.chars().peekable();
  while let Some(c) = chars.next() {
    if c != '%' {
      write!(out, "{c}")?;
      continue;
    }
    match chars.next() {
      Some('H') => write!(out, "{}", hex(&entry.commit))?,
      Some('h') => write!(out, "{}", short(&entry.commit))?,
      Some('T') => write!(out, "{}", hex(&entry.tree))?,
      Some('t') => write!(out, "{}", short(&entry.tree))?,
      Some('s') => write!(out, "{}", subject(&entry.message))?,
      Some('b') => out.write_all(body(&entry.message))?,
      Some('B') => out.write_all(&entry.message)?,
      Some('a') => match chars.next() {
        Some('n') => out.write_all(&entry.author_name)?,
        Some('e') => out.write_all(&entry.author_email)?,
        Some('t') => write!(out, "{}", entry.author_time)?,
        Some('d') => write!(
          out,
          "{}",
          gitdate::default_format(entry.author_time, entry.author_tz_offset_minutes)
        )?,
        Some('i') => write!(
          out,
          "{}",
          gitdate::iso(entry.author_time, entry.author_tz_offset_minutes)
        )?,
        Some('I') => write!(
          out,
          "{}",
          gitdate::iso_strict(entry.author_time, entry.author_tz_offset_minutes)
        )?,
        Some('r') => write!(out, "{}", gitdate::relative(entry.author_time, now))?,
        other => bail!(
          "unsupported format placeholder `%a{}`",
          other.unwrap_or(' ')
        ),
      },
      Some('c') => match chars.next() {
        Some('n') => out.write_all(&entry.committer_name)?,
        Some('e') => out.write_all(&entry.committer_email)?,
        Some('t') => write!(out, "{}", entry.committer_time)?,
        Some('d') => write!(
          out,
          "{}",
          gitdate::default_format(entry.committer_time, entry.committer_tz_offset_minutes)
        )?,
        Some('i') => write!(
          out,
          "{}",
          gitdate::iso(entry.committer_time, entry.committer_tz_offset_minutes)
        )?,
        Some('I') => write!(
          out,
          "{}",
          gitdate::iso_strict(entry.committer_time, entry.committer_tz_offset_minutes)
        )?,
        Some('r') => write!(out, "{}", gitdate::relative(entry.committer_time, now))?,
        other => bail!(
          "unsupported format placeholder `%c{}`",
          other.unwrap_or(' ')
        ),
      },
      Some('P') => write!(out, "{}", parents(entry, |p| hex(p).to_owned()))?,
      Some('p') => write!(out, "{}", parents(entry, short))?,
      Some('n') => writeln!(out)?,
      Some('%') => write!(out, "%")?,
      other => bail!(
        "unsupported format placeholder `%{}`. Supported: %H %h %T %t %P %p %s %b %B \
         %an %ae %at %ad %ai %aI %ar %cn %ce %ct %cd %ci %cI %cr %n %%",
        other.unwrap_or(' ')
      ),
    }
  }
  writeln!(out)?;
  Ok(())
}

fn parents(entry: &LogEntry, render: impl Fn(&str) -> String) -> String {
  entry
    .parents
    .iter()
    .map(|p| render(p))
    .collect::<Vec<_>>()
    .join(" ")
}

/// The full `git log` header block, for the default (unformatted) rendering.
pub fn write_default(out: &mut impl std::io::Write, entry: &LogEntry) -> Result<()> {
  writeln!(out, "commit {}", hex(&entry.commit))?;
  if entry.parents.len() > 1 {
    // Named, because a merge is the one commit where "what did this change"
    // depends on which parent you ask about. `gfs show` says the same thing.
    writeln!(
      out,
      "Merge: {}",
      entry
        .parents
        .iter()
        .map(|p| short(p))
        .collect::<Vec<_>>()
        .join(" ")
    )?;
  }
  out.write_all(b"Author: ")?;
  out.write_all(&entry.author_name)?;
  out.write_all(b" <")?;
  out.write_all(&entry.author_email)?;
  out.write_all(b">\n")?;
  writeln!(
    out,
    "Date:   {}",
    gitdate::default_format(entry.author_time, entry.author_tz_offset_minutes)
  )?;
  writeln!(out)?;
  for line in entry.message.split(|b| *b == b'\n') {
    out.write_all(b"    ")?;
    out.write_all(line)?;
    out.write_all(b"\n")?;
  }
  Ok(())
}

/// How a caller asked for a diff to be rendered.
#[derive(Clone, Debug, Default)]
pub struct DiffFlags {
  pub format: DiffFormat,
  /// `None` takes the server's default of three. `Some(0)` is a real request
  /// for no context, which a bare count could not express.
  pub context_lines: Option<u32>,
  /// Root-relative paths the diff is limited to.
  pub paths: Vec<Vec<u8>>,
}

impl DiffFlags {
  /// Parse the flags `log`, `show` and `diff` all accept. Returns whether the
  /// argument was one of them, so each caller keeps its own rejection message.
  pub fn accept(&mut self, arg: &str) -> bool {
    match arg {
      "--stat" => self.format = DiffFormat::Stat,
      "--name-only" => self.format = DiffFormat::NameOnly,
      "--name-status" => self.format = DiffFormat::NameStatus,
      "-p" | "-u" | "--patch" => self.format = DiffFormat::Patch,
      other => {
        // `-U<n>` and `--unified=<n>`, as `git diff` spells them.
        let count = other
          .strip_prefix("-U")
          .or_else(|| other.strip_prefix("--unified="));
        match count.and_then(|n| n.parse::<u32>().ok()) {
          Some(n) => self.context_lines = Some(n),
          None => return false,
        }
      }
    }
    true
  }
}

/// Ask the daemon for a rendered diff between two revisions.
///
/// `from` of `None` means "the parent being reviewed against", which the daemon
/// resolves — including the root-commit case, where the other side is the empty
/// tree.
pub fn request_diff(
  state_dir: &std::path::Path,
  from: Option<String>,
  to: String,
  parent: Option<u32>,
  flags: &DiffFlags,
) -> Result<RevDiffReport> {
  let response = workspace::call(
    state_dir,
    &Request::DiffRevs {
      from,
      to,
      parent,
      format: flags.format,
      context_lines: flags.context_lines,
      paths_b64url: flags
        .paths
        .iter()
        .map(|p| gfs_types::path::b64url_encode(p))
        .collect(),
    },
  )?;
  match response {
    Response::RevDiff(report) => Ok(*report),
    _ => bail!("the daemon answered a diff request with something else"),
  }
}

/// Write a rendered diff, and warn on stderr if it was cut short.
///
/// The warning goes to stderr rather than into the patch because the patch may
/// be piped into `git apply`, and a truncated one that *says* so in its body
/// would fail to apply for a confusing reason. On stderr it reaches the person
/// and not the pipe.
pub fn print_diff(out: &mut impl std::io::Write, report: &RevDiffReport) -> Result<()> {
  let rendered = gfs_types::path::b64url_decode(&report.rendered_b64url)
    .map_err(|e| anyhow::anyhow!("the daemon returned an undecodable diff: {}", e.message))?;
  out.write_all(&rendered)?;
  out.flush()?;
  if report.truncated {
    eprintln!(
      "gfs: this diff was cut short at the server's size limit; \
       {} file(s) changed. Limit it with `-- <path>` for the whole patch.",
      report.files.len()
    );
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn entry(message: &str) -> LogEntry {
    LogEntry {
      commit: "sha1:0123456789abcdef0123456789abcdef01234567".to_owned(),
      tree: "sha1:fedcba9876543210fedcba9876543210fedcba98".to_owned(),
      parents: vec!["sha1:1111111111111111111111111111111111111111".to_owned()],
      author_name: b"A U Thor".to_vec(),
      author_email: b"author@example.com".to_vec(),
      author_time: 1_112_911_993,
      author_tz_offset_minutes: -420,
      committer_name: b"C O Mitter".to_vec(),
      committer_email: b"committer@example.com".to_vec(),
      committer_time: 1_112_911_993,
      committer_tz_offset_minutes: 0,
      message: message.as_bytes().to_vec(),
    }
  }

  fn rendered(format: &str, message: &str) -> String {
    let mut out = Vec::new();
    write_formatted(&mut out, &entry(message), format, 1_700_000_000).unwrap();
    String::from_utf8(out).unwrap()
  }

  #[test]
  fn the_body_is_everything_after_the_subject_and_its_blank_line() {
    // The gap the 2026-07-29 agent report named: `%s` alone says what changed
    // and the body says why.
    assert_eq!(body(b"subject\n\nwhy it was done\n"), b"why it was done\n");
    // A one-line message has no body, and that is not an error.
    assert_eq!(body(b"subject only"), b"");
    assert_eq!(body(b"subject only\n"), b"");
    // Only *one* blank line is consumed, so a body that starts with one keeps it.
    assert_eq!(body(b"subject\n\n\nindented\n"), b"\nindented\n");
  }

  #[test]
  fn the_new_placeholders_render() {
    assert_eq!(rendered("%b", "subject\n\nwhy\n"), "why\n\n");
    assert_eq!(rendered("%s", "subject\n\nwhy\n"), "subject\n");
    assert_eq!(rendered("%t", "s"), "fedcba9\n");
    assert_eq!(rendered("%ad", "s"), "Thu Apr 7 15:13:13 2005 -0700\n");
    assert_eq!(rendered("%ai", "s"), "2005-04-07 15:13:13 -0700\n");
    assert_eq!(rendered("%aI", "s"), "2005-04-07T15:13:13-07:00\n");
    // The committer's own offset, not the author's.
    assert_eq!(rendered("%ci", "s"), "2005-04-07 22:13:13 +0000\n");
    assert!(rendered("%ar", "s").ends_with(" ago\n"));
    assert_eq!(rendered("%p", "s"), "1111111\n");
  }

  #[test]
  fn an_unknown_placeholder_names_what_is_supported() {
    let mut out = Vec::new();
    let e = write_formatted(&mut out, &entry("s"), "%z", 0).unwrap_err();
    let message = format!("{e}");
    assert!(message.contains("%z"), "{message}");
    assert!(message.contains("%b"), "{message}");
  }

  #[test]
  fn the_diff_flags_are_the_ones_git_spells_the_same_way() {
    let mut flags = DiffFlags::default();
    assert!(flags.accept("--stat"));
    assert_eq!(flags.format, DiffFormat::Stat);
    assert!(flags.accept("-p"));
    assert_eq!(flags.format, DiffFormat::Patch);
    assert!(flags.accept("-U0"));
    // `Some(0)` and `None` are different requests, which is why this is an
    // Option: zero context is a real thing to ask for.
    assert_eq!(flags.context_lines, Some(0));
    assert!(flags.accept("--unified=7"));
    assert_eq!(flags.context_lines, Some(7));
    assert!(!flags.accept("--color"));
    assert!(!flags.accept("-U"));
  }
}
