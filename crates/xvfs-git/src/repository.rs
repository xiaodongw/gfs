//! The `GitRepository` trait and its async admission-controlled wrapper.
//!
//! DESIGN.md section 6.1 requires libgit2 to sit behind a trait so FFI lifetimes,
//! blocking work, library upgrades, and format quirks do not leak into HTTP,
//! search, or FUSE code. The trait boundary also exists because libgit2 does not
//! cover every repository format stock Git can produce, so supported formats are
//! a validated property of a mirror rather than an assumption.
//!
//! The trait is **synchronous**, matching libgit2's nature, and
//! [`AsyncRepository`] is the wrapper every request path uses.

use std::sync::Arc;

use xvfs_types::error::{ErrorCode, XvfsError};
use xvfs_types::{
  BytePath, CommitMeta, HashAlgorithm, ObjectId, ResolvedRevision, RevisionSelector,
};

use crate::format::RepositoryFormat;
use crate::tree::TreeCacheStats;

/// The result of looking up one path: found, absent, or failed.
///
/// The nesting is deliberate. `Ok(None)` is an absent path, which is an ordinary
/// negative lookup and the single most common result on a FUSE `lookup` path;
/// `Err` is a real failure. Collapsing the two -- by returning `Err(NotFound)` --
/// would make a routine miss indistinguishable from a corrupt object.
pub type EntryLookup = Result<Option<xvfs_types::TreeEntryInfo>, XvfsError>;

/// One directory page: the entries and the token to resume after.
#[derive(Debug, Clone)]
pub struct DirectoryPage {
  pub entries: Vec<xvfs_types::TreeEntryInfo>,
  /// `None` when the page reached the end of the directory.
  pub next_page_token: Option<Vec<u8>>,
}

/// Read access to one bare repository.
///
/// Implementations must be `Send + Sync`. Every method is allowed to block.
pub trait GitRepository: Send + Sync + std::fmt::Debug {
  /// The repository's hash algorithm, needed to parse any caller-supplied OID.
  fn algorithm(&self) -> HashAlgorithm;

  fn format(&self) -> &RepositoryFormat;

  /// Resolve a validated selector to a pinned commit.
  ///
  /// Takes a [`RevisionSelector`] rather than a `&str` on purpose: the closed
  /// four-shape grammar is enforced by the type, so no implementation can be
  /// handed a `revparse` expression. `ref_version` is not known here -- it is a
  /// catalog fact -- and is left zero for the catalog to fill in.
  fn resolve(&self, selector: &RevisionSelector) -> Result<ResolvedRevision, XvfsError>;

  fn read_commit(&self, commit: &ObjectId) -> Result<CommitMeta, XvfsError>;

  /// Look up one path in a commit's tree. `Ok(None)` means the path is absent,
  /// which is an ordinary result rather than an error.
  fn entry(&self, commit: &ObjectId, path: &BytePath) -> EntryLookup;

  /// Look up many paths in one pass, reusing decoded trees across them.
  ///
  /// The per-path result is itself a `Result`, so one unreadable path does not
  /// discard the rest of the batch.
  fn batch_entries(&self, commit: &ObjectId, paths: &[BytePath]) -> Vec<EntryLookup>;

  fn list_directory(
    &self,
    commit: &ObjectId,
    path: &BytePath,
    after: Option<&[u8]>,
    limit: usize,
  ) -> Result<DirectoryPage, XvfsError>;

  /// Read a whole blob, verifying its object ID against its contents.
  fn read_blob(&self, blob: &ObjectId) -> Result<Vec<u8>, XvfsError>;

  fn blob_size(&self, blob: &ObjectId) -> Result<u64, XvfsError>;

  /// Every ref a caller may see. Excludes the reserved internal namespace.
  fn visible_refs(&self) -> Result<Vec<(String, ObjectId)>, XvfsError>;

  /// Whether a commit is reachable from any currently visible ref.
  ///
  /// This is the predicate M1.5's object-authorization rule turns on: a commit
  /// that is *not* reachable can only be reached with a mount capability.
  fn is_visible(&self, commit: &ObjectId) -> Result<bool, XvfsError>;

  /// Create the lease anchor ref for a mount, pointing at `commit`.
  ///
  /// Must fail rather than overwrite if the ref already points elsewhere, so a
  /// mount ID collision cannot silently re-anchor another mount's lease.
  fn create_lease_anchor(&self, anchor_ref: &str, commit: &ObjectId) -> Result<(), XvfsError>;

