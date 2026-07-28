//! Query execution, and the two-dimensional completion contract.
//!
//! DESIGN.md section 7.5 is emphatic that silence must never be ambiguous: an
//! agent that receives an empty result concludes the symbol does not exist and
//! acts on it. So every answer carries two **independent** facts:
//!
//! * **execution status** — did the query finish evaluating the searchable
//!   corpus, or did a budget cut it short;
//! * **coverage** — what was outside that corpus *within the requested scope*,
//!   grouped by reason.
//!
//! ADR 0004 froze the exit codes they map to, and [`exit_code`] is the only
//! place that mapping exists so no caller can invent its own:
//!
//! | Outcome | Exit |
//! | --- | ---: |
//! | Complete, matches found | 0 |
//! | Complete, no matches | 1 |
//! | Missing terminal message / transport failure | 2 |
//! | Execution truncated | 3 |
//! | Coverage gap under `--require-exhaustive` | 4 |
//!
//! The pair that justifies the mechanism is 1 against 3. Both return nothing;
//! one means the symbol is absent and the other means the question was not
//! finished.
//!
//! # Coverage is scoped to the request
//!
//! ADR 0004 measured a query scoped to `src/vs/editor` reporting 4 excluded
//! paths where the repository had 240. Reporting repository-wide exclusions
//! would make every scoped query look incomplete, and an agent that sees a
//! warning on every query stops reading warnings.
//!
//! # Execution order
//!
//! 1. Resolve the scope to an ordinal range of the manifest's sorted path table.
//! 2. Classify every path in scope: eligible, policy-excluded, or an index gap.
//! 3. Intersect the required literals' postings with the snapshot's membership.
//! 4. Read only candidate blobs, **once each**, and verify with the real matcher.
//! 5. Emit in path order, so two runs of the same query produce the same answer.
//!
//! Step 4 reads per *blob*, step 5 emits per *path*. A blob at forty paths is
//! inflated once and reported forty times, which is ADR 0004's "repeated blobs
//! are free" claim realized at query time.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::{BytePath, ObjectId};

use crate::classify::{classify_path, CorpusPolicy, ExclusionReason};
use crate::glob::Glob;
use crate::lines;
use crate::manifest::Manifest;
use crate::postings::PostingStore;
use crate::registry::{BlobKey, BlobRecord};
use crate::trigram;
use crate::BlobSource;

/// Did the query finish evaluating the searchable corpus?
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExecutionStatus {
  Complete,
  Truncated,
}

/// Why it did not.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncationReason {
  ResultLimit,
  TimeBudget,
  BytesBudget,
  CandidateBudget,
  /// The query had retained as much line text as it was granted. Distinct from
  /// `ResultLimit`: the page was not full, the answer was simply too wide to
  /// hold, and a caller that narrows the pattern gets further than one that
  /// pages forward.
  DisplayBudget,
  /// The pattern had no literal the index could bound it with, so the query was
  /// limited by a scan budget rather than by the index.
  ///
  /// ADR 0004 makes this `TRUNCATED` rather than `COMPLETE` even when the scan
  /// finished. That looks pedantic and is not: the scan finished *within a
  /// budget*, and the difference between an honest answer and a plausible one is
  /// saying which.
  NoRequiredLiteral,
  /// A backend read failed partway through. Reported as truncation rather than
  /// as a whole-query failure when some results are already valid, so a caller
  /// gets what was found *and* knows it is incomplete.
  BackendFailure,
}

/// What was outside the searchable corpus, within the requested scope.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Coverage {
  /// The scope as requested, as bytes. Not a `String`: a path scope need not be
  /// UTF-8.
  #[serde(with = "crate::query::bytes_as_b64")]
  pub scope: Vec<u8>,
  /// Paths in scope that were actually searched.
  pub eligible_paths: u64,
  /// Paths in scope left out, by reason.
  pub excluded: BTreeMap<String, u64>,
  /// What this policy excludes at all, whether or not this scope contained any.
  ///
  /// Present so an agent can distinguish "the policy excludes binaries and this
  /// scope has none" from "the policy excludes nothing".
  pub declared_exclusions: Vec<String>,
}

impl Coverage {
  pub fn has_gaps(&self) -> bool {
    self.excluded.values().any(|n| *n > 0)
  }

  fn record(&mut self, reason: ExclusionReason) {
    *self.excluded.entry(reason.as_str().to_owned()).or_default() += 1;
  }
}

/// The terminal message. Exactly one per successful stream.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Completion {
  pub execution_status: ExecutionStatus,
  pub truncation: Option<TruncationReason>,
  pub coverage: Coverage,
  /// The index generation the answer was computed against.
  pub index_generation: u64,
  /// The pinned commit, repeated so a logged result identifies its own snapshot.
  pub commit: String,
  /// The budget that stopped the query, when one did.
  pub stop_budget: Option<String>,
  pub candidates_considered: u64,
  pub bytes_read: u64,
  pub elapsed_ms: u64,
}

/// One match, at one path.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Match {
  #[serde(with = "crate::query::bytes_as_b64")]
  pub path: Vec<u8>,
  /// 1-based.
  pub line: u64,
  /// 1-based, in **bytes** from the start of the line. See [`mod@crate::lines`]: an
  /// agent edits with this number, so it is an offset into the file as stored.
  pub column: u64,
  /// The matched bytes.
  #[serde(with = "crate::query::bytes_as_b64")]
  pub matched: Vec<u8>,
  /// The line, with a CRLF's `\r` removed for display only, cut to the budget's
  /// `max_line_bytes`. See [`line_truncated`](Match::line_truncated).
  #[serde(with = "crate::query::bytes_as_b64")]
  pub line_text: Vec<u8>,
  #[serde(with = "crate::query::bytes_list_as_b64")]
  pub before: Vec<Vec<u8>>,
  #[serde(with = "crate::query::bytes_list_as_b64")]
  pub after: Vec<Vec<u8>>,
  /// Whether `line_text` or a context line was longer than `max_line_bytes` and
  /// holds only its first bytes.
  ///
  /// **`column` is still an offset into the whole line**, so on a truncated line
  /// it can point past the end of `line_text`. That is not an inconsistency: the
  /// column is what an agent edits with and the text is what it displays, and
  /// [`mod@crate::lines`] keeps those two jobs apart on purpose. `blob_oid` is
  /// the way to the untruncated bytes.
  ///
  /// A flag rather than an ellipsis in the text: an agent that greps `line_text`
  /// must not find a marker this code invented, and a byte field that is
  /// sometimes bytes-as-stored and sometimes bytes-plus-commentary cannot be
  /// consumed at all.
  pub line_truncated: bool,
  /// The blob the match is in. Lets a caller fetch or deduplicate by content.
  pub blob_oid: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
  pub matches: Vec<Match>,
  pub completion: Completion,
}

