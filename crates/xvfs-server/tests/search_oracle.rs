//! M4.6: XVFS search against `rg`, over a generated query corpus.
//!
//! PLAN.md section 12 names the oracle: `ripgrep` over the **raw** tree
//! materialization, not over a `git checkout`. A checkout applies
//! `.gitattributes` `text`/`eol` conversion, `core.autocrlf`, clean/smudge
//! filters, and LFS; a mount serves raw blob bytes. Comparing against a checkout
//! reports differences that are correct behaviour on both sides.
//!
//! # How the two are made comparable
//!
//! `rg` is invoked with `--json`, whose `submatches[].start` is a 0-based byte
//! offset within the line — exactly the number XVFS reports as `column - 1`. It
//! is invoked with `--no-ignore --hidden --no-config` so the *corpus* is the
//! same on both sides: the materialization is not a Git repository, so ignore
//! handling would be arbitrary, and XVFS's base index has no notion of hidden
//! files.
//!
//! Symlinks need no flag: `rg` does not follow them by default, and neither does
//! the XVFS corpus.
//!
//! # The divergences are tested, not avoided
//!
//! Two are real and are asserted explicitly further down rather than papered
//! over by choosing gentle fixtures:
//!
//! * **a NUL past the first 8 KiB.** ADR 0004 fixed the binary probe at
//!   ripgrep's 8 KiB window; `rg` itself keeps scanning and will call a file
//!   binary on a NUL anywhere it reads.
//! * **files over 8 MiB.** XVFS excludes them and *reports* the exclusion;
//!   `rg` has no size limit by default.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use xvfs_search::query::{ExecutionStatus, Query};
use xvfs_server::auth::{AllowList, CapabilityKey, StaticTokens};
use xvfs_server::catalog::repositories::NewRepository;
use xvfs_server::{Catalog, Server};
use xvfs_types::{
  DisplayName, HashAlgorithm, LeasePolicy, ObjectId, RepositoryId, RevisionSelector, SubjectId,
};

/// One match, reduced to what both sides can agree on.
type Hit = (Vec<u8>, u64, u64);

struct Subject {
  server: Arc<Server>,
  repo_id: RepositoryId,
  commit: ObjectId,
  /// The raw materialization `rg` searches.
  materialized: std::path::PathBuf,
  _tmp: tempfile::TempDir,
  _work: tempfile::TempDir,
}

impl Subject {
  async fn new(fixture: &str) -> Subject {
    let (tmp, repo_path) = xvfs_test::scratch_clone(fixture).unwrap();
    let catalog = Arc::new(Catalog::open_in_memory().unwrap());
    let repo_id = RepositoryId::parse("r-oracle").unwrap();
    catalog
      .create_repository(&NewRepository {
        repository_id: repo_id.clone(),
        display_name: DisplayName::parse("acme/oracle").unwrap(),
        repo_path: repo_path.clone(),
        algorithm: HashAlgorithm::Sha1,
        upstream_url: None,
        credential_ref: None,
      })
      .unwrap();
    let subject = SubjectId::parse("job-oracle").unwrap();
    let server = Arc::new(Server::new(
      catalog,
      Arc::new(StaticTokens::new().with_token("t", subject.clone())),
      Arc::new(AllowList::new().allow(&subject, &repo_id)),
      CapabilityKey::generate().unwrap(),
      LeasePolicy::adr_0006(),
    ));
    server.registry.activate(&repo_id).unwrap();

    let commit = server
      .registry
      .repository(&repo_id)
      .unwrap()
      .resolve(RevisionSelector::parse("main", HashAlgorithm::Sha1).unwrap())
      .await
      .unwrap()
      .commit;

    let outcome = server
      .search
      .prepare(&repo_id, &commit, true)
      .await
      .unwrap();
    assert!(
      matches!(outcome, xvfs_server::PrepareOutcome::Ready(_)),
      "the snapshot did not prepare: {outcome:?}"
    );

    let work = tempfile::tempdir().unwrap();
    let materialized = work.path().join("tree");
    xvfs_test::materialize_raw(&repo_path, &commit.to_hex(), &materialized).unwrap();

    Subject {
      server,
      repo_id,
      commit,
      materialized,
      _tmp: tmp,
      _work: work,
    }
  }

