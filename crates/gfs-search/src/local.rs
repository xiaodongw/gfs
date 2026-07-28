//! Searching local content, with the same rules the server uses.
//!
//! PLAN.md M4.5 asks for the local half of a search to be bounded "the same way
//! the server bounds its own", and lists the specific ways: honour ignore files
//! from the merged workspace, apply the server's binary and size
//! classification, and enforce a local time and bytes-read budget. It then names
//! the failure mode plainly — `gfs search` must not become slower than the `rg`
//! invocation it replaces.
//!
//! This module is the shared implementation of those rules, so the local half
//! and the server half cannot drift. It takes content through a callback rather
//! than reading files itself, because "local content" means an overlay journal
//! row on the client and a `HashMap` in a test, and neither is a directory walk.
//!
//! # Ignore rules apply to created files, not to edited ones
//!
//! A file the base already tracks is not ignored, whatever `.gitignore` says —
//! Git's own rule, and the reason `git status` reports a modification to a
//! tracked file inside `target/`. So the matcher is consulted only for overlay
//! rows with no base entry. Getting this backwards would hide an agent's edits
//! to a tracked file the moment a broad ignore pattern existed, which is a
//! silent wrong answer of exactly the kind M4's exit gate forbids.
//!
//! # Why this does not fetch anything
//!
//! Nothing here reads the base. A local search examines local bytes only, and
//! the paths it examines are the ones the overlay journal already names. That is
//! what keeps `gfs search` at zero base hydration on a clean workspace: with an
//! empty journal there is nothing for this module to do at all.

use std::time::{Duration, Instant};

use gfs_types::error::GfsError;

use crate::classify::{classify_content, ContentClass, CorpusPolicy, ExclusionReason};
use crate::glob::Glob;
use crate::query::{Match, Query};

/// One local path offered to the search.
#[derive(Clone, Debug)]
pub struct LocalPath {
  pub path: Vec<u8>,
  /// Whether the pinned commit also has an entry here.
  ///
  /// Decides whether ignore rules apply; see the module docs.
  pub tracked_in_base: bool,
  pub size: u64,
}

/// Ignore rules, compiled once.
///
/// Sourced from the merged workspace's `.gitignore` files and from
/// `.git/info/exclude`. The synthesized `.git` surface (ADR 0005) has no
/// `info/exclude`, so on a GFS mount that source is empty by construction —
/// stated rather than left for a reader to discover, because "we honour
/// `info/exclude`" would otherwise read as a capability that does nothing.
#[derive(Clone, Debug, Default)]
pub struct IgnoreRules {
  rules: Vec<IgnoreRule>,
}

#[derive(Clone, Debug)]
struct IgnoreRule {
  glob: Glob,
  /// A `!` prefix: this pattern un-ignores what an earlier one ignored.
  negated: bool,
  /// The directory the file was found in, so a nested `.gitignore` only governs
  /// its own subtree.
  base: Vec<u8>,
}

impl IgnoreRules {
  /// Add the contents of one ignore file, found at `directory`.
  ///
  /// `directory` is a path from the workspace root, empty for the root's own
  /// `.gitignore`.
  pub fn add_file(&mut self, directory: &[u8], contents: &[u8]) {
    for line in contents.split(|b| *b == b'\n') {
      let line = trim(line);
      if line.is_empty() || line[0] == b'#' {
        continue;
      }
      let (negated, pattern) = match line[0] {
        b'!' => (true, &line[1..]),
        _ => (false, line),
      };
      if pattern.is_empty() {
        continue;
      }
      self.rules.push(IgnoreRule {
        glob: Glob::from_bytes(pattern),
        negated,
        base: directory.to_vec(),
      });
    }
  }

  pub fn is_empty(&self) -> bool {
    self.rules.is_empty()
  }

  /// Whether a path is ignored.
  ///
  /// Last matching rule wins, which is Git's precedence and the reason a
  /// `!keep.log` after `*.log` works.
  pub fn ignores(&self, path: &[u8]) -> bool {
    let mut ignored = false;
    for rule in &self.rules {
      let Some(relative) = strip_base(path, &rule.base) else {
        continue;
      };
      if rule.glob.matches(relative) {
        ignored = !rule.negated;
      }
    }
    ignored
  }
}