/// What a caller may observe.
///
/// An enum so that "the stream ended without a terminal message" is
/// *representable*, and therefore testable. The design says a missing completion
/// is an error; a type that could not express the state would let that rule
/// lapse the first time someone wrote `unwrap_or_default`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SearchOutcome {
  Completed(SearchResult),
  /// EOF, transport reset, or backend failure before the terminal message.
  FailedBeforeCompletion(String),
}

/// Per-query quotas.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
  pub max_results: usize,
  pub max_time: Duration,
  pub max_bytes_read: u64,
  /// Cap on candidate blobs examined, so a one-trigram query over a monorepo
  /// cannot turn into a full scan without saying so.
  pub max_candidates: u64,
  /// Compiled-regex size cap, so a pathological pattern costs memory it was
  /// granted rather than memory it was not.
  pub max_regex_bytes: usize,
  /// Cap on the bytes of any one line carried back for display, matched or
  /// context. `rg --max-columns`, with the difference that `rg` suppresses the
  /// line and this keeps its first bytes and says it did.
  ///
  /// A result carries a *copy* of its line, so an uncapped line is a per-match
  /// cost rather than a per-file one: a 4 MiB minified bundle with a thousand
  /// matches on its single line is 4 GiB of `line_text`. No reader wants the
  /// four-thousandth column of a generated file, so the cap loses nothing a
  /// caller would have read, and `blob_oid` still leads to the real bytes.
  pub max_line_bytes: usize,
  /// Cap on the total display bytes -- matched lines plus context -- one query
  /// retains.
  ///
  /// Distinct from `max_bytes_read`, which bounds what is *read* and handed
  /// straight back. This bounds what is *kept* until the answer is serialized,
  /// and the two differ by the number of matches sharing a line. Without it the
  /// per-line cap still multiplies: `max_results` times one plus the context
  /// lines, which a caller may legitimately set to 10 000 and 128.
  pub max_display_bytes: u64,
}

impl Default for Budget {
  fn default() -> Self {
    Budget {
      max_results: 1000,
      max_time: Duration::from_secs(10),
      max_bytes_read: 512 * 1024 * 1024,
      max_candidates: 200_000,
      max_regex_bytes: 16 * 1024 * 1024,
      // 8 KiB is ADR 0004's binary-probe window, reused deliberately: it is
      // already the distance this design treats as "far enough into a file to
      // know what it is", and it is one to two orders of magnitude past any line
      // a person or an agent reads.
      max_line_bytes: 8 * 1024,
      max_display_bytes: 64 * 1024 * 1024,
    }
  }
}

/// A query.
#[derive(Clone, Debug, Default)]
pub struct Query {
  pub pattern: String,
  /// Treat the pattern as bytes to find rather than as a regex.
  pub literal: bool,
  pub case_insensitive: bool,
  /// Path prefix. Empty means the whole snapshot.
  pub scope: Vec<u8>,
  pub include_globs: Vec<Glob>,
  pub exclude_globs: Vec<Glob>,
  pub context_before: usize,
  pub context_after: usize,
  /// Resume after this path, for pagination. Deterministic because results are
  /// ordered by path.
  pub start_after_path: Option<Vec<u8>>,
  pub budget: Budget,
}

/// Everything a query reads besides content.
#[derive(Debug)]
pub struct SearchInputs<'a> {
  pub manifest: &'a Manifest,
  pub postings: &'a PostingStore,
  pub policy: &'a CorpusPolicy,
  pub index_generation: u64,
  /// Blob records for every key in the manifest, keyed by blob key.
  pub records: &'a HashMap<BlobKey, BlobRecord>,
}