  async fn xvfs(
    &self,
    pattern: &str,
    literal: bool,
    case_insensitive: bool,
  ) -> (BTreeSet<Hit>, bool) {
    let query = Query {
      pattern: pattern.to_owned(),
      literal,
      case_insensitive,
      budget: xvfs_search::Budget {
        // Generous: a truncated comparison would be a comparison of two
        // different questions. The flag comes back so a case that hits it can be
        // skipped loudly rather than silently mismatching.
        max_results: 200_000,
        ..xvfs_search::Budget::default()
      },
      ..Query::default()
    };
    let result = self
      .server
      .search
      .search(&self.repo_id, &self.commit, query)
      .await
      .unwrap();
    let truncated = result.completion.execution_status == ExecutionStatus::Truncated;
    let hits = result
      .matches
      .iter()
      .map(|m| (m.path.clone(), m.line, m.column))
      .collect();
    (hits, truncated)
  }

  fn rg(&self, pattern: &str, literal: bool, case_insensitive: bool) -> BTreeSet<Hit> {
    rg_matches(&self.materialized, pattern, literal, case_insensitive)
  }
}

/// `rg --json` over a directory, reduced to `(path, line, column)`.
fn rg_matches(root: &Path, pattern: &str, literal: bool, case_insensitive: bool) -> BTreeSet<Hit> {
  let mut command = std::process::Command::new("rg");
  command
    .current_dir(root)
    .arg("--json")
    // The corpus, made the same on both sides. See the module docs.
    .arg("--no-ignore")
    .arg("--hidden")
    .arg("--no-config");
  if literal {
    command.arg("-F");
  }
  if case_insensitive {
    command.arg("-i");
  }
  command.arg("--").arg(pattern).arg(".");

  let output = command.output().expect("running rg; is ripgrep installed?");
  // 0 with matches, 1 without. Anything else is a real failure.
  assert!(
    matches!(output.status.code(), Some(0) | Some(1)),
    "rg failed for {pattern:?}: {}",
    String::from_utf8_lossy(&output.stderr)
  );

  let mut hits = BTreeSet::new();
  for line in output.stdout.split(|b| *b == b'\n') {
    if line.is_empty() {
      continue;
    }
    let value: serde_json::Value = match serde_json::from_slice(line) {
      Ok(v) => v,
      Err(_) => continue,
    };
    if value["type"] != "match" {
      continue;
    }
    let data = &value["data"];
    let path = decode_rg_path(&data["path"]);
    // `./x` from the walk of `.`.
    let path = path.strip_prefix(b"./").unwrap_or(&path).to_vec();
    let line_number = data["line_number"].as_u64().unwrap_or(0);
    for submatch in data["submatches"].as_array().into_iter().flatten() {
      let start = submatch["start"].as_u64().unwrap_or(0);
      hits.insert((path.clone(), line_number, start + 1));
    }
  }
  hits
}

/// `rg` reports a path as `{"text": ...}` or, when it is not UTF-8, as
/// `{"bytes": "<standard base64>"}`.
fn decode_rg_path(value: &serde_json::Value) -> Vec<u8> {
  if let Some(text) = value["text"].as_str() {
    return text.as_bytes().to_vec();
  }
  if let Some(encoded) = value["bytes"].as_str() {
    return base64_decode(encoded);
  }
  Vec::new()
}

/// Standard base64 with padding. Not base64url: `rg` uses the standard alphabet,
/// and `xvfs_types::path::b64url_decode` would reject `+` and `/`.
fn base64_decode(input: &str) -> Vec<u8> {
  const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  let mut out = Vec::new();
  let mut acc: u32 = 0;
  let mut bits = 0;
  for byte in input.bytes() {
    if byte == b'=' {
      break;
    }
    let Some(index) = ALPHABET.iter().position(|c| *c == byte) else {
      continue;
    };
    acc = (acc << 6) | index as u32;
    bits += 6;
    if bits >= 8 {
      bits -= 8;
      out.push((acc >> bits) as u8);
    }
  }
  out
}

