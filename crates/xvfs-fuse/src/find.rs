//! Filename search over the merged workspace.
//!
//! The `find` half of what `xvfs-rg` is for content: an agent asking "which files
//! are called this?" without walking the mount. The server answers for the pinned
//! commit's tree in one round trip; the overlay's own changes are merged here.
//!
//! # Why this is not `git ls-files` through the shim
//!
//! The shim answered `ls-files` by listing one directory at a time through the
//! snapshot API. That is one round trip per directory, which on a 7 000-file
//! repository was measured at 56 seconds and hydrated nothing — the cost was
//! entirely round trips. `FindPaths` walks the tree server-side and returns the
//! matching set, so the same question is one request.
//!
//! # The merge is the same rule as content search
//!
//! A base path is placed through [`OverlayView::place`], so a renamed file is
//! reported at its new path, a deleted one is not reported, and a file the
//! workspace created is added. Sharing the rule with `search` is deliberate: two
//! answers about the same workspace that disagreed on which files exist would be
//! worse than either being wrong on its own.
//!
//! One consequence is worth stating. A rename moves a path *out* of a glob's
//! reach as well as into it — renaming `lorem_ipsum.py` to `lorem_text.py` means
//! `*ipsum*` must stop matching it — so the globs are re-checked against the
//! placed path rather than trusted from the server's answer.

use std::sync::Arc;

use xvfs_overlay::Overlay;
use xvfs_types::error::XvfsError;
use xvfs_types::BytePath;

use crate::client::SnapshotClient;
use crate::search::OverlayView;

/// What to look for.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct FindRequest {
  /// Directory prefix to search. Empty searches the whole workspace.
  pub scope: Vec<u8>,
  /// A path matches when any include glob matches it, and no exclude glob does.
  /// No includes means every path in scope.
  pub include_globs: Vec<String>,
  pub exclude_globs: Vec<String>,
  /// Maximum paths to return. Zero takes the client default.
  pub max_results: u32,
}

/// What was found, and whether that is all of it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct FindReport {
  pub base_commit: String,
  pub ref_name: Option<String>,
  /// Matching paths, ordered by path so two runs of one query agree.
  pub paths: Vec<BytePath>,
  /// True when a limit stopped the search. Distinct from an empty result for the
  /// same reason ADR 0004 separates them for content search: "nothing matched"
  /// and "I stopped early" are different answers.
  pub truncated: bool,
}

/// How many paths a request without an explicit limit returns.
///
/// Generous because a filename search is a discovery step and a truncated answer
/// sends an agent looking for a narrower glob it may not have.
pub const DEFAULT_MAX_RESULTS: usize = 10_000;

/// Run a merged filename search.
pub async fn find(
  client: &Arc<SnapshotClient>,
  overlay: &Arc<Overlay>,
  request: &FindRequest,
) -> Result<(Vec<BytePath>, bool), XvfsError> {
  let limit = if request.max_results == 0 {
    DEFAULT_MAX_RESULTS
  } else {
    request.max_results as usize
  };

  let view = tokio::task::spawn_blocking({
    let overlay = Arc::clone(overlay);
    move || OverlayView::of(&overlay)
  })
  .await
  .map_err(|e| XvfsError::internal(format!("the overlay scan task failed: {e}")))?;

  let scope = BytePath::new(request.scope.clone());
  scope.validate()?;

  let includes: Vec<xvfs_search::Glob> = request
    .include_globs
    .iter()
    .map(|g| xvfs_search::Glob::new(g))
    .collect();
  let excludes: Vec<xvfs_search::Glob> = request
    .exclude_globs
    .iter()
    .map(|g| xvfs_search::Glob::new(g))
    .collect();
  let matches = |path: &[u8]| -> bool {
    (includes.is_empty() || includes.iter().any(|g| g.matches(path)))
      && !excludes.iter().any(|g| g.matches(path))
  };

  // The base half is asked for more than the caller wants, because the overlay
  // can remove paths from it and a page trimmed to the limit before the merge
  // would come back short with nothing to say so.
  let (base, base_truncated) = client
    .find_paths(
      &scope,
      &request.include_globs,
      &request.exclude_globs,
      limit.saturating_add(1),
    )
    .await?;

  let mut paths: Vec<Vec<u8>> = Vec::new();
  for entry in base {
    if let Some(placed) = view.place(entry.path.as_bytes()) {
      // Re-checked against the placed path: a rename can move a file out of the
      // glob it matched at its old name.
      if matches(&placed) && xvfs_search::local::within_scope(&placed, request.scope.as_slice()) {
        paths.push(placed);
      }
    }
  }
  for local in view.local_paths() {
    if matches(&local.path) && xvfs_search::local::within_scope(&local.path, request.scope.as_slice())
    {
      paths.push(local.path.clone());
    }
  }

  paths.sort();
  paths.dedup();
  let truncated = base_truncated || paths.len() > limit;
  paths.truncate(limit);

  Ok((paths.into_iter().map(BytePath::new).collect(), truncated))
}