/// Run one query against one snapshot.
///
/// Returns `Err` only for a failure that makes the whole answer meaningless — an
/// unparseable pattern, a corrupt index. Anything that merely *bounds* the answer
/// arrives as a truncation inside the completion message, because a bounded
/// answer with results in it is more useful than an error, and the caller can
/// tell the difference.
pub fn search(
  source: &dyn BlobSource,
  inputs: &SearchInputs<'_>,
  query: &Query,
) -> Result<SearchResult, GfsError> {
  let started = Instant::now();
  let matcher = build_matcher(query)?;

  // 1. Scope.
  let range = inputs.manifest.scope(&query.scope);
  let in_scope = &inputs.manifest.paths()[range];

  // 2. Classify every path in scope.
  let mut coverage = Coverage {
    scope: query.scope.clone(),
    declared_exclusions: inputs
      .policy
      .declared_exclusions()
      .iter()
      .map(|r| r.as_str().to_owned())
      .collect(),
    ..Coverage::default()
  };
  let mut eligible: Vec<&crate::manifest::PathEntry> = Vec::with_capacity(in_scope.len());
  for entry in in_scope {
    let path = entry.path.as_bytes();
    if !query.include_globs.is_empty() && !query.include_globs.iter().any(|g| g.matches(path)) {
      // A path the caller did not ask about is out of scope, not excluded. It is
      // not counted as a coverage gap, because it was never in the question.
      continue;
    }
    if query.exclude_globs.iter().any(|g| g.matches(path)) {
      continue;
    }
    let record = inputs.records.get(&entry.key);
    let reason = match record {
      // Interned but never classified, or classified but with no postings: an
      // index gap. Reported, never silently skipped -- a path the index cannot
      // answer for is the exact thing that would otherwise read as "no match".
      None => Some(ExclusionReason::IndexGap),
      Some(record) => match record.class {
        None => Some(ExclusionReason::IndexGap),
        Some(class) => inputs
          .policy
          .excludes(class, classify_path(inputs.policy, path))
          .or_else(|| (!record.indexed).then_some(ExclusionReason::IndexGap)),
      },
    };
    match reason {
      Some(reason) => coverage.record(reason),
      None => eligible.push(entry),
    }
  }
  coverage.eligible_paths = eligible.len() as u64;

  // How many *in-scope* paths carry each blob. The manifest's reverse table
  // counts every path in the snapshot, which would overcount here and make the
  // result-limit accounting below wrong in the direction that matters.
  let mut in_scope_paths: HashMap<BlobKey, usize> = HashMap::new();
  for entry in &eligible {
    *in_scope_paths.entry(entry.key).or_default() += 1;
  }

  // 3. Candidate reduction, before a single blob is read.
  let literals = trigram::required_literals(&query.pattern, query.literal, query.case_insensitive);
  let mut truncation = None;
  let candidates = match &literals {
    Some(lits) => Some(inputs.postings.candidates(
      lits,
      inputs.manifest.members(),
      query.case_insensitive,
    )?),
    None => {
      note(&mut truncation, TruncationReason::NoRequiredLiteral);
      None
    }
  };

  // 4. Read candidate blobs once each.
  let mut per_blob: HashMap<BlobKey, Vec<LineHit>> = HashMap::new();
  let mut examined: std::collections::HashSet<BlobKey> = std::collections::HashSet::new();
  let mut bytes_read = 0u64;
  let mut considered = 0u64;
  let mut potential = 0usize;
  // Set when the candidate loop stops with candidates still unexamined because
  // it already has enough matches to fill the page. Tracked explicitly: the
  // first version inferred truncation from whether the emit loop hit the limit,
  // which reported COMPLETE whenever the page came out exactly full -- a query
  // that stopped early, answered plausibly, and said nothing. That is the
  // precise failure ADR 0004's execution-status dimension exists to prevent,
  // and a round-trip test caught it.
  let mut stopped_early = false;
  // Spent across blobs, not per blob: the thing being bounded is the size of the
  // answer.
  let mut display = DisplayBudget::new(&query.budget);

  for entry in &eligible {
    if let Some(candidates) = &candidates {
      if !candidates.contains(entry.key) {
        continue;
      }
    }
    if !examined.insert(entry.key) {
      continue;
    }
    if display.exhausted() {
      note(&mut truncation, TruncationReason::DisplayBudget);
      break;
    }
    if started.elapsed() > query.budget.max_time {
      note(&mut truncation, TruncationReason::TimeBudget);
      break;
    }
    if bytes_read > query.budget.max_bytes_read {
      note(&mut truncation, TruncationReason::BytesBudget);
      break;
    }
    if considered >= query.budget.max_candidates {
      note(&mut truncation, TruncationReason::CandidateBudget);
      break;
    }
    considered += 1;

    let Some(record) = inputs.records.get(&entry.key) else {
      continue;
    };
    let content = match source.read(&record.oid) {
      Ok(content) => content,
      Err(e) if e.code == ErrorCode::NotFound => {
        // The object vanished under us -- a `gc` race. Recorded as a coverage
        // gap rather than dropped, so the caller is told the answer has a hole.
        coverage.record(ExclusionReason::IndexGap);
        continue;
      }
      Err(e) => {
        // A real backend failure with results already in hand. Truncate rather
        // than discard: partial-and-labelled beats nothing.
        tracing::warn!(error = %e, "a candidate blob could not be read");
        note(&mut truncation, TruncationReason::BackendFailure);
        break;
      }
    };
    bytes_read += content.len() as u64;

    // One more than the page, never the whole blob. No path can emit more than
    // `max_results` matches, so a hit past that can never be reported -- and
    // materializing it costs a copy of its whole line. A 4 MiB single-line blob
    // searched for `xxx` has 1.4 million matches, and collecting them all asked
    // for terabytes before the emit loop's limit was ever consulted. The `+ 1`
    // is what makes an over-full page distinguishable from an exactly-full one
    // below.
    let hits = find_in_blob(
      &matcher,
      &content,
      query,
      query.budget.max_results.saturating_add(1),
      &mut display,
    );
    if !hits.is_empty() {
      potential += hits.len() * in_scope_paths.get(&entry.key).copied().unwrap_or(1);
      per_blob.insert(entry.key, hits);
      if potential > query.budget.max_results {
        // More matches than fit on the page, with candidates still unexamined.
        // Strictly greater, so a query whose results happen to fill the page
        // exactly is not reported as truncated when it in fact finished.
        stopped_early = true;
        break;
      }
    }
  }

  // 5. Emit in path order.
  let mut matches = Vec::new();
  'emit: for entry in &eligible {
    let path = entry.path.as_bytes();
    if let Some(after) = &query.start_after_path {
      if path <= after.as_slice() {
        continue;
      }
    }
    let Some(hits) = per_blob.get(&entry.key) else {
      continue;
    };
    let oid = inputs
      .records
      .get(&entry.key)
      .map(|r| r.oid.to_qualified())
      .unwrap_or_default();
    for hit in hits {
      if matches.len() >= query.budget.max_results {
        note(&mut truncation, TruncationReason::ResultLimit);
        break 'emit;
      }
      matches.push(Match {
        path: path.to_vec(),
        line: hit.line,
        column: hit.column,
        matched: hit.matched.clone(),
        line_text: hit.line_text.clone(),
        before: hit.before.clone(),
        after: hit.after.clone(),
        line_truncated: hit.line_truncated,
        blob_oid: oid.clone(),
      });
    }
  }

  // The loop-top check only fires if there is a next candidate, so a budget spent
  // on the last one would otherwise go unreported -- and hits were dropped inside
  // that blob, which is the case least allowed to look complete.
  if display.exhausted() {
    note(&mut truncation, TruncationReason::DisplayBudget);
  }

  // The candidate loop stopped with work left. If the emit loop did not already
  // record that, record it here: over-reporting truncation costs a caller one
  // unnecessary retry, and under-reporting it costs them a wrong conclusion.
  if stopped_early {
    note(&mut truncation, TruncationReason::ResultLimit);
  }

  let execution_status = if truncation.is_some() {
    ExecutionStatus::Truncated
  } else {
    ExecutionStatus::Complete
  };

  Ok(SearchResult {
    matches,
    completion: Completion {
      execution_status,
      truncation,
      stop_budget: truncation.map(|t| describe_budget(t, &query.budget)),
      coverage,
      index_generation: inputs.index_generation,
      commit: inputs.manifest.commit().to_qualified(),
      candidates_considered: considered,
      bytes_read,
      elapsed_ms: started.elapsed().as_millis() as u64,
    },
  })
}

/// The exit code for an outcome. ADR 0004's table, in one place.
pub fn exit_code(outcome: &SearchOutcome, require_exhaustive: bool) -> i32 {
  match outcome {
    SearchOutcome::FailedBeforeCompletion(_) => 2,
    SearchOutcome::Completed(result) => {
      if result.completion.execution_status == ExecutionStatus::Truncated {
        return 3;
      }
      if require_exhaustive && result.completion.coverage.has_gaps() {
        return 4;
      }
      if result.matches.is_empty() {
        1
      } else {
        0
      }
    }
  }
}

fn describe_budget(reason: TruncationReason, budget: &Budget) -> String {
  match reason {
    TruncationReason::ResultLimit => format!("max_results={}", budget.max_results),
    TruncationReason::TimeBudget => format!("max_time_ms={}", budget.max_time.as_millis()),
    TruncationReason::BytesBudget => format!("max_bytes_read={}", budget.max_bytes_read),
    TruncationReason::CandidateBudget => format!("max_candidates={}", budget.max_candidates),
    TruncationReason::DisplayBudget => format!(
      "max_display_bytes={} (line text retained, at max_line_bytes={} per line)",
      budget.max_display_bytes, budget.max_line_bytes
    ),
    TruncationReason::NoRequiredLiteral => {
      "the pattern has no literal of three or more bytes that every match must \
       contain, so the query was bounded by a scan budget rather than by the index"
        .to_owned()
    }
    TruncationReason::BackendFailure => "a backend read failed".to_owned(),
  }
}