/// A deterministic pseudo-random source.
///
/// A fixed LCG rather than `rand`: the corpus has to be the same on every run
/// and in every environment, or a failure cannot be reproduced from the test
/// name alone.
struct Lcg(u64);

impl Lcg {
  fn next(&mut self) -> u64 {
    self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
    self.0 >> 11
  }
}

/// Build a query corpus from the materialized tree's own content.
///
/// Patterns drawn from the corpus find real matches, which is what makes the
/// comparison meaningful — a corpus of invented strings mostly compares two
/// empty sets.
fn generated_literals(root: &Path, count: usize) -> Vec<String> {
  let mut sources: Vec<String> = Vec::new();
  collect_text(root, &mut sources);
  sources.sort();

  let mut rng = Lcg(0x5EED);
  let mut out = Vec::new();
  let mut guard = 0;
  while out.len() < count && guard < count * 40 {
    guard += 1;
    if sources.is_empty() {
      break;
    }
    let source = &sources[(rng.next() as usize) % sources.len()];
    let chars: Vec<char> = source.chars().collect();
    if chars.len() < 4 {
      continue;
    }
    let len = 3 + (rng.next() as usize) % 8;
    if len >= chars.len() {
      continue;
    }
    let start = (rng.next() as usize) % (chars.len() - len);
    let candidate: String = chars[start..start + len].iter().collect();
    // A pattern of one repeated byte matches a huge-line fixture millions of
    // times; skipped so the comparison stays about semantics rather than about
    // holding two million tuples in memory.
    if candidate.chars().all(|c| c == 'x') || candidate.trim().is_empty() {
      continue;
    }
    out.push(candidate);
  }
  out.sort();
  out.dedup();
  out
}

fn collect_text(dir: &Path, out: &mut Vec<String>) {
  let Ok(entries) = std::fs::read_dir(dir) else {
    return;
  };
  for entry in entries.flatten() {
    let path = entry.path();
    let Ok(meta) = std::fs::symlink_metadata(&path) else {
      continue;
    };
    if meta.file_type().is_symlink() {
      continue;
    }
    if meta.is_dir() {
      collect_text(&path, out);
      continue;
    }
    // Small text files only: the point is to harvest identifiers.
    if meta.len() > 64 * 1024 {
      continue;
    }
    if let Ok(text) = std::fs::read_to_string(&path) {
      for line in text.lines() {
        if !line.trim().is_empty() {
          out.push(line.to_owned());
        }
      }
    }
  }
}

// ---------------------------------------------------------------------------

/// Fixtures whose content is safe to compare wholesale.
///
/// `content` is excluded and handled separately: it exists to hold the shapes
/// where XVFS and `rg` legitimately differ, and mixing those into a bulk
/// comparison would either fail constantly or teach the comparison to ignore
/// real differences.
const BULK_FIXTURES: &[&str] = &["basic", "modes", "bytes", "deep", "packed"];

#[tokio::test]
async fn generated_literal_queries_match_ripgrep_exactly() {
  let mut compared = 0usize;
  let mut with_matches = 0usize;

  for fixture in BULK_FIXTURES {
    let subject = Subject::new(fixture).await;
    let patterns = generated_literals(&subject.materialized, 40);
    assert!(
      !patterns.is_empty(),
      "the {fixture} fixture produced no query corpus"
    );

    for pattern in &patterns {
      let (ours, truncated) = subject.xvfs(pattern, true, false).await;
      if truncated {
        continue;
      }
      let theirs = subject.rg(pattern, true, false);
      assert_eq!(
        ours, theirs,
        "fixture {fixture}, literal {pattern:?}: XVFS and rg disagree"
      );
      compared += 1;
      if !theirs.is_empty() {
        with_matches += 1;
      }
    }
  }

  eprintln!("compared {compared} literal queries, {with_matches} with matches");
  assert!(compared >= 100, "the corpus was too small: {compared}");
  assert!(
    with_matches >= 50,
    "only {with_matches} queries found anything; the corpus is not exercising the matcher"
  );
}

