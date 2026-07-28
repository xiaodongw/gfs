//! Overlay-aware search: the pinned commit's index, minus what the workspace
//! changed, plus what the workspace has instead.
//!
//! PLAN.md M4.5 states the rule and this module is that rule:
//!
//! * query the **exact pinned commit**, never the branch name it came from;
//! * exclude changed, deleted, renamed-from, and type-changed paths from the
//!   base results;
//! * search created, copied-up, and modified local files **without contacting
//!   the server**;
//! * merge execution status and coverage into one report.
//!
//! # `mv` and `chmod +x` still cost nothing
//!
//! M3 measured a directory rename and a `chmod +x` on a 12 MiB blob at zero
//! transferred bytes, because the overlay records them as metadata changes over
//! `Content::Base`. Search preserves that. A row whose bytes are still the
//! pinned commit's is not searched locally and does not fetch anything; instead
//! the server's result for the *source* path is re-pathed to where the workspace
//! now keeps it. A `mv src source` therefore changes which paths a search
//! reports without changing what it reads.
//!
//! # The one place a local search touches the base
//!
//! Ignore rules. A file the base does not track can be ignored, and deciding
//! that needs the `.gitignore` files above it — which may live in the pinned
//! commit. So the daemon fetches `.gitignore` for the ancestors of *untracked
//! overlay paths only*, which means:
//!
//! * a clean workspace fetches nothing, which is what keeps M4.6's zero-client-
//!   hydration criterion true for a server search;
//! * the fetch is bounded by the overlay's own directory set, not by the
//!   repository's.
//!
//! ADR 0005's synthesized `.git` has no `info/exclude`, so that ignore source is
//! empty by construction here rather than unimplemented.
//!
//! # Merging is not concatenation
//!
//! Two halves that each finished are one complete answer. If **either** half was
//! truncated the whole answer is truncated, and if either reported a coverage
//! exclusion the merged report carries it. Anything weaker would let a
//! budget-stopped local scan hide behind a complete server half, which is the
//! shape of failure the whole contract exists to prevent.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use gfs_overlay::{Content, Overlay, OverlayKind};
use gfs_search::classify::{CorpusPolicy, ExclusionReason};
use gfs_search::local::{IgnoreRules, LocalBudget, LocalPath};
use gfs_search::query::{
  Completion, ExecutionStatus, Match, Query, SearchResult, TruncationReason,
};
use gfs_search::SearchOutcome;
use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::BytePath;

use crate::client::SnapshotClient;

/// The name of an ignore file, in one place.
const GITIGNORE: &[u8] = b".gitignore";

/// What a search returns to the CLI.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SearchReport {
  pub base_commit: String,
  pub ref_name: Option<String>,
  /// Matches found in the overlay rather than in the pinned commit. Reported so
  /// an agent can tell its own edits apart from the repository's content.
  pub local_matches: usize,
  pub outcome: SearchOutcome,
}

/// What the caller asked for.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SearchRequest {
  pub pattern: String,
  pub literal: bool,
  pub case_insensitive: bool,
  pub scope: Vec<u8>,
  pub include_globs: Vec<String>,
  pub exclude_globs: Vec<String>,
  pub context_before: u32,
  pub context_after: u32,
  pub max_results: u32,
  /// Cap on the bytes of any one line returned for display. Zero means the
  /// default. Only ever narrows it: the default is also the ceiling, because the
  /// memory it bounds is the daemon's.
  #[serde(default)]
  pub max_line_bytes: u32,
  /// Search files the workspace's ignore rules would otherwise skip.
  pub search_ignored: bool,
}

/// How the overlay changes what the base results mean.
#[derive(Debug, Default)]
pub struct OverlayView {
  /// Paths whose base result the overlay overrides. Dropped from base results.
  touched: HashSet<Vec<u8>>,
  /// Directory prefixes under which the base is entirely hidden.
  masked: Vec<Vec<u8>>,
  /// `source path -> the path the workspace keeps that content at`.
  ///
  /// Only for rows whose bytes are still the pinned commit's. This is what makes
  /// a rename or a mode change free: the server already searched those bytes.
  rehomed: HashMap<Vec<u8>, Vec<u8>>,
  /// Local files with their own bytes, to be searched here.
  locals: Vec<LocalPath>,
}