/// Record the **first** thing that bounded the query.
///
/// First rather than last, because the first reason characterizes the query
/// itself. A pattern the index could not bound was always going to stop on a
/// budget, so reporting the budget would bury the fact that answers to this
/// shape of query are never index-complete.
fn note(slot: &mut Option<TruncationReason>, reason: TruncationReason) {
  slot.get_or_insert(reason);
}

struct LineHit {
  line: u64,
  column: u64,
  matched: Vec<u8>,
  line_text: Vec<u8>,
  before: Vec<Vec<u8>>,
  after: Vec<Vec<u8>>,
  line_truncated: bool,
}

/// How much line text a query may still retain, and how wide any one line may
/// be.
///
/// Carried across blobs — and, for a merged search, across the local half too —
/// because the thing being bounded is the size of the *answer*, not the cost of
/// any one file.
#[derive(Debug)]
pub struct DisplayBudget {
  per_line: usize,
  remaining: u64,
  exhausted: bool,
}

impl DisplayBudget {
  pub fn new(budget: &Budget) -> DisplayBudget {
    DisplayBudget {
      per_line: budget.max_line_bytes,
      remaining: budget.max_display_bytes,
      exhausted: false,
    }
  }

  /// Whether the budget ran out. A query that sees this must report
  /// [`TruncationReason::DisplayBudget`]: it stopped, so it did not finish.
  pub fn exhausted(&self) -> bool {
    self.exhausted
  }

  /// One line, cut to the per-line cap and charged to the running total.
  ///
  /// Returns the bytes and whether anything was dropped. Charging *after*
  /// copying means the budget can go one line over, which is the price of
  /// knowing the line's capped length before deciding — bounded, and bounded by
  /// the per-line cap.
  fn take(&mut self, line: &[u8]) -> (Vec<u8>, bool) {
    let end = utf8_safe_cut(line, self.per_line);
    let kept = line[..end].to_vec();
    if (kept.len() as u64) >= self.remaining {
      self.exhausted = true;
    }
    self.remaining = self.remaining.saturating_sub(kept.len() as u64);
    (kept, end < line.len())
  }
}

/// The largest cut of `line` at or below `limit` that does not split a UTF-8
/// sequence.
///
/// Content need not be UTF-8 and this makes no attempt to validate it — it only
/// declines to cut in the middle of a multi-byte sequence, so a terminal that
/// prints the result does not show a replacement character the file does not
/// contain. On bytes that are not UTF-8 at all, it gives up at most three of
/// them. The same reasoning as [`crate::lines::Line::display`]: this field is
/// for reading, and the fields that drive an edit are computed elsewhere.
fn utf8_safe_cut(line: &[u8], limit: usize) -> usize {
  if line.len() <= limit {
    return line.len();
  }
  let mut end = limit;
  // A continuation byte is `10xxxxxx`. Walk back to the sequence's start, at
  // most the three that can precede a lead byte.
  let floor = limit.saturating_sub(3);
  while end > floor && (line[end] & 0b1100_0000) == 0b1000_0000 {
    end -= 1;
  }
  end
}

/// Compile a query's pattern.
///
/// Public because the local half of a client search must use the *same* matcher
/// as the server: a client that built its own would be free to differ on case
/// folding, Unicode classes, or the size limit, and the merged answer would be
/// two different searches presented as one.
pub fn compile(query: &Query) -> Result<regex::bytes::Regex, GfsError> {
  build_matcher(query)
}

/// Every match in one blob, as [`Match`] values at `path`.
///
/// The other half of the shared-matcher rule: line numbering, column
/// derivation, and per-occurrence counting are done once, here, for both halves
/// of a merged search.
///
/// `limit` bounds how many hits are *materialized*, not how many exist. A caller
/// that will keep at most `n` more matches must pass `n + 1`: the extra hit is
/// how it learns there was more, and stopping at `n` would report a truncated
/// answer as complete.
pub fn find_matches(
  matcher: &regex::bytes::Regex,
  content: &[u8],
  query: &Query,
  path: &[u8],
  limit: usize,
  display: &mut DisplayBudget,
) -> Vec<Match> {
  find_in_blob(matcher, content, query, limit, display)
    .into_iter()
    .map(|hit| Match {
      path: path.to_vec(),
      line: hit.line,
      column: hit.column,
      matched: hit.matched,
      line_text: hit.line_text,
      before: hit.before,
      after: hit.after,
      line_truncated: hit.line_truncated,
      // Local content has no blob object ID until it is hashed, and hashing it
      // to fill a display field would be a real cost for no reader. Empty means
      // "not from the pinned commit", which is exactly what a caller needs to
      // know about a local match.
      blob_oid: String::new(),
    })
    .collect()
}

fn build_matcher(query: &Query) -> Result<regex::bytes::Regex, GfsError> {
  let pattern = if query.literal {
    regex::escape(&query.pattern)
  } else {
    query.pattern.clone()
  };
  regex::bytes::RegexBuilder::new(&pattern)
    .case_insensitive(query.case_insensitive)
    // Bytes, not UTF-8: a blob may not be valid UTF-8 and `rg` searches it
    // anyway. `unicode(true)` stays on so `\w` matches what `rg` says it does.
    .size_limit(query.budget.max_regex_bytes)
    .dfa_size_limit(query.budget.max_regex_bytes)
    .build()
    .map_err(|e| {
      GfsError::new(
        ErrorCode::InvalidArgument,
        format!("the pattern could not be compiled: {e}"),
      )
    })
}

/// Every match in one blob, one per *occurrence* rather than per line.
///
/// `rg --count-matches` counts every match on a line, and a line-oriented count
/// silently under-reports exactly the patterns agents use most: a brace, a
/// quote, an identifier appearing twice in one call.
///
/// Stops after `limit` hits, or when `display` runs out. Every hit carries a
/// copy of its line, so the hit count is a memory cost and not merely a time
/// one: a blob with more matches than any caller could report has to stop being
/// *read*, not stop being reported.
fn find_in_blob(
  matcher: &regex::bytes::Regex,
  content: &[u8],
  query: &Query,
  limit: usize,
  display: &mut DisplayBudget,
) -> Vec<LineHit> {
  let all: Vec<lines::Line<'_>> = lines::lines(content).collect();
  let mut out = Vec::new();
  for (index, line) in all.iter().enumerate() {
    for m in matcher.find_iter(line.bytes) {
      if out.len() >= limit || display.exhausted() {
        return out;
      }
      let mut truncated = false;
      let mut take = |bytes: &[u8], display: &mut DisplayBudget| {
        let (kept, cut) = display.take(bytes);
        truncated |= cut;
        kept
      };
      let before = all[index.saturating_sub(query.context_before)..index]
        .iter()
        .map(|l| take(l.display(), display))
        .collect();
      let after = all[(index + 1).min(all.len())..(index + 1 + query.context_after).min(all.len())]
        .iter()
        .map(|l| take(l.display(), display))
        .collect();
      let line_text = take(line.display(), display);
      // `matched` is capped too. A regex is free to match a whole line -- `x+`
      // over a minified bundle matches all 4 MiB of it -- so the span is as
      // unbounded as the line it came from, and for the same reason.
      let matched = take(m.as_bytes(), display);
      out.push(LineHit {
        line: line.number,
        column: line.column(m.start()),
        matched,
        line_text,
        before,
        after,
        line_truncated: truncated,
      });
    }
  }
  out
}