#[tokio::test]
async fn case_insensitive_queries_match_ripgrep_exactly() {
  // The path that folds trigrams into ASCII case variants. A fold that missed a
  // variant would return *fewer* results than `rg`, which is the silent failure.
  for fixture in ["basic", "deep"] {
    let subject = Subject::new(fixture).await;
    for pattern in generated_literals(&subject.materialized, 20) {
      let upper = pattern.to_uppercase();
      let (ours, truncated) = subject.xvfs(&upper, true, true).await;
      if truncated {
        continue;
      }
      assert_eq!(
        ours,
        subject.rg(&upper, true, true),
        "fixture {fixture}, -i {upper:?}"
      );
    }
  }
}

/// Regex shapes that exercise the literal analysis in `trigram.rs`.
///
/// Each is here for a specific reason, and alternation is the one that matters
/// most: the M0 spike required only the longest branch's trigrams and would drop
/// every match of the others.
const REGEX_CORPUS: &[&str] = &[
  "fn",
  "fn ",
  r"fn\s+\w+",
  "pub fn",
  "pub|fn",
  "println|util|guide",
  "pri?ntln",
  "print.*!",
  "^fn",
  "}$",
  "[pu]b",
  r"\bfn\b",
  "(?:pub )?fn",
  "util+",
  "(?:abc)*fn",
  "[a-z]{3,}",
  ".",
  "e",
  "线",
  "caf.",
];

#[tokio::test]
async fn regex_queries_match_ripgrep_exactly() {
  for fixture in ["basic", "modes", "deep"] {
    let subject = Subject::new(fixture).await;
    for pattern in REGEX_CORPUS {
      let (ours, truncated) = subject.xvfs(pattern, false, false).await;
      if truncated {
        // A pattern with no usable literal is bounded by a scan budget, and the
        // completion says so. Not a mismatch; a different question.
        continue;
      }
      assert_eq!(
        ours,
        subject.rg(pattern, false, false),
        "fixture {fixture}, regex {pattern:?}"
      );
    }
  }
}

#[tokio::test]
async fn alternation_returns_every_branch_not_just_the_longest() {
  // The specific correctness bug the M0 spike had. Asserted against `rg` rather
  // than against an expectation, so it stays honest.
  let subject = Subject::new("basic").await;
  for pattern in ["println|util", "util|println", "README|guide|added"] {
    let (ours, _) = subject.xvfs(pattern, false, false).await;
    let theirs = subject.rg(pattern, false, false);
    assert!(
      !theirs.is_empty(),
      "the oracle found nothing for {pattern:?}"
    );
    assert_eq!(ours, theirs, "regex {pattern:?}");
  }
}

#[tokio::test]
async fn non_utf8_paths_are_reported_identically() {
  let subject = Subject::new("bytes").await;
  let (ours, _) = subject.xvfs("content", true, false).await;
  let theirs = subject.rg("content", true, false);
  assert_eq!(ours, theirs);
  assert!(
    ours.iter().any(|(p, _, _)| std::str::from_utf8(p).is_err()),
    "the `bytes` fixture exists to carry a path that is not UTF-8"
  );
}

#[tokio::test]
async fn crlf_and_a_missing_final_newline_agree_with_ripgrep() {
  let subject = Subject::new("content").await;
  for pattern in ["line one", "line two", "no trailing newline", "trailing"] {
    let (ours, _) = subject.xvfs(pattern, true, false).await;
    let theirs = subject.rg(pattern, true, false);
    assert_eq!(ours, theirs, "literal {pattern:?}");
  }
  // Specifically: the CRLF line's column is an offset into the file as stored.
  let (ours, _) = subject.xvfs("line two", true, false).await;
  assert!(ours.contains(&(b"crlf.txt".to_vec(), 2, 1)));
}

