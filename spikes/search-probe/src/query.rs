//! Query execution and the two-dimensional completion contract.
//!
//! DESIGN.md section 7.5 is emphatic that silence must never be ambiguous: an
//! agent that gets an empty result concludes the symbol does not exist. So a
//! result carries two independent things — whether the query finished evaluating
//! the searchable corpus (execution status), and what was outside that corpus
//! within the requested scope (coverage). A budget-truncated query and a clean
//! empty result must never look alike.

use crate::index::{BlobRegistry, Exclusion, Manifest, TrigramIndex};
use roaring::RoaringBitmap;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExecutionStatus {
  Complete,
  Truncated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncationReason {
  ResultLimit,
  TimeBudget,
  BytesBudget,
  /// The pattern had no usable literal, so no posting list could bound it.
  NoRequiredLiteral,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Coverage {
  /// Paths in the requested scope that were eligible to be searched.
  pub eligible_paths: usize,
  /// Paths in scope excluded from the corpus, grouped by reason.
  pub excluded_by_reason: BTreeMap<String, usize>,
  pub requested_scope: String,
}

impl Coverage {
  pub fn has_gaps(&self) -> bool {
    self.excluded_by_reason.values().any(|n| *n > 0)
  }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Completion {
  pub execution_status: ExecutionStatus,
  pub truncation: Option<TruncationReason>,
  pub coverage: Coverage,
  pub index_generation: u64,
  pub commit: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Match {
  pub path: String,
  pub line: u64,
  pub column: u64,
  pub snippet: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchResult {
  pub matches: Vec<Match>,
  /// Always present on a successful search. Its absence is itself an error;
  /// see `SearchOutcome`.
  pub completion: Completion,
  pub candidates_considered: u64,
  pub bytes_read: u64,
  pub elapsed_ms: f64,
}

/// What a caller may observe. Modelled as an enum so that "the stream ended
/// without a completion message" is representable and therefore testable —
/// making it the failure the design says it is, rather than an empty result.
pub enum SearchOutcome {
  Completed(SearchResult),
  /// EOF, transport reset, or backend failure before the terminal message.
  ///
  /// Never constructed by this probe's happy path, and deliberately kept: the
  /// design says a missing terminal message is an error, and a type that
  /// cannot express that state would let the rule lapse silently. Exercised
  /// in the tests below.
  #[allow(dead_code)]
  FailedBeforeCompletion(String),
}

pub struct Budget {
  pub max_results: usize,
  pub max_time: Duration,
  pub max_bytes_read: u64,
}

impl Default for Budget {
  fn default() -> Self {
    Budget {
      max_results: 1000,
      max_time: Duration::from_secs(10),
      max_bytes_read: 512 * 1024 * 1024,
    }
  }
}

/// Extract the trigrams a match must contain.
///
/// Only literal runs count. A pattern whose literals are all shorter than three
/// bytes gives no trigrams, which is not a failure — it is the case the caller
/// must handle by scanning under a budget, and it is reported as such rather
/// than quietly returning nothing.
pub fn required_trigrams(pattern: &str, literal: bool) -> Vec<u32> {
  let runs: Vec<Vec<u8>> = if literal {
    vec![pattern.as_bytes().to_vec()]
  } else {
    literal_runs(pattern)
  };
  // The longest literal run bounds the candidate set best; using every run's
  // trigrams would be wrong, because alternation means a match need not
  // contain all of them.
  match runs.into_iter().max_by_key(|r| r.len()) {
    Some(run) if run.len() >= 3 => crate::index::trigrams(&run),
    _ => Vec::new(),
  }
}

/// Split a regex into the literal byte runs that any match must contain.
///
/// Conservative on purpose: anything that could introduce optionality or
/// alternation ends the current run. Being too conservative costs candidate
/// scanning; being too aggressive drops real matches, which is unacceptable.
fn literal_runs(pattern: &str) -> Vec<Vec<u8>> {
  let mut runs = Vec::new();
  let mut cur: Vec<u8> = Vec::new();
  let bytes = pattern.as_bytes();
  let mut i = 0;
  while i < bytes.len() {
    let c = bytes[i];
    match c {
      b'\\' if i + 1 < bytes.len() => {
        // An escaped metacharacter is a literal; an escape class is not.
        let n = bytes[i + 1];
        if n.is_ascii_alphanumeric() {
          runs.push(std::mem::take(&mut cur));
        } else {
          cur.push(n);
        }
        i += 2;
        continue;
      }
      // Quantifiers make the *previous* byte optional, so that byte must
      // leave the run too.
      b'?' | b'*' => {
        cur.pop();
        runs.push(std::mem::take(&mut cur));
      }
      b'+' => {
        // The previous byte is still required at least once.
        runs.push(std::mem::take(&mut cur));
      }
      b'|' | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'.' | b'^' | b'$' => {
        runs.push(std::mem::take(&mut cur));
      }
      _ => cur.push(c),
    }
    i += 1;
  }
  runs.push(cur);
  runs.retain(|r| !r.is_empty());
  runs
}

pub struct SearchInput<'a> {
  pub pattern: &'a str,
  pub literal: bool,
  pub path_prefix: Option<&'a [u8]>,
  pub budget: Budget,
  pub index_generation: u64,
}

/// Run a query against one snapshot.
pub fn search(
  repo: &git2::Repository,
  manifest: &Manifest,
  registry: &BlobRegistry,
  index: &TrigramIndex,
  input: &SearchInput,
) -> anyhow::Result<SearchOutcome> {
  let start = Instant::now();
  let re = if input.literal {
    regex::bytes::RegexBuilder::new(&regex::escape(input.pattern)).build()?
  } else {
    regex::bytes::RegexBuilder::new(input.pattern)
      // A bounded automaton, so a pathological pattern costs memory it was
      // granted rather than memory it was not.
      .size_limit(16 * 1024 * 1024)
      .build()?
  };

  // Scope first: coverage is reported within the requested path scope, not
  // across the whole repository, so an unrelated binary elsewhere does not
  // make every query look incomplete.
  let in_scope: Vec<&(Vec<u8>, u32, u32)> = manifest
    .paths
    .iter()
    .filter(|(p, _, _)| match input.path_prefix {
      Some(prefix) => p.starts_with(prefix),
      None => true,
    })
    .collect();

  let mut excluded_by_reason: BTreeMap<String, usize> = BTreeMap::new();
  let mut eligible: Vec<&(Vec<u8>, u32, u32)> = Vec::with_capacity(in_scope.len());
  for e in &in_scope {
    match registry.excluded.get(&e.2) {
      Some(reason) => {
        let name = match reason {
          Exclusion::Binary => "binary",
          Exclusion::Oversized => "oversized",
          Exclusion::InvalidUtf8 => "invalid_utf8",
        };
        *excluded_by_reason.entry(name.to_string()).or_default() += 1;
      }
      None => eligible.push(e),
    }
  }

  let coverage = Coverage {
    eligible_paths: eligible.len(),
    excluded_by_reason,
    requested_scope: input
      .path_prefix
      .map(|p| String::from_utf8_lossy(p).into_owned())
      .unwrap_or_else(|| "/".into()),
  };

  // Candidate reduction. This is where the representation earns its keep: the
  // posting intersection happens against the snapshot bitmap before a single
  // blob is inflated.
  let required = required_trigrams(input.pattern, input.literal);
  let candidates: Option<RoaringBitmap> = index.candidates(&required, &manifest.members);

  let mut truncation = None;
  if candidates.is_none() {
    // No usable literal: the query would have to scan the whole eligible
    // corpus. Recorded as a truncation reason rather than silently scanning,
    // because the caller must know the answer was budget-bounded.
    truncation = Some(TruncationReason::NoRequiredLiteral);
  }

  let mut matches = Vec::new();
  let mut bytes_read = 0u64;
  let mut considered = 0u64;
  let odb = repo.odb()?;

  'outer: for (path, _mode, key) in eligible.iter() {
    if let Some(c) = &candidates {
      if !c.contains(*key) {
        continue;
      }
    }
    considered += 1;

    if start.elapsed() > input.budget.max_time {
      truncation = Some(TruncationReason::TimeBudget);
      break;
    }
    if bytes_read > input.budget.max_bytes_read {
      truncation = Some(TruncationReason::BytesBudget);
      break;
    }

    let oid = git2::Oid::from_bytes(&registry.oid_by_key[*key as usize])?;
    let blob = odb.read(oid)?;
    let content = blob.data();
    bytes_read += content.len() as u64;

    // Verification against the real bytes. The trigram stage only proposes;
    // exact line, column, and snippet come from the content.
    // One result per *occurrence*, not per line. `rg --count-matches` counts
    // every match on a line, and a line-oriented count silently under-reports
    // exactly the patterns agents use most (a brace, a quote, a common
    // identifier appearing twice in one call).
    let mut line_no = 1u64;
    let mut line_start = 0usize;
    let emit = |line: &[u8], line_no: u64, matches: &mut Vec<Match>| -> bool {
      for m in re.find_iter(line) {
        matches.push(Match {
          path: String::from_utf8_lossy(path).into_owned(),
          line: line_no,
          column: m.start() as u64 + 1,
          snippet: String::from_utf8_lossy(&line[..line.len().min(200)]).into_owned(),
        });
        if matches.len() >= input.budget.max_results {
          return true;
        }
      }
      false
    };
    for i in memchr::memchr_iter(b'\n', content) {
      if emit(&content[line_start..i], line_no, &mut matches) {
        truncation = Some(TruncationReason::ResultLimit);
        break 'outer;
      }
      line_no += 1;
      line_start = i + 1;
    }
    // A final line without a trailing newline still counts.
    if line_start < content.len() && emit(&content[line_start..], line_no, &mut matches) {
      truncation = Some(TruncationReason::ResultLimit);
      break;
    }
  }

  // `NoRequiredLiteral` alone means the scan completed but was unbounded by
  // the index; that is still an execution caveat, not a clean COMPLETE.
  let execution_status = if truncation.is_some() {
    ExecutionStatus::Truncated
  } else {
    ExecutionStatus::Complete
  };

  Ok(SearchOutcome::Completed(SearchResult {
    matches,
    completion: Completion {
      execution_status,
      truncation,
      coverage,
      index_generation: input.index_generation,
      commit: manifest.commit.clone(),
    },
    candidates_considered: considered,
    bytes_read,
    elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
  }))
}

/// The exit code an agent-facing CLI must use, derived only from the outcome.
///
/// Encoded as a function so the contract is testable: no caller gets to invent
/// its own mapping, and "truncated" can never collapse into "success".
pub fn exit_code(outcome: &SearchOutcome, require_exhaustive: bool) -> i32 {
  match outcome {
    // Transport loss or a missing terminal message is always a failure.
    SearchOutcome::FailedBeforeCompletion(_) => 2,
    SearchOutcome::Completed(r) => {
      if r.completion.execution_status == ExecutionStatus::Truncated {
        return 3;
      }
      if require_exhaustive && r.completion.coverage.has_gaps() {
        return 4;
      }
      // 0 with matches, 1 without: ripgrep's convention.
      if r.matches.is_empty() {
        1
      } else {
        0
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn result(status: ExecutionStatus, gaps: bool, matches: usize) -> SearchOutcome {
    let mut excluded = BTreeMap::new();
    if gaps {
      excluded.insert("binary".to_string(), 3);
    }
    SearchOutcome::Completed(SearchResult {
      matches: (0..matches)
        .map(|_| Match {
          path: "a".into(),
          line: 1,
          column: 1,
          snippet: String::new(),
        })
        .collect(),
      completion: Completion {
        execution_status: status,
        truncation: (status == ExecutionStatus::Truncated).then_some(TruncationReason::ResultLimit),
        coverage: Coverage {
          eligible_paths: 10,
          excluded_by_reason: excluded,
          requested_scope: "/".into(),
        },
        index_generation: 1,
        commit: "abc".into(),
      },
      candidates_considered: 0,
      bytes_read: 0,
      elapsed_ms: 0.0,
    })
  }

  #[test]
  fn truncation_is_never_reported_as_a_clean_empty_result() {
    // The failure this whole contract exists to prevent.
    let empty_complete = result(ExecutionStatus::Complete, false, 0);
    let empty_truncated = result(ExecutionStatus::Truncated, false, 0);
    assert_eq!(exit_code(&empty_complete, false), 1);
    assert_eq!(exit_code(&empty_truncated, false), 3);
    assert_ne!(
      exit_code(&empty_complete, false),
      exit_code(&empty_truncated, false)
    );
  }

  #[test]
  fn coverage_gaps_warn_by_default_and_fail_under_require_exhaustive() {
    let with_gaps = result(ExecutionStatus::Complete, true, 5);
    assert_eq!(exit_code(&with_gaps, false), 0);
    assert_eq!(exit_code(&with_gaps, true), 4);
  }

  #[test]
  fn a_missing_terminal_message_is_a_failure_not_an_empty_result() {
    let lost = SearchOutcome::FailedBeforeCompletion("connection reset".into());
    assert_eq!(exit_code(&lost, false), 2);
  }

  #[test]
  fn literal_runs_are_conservative_about_optionality() {
    assert_eq!(
      literal_runs("RequestContext"),
      vec![b"RequestContext".to_vec()]
    );
    // `s?` makes the preceding byte optional, so it cannot be required.
    assert_eq!(literal_runs("colors?"), vec![b"color".to_vec()]);
    // Alternation splits; no single run is required by every match.
    assert_eq!(
      literal_runs("foo|bar"),
      vec![b"foo".to_vec(), b"bar".to_vec()]
    );
    assert_eq!(
      literal_runs(r"fn\s+authorize_"),
      vec![b"fn".to_vec(), b"authorize_".to_vec()]
    );
  }

  #[test]
  fn a_pattern_without_a_three_byte_literal_yields_no_trigrams() {
    assert!(required_trigrams("a.b", false).is_empty());
    assert!(!required_trigrams("authorize", false).is_empty());
  }
}
