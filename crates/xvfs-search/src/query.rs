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

use xvfs_types::error::{ErrorCode, XvfsError};
use xvfs_types::{BytePath, ObjectId};

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
  /// The whole line, with a CRLF's `\r` removed for display only.
  #[serde(with = "crate::query::bytes_as_b64")]
  pub line_text: Vec<u8>,
  #[serde(with = "crate::query::bytes_list_as_b64")]
  pub before: Vec<Vec<u8>>,
  #[serde(with = "crate::query::bytes_list_as_b64")]
  pub after: Vec<Vec<u8>>,
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
}

impl Default for Budget {
  fn default() -> Self {
    Budget {
      max_results: 1000,
      max_time: Duration::from_secs(10),
      max_bytes_read: 512 * 1024 * 1024,
      max_candidates: 200_000,
      max_regex_bytes: 16 * 1024 * 1024,
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
) -> Result<SearchResult, XvfsError> {
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

  for entry in &eligible {
    if let Some(candidates) = &candidates {
      if !candidates.contains(entry.key) {
        continue;
      }
    }
    if !examined.insert(entry.key) {
      continue;
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

    let hits = find_in_blob(&matcher, &content, query);
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
        blob_oid: oid.clone(),
      });
    }
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
}

/// Compile a query's pattern.
///
/// Public because the local half of a client search must use the *same* matcher
/// as the server: a client that built its own would be free to differ on case
/// folding, Unicode classes, or the size limit, and the merged answer would be
/// two different searches presented as one.
pub fn compile(query: &Query) -> Result<regex::bytes::Regex, XvfsError> {
  build_matcher(query)
}

/// Every match in one blob, as [`Match`] values at `path`.
///
/// The other half of the shared-matcher rule: line numbering, column
/// derivation, and per-occurrence counting are done once, here, for both halves
/// of a merged search.
pub fn find_matches(
  matcher: &regex::bytes::Regex,
  content: &[u8],
  query: &Query,
  path: &[u8],
) -> Vec<Match> {
  find_in_blob(matcher, content, query)
    .into_iter()
    .map(|hit| Match {
      path: path.to_vec(),
      line: hit.line,
      column: hit.column,
      matched: hit.matched,
      line_text: hit.line_text,
      before: hit.before,
      after: hit.after,
      // Local content has no blob object ID until it is hashed, and hashing it
      // to fill a display field would be a real cost for no reader. Empty means
      // "not from the pinned commit", which is exactly what a caller needs to
      // know about a local match.
      blob_oid: String::new(),
    })
    .collect()
}

fn build_matcher(query: &Query) -> Result<regex::bytes::Regex, XvfsError> {
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
      XvfsError::new(
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
fn find_in_blob(matcher: &regex::bytes::Regex, content: &[u8], query: &Query) -> Vec<LineHit> {
  let all: Vec<lines::Line<'_>> = lines::lines(content).collect();
  let mut out = Vec::new();
  for (index, line) in all.iter().enumerate() {
    for m in matcher.find_iter(line.bytes) {
      let before = all[index.saturating_sub(query.context_before)..index]
        .iter()
        .map(|l| l.display().to_vec())
        .collect();
      let after = all[(index + 1).min(all.len())..(index + 1 + query.context_after).min(all.len())]
        .iter()
        .map(|l| l.display().to_vec())
        .collect();
      out.push(LineHit {
        line: line.number,
        column: line.column(m.start()),
        matched: m.as_bytes().to_vec(),
        line_text: line.display().to_vec(),
        before,
        after,
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
    s.serialize_str(&xvfs_types::path::b64url_encode(bytes))
  }

  pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    let text = String::deserialize(d)?;
    xvfs_types::path::b64url_decode(&text).map_err(serde::de::Error::custom)
  }
}

pub(crate) mod bytes_list_as_b64 {
  use serde::ser::SerializeSeq;
  use serde::{Deserialize, Deserializer, Serializer};

  pub fn serialize<S: Serializer>(list: &[Vec<u8>], s: S) -> Result<S::Ok, S::Error> {
    let mut seq = s.serialize_seq(Some(list.len()))?;
    for item in list {
      seq.serialize_element(&xvfs_types::path::b64url_encode(item))?;
    }
    seq.end()
  }

  pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Vec<u8>>, D::Error> {
    let items = Vec::<String>::deserialize(d)?;
    items
      .into_iter()
      .map(|t| xvfs_types::path::b64url_decode(&t).map_err(serde::de::Error::custom))
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
  use std::collections::HashMap;
  use std::sync::Arc;
  use xvfs_types::{HashAlgorithm, RepositoryId};

  struct MapSource(HashMap<String, Vec<u8>>);

  impl BlobSource for MapSource {
    fn size(&self, oid: &ObjectId) -> Result<u64, XvfsError> {
      self
        .0
        .get(&oid.to_qualified())
        .map(|b| b.len() as u64)
        .ok_or_else(|| XvfsError::not_found("no such blob"))
    }
    fn read(&self, oid: &ObjectId) -> Result<Vec<u8>, XvfsError> {
      self
        .0
        .get(&oid.to_qualified())
        .cloned()
        .ok_or_else(|| XvfsError::not_found("no such blob"))
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
        mode: xvfs_types::mode::REGULAR,
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
      mode: xvfs_types::mode::REGULAR,
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
  fn the_completion_carries_the_generation_and_the_commit() {
    let f = fixture(&[("a.rs", b"needle\n")]);
    let result = run(&f, literal("needle"));
    assert_eq!(result.completion.index_generation, 3);
    assert_eq!(result.completion.commit, oid(200).to_qualified());
  }
}