impl OverlayView {
  /// Derive the view from the journal alone. No base access, no network.
  pub fn of(overlay: &Overlay) -> OverlayView {
    let mut view = OverlayView::default();
    for entry in overlay.entries() {
      let path = entry.path.as_bytes().to_vec();
      view.touched.insert(path.clone());

      if !entry.present {
        // A whiteout hides the path and, if it was a directory, everything under
        // it. Masking unconditionally is correct and cheap: a whiteout on a file
        // has no descendants for the prefix to catch.
        view.masked.push(path);
        continue;
      }

      match entry.kind {
        OverlayKind::Directory => {
          if entry.opaque {
            // A created directory shadows whatever the base has at that path.
            view.masked.push(path);
          }
        }
        OverlayKind::Symlink => {
          // Outside the searchable corpus on both sides; the base result is
          // dropped by `touched` and nothing replaces it.
        }
        OverlayKind::Regular | OverlayKind::Executable => {
          // A file where the base had a directory hides that directory's
          // children too.
          if entry.base.as_ref().is_some_and(|b| b.kind.is_dir_like()) {
            view.masked.push(path.clone());
          }
          match &entry.content {
            Content::Base(_) => {
              // Bytes unchanged. The server searched them at the source path.
              let source = entry
                .renamed_from
                .as_ref()
                .map(|p| p.as_bytes().to_vec())
                .unwrap_or_else(|| path.clone());
              view.rehomed.insert(source, path);
            }
            Content::Local(_) => view.locals.push(LocalPath {
              path,
              tracked_in_base: entry.base.is_some(),
              size: entry.size,
            }),
            // A directory's content; unreachable for a file kind, and doing
            // nothing is the right answer if it ever were.
            Content::None => {}
          }
        }
      }
    }
    view.masked.sort();
    view
  }

  pub fn local_paths(&self) -> &[LocalPath] {
    &self.locals
  }

  /// Where a base match belongs now, or `None` if the workspace overrode it.
  ///
  /// `pub(crate)` because a filename search merges its base half exactly the same
  /// way a content search does — a renamed file is reported at its new path, a
  /// deleted one is not reported — and duplicating the rule would let the two
  /// answers disagree about the same workspace.
  pub(crate) fn place(&self, path: &[u8]) -> Option<Vec<u8>> {
    if let Some(destination) = self.rehomed.get(path) {
      return Some(destination.clone());
    }
    if self.touched.contains(path) {
      return None;
    }
    if self
      .masked
      .iter()
      .any(|prefix| gfs_search::local::within_scope(path, prefix) && path != prefix.as_slice())
    {
      return None;
    }
    Some(path.to_vec())
  }
}

/// Run a merged search.
///
/// `client` supplies the base half; `overlay` supplies the local half.
pub async fn search(
  client: &Arc<SnapshotClient>,
  overlay: &Arc<Overlay>,
  request: &SearchRequest,
) -> Result<(SearchOutcome, usize), GfsError> {
  let policy = CorpusPolicy::default();
  let query = build_query(request);

  let view = tokio::task::spawn_blocking({
    let overlay = Arc::clone(overlay);
    move || OverlayView::of(&overlay)
  })
  .await
  .map_err(|e| GfsError::internal(format!("the overlay scan task failed: {e}")))?;

  // The base half. Always the pinned commit: `SnapshotClient` is constructed
  // around one commit and has no method that takes a selector, so this cannot
  // accidentally become a branch query.
  let base = match client.search(&query, request.max_results).await {
    Ok(outcome) => outcome,
    // The server reports this whenever the snapshot manifest is absent, and its
    // `Search` never builds one — only `PrepareSnapshot` does. Asking and
    // retrying once is what turns a permanent failure into a first search that
    // is merely slow. `prepare_snapshot` waits up to the server's own bound
    // (ADR 0006: under 5 seconds to READY) before answering, so a false return
    // means the build is genuinely still running and the original error, which
    // is retryable and says so, is the honest thing to surface.
    Err(e) if e.code == ErrorCode::SnapshotBuilding => {
      if client.prepare_snapshot().await? {
        client.search(&query, request.max_results).await?
      } else {
        return Err(e);
      }
    }
    Err(e) => return Err(e),
  };
  let SearchOutcome::Completed(base) = base else {
    // The stream ended without a terminal message. Nothing local can repair
    // that: the base half's answer is unknown, so the merged answer is too.
    return Ok((base, 0));
  };

  let ignore = load_ignore_rules(client, overlay, &view, request).await?;
  let local = search_overlay(overlay, &view, &query, &policy, &ignore, request).await?;

  let local_matches = local.matches.len();
  Ok((merge(base, local, &view), local_matches))
}