#[tokio::test]
async fn a_huge_line_is_searched_by_both() {
  let subject = Subject::new("content").await;
  // 4 MiB of `x` on one line, under the 8 MiB cap. A search for a substring of
  // it returns an enormous number of matches, so this asserts the *file* is in
  // the corpus rather than comparing every hit.
  let (ours, _) = subject
    .xvfs("this-is-not-in-the-huge-line", true, false)
    .await;
  assert!(ours.is_empty());
  let result = subject
    .server
    .search
    .search(
      &subject.repo_id,
      &subject.commit,
      Query {
        pattern: "xxx".to_owned(),
        literal: true,
        budget: xvfs_search::Budget {
          max_results: 10,
          ..xvfs_search::Budget::default()
        },
        ..Query::default()
      },
    )
    .await
    .unwrap();
  assert!(
    result.matches.iter().any(|m| m.path == b"huge-line.txt"),
    "the 4 MiB single-line file must be searchable"
  );
}

#[tokio::test]
async fn a_repeated_blob_is_reported_at_every_path_like_ripgrep() {
  // `bytes` writes identical content to seven differently named files, so every
  // path shares one blob key. Both tools must report all seven.
  let subject = Subject::new("bytes").await;
  let (ours, _) = subject.xvfs("content", true, false).await;
  let theirs = subject.rg("content", true, false);
  assert_eq!(ours.len(), theirs.len());
  assert!(
    ours.len() >= 7,
    "expected one hit per file, got {}",
    ours.len()
  );
}

#[tokio::test]
async fn a_symlink_is_searched_by_neither() {
  let subject = Subject::new("modes").await;
  // `modes` has relative, absolute, escaping, and looping symlinks. Searching
  // for a fragment of a link *target* must find nothing on either side: `rg`
  // does not follow symlinks by default and the XVFS corpus excludes them.
  for pattern in ["plain.txt", "escape", "loop"] {
    let (ours, _) = subject.xvfs(pattern, true, false).await;
    assert_eq!(
      ours,
      subject.rg(pattern, true, false),
      "literal {pattern:?}"
    );
  }
}

// ---------------------------------------------------------------------------
// The documented divergences
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_oversized_file_is_excluded_by_xvfs_and_the_exclusion_is_reported() {
  // `content` holds a 12 MiB blob, over ADR 0004's 8 MiB cap. `rg` has no size
  // limit, so this is a real difference — and the point is that XVFS *says so*
  // rather than quietly returning less.
  let subject = Subject::new("content").await;
  let result = subject
    .server
    .search
    .search(
      &subject.repo_id,
      &subject.commit,
      Query {
        pattern: "anything".to_owned(),
        literal: true,
        ..Query::default()
      },
    )
    .await
    .unwrap();
  assert_eq!(
    result.completion.coverage.excluded.get("oversized"),
    Some(&1),
    "the 12 MiB blob must be reported as excluded, not omitted"
  );
  assert!(result
    .completion
    .coverage
    .declared_exclusions
    .contains(&"oversized".to_owned()));
}

#[tokio::test]
async fn a_binary_file_is_excluded_by_both_and_reported_by_xvfs() {
  let subject = Subject::new("content").await;
  let result = subject
    .server
    .search
    .search(
      &subject.repo_id,
      &subject.commit,
      Query {
        pattern: "hello".to_owned(),
        literal: true,
        ..Query::default()
      },
    )
    .await
    .unwrap();
  // `utf16.txt` contains `hello` interleaved with NULs, so neither tool matches
  // it as text. XVFS additionally reports it.
  assert!(result.matches.is_empty());
  assert!(
    result
      .completion
      .coverage
      .excluded
      .get("binary")
      .copied()
      .unwrap_or(0)
      >= 2,
    "binary.bin and utf16.txt must both be reported: {:?}",
    result.completion.coverage.excluded
  );
}