/// Byte fields as base64url in JSON.
///
/// A path, a matched span, and a line are all bytes that need not be UTF-8.
/// `serde_json` cannot hold a non-UTF-8 string, and a lossy conversion would put
/// U+FFFD into the one field an agent uses to open the file.
pub(crate) mod bytes_as_b64 {
  use serde::{Deserialize, Deserializer, Serializer};

  pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&gfs_types::path::b64url_encode(bytes))
  }

  pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    let text = String::deserialize(d)?;
    gfs_types::path::b64url_decode(&text).map_err(serde::de::Error::custom)
  }
}

pub(crate) mod bytes_list_as_b64 {
  use serde::ser::SerializeSeq;
  use serde::{Deserialize, Deserializer, Serializer};

  pub fn serialize<S: Serializer>(list: &[Vec<u8>], s: S) -> Result<S::Ok, S::Error> {
    let mut seq = s.serialize_seq(Some(list.len()))?;
    for item in list {
      seq.serialize_element(&gfs_types::path::b64url_encode(item))?;
    }
    seq.end()
  }

  pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Vec<u8>>, D::Error> {
    let items = Vec::<String>::deserialize(d)?;
    items
      .into_iter()
      .map(|t| gfs_types::path::b64url_decode(&t).map_err(serde::de::Error::custom))
      .collect()
  }
}

/// Build the record map a query needs from a registry.
pub fn records_by_key(records: Vec<BlobRecord>) -> HashMap<BlobKey, BlobRecord> {
  records.into_iter().map(|r| (r.key, r)).collect()
}

/// A commit-scoped blob-OID lookup, for callers that only have keys.
pub fn oid_for_key(records: &HashMap<BlobKey, BlobRecord>, key: BlobKey) -> Option<&ObjectId> {
  records.get(&key).map(|r| &r.oid)
}