fn build_query(request: &SearchRequest) -> Query {
  Query {
    pattern: request.pattern.clone(),
    literal: request.literal,
    case_insensitive: request.case_insensitive,
    scope: request.scope.clone(),
    include_globs: request
      .include_globs
      .iter()
      .map(|g| gfs_search::Glob::new(g))
      .collect(),
    exclude_globs: request
      .exclude_globs
      .iter()
      .map(|g| gfs_search::Glob::new(g))
      .collect(),
    context_before: request.context_before as usize,
    context_after: request.context_after as usize,
    start_after_path: None,
    budget: gfs_search::Budget {
      max_results: if request.max_results == 0 {
        gfs_search::Budget::default().max_results
      } else {
        request.max_results as usize
      },
      // `min`, not a plain override: the default is the ceiling as well as the
      // fallback. A client may ask for narrower lines than the daemon retains by
      // default; it may not ask the daemon to retain wider ones, because the
      // memory that bounds is not the client's.
      max_line_bytes: if request.max_line_bytes == 0 {
        gfs_search::Budget::default().max_line_bytes
      } else {
        (request.max_line_bytes as usize).min(gfs_search::Budget::default().max_line_bytes)
      },
      ..gfs_search::Budget::default()
    },
  }
}

/// Fetch the ignore files that could govern the overlay's untracked paths.
///
/// Only those. See the module docs: this is the one base access a local search
/// makes, and a clean workspace makes none.
async fn load_ignore_rules(
  client: &Arc<SnapshotClient>,
  overlay: &Arc<Overlay>,
  view: &OverlayView,
  request: &SearchRequest,
) -> Result<IgnoreRules, GfsError> {
  let mut rules = IgnoreRules::default();
  if request.search_ignored {
    // Explicitly asked to ignore the ignore rules, so there is nothing to load
    // and nothing to fetch.
    return Ok(rules);
  }

  let mut directories: Vec<Vec<u8>> = Vec::new();
  for local in &view.locals {
    if local.tracked_in_base {
      continue;
    }
    // Every ancestor directory of an untracked path, root first.
    let mut prefix: Vec<u8> = Vec::new();
    directories.push(Vec::new());
    for component in BytePath::new(local.path.clone()).components() {
      if !prefix.is_empty() {
        prefix.push(b'/');
      }
      prefix.extend_from_slice(component);
      directories.push(prefix.clone());
    }
    // The last element is the file itself, not a directory.
    directories.pop();
  }
  directories.sort();
  directories.dedup();

  for directory in directories {
    let path = if directory.is_empty() {
      BytePath::new(GITIGNORE.to_vec())
    } else {
      BytePath::new(directory.clone()).join(GITIGNORE)
    };

    // The overlay first: a `.gitignore` the job wrote or edited is the one that
    // applies, and reading it costs nothing.
    if let Some(bytes) = read_overlay_file(overlay, &path).await? {
      rules.add_file(&directory, &bytes);
      continue;
    }
    if view.place(path.as_bytes()).is_none() {
      // Deleted or masked by the overlay. The base's copy no longer applies.
      continue;
    }
    if let Some(bytes) = read_base_file(client, &path).await? {
      rules.add_file(&directory, &bytes);
    }
  }
  Ok(rules)
}

async fn read_overlay_file(
  overlay: &Arc<Overlay>,
  path: &BytePath,
) -> Result<Option<Vec<u8>>, GfsError> {
  let overlay = Arc::clone(overlay);
  let path = path.clone();
  tokio::task::spawn_blocking(move || {
    let gfs_overlay::Resolution::Overlay(entry) = overlay.resolve(&path) else {
      return Ok(None);
    };
    if !matches!(entry.content, Content::Local(_)) {
      return Ok(None);
    }
    let mut file = overlay
      .open_content(&entry)
      .map_err(crate::fs::overlay_as_service_error)?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes)
      .map_err(|e| GfsError::internal(format!("reading a local ignore file: {e}")))?;
    Ok(Some(bytes))
  })
  .await
  .map_err(|e| GfsError::internal(format!("the ignore-file task failed: {e}")))?
}

async fn read_base_file(
  client: &Arc<SnapshotClient>,
  path: &BytePath,
) -> Result<Option<Vec<u8>>, GfsError> {
  let Some(entry) = client.get_entry(path, true).await? else {
    return Ok(None);
  };
  if !entry.kind.has_blob_content() {
    return Ok(None);
  }
  let ticket = entry.blob_ticket.clone().unwrap_or_default();
  Ok(Some(client.read_blob(&entry.oid, &ticket).await?))
}