  /// Remove a lease anchor. Succeeds if it is already absent, so release and
  /// restart reconciliation are both idempotent.
  fn delete_lease_anchor(&self, anchor_ref: &str) -> Result<(), XvfsError>;

  /// The commit an existing lease anchor points at, if it exists.
  fn read_lease_anchor(&self, anchor_ref: &str) -> Result<Option<ObjectId>, XvfsError>;

  fn tree_cache_stats(&self) -> TreeCacheStats;
}

/// An async facade that applies admission control before touching libgit2.
///
/// The ordering here is the whole point. A permit is acquired **asynchronously**,
/// before entering `spawn_blocking`, so a caller that has to wait parks on a
/// Tokio semaphore rather than occupying a blocking thread while it waits on the
/// pool's condvar. With the permit count equal to the pool's handle count, the
/// checkout inside the blocking closure never waits.
///
/// Doing it the other way -- `spawn_blocking` first, then wait for a handle --
/// would still bound libgit2 concurrency, but a burst of a thousand requests
/// would occupy a thousand blocking threads doing nothing. Tokio's blocking pool
/// defaults to 512 threads, so the burst would exhaust it and stall unrelated
/// work such as overlay writes.
#[derive(Debug, Clone)]
pub struct AsyncRepository {
  inner: Arc<dyn GitRepository>,
  permits: Arc<tokio::sync::Semaphore>,
}

impl AsyncRepository {
  /// `concurrency` should equal the handle pool's bound.
  pub fn new(inner: Arc<dyn GitRepository>, concurrency: usize) -> Self {
    AsyncRepository {
      inner,
      permits: Arc::new(tokio::sync::Semaphore::new(concurrency.max(1))),
    }
  }

  pub fn algorithm(&self) -> HashAlgorithm {
    self.inner.algorithm()
  }

  pub fn blocking(&self) -> &Arc<dyn GitRepository> {
    &self.inner
  }

  /// Run one blocking repository operation under admission control.
  pub async fn run<T, F>(&self, op: F) -> Result<T, XvfsError>
  where
    T: Send + 'static,
    F: FnOnce(&dyn GitRepository) -> Result<T, XvfsError> + Send + 'static,
  {
    let permit = self
      .permits
      .clone()
      .acquire_owned()
      .await
      // Only fails if the semaphore was closed, which this code never does.
      .map_err(|_| XvfsError::new(ErrorCode::Unavailable, "repository pool is shut down"))?;
    let inner = Arc::clone(&self.inner);
    tokio::task::spawn_blocking(move || {
      let out = op(inner.as_ref());
      drop(permit);
      out
    })
    .await
    .map_err(|e| {
      if e.is_cancelled() {
        XvfsError::new(ErrorCode::Cancelled, "repository operation was cancelled")
      } else {
        // A panic inside libgit2 glue. Reported as Internal without the panic
        // payload, which can contain arbitrary text.
        XvfsError::new(ErrorCode::Internal, "repository operation failed")
      }
    })?
  }

  pub async fn resolve(&self, selector: RevisionSelector) -> Result<ResolvedRevision, XvfsError> {
    self.run(move |r| r.resolve(&selector)).await
  }

  pub async fn read_commit(&self, commit: ObjectId) -> Result<CommitMeta, XvfsError> {
    self.run(move |r| r.read_commit(&commit)).await
  }

  pub async fn entry(&self, commit: ObjectId, path: BytePath) -> EntryLookup {
    self.run(move |r| r.entry(&commit, &path)).await
  }

  pub async fn batch_entries(
    &self,
    commit: ObjectId,
    paths: Vec<BytePath>,
  ) -> Result<Vec<EntryLookup>, XvfsError> {
    self
      .run(move |r| Ok(r.batch_entries(&commit, &paths)))
      .await
  }

  pub async fn list_directory(
    &self,
    commit: ObjectId,
    path: BytePath,
    after: Option<Vec<u8>>,
    limit: usize,
  ) -> Result<DirectoryPage, XvfsError> {
    self
      .run(move |r| r.list_directory(&commit, &path, after.as_deref(), limit))
      .await
  }

  pub async fn read_blob(&self, blob: ObjectId) -> Result<Vec<u8>, XvfsError> {
    self.run(move |r| r.read_blob(&blob)).await
  }

  pub async fn visible_refs(&self) -> Result<Vec<(String, ObjectId)>, XvfsError> {
    self.run(move |r| r.visible_refs()).await
  }

  pub async fn is_visible(&self, commit: ObjectId) -> Result<bool, XvfsError> {
    self.run(move |r| r.is_visible(&commit)).await
  }
}