fn strip_base<'a>(path: &'a [u8], base: &[u8]) -> Option<&'a [u8]> {
  if base.is_empty() {
    return Some(path);
  }
  let with_sep: Vec<u8> = base.iter().copied().chain(std::iter::once(b'/')).collect();
  path.strip_prefix(with_sep.as_slice())
}

fn trim(line: &[u8]) -> &[u8] {
  let start = line.iter().position(|b| !b.is_ascii_whitespace());
  let end = line.iter().rposition(|b| !b.is_ascii_whitespace());
  match (start, end) {
    (Some(s), Some(e)) => &line[s..=e],
    _ => &[],
  }
}

/// How the local half is bounded.
#[derive(Clone, Copy, Debug)]
pub struct LocalBudget {
  pub max_time: Duration,
  pub max_bytes_read: u64,
  pub max_results: usize,
}

impl Default for LocalBudget {
  fn default() -> Self {
    LocalBudget {
      // Deliberately tighter than the server's. The server searches an index;
      // this scans, and PLAN.md M4.5's benchmark is "must not become slower than
      // the `rg` invocation it replaces". A budget that let a full build tree in
      // the overlay dominate the query would fail that outright.
      max_time: Duration::from_secs(5),
      max_bytes_read: 256 * 1024 * 1024,
      max_results: 1000,
    }
  }
}

/// What the local half found, and what it left out.
#[derive(Clone, Debug, Default)]
pub struct LocalOutcome {
  pub matches: Vec<Match>,
  /// Excluded paths, by reason, merged into the caller's coverage report.
  pub excluded: std::collections::BTreeMap<ExclusionReason, u64>,
  pub eligible_paths: u64,
  pub bytes_read: u64,
  /// Set when a budget stopped the scan. The caller must fold this into the
  /// execution status: a local half that stopped early makes the *whole* answer
  /// truncated, however complete the server's half was.
  pub truncated: bool,
}

/// Search local content.
///
/// `read` is called at most once per eligible path and may return `None` when
/// the content has since vanished — an unlink racing the search — which is
/// recorded as an index gap rather than treated as a miss.
pub fn search_local(
  paths: &[LocalPath],
  query: &Query,
  policy: &CorpusPolicy,
  ignore: &IgnoreRules,
  budget: &LocalBudget,
  search_ignored: bool,
  mut read: impl FnMut(&LocalPath) -> Result<Option<Vec<u8>>, GfsError>,
) -> Result<LocalOutcome, GfsError> {
  let started = Instant::now();
  let matcher = crate::query::compile(query)?;
  let mut out = LocalOutcome::default();
  // From the query's budget, not `LocalBudget`: both halves of a merged answer
  // must cut lines at the same width, or the same match found by both would come
  // back at two different lengths and a caller could not deduplicate it.
  let mut display = crate::query::DisplayBudget::new(&query.budget);

  // Sorted so the local half's own ordering is deterministic before it is merged
  // with the server's.
  let mut ordered: Vec<&LocalPath> = paths.iter().collect();
  ordered.sort_by(|a, b| a.path.cmp(&b.path));

  for entry in ordered {
    let path = entry.path.as_slice();
    if !within_scope(path, &query.scope) {
      continue;
    }
    if !query.include_globs.is_empty() && !query.include_globs.iter().any(|g| g.matches(path)) {
      continue;
    }
    if query.exclude_globs.iter().any(|g| g.matches(path)) {
      continue;
    }
    // Only a path the base does not track can be ignored. See the module docs.
    if !search_ignored && !entry.tracked_in_base && ignore.ignores(path) {
      // Not a coverage gap: an ignored file is outside the question the way an
      // unmatched glob is, and `rg` does not report it either. `--no-ignore`
      // brings it back.
      continue;
    }
    // Size first, so an oversized local file costs no read.
    if entry.size > policy.max_blob_bytes {
      *out.excluded.entry(ExclusionReason::Oversized).or_default() += 1;
      continue;
    }

    if started.elapsed() > budget.max_time
      || out.bytes_read > budget.max_bytes_read
      || display.exhausted()
    {
      out.truncated = true;
      break;
    }

    let Some(content) = read(entry)? else {
      // Unlinked under the search. The answer for this path is unknown, which is
      // not the same as "no match here".
      *out.excluded.entry(ExclusionReason::IndexGap).or_default() += 1;
      continue;
    };
    out.bytes_read += content.len() as u64;

    let class = classify_content(policy, &content);
    if let Some(reason) = policy.excludes_content(class) {
      *out.excluded.entry(reason).or_default() += 1;
      continue;
    }
    if class == ContentClass::Oversized {
      *out.excluded.entry(ExclusionReason::Oversized).or_default() += 1;
      continue;
    }
    out.eligible_paths += 1;

    // Only as many as the page can still take, plus the one that proves there
    // were more. Collecting every match first made the budget a bound on what
    // was *reported* rather than on what was built, and a single long line can
    // hold more matches than memory holds copies of it.
    let room = budget
      .max_results
      .saturating_sub(out.matches.len())
      .saturating_add(1);
    for hit in crate::query::find_matches(&matcher, &content, query, path, room, &mut display) {
      if out.matches.len() >= budget.max_results {
        out.truncated = true;
        return Ok(out);
      }
      out.matches.push(hit);
    }
  }

  // The loop-top check needs a next path to fire on. A budget spent on the last
  // one still cut a line short, and an answer that dropped bytes is not complete.
  if display.exhausted() {
    out.truncated = true;
  }

  Ok(out)
}