async fn search_overlay(
  overlay: &Arc<Overlay>,
  view: &OverlayView,
  query: &Query,
  policy: &CorpusPolicy,
  ignore: &IgnoreRules,
  request: &SearchRequest,
) -> Result<gfs_search::LocalOutcome, GfsError> {
  if view.locals.is_empty() {
    return Ok(gfs_search::LocalOutcome::default());
  }
  let overlay = Arc::clone(overlay);
  let locals = view.locals.clone();
  let query = query.clone();
  let policy = policy.clone();
  let ignore = ignore.clone();
  let search_ignored = request.search_ignored;

  tokio::task::spawn_blocking(move || {
    gfs_search::search_local(
      &locals,
      &query,
      &policy,
      &ignore,
      &LocalBudget::default(),
      search_ignored,
      |local| {
        let path = BytePath::new(local.path.clone());
        let gfs_overlay::Resolution::Overlay(entry) = overlay.resolve(&path) else {
          // Unlinked between building the view and reading it.
          return Ok(None);
        };
        let mut file = overlay
          .open_content(&entry)
          .map_err(crate::fs::overlay_as_service_error)?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes)
          .map_err(|e| GfsError::internal(format!("reading local content: {e}")))?;
        Ok(Some(bytes))
      },
    )
  })
  .await
  .map_err(|e| GfsError::internal(format!("the local search task failed: {e}")))?
}

/// Combine the two halves into one answer.
fn merge(base: SearchResult, local: gfs_search::LocalOutcome, view: &OverlayView) -> SearchOutcome {
  let SearchResult {
    matches: base_matches,
    mut completion,
  } = base;

  let mut matches: Vec<Match> = Vec::with_capacity(base_matches.len() + local.matches.len());
  let mut dropped_paths: HashSet<Vec<u8>> = HashSet::new();
  for mut m in base_matches {
    match view.place(&m.path) {
      Some(placed) => {
        m.path = placed;
        matches.push(m);
      }
      None => {
        dropped_paths.insert(m.path);
      }
    }
  }
  matches.extend(local.matches);

  // One ordering over both halves, so a merged answer is as deterministic as
  // either half alone.
  matches.sort_by(|a, b| {
    a.path
      .cmp(&b.path)
      .then(a.line.cmp(&b.line))
      .then(a.column.cmp(&b.column))
  });

  // Coverage: the base's, adjusted for paths the overlay overrode, plus the
  // local half's own exclusions.
  let mut excluded: BTreeMap<String, u64> = completion.coverage.excluded.clone();
  for (reason, count) in &local.excluded {
    *excluded.entry(reason.as_str().to_owned()).or_default() += count;
  }
  completion.coverage.excluded = excluded;
  completion.coverage.eligible_paths = completion
    .coverage
    .eligible_paths
    .saturating_sub(dropped_paths.len() as u64)
    + local.eligible_paths;

  // Execution status: truncated if *either* half was.
  if local.truncated {
    completion.execution_status = ExecutionStatus::Truncated;
    completion
      .truncation
      .get_or_insert(TruncationReason::ResultLimit);
    completion
      .stop_budget
      .get_or_insert_with(|| "the local overlay scan hit its budget".to_owned());
  }
  completion.bytes_read += local.bytes_read;

  SearchOutcome::Completed(SearchResult {
    matches,
    completion,
  })
}

/// Re-exported so the CLI does not have to name `gfs_search` to build a report.
pub use gfs_search::exit_code;

/// The completion an empty local-only search would produce.
///
/// Used when the base half could not be consulted at all. Kept out of the happy
/// path deliberately: it exists so a caller cannot be handed a `SearchResult`
/// with no completion in it.
pub fn empty_completion(commit: &str) -> Completion {
  Completion {
    execution_status: ExecutionStatus::Truncated,
    truncation: Some(TruncationReason::BackendFailure),
    coverage: Default::default(),
    index_generation: 0,
    commit: commit.to_owned(),
    stop_budget: Some("the base half of the search was unavailable".to_owned()),
    candidates_considered: 0,
    bytes_read: 0,
    elapsed_ms: 0,
  }
}

/// The exclusion vocabulary, for callers rendering a coverage report.
pub fn exclusion_reasons() -> &'static [ExclusionReason] {
  &[
    ExclusionReason::Binary,
    ExclusionReason::Oversized,
    ExclusionReason::InvalidUtf8,
    ExclusionReason::Generated,
    ExclusionReason::Vendored,
    ExclusionReason::IndexGap,
  ]
}