/// The path of a manifest entry, as a validated byte path.
pub fn entry_path(entry: &crate::manifest::PathEntry) -> &BytePath {
  &entry.path
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::manifest::PathEntry;
  use crate::postings::PostingBatch;
  use crate::SearchStore;
  use gfs_types::{HashAlgorithm, RepositoryId};
  use std::collections::HashMap;
  use std::sync::Arc;

  struct MapSource(HashMap<String, Vec<u8>>);

  impl BlobSource for MapSource {
    fn size(&self, oid: &ObjectId) -> Result<u64, GfsError> {
      self
        .0
        .get(&oid.to_qualified())
        .map(|b| b.len() as u64)
        .ok_or_else(|| GfsError::not_found("no such blob"))
    }
    fn read(&self, oid: &ObjectId) -> Result<Vec<u8>, GfsError> {
      self
        .0
        .get(&oid.to_qualified())
        .cloned()
        .ok_or_else(|| GfsError::not_found("no such blob"))
    }
  }

  /// A source that fails on one blob and serves the rest.
  ///
  /// A *failure*, not a `NotFound`: those are different faults with different
  /// answers. A missing object is a `gc` race and becomes a coverage gap; a read
  /// that errors is the backend being broken, and the results already in hand
  /// stay valid while the answer stops being complete.
  struct FailingSource {
    inner: MapSource,
    broken: String,
  }

  impl BlobSource for FailingSource {
    fn size(&self, oid: &ObjectId) -> Result<u64, GfsError> {
      self.inner.size(oid)
    }
    fn read(&self, oid: &ObjectId) -> Result<Vec<u8>, GfsError> {
      if oid.to_qualified() == self.broken {
        return Err(GfsError::internal("the object database is unreadable"));
      }
      self.inner.read(oid)
    }
  }

  fn oid(n: u8) -> ObjectId {
    ObjectId::from_raw(HashAlgorithm::Sha1, &[n; 20]).unwrap()
  }

  /// A snapshot with `(path, content)` entries, fully indexed.
  struct Fixture {
    manifest: Manifest,
    postings: PostingStore,
    records: HashMap<BlobKey, BlobRecord>,
    source: MapSource,
    policy: CorpusPolicy,
  }

  fn fixture(entries: &[(&str, &[u8])]) -> Fixture {
    let store = Arc::new(SearchStore::open_in_memory().unwrap());
    let postings = PostingStore::new(store, RepositoryId::parse("r-q").unwrap());
    let policy = CorpusPolicy::default();

    let mut batch = PostingBatch::new();
    let mut paths = Vec::new();
    let mut records = HashMap::new();
    let mut blobs = HashMap::new();
    for (index, (path, content)) in entries.iter().enumerate() {
      let key = index as BlobKey;
      let id = oid(index as u8 + 1);
      let class = crate::classify::classify_content(&policy, content);
      if class.is_indexable() {
        batch.add(key, content);
      }
      blobs.insert(id.to_qualified(), content.to_vec());
      records.insert(
        key,
        BlobRecord {
          key,
          oid: id,
          size: content.len() as u64,
          class: Some(class),
          indexed: class.is_indexable(),
        },
      );
      paths.push(PathEntry {
        path: BytePath::new(path.as_bytes().to_vec()),
        mode: gfs_types::mode::REGULAR,
        key,
      });
    }
    postings.merge(&batch).unwrap();

    Fixture {
      manifest: Manifest::build(oid(200), paths),
      postings,
      records,
      source: MapSource(blobs),
      policy,
    }
  }

  fn run(f: &Fixture, query: Query) -> SearchResult {
    let inputs = SearchInputs {
      manifest: &f.manifest,
      postings: &f.postings,
      policy: &f.policy,
      index_generation: 3,
      records: &f.records,
    };
    search(&f.source, &inputs, &query).unwrap()
  }

  fn literal(pattern: &str) -> Query {
    Query {
      pattern: pattern.to_owned(),
      literal: true,
      ..Query::default()
    }
  }

  #[test]
  fn a_literal_match_reports_path_line_and_column() {
    let f = fixture(&[("src/main.rs", b"fn main() {}\nlet needle = 1;\n")]);
    let result = run(&f, literal("needle"));
    assert_eq!(result.matches.len(), 1);
    let m = &result.matches[0];
    assert_eq!(m.path, b"src/main.rs");
    assert_eq!(m.line, 2);
    assert_eq!(m.column, 5);
    assert_eq!(m.matched, b"needle");
    assert_eq!(
      result.completion.execution_status,
      ExecutionStatus::Complete
    );
  }

  #[test]
  fn a_clean_empty_result_and_a_truncated_one_have_different_exit_codes() {
    // The failure the whole contract exists to prevent.
    let f = fixture(&[("a.rs", b"nothing here\n")]);
    let empty = run(&f, literal("zzzzzz"));
    assert!(empty.matches.is_empty());
    assert_eq!(
      exit_code(&SearchOutcome::Completed(empty), false),
      1,
      "an honest empty result"
    );

    // A pattern with no usable literal is truncated even though it found
    // nothing: it was bounded by a scan budget, not by the index.
    let no_literal = run(
      &f,
      Query {
        pattern: "z.z".to_owned(),
        ..Query::default()
      },
    );
    assert_eq!(
      no_literal.completion.truncation,
      Some(TruncationReason::NoRequiredLiteral)
    );
    assert_eq!(exit_code(&SearchOutcome::Completed(no_literal), false), 3);
  }

  #[test]
  fn a_missing_terminal_message_is_a_failure_not_an_empty_result() {
    let lost = SearchOutcome::FailedBeforeCompletion("connection reset".into());
    assert_eq!(exit_code(&lost, false), 2);
    assert_eq!(exit_code(&lost, true), 2);
  }

  #[test]
  fn a_binary_file_is_a_reported_coverage_gap_not_a_silent_omission() {
    let f = fixture(&[
      ("src/a.rs", b"needle\n"),
      ("assets/blob.bin", b"needle\0binary"),
    ]);
    let result = run(&f, literal("needle"));
    assert_eq!(result.matches.len(), 1, "the binary file is not searched");
    assert_eq!(result.completion.coverage.excluded["binary"], 1);
    assert!(result.completion.coverage.has_gaps());
    // Default behaviour warns; --require-exhaustive fails.
    assert_eq!(
      exit_code(&SearchOutcome::Completed(result.clone()), false),
      0
    );
    assert_eq!(exit_code(&SearchOutcome::Completed(result), true), 4);
  }

  #[test]
  fn coverage_is_scoped_to_the_request() {
    // ADR 0004: a scoped query reporting the repository's exclusions makes every
    // query look incomplete, and an agent stops reading the warning.
    let f = fixture(&[
      ("src/a.rs", b"needle\n"),
      ("assets/one.bin", b"\0\0\0"),
      ("assets/two.bin", b"\0\0\0"),
    ]);
    let scoped = run(
      &f,
      Query {
        scope: b"src".to_vec(),
        ..literal("needle")
      },
    );
    assert!(!scoped.completion.coverage.has_gaps());
    assert_eq!(scoped.completion.coverage.eligible_paths, 1);

    let whole = run(&f, literal("needle"));
    assert_eq!(whole.completion.coverage.excluded["binary"], 2);
  }

  #[test]
  fn a_declared_policy_is_reported_even_when_this_scope_has_no_exclusions() {
    let f = fixture(&[("a.rs", b"needle\n")]);
    let result = run(&f, literal("needle"));
    assert!(result
      .completion
      .coverage
      .declared_exclusions
      .contains(&"binary".to_owned()));
    assert!(!result.completion.coverage.has_gaps());
  }

  #[test]
  fn an_unindexed_blob_is_an_index_gap_rather_than_a_miss() {
    // The state M4.1 kept representable: interned, not yet indexed. A query over
    // it must say so, because "no postings" and "no matches" are the same
    // observation from the outside.
    let mut f = fixture(&[("a.rs", b"needle\n"), ("b.rs", b"needle\n")]);
    f.records.get_mut(&1).unwrap().indexed = false;
    let result = run(&f, literal("needle"));
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.completion.coverage.excluded["index_gap"], 1);
    assert_eq!(exit_code(&SearchOutcome::Completed(result), true), 4);
  }

  #[test]
  fn an_unclassified_blob_is_also_an_index_gap() {
    let mut f = fixture(&[("a.rs", b"needle\n")]);
    f.records.get_mut(&0).unwrap().class = None;
    let result = run(&f, literal("needle"));
    assert!(result.matches.is_empty());
    assert_eq!(result.completion.coverage.excluded["index_gap"], 1);
    // And it is *not* reported as a clean empty result.
    assert_eq!(exit_code(&SearchOutcome::Completed(result), true), 4);
  }

  #[test]
  fn a_result_limit_truncates_and_says_which_budget_stopped_it() {
    let f = fixture(&[("a.rs", b"xyz\nxyz\nxyz\nxyz\nxyz\n")]);
    let result = run(
      &f,
      Query {
        budget: Budget {
          max_results: 2,
          ..Budget::default()
        },
        ..literal("xyz")
      },
    );
    assert_eq!(result.matches.len(), 2);
    assert_eq!(
      result.completion.truncation,
      Some(TruncationReason::ResultLimit)
    );
    assert_eq!(
      result.completion.stop_budget.as_deref(),
      Some("max_results=2")
    );
    assert_eq!(exit_code(&SearchOutcome::Completed(result), false), 3);
  }

  #[test]
  fn a_page_that_fills_exactly_is_not_reported_as_truncated() {
    // The other half of the previous test, and the reason the candidate loop
    // compares strictly greater. Over-reporting truncation costs a caller a
    // pointless retry; the contract only forbids under-reporting, but a warning
    // that fires on every complete query is a warning agents learn to ignore.
    let f = fixture(&[("a.rs", b"xyz\nxyz\n")]);
    let result = run(
      &f,
      Query {
        budget: Budget {
          max_results: 2,
          ..Budget::default()
        },
        ..literal("xyz")
      },
    );
    assert_eq!(result.matches.len(), 2);
    assert_eq!(
      result.completion.execution_status,
      ExecutionStatus::Complete
    );
    assert_eq!(exit_code(&SearchOutcome::Completed(result), false), 0);
  }

  #[test]
  fn stopping_early_with_candidates_left_is_truncation_even_on_a_full_page() {
    // The bug a round-trip test caught: the candidate loop stopped because it
    // had enough matches, the emit loop never tried for one more, and the result
    // came back COMPLETE with unexamined candidates behind it.
    let f = fixture(&[
      ("a.rs", b"xyz\nxyz\nxyz\n"),
      ("b.rs", b"xyz\n"),
      ("c.rs", b"xyz\n"),
    ]);
    let result = run(
      &f,
      Query {
        budget: Budget {
          max_results: 2,
          ..Budget::default()
        },
        ..literal("xyz")
      },
    );
    assert_eq!(result.matches.len(), 2);
    assert_eq!(
      result.completion.execution_status,
      ExecutionStatus::Truncated
    );
    assert_eq!(exit_code(&SearchOutcome::Completed(result), false), 3);
  }

  /// A line of `x`, big enough to have far more matches than any page.
  ///
  /// 64 KiB rather than the fixture matrix's 4 MiB: the property is the same and
  /// a regression should cost a failed assertion, not the machine's memory.
  fn one_long_line() -> Vec<u8> {
    vec![b'x'; 64 * 1024]
  }

  #[test]
  fn a_limit_bounds_what_is_built_not_only_what_is_reported() {
    // The OOM. Every hit carries a copy of its line, so per-blob cost is
    // `matches * line length`, and a 4 MiB single-line blob searched for `xxx`
    // has 1.4 million matches -- terabytes, asked for before the emit loop's
    // `max_results` was ever consulted. Asserted on `find_matches` directly
    // because the emit loop truncates the *page* either way, so an end-to-end
    // assertion on `matches.len()` cannot tell the two versions apart.
    let content = one_long_line();
    let query = Query {
      pattern: "xxx".to_owned(),
      literal: true,
      ..Query::default()
    };
    let matcher = compile(&query).unwrap();
    let mut display = DisplayBudget::new(&query.budget);
    let hits = find_matches(&matcher, &content, &query, b"huge.txt", 6, &mut display);
    assert_eq!(hits.len(), 6);
    assert!(hits.iter().all(|m| m.line == 1));
  }

  #[test]
  fn a_line_wider_than_the_budget_is_cut_and_says_so() {
    // The second half of the same cost: capping the hit *count* still leaves
    // `max_results` copies of a line that may itself be megabytes.
    let content = one_long_line();
    let query = Query {
      budget: Budget {
        max_line_bytes: 100,
        ..Budget::default()
      },
      ..literal("xxx")
    };
    let matcher = compile(&query).unwrap();
    let mut display = DisplayBudget::new(&query.budget);
    let hits = find_matches(&matcher, &content, &query, b"huge.txt", 3, &mut display);
    assert_eq!(hits[0].line_text.len(), 100);
    assert!(hits[0].line_truncated);
    // The column still describes the whole line, not the cut of it. An agent
    // edits with this number; the text is only what it shows.
    assert_eq!(hits[1].column, 4);
  }

  #[test]
  fn a_short_line_is_not_marked_truncated() {
    // The flag has to mean something, so it must be off on the common case.
    let f = fixture(&[("a.rs", b"let needle = 1;\n")]);
    let result = run(&f, literal("needle"));
    assert!(!result.matches[0].line_truncated);
    assert_eq!(result.matches[0].line_text, b"let needle = 1;");
  }

  #[test]
  fn a_cut_line_does_not_split_a_utf8_sequence() {
    // `display()` exists so a snippet does not corrupt a terminal; a cut that
    // landed mid-codepoint would put a replacement character in the output that
    // the file does not contain.
    // `needle ` is 7 bytes and `線` is 3, so byte 10 is a boundary and 11 is not.
    let mut content = b"needle ".to_vec();
    content.extend("線線線線線線".as_bytes());
    content.push(b'\n');
    let query = Query {
      budget: Budget {
        max_line_bytes: 11,
        ..Budget::default()
      },
      ..literal("needle")
    };
    let matcher = compile(&query).unwrap();
    let mut display = DisplayBudget::new(&query.budget);
    let hits = find_matches(&matcher, &content, &query, b"a.rs", 2, &mut display);
    assert_eq!(hits[0].line_text.len(), 10);
    assert!(std::str::from_utf8(&hits[0].line_text).is_ok());
    assert!(hits[0].line_truncated);
  }

  #[test]
  fn the_display_budget_truncates_the_query_rather_than_the_machine() {
    // The per-line cap alone still multiplies: `max_results` times one plus the
    // context lines. This is the bound on the product, and it is a truncation
    // rather than a silent shortfall.
    let content = one_long_line();
    let f = fixture(&[("huge.txt", content.as_slice())]);
    let result = run(
      &f,
      Query {
        budget: Budget {
          max_results: 10_000,
          max_line_bytes: 1024,
          max_display_bytes: 16 * 1024,
          ..Budget::default()
        },
        ..literal("xxx")
      },
    );
    assert!(
      result.matches.len() < 10_000,
      "the display budget stopped it well before the result limit"
    );
    assert_eq!(
      result.completion.truncation,
      Some(TruncationReason::DisplayBudget)
    );
    assert_eq!(exit_code(&SearchOutcome::Completed(result), false), 3);
  }

  #[test]
  fn a_blob_with_more_matches_than_the_page_still_reports_the_page_and_truncates() {
    // The other half: bounding the read must not change the answer.
    let content = one_long_line();
    let f = fixture(&[("huge.txt", content.as_slice())]);
    let result = run(
      &f,
      Query {
        budget: Budget {
          max_results: 5,
          ..Budget::default()
        },
        ..literal("xxx")
      },
    );
    assert_eq!(result.matches.len(), 5);
    assert_eq!(result.matches[0].line, 1);
    assert_eq!(result.matches[0].column, 1);
    assert_eq!(
      result.completion.truncation,
      Some(TruncationReason::ResultLimit)
    );
  }

  #[test]
  fn matches_are_ordered_by_path_deterministically() {
    let f = fixture(&[
      ("z.rs", b"needle\n"),
      ("a.rs", b"needle\n"),
      ("m.rs", b"needle\n"),
    ]);
    let first = run(&f, literal("needle"));
    let second = run(&f, literal("needle"));
    let paths: Vec<Vec<u8>> = first.matches.iter().map(|m| m.path.clone()).collect();
    assert_eq!(
      paths,
      vec![b"a.rs".to_vec(), b"m.rs".to_vec(), b"z.rs".to_vec()]
    );
    assert_eq!(first.matches, second.matches);
  }

  #[test]
  fn a_repeated_blob_is_read_once_and_reported_at_every_path() {
    let f = fixture(&[("a/LICENSE", b"Apache License needle\n")]);
    // Two paths, one blob key.
    let mut paths = f.manifest.paths().to_vec();
    paths.push(PathEntry {
      path: BytePath::new(b"b/LICENSE".to_vec()),
      mode: gfs_types::mode::REGULAR,
      key: 0,
    });
    let f = Fixture {
      manifest: Manifest::build(oid(200), paths),
      ..f
    };
    let result = run(&f, literal("needle"));
    assert_eq!(result.matches.len(), 2);
    assert_eq!(
      result.completion.candidates_considered, 1,
      "one blob, read once"
    );
    assert_eq!(result.completion.bytes_read, 22);
  }

  #[test]
  fn every_occurrence_on_a_line_is_reported_not_just_the_first() {
    let f = fixture(&[("a.rs", b"foo(foo, foo)\n")]);
    let result = run(&f, literal("foo"));
    assert_eq!(result.matches.len(), 3);
    assert_eq!(
      result.matches.iter().map(|m| m.column).collect::<Vec<_>>(),
      vec![1, 5, 10]
    );
  }

  #[test]
  fn crlf_does_not_shift_the_reported_column() {
    let f = fixture(&[("a.rs", b"first\r\n  needle\r\n")]);
    let result = run(&f, literal("needle"));
    assert_eq!(result.matches[0].line, 2);
    assert_eq!(result.matches[0].column, 3);
    assert_eq!(result.matches[0].line_text, b"  needle");
  }

  #[test]
  fn a_file_with_no_final_newline_still_matches_on_its_last_line() {
    let f = fixture(&[("a.rs", b"one\ntwo needle")]);
    let result = run(&f, literal("needle"));
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].line, 2);
  }

  #[test]
  fn invalid_utf8_content_is_searched_like_ripgrep_searches_it() {
    let f = fixture(&[("a.rs", b"caf\xe9 needle\n")]);
    let result = run(&f, literal("needle"));
    assert_eq!(result.matches.len(), 1);
    assert!(!result.completion.coverage.has_gaps());
  }

  #[test]
  fn case_insensitive_search_uses_the_index_and_finds_the_other_casing() {
    let f = fixture(&[("a.rs", b"NeedleInHaystack\n")]);
    let result = run(
      &f,
      Query {
        case_insensitive: true,
        ..literal("needle")
      },
    );
    assert_eq!(result.matches.len(), 1);
    assert_eq!(
      result.completion.execution_status,
      ExecutionStatus::Complete,
      "the index bounded it, so this is not a scan"
    );
  }

  #[test]
  fn globs_narrow_the_question_rather_than_creating_a_coverage_gap() {
    let f = fixture(&[("a.rs", b"needle\n"), ("b.py", b"needle\n")]);
    let result = run(
      &f,
      Query {
        include_globs: vec![Glob::new("*.rs")],
        ..literal("needle")
      },
    );
    assert_eq!(result.matches.len(), 1);
    assert!(
      !result.completion.coverage.has_gaps(),
      "a path the caller did not ask about is not a gap"
    );
    assert_eq!(result.completion.coverage.eligible_paths, 1);
  }

  #[test]
  fn context_lines_come_from_the_same_blob() {
    let f = fixture(&[("a.rs", b"one\ntwo\nneedle\nfour\nfive\n")]);
    let result = run(
      &f,
      Query {
        context_before: 2,
        context_after: 1,
        ..literal("needle")
      },
    );
    let m = &result.matches[0];
    assert_eq!(m.before, vec![b"one".to_vec(), b"two".to_vec()]);
    assert_eq!(m.after, vec![b"four".to_vec()]);
  }

  #[test]
  fn pagination_resumes_after_a_path_without_repeating_it() {
    let f = fixture(&[
      ("a.rs", b"needle\n"),
      ("b.rs", b"needle\n"),
      ("c.rs", b"needle\n"),
    ]);
    let rest = run(
      &f,
      Query {
        start_after_path: Some(b"a.rs".to_vec()),
        ..literal("needle")
      },
    );
    let paths: Vec<Vec<u8>> = rest.matches.iter().map(|m| m.path.clone()).collect();
    assert_eq!(paths, vec![b"b.rs".to_vec(), b"c.rs".to_vec()]);
  }

  #[test]
  fn a_regex_too_large_to_compile_is_an_error_not_a_wrong_answer() {
    let f = fixture(&[("a.rs", b"needle\n")]);
    let inputs = SearchInputs {
      manifest: &f.manifest,
      postings: &f.postings,
      policy: &f.policy,
      index_generation: 1,
      records: &f.records,
    };
    let query = Query {
      pattern: r"\p{Any}{1000}".to_owned(),
      budget: Budget {
        max_regex_bytes: 128,
        ..Budget::default()
      },
      ..Query::default()
    };
    let err = search(&f.source, &inputs, &query).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
  }

  #[test]
  fn a_vanished_blob_becomes_a_coverage_gap_rather_than_a_miss() {
    // A `gc` race. The path was eligible and the answer for it is unknown, which
    // is not the same as "no match here".
    let mut f = fixture(&[("a.rs", b"needle\n"), ("b.rs", b"needle\n")]);
    let missing = f.records[&0].oid.to_qualified();
    f.source.0.remove(&missing);
    let result = run(&f, literal("needle"));
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.completion.coverage.excluded["index_gap"], 1);
  }

  #[test]
  fn a_backend_failure_partway_through_keeps_the_results_and_stops_claiming_completeness() {
    // Partial backend failure, which is not the same fault as a vanished blob
    // above. Two results are already valid and discarding them would help
    // nobody; reporting them as the answer would be a lie. So: both.
    let f = fixture(&[
      ("a.rs", b"needle\n"),
      ("b.rs", b"needle two\n"),
      ("c.rs", b"needle three\n"),
    ]);
    let broken = f.records[&2].oid.to_qualified();
    let source = FailingSource {
      inner: MapSource(f.source.0.clone()),
      broken,
    };
    let inputs = SearchInputs {
      manifest: &f.manifest,
      postings: &f.postings,
      policy: &f.policy,
      index_generation: 1,
      records: &f.records,
    };
    let result = search(&source, &inputs, &literal("needle")).unwrap();

    assert_eq!(
      result.matches.len(),
      2,
      "the blobs that were readable still answered"
    );
    assert_eq!(
      result.completion.truncation,
      Some(TruncationReason::BackendFailure)
    );
    assert_eq!(
      result.completion.execution_status,
      ExecutionStatus::Truncated
    );
    // And the code an agent acts on is the "not finished" one, not the "found
    // some" one.
    assert_eq!(exit_code(&SearchOutcome::Completed(result), false), 3);
  }

  #[test]
  fn a_backend_that_fails_on_the_first_blob_is_not_an_empty_result() {
    // The worst shape of the same fault: nothing was readable, so the answer is
    // empty *and* wrong. Exit 1 here would tell an agent the symbol does not
    // exist anywhere in the repository.
    let f = fixture(&[("a.rs", b"needle\n")]);
    let broken = f.records[&0].oid.to_qualified();
    let source = FailingSource {
      inner: MapSource(f.source.0.clone()),
      broken,
    };
    let inputs = SearchInputs {
      manifest: &f.manifest,
      postings: &f.postings,
      policy: &f.policy,
      index_generation: 1,
      records: &f.records,
    };
    let result = search(&source, &inputs, &literal("needle")).unwrap();

    assert!(result.matches.is_empty());
    assert_eq!(
      result.completion.truncation,
      Some(TruncationReason::BackendFailure)
    );
    assert_eq!(exit_code(&SearchOutcome::Completed(result), false), 3);
  }

  #[test]
  fn the_completion_carries_the_generation_and_the_commit() {
    let f = fixture(&[("a.rs", b"needle\n")]);
    let result = run(&f, literal("needle"));
    assert_eq!(result.completion.index_generation, 3);
    assert_eq!(result.completion.commit, oid(200).to_qualified());
  }
}