/// Whether a path is inside a scope prefix, at a path boundary.
pub fn within_scope(path: &[u8], scope: &[u8]) -> bool {
  if scope.is_empty() {
    return true;
  }
  if path == scope {
    return true;
  }
  let mut with_sep = scope.to_vec();
  if with_sep.last() != Some(&b'/') {
    with_sep.push(b'/');
  }
  path.starts_with(with_sep.as_slice())
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashMap;

  fn query(pattern: &str) -> Query {
    Query {
      pattern: pattern.to_owned(),
      literal: true,
      ..Query::default()
    }
  }

  fn run(
    files: &[(&str, &[u8], bool)],
    q: Query,
    ignore: &IgnoreRules,
    search_ignored: bool,
  ) -> LocalOutcome {
    let contents: HashMap<Vec<u8>, Vec<u8>> = files
      .iter()
      .map(|(p, c, _)| (p.as_bytes().to_vec(), c.to_vec()))
      .collect();
    let paths: Vec<LocalPath> = files
      .iter()
      .map(|(p, c, tracked)| LocalPath {
        path: p.as_bytes().to_vec(),
        tracked_in_base: *tracked,
        size: c.len() as u64,
      })
      .collect();
    search_local(
      &paths,
      &q,
      &CorpusPolicy::default(),
      ignore,
      &LocalBudget::default(),
      search_ignored,
      |entry| Ok(contents.get(&entry.path).cloned()),
    )
    .unwrap()
  }

  #[test]
  fn local_content_is_searched_without_touching_the_base() {
    let out = run(
      &[("src/new.rs", b"let needle = 1;\n", false)],
      query("needle"),
      &IgnoreRules::default(),
      false,
    );
    assert_eq!(out.matches.len(), 1);
    assert_eq!(out.matches[0].path, b"src/new.rs");
    assert_eq!(out.matches[0].line, 1);
    assert_eq!(out.matches[0].column, 5);
  }

  #[test]
  fn an_ignored_created_file_is_skipped_but_a_tracked_one_is_not() {
    // Git's rule, and the failure it prevents: an agent's edit to a tracked file
    // must not disappear because a broad ignore pattern exists.
    let mut ignore = IgnoreRules::default();
    ignore.add_file(b"", b"target/\n");

    let out = run(
      &[
        ("target/build.rs", b"needle\n", false),
        ("target/tracked.rs", b"needle\n", true),
      ],
      query("needle"),
      &ignore,
      false,
    );
    let paths: Vec<&[u8]> = out.matches.iter().map(|m| m.path.as_slice()).collect();
    assert_eq!(paths, vec![&b"target/tracked.rs"[..]]);
  }

  #[test]
  fn searching_ignored_files_is_available_explicitly() {
    let mut ignore = IgnoreRules::default();
    ignore.add_file(b"", b"target/\n");
    let out = run(
      &[("target/build.rs", b"needle\n", false)],
      query("needle"),
      &ignore,
      true,
    );
    assert_eq!(out.matches.len(), 1);
  }

  #[test]
  fn a_negated_ignore_pattern_wins_when_it_comes_last() {
    let mut ignore = IgnoreRules::default();
    ignore.add_file(b"", b"*.log\n!keep.log\n");
    assert!(ignore.ignores(b"debug.log"));
    assert!(!ignore.ignores(b"keep.log"));
  }

  #[test]
  fn a_nested_ignore_file_governs_only_its_own_subtree() {
    let mut ignore = IgnoreRules::default();
    ignore.add_file(b"web", b"dist/\n");
    assert!(ignore.ignores(b"web/dist/app.js"));
    assert!(!ignore.ignores(b"server/dist/app.js"));
  }

  #[test]
  fn comments_and_blank_lines_are_not_patterns() {
    let mut ignore = IgnoreRules::default();
    ignore.add_file(b"", b"# a comment\n\n  \n*.tmp\n");
    assert!(ignore.ignores(b"x.tmp"));
    assert!(!ignore.ignores(b"# a comment"));
  }

  #[test]
  fn a_local_binary_file_is_a_reported_exclusion() {
    let out = run(
      &[("a.bin", b"\0\0needle", false)],
      query("needle"),
      &IgnoreRules::default(),
      false,
    );
    assert!(out.matches.is_empty());
    assert_eq!(out.excluded[&ExclusionReason::Binary], 1);
  }

  #[test]
  fn an_oversized_local_file_is_excluded_without_being_read() {
    let policy = CorpusPolicy::default();
    let paths = vec![LocalPath {
      path: b"huge.bin".to_vec(),
      tracked_in_base: false,
      size: policy.max_blob_bytes + 1,
    }];
    let out = search_local(
      &paths,
      &query("needle"),
      &policy,
      &IgnoreRules::default(),
      &LocalBudget::default(),
      false,
      |_| panic!("an oversized file must not be read"),
    )
    .unwrap();
    assert_eq!(out.excluded[&ExclusionReason::Oversized], 1);
  }

  #[test]
  fn a_file_unlinked_under_the_search_is_an_index_gap_not_a_miss() {
    let paths = vec![LocalPath {
      path: b"gone.rs".to_vec(),
      tracked_in_base: false,
      size: 10,
    }];
    let out = search_local(
      &paths,
      &query("needle"),
      &CorpusPolicy::default(),
      &IgnoreRules::default(),
      &LocalBudget::default(),
      false,
      |_| Ok(None),
    )
    .unwrap();
    assert!(out.matches.is_empty());
    assert_eq!(out.excluded[&ExclusionReason::IndexGap], 1);
  }

  #[test]
  fn a_result_budget_truncates_and_says_so() {
    let paths: Vec<LocalPath> = (0..10)
      .map(|i| LocalPath {
        path: format!("f{i}.rs").into_bytes(),
        tracked_in_base: false,
        size: 8,
      })
      .collect();
    let out = search_local(
      &paths,
      &query("needle"),
      &CorpusPolicy::default(),
      &IgnoreRules::default(),
      &LocalBudget {
        max_results: 3,
        ..LocalBudget::default()
      },
      false,
      |_| Ok(Some(b"needle\n".to_vec())),
    )
    .unwrap();
    assert_eq!(out.matches.len(), 3);
    assert!(
      out.truncated,
      "a local half that stopped early makes the whole answer truncated"
    );
  }

  #[test]
  fn the_scope_applies_to_local_paths_too() {
    let out = run(
      &[
        ("src/a.rs", b"needle\n", false),
        ("docs/b.md", b"needle\n", false),
      ],
      Query {
        scope: b"src".to_vec(),
        ..query("needle")
      },
      &IgnoreRules::default(),
      false,
    );
    assert_eq!(out.matches.len(), 1);
    assert_eq!(out.matches[0].path, b"src/a.rs");
  }

  #[test]
  fn scope_matching_stops_at_a_path_boundary() {
    assert!(within_scope(b"src/main.rs", b"src"));
    assert!(within_scope(b"src", b"src"));
    assert!(!within_scope(b"srcutil/main.rs", b"src"));
    assert!(within_scope(b"anything", b""));
  }

  #[test]
  fn local_results_are_ordered_by_path() {
    let out = run(
      &[
        ("z.rs", b"needle\n", false),
        ("a.rs", b"needle\n", false),
        ("m.rs", b"needle\n", false),
      ],
      query("needle"),
      &IgnoreRules::default(),
      false,
    );
    let paths: Vec<&[u8]> = out.matches.iter().map(|m| m.path.as_slice()).collect();
    assert_eq!(paths, vec![&b"a.rs"[..], &b"m.rs"[..], &b"z.rs"[..]]);
  }
}
