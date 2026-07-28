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

use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::{
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
pub type EntryLookup = Result<Option<gfs_types::TreeEntryInfo>, GfsError>;

/// One directory page: the entries and the token to resume after.
#[derive(Debug, Clone)]
pub struct DirectoryPage {
  pub entries: Vec<gfs_types::TreeEntryInfo>,
  /// `None` when the page reached the end of the directory.
  pub next_page_token: Option<Vec<u8>>,
}

/// One file a recursive tree walk found.
///
/// Only regular and executable files appear. That is the searchable corpus, and
/// it is chosen to agree with `rg`: ripgrep does not follow symlinks by default,
/// so a symlink's target text is not searched by either tool, and a gitlink's
/// contents live in another repository entirely. Including them would make GFS
/// return matches `rg` does not, which is a worse failure than missing some.
///
/// `size` comes from the object header rather than from inflating the blob, so a
/// walk over a monorepo costs one header read per file and no decompression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalkEntry {
  /// Full path from the commit's root, even when the walk started in a subtree.
  pub path: BytePath,
  pub mode: u32,
  pub oid: ObjectId,
  pub size: u64,
}

/// How one path differs between two commits.
///
/// Deliberately two cases rather than Git's five. A manifest is a map from path
/// to blob, so "modified", "added", and "type changed" are all the same
/// operation on it — write this path's new blob — and "deleted" is the only
/// other thing that can happen. Rename detection is *not* enabled: a rename is
/// a delete plus an add to a path map, and asking libgit2 to pair them up would
/// cost similarity scoring for information the manifest cannot use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeDelta {
  Removed {
    path: BytePath,
  },
  Upserted {
    path: BytePath,
    mode: u32,
    oid: ObjectId,
    size: u64,
  },
}

impl TreeDelta {
  pub fn path(&self) -> &BytePath {
    match self {
      TreeDelta::Removed { path } | TreeDelta::Upserted { path, .. } => path,
    }
  }
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
  fn resolve(&self, selector: &RevisionSelector) -> Result<ResolvedRevision, GfsError>;

  fn read_commit(&self, commit: &ObjectId) -> Result<CommitMeta, GfsError>;

  /// Walk `commit`'s ancestry, newest first, in Git's own topological-and-date
  /// order — the order `git log` prints.
  ///
  /// Returns at most `limit` commits after skipping `skip`, and whether the walk
  /// stopped with ancestry still unvisited. The caller gets that flag rather than
  /// inferring it from a full page, because a history whose length is an exact
  /// multiple of the page size would otherwise look truncated forever.
  ///
  /// Paging is by `skip` rather than a cursor: a revwalk from a pinned commit is
  /// deterministic, so repeating the prefix costs traversal but cannot produce a
  /// page that disagrees with the one before it.
  fn log(
    &self,
    commit: &ObjectId,
    skip: usize,
    limit: usize,
  ) -> Result<(Vec<CommitMeta>, bool), GfsError>;

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
  ) -> Result<DirectoryPage, GfsError>;

  /// Walk every searchable file under `root`, recursively.
  ///
  /// `root` is a directory path from the commit root, or empty for the whole
  /// tree. Emitted paths are always full paths, so a caller that fanned the walk
  /// out over top-level directories can concatenate the results without knowing
  /// which task produced which entry.
  ///
  /// The visitor is called in Git's tree order within each directory. It returns
  /// a `Result` so a caller can stop the walk by returning an error — a budget or
  /// a cancellation, which a walk over a million-entry tree needs.
  ///
  /// A `root` that does not exist, or is not a directory, is an error rather than
  /// an empty walk: a manifest built from a mistyped scope that silently produced
  /// nothing would be indistinguishable from a repository with nothing in it.
  fn walk_tree(
    &self,
    commit: &ObjectId,
    root: &BytePath,
    visit: &mut dyn FnMut(WalkEntry) -> Result<(), GfsError>,
  ) -> Result<(), GfsError>;

  /// Walk every *named* entry under `root`, recursively: files, symlinks,
  /// gitlinks, and modes GFS does not model. Directories are recursed into but
  /// not themselves emitted, which is the set `git ls-files` reports.
  ///
  /// Deliberately not [`GitRepository::walk_tree`]. That walk's corpus is the
  /// *searchable* one — it drops symlinks and gitlinks to agree with `rg`, and
  /// reads a blob header per file to get a size. Answering a filename query from
  /// it would silently omit every symlink in the repository, which on the M0.1
  /// corpus is 99 paths in linux and 4 in django, and would charge an object
  /// lookup per file for a size the caller never asked about.
  fn walk_paths(
    &self,
    commit: &ObjectId,
    root: &BytePath,
    visit: &mut dyn FnMut(BytePath, u32) -> Result<(), GfsError>,
  ) -> Result<(), GfsError>;

  /// The paths that differ between two commits' trees.
  ///
  /// Used for first-parent incremental manifest construction, which is what makes
  /// preparing the next commit on a branch cost its diff rather than its tree.
  fn diff_commits(&self, from: &ObjectId, to: &ObjectId) -> Result<Vec<TreeDelta>, GfsError>;

  /// Read a whole blob, verifying its object ID against its contents.
  fn read_blob(&self, blob: &ObjectId) -> Result<Vec<u8>, GfsError>;

  fn blob_size(&self, blob: &ObjectId) -> Result<u64, GfsError>;

  /// Every ref a caller may see. Excludes the reserved internal namespace.
  fn visible_refs(&self) -> Result<Vec<(String, ObjectId)>, GfsError>;

  /// Every ref *inside* the reserved internal namespace.
  ///
  /// Separated from [`Self::visible_refs`] rather than offered as a flag, because
  /// the two have opposite audiences. `visible_refs` answers a request; this
  /// answers a reconciliation or maintenance pass, and nothing on a request path
  /// should be able to enumerate lease anchors -- ADR 0002 records that hiding
  /// them prevents discovery, and a `list_refs(include_internal: bool)` would put
  /// the difference one mistaken argument away.
  fn reserved_refs(&self) -> Result<Vec<String>, GfsError>;

  /// Whether a commit is reachable from any currently visible ref.
  ///
  /// This is the predicate M1.5's object-authorization rule turns on: a commit
  /// that is *not* reachable can only be reached with a mount capability.
  fn is_visible(&self, commit: &ObjectId) -> Result<bool, GfsError>;

  /// Create the lease anchor ref for a mount, pointing at `commit`.
  ///
  /// Must fail rather than overwrite if the ref already points elsewhere, so a
  /// mount ID collision cannot silently re-anchor another mount's lease.
  fn create_lease_anchor(&self, anchor_ref: &str, commit: &ObjectId) -> Result<(), GfsError>;

  /// Remove a lease anchor. Succeeds if it is already absent, so release and
  /// restart reconciliation are both idempotent.
  fn delete_lease_anchor(&self, anchor_ref: &str) -> Result<(), GfsError>;

  /// The commit an existing lease anchor points at, if it exists.
  fn read_lease_anchor(&self, anchor_ref: &str) -> Result<Option<ObjectId>, GfsError>;

  fn tree_cache_stats(&self) -> TreeCacheStats;

  // ---- The write half ----
  //
  // Everything above reads history. The four below create objects, and they are
  // the only way anything in GFS does. Two rules bound them, both enforced here
  // rather than by the callers:
  //
  // * a ref may only be written inside the reserved namespace, because a branch
  //   under `refs/heads/` that upstream does not have is deleted by the next
  //   mirror fetch -- it runs `--prune`, silently, as routine maintenance;
  // * a ref update is a compare-and-swap, because two views of one mirror may be
  //   committing to the same work branch at the same moment, and last-write-wins
  //   would drop a commit while reporting success.

  /// Build a tree by applying `changes` to `base`'s tree.
  ///
  /// `base` is `None` for an orphan tree. Intermediate directories are created
  /// and emptied ones removed, so a caller supplies only the paths that changed.
  fn write_tree(
    &self,
    base: Option<&ObjectId>,
    changes: &[TreeChange],
  ) -> Result<ObjectId, GfsError>;

  /// Create a commit object. Updates no ref; [`GitRepository::update_work_ref`]
  /// does that, so the caller can decide what to do if the branch moved.
  fn create_commit(
    &self,
    tree: &ObjectId,
    parents: &[ObjectId],
    author: &CommitSignature,
    committer: &CommitSignature,
    message: &str,
  ) -> Result<ObjectId, GfsError>;

  /// Point a reserved-namespace ref at a commit, if it still holds `expected`.
  ///
  /// `expected` is `None` to *create* the ref, and the creation fails if it
  /// already exists. Anything outside the reserved namespace is refused.
  fn update_work_ref(
    &self,
    name: &str,
    new: &ObjectId,
    expected: Option<&ObjectId>,
  ) -> Result<(), GfsError>;

  /// The commit a ref points at, or `None` when it does not exist.
  fn read_ref(&self, name: &str) -> Result<Option<ObjectId>, GfsError>;
}

/// One path's new state when building a tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeChange {
  pub path: BytePath,
  pub kind: TreeChangeKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeChangeKind {
  /// Create or replace the path with this content and mode.
  Upsert { mode: u32, content: Vec<u8> },
  /// Remove the path.
  Delete,
}

/// An author or committer.
///
/// Carries its own timestamp rather than taking "now" inside the repository
/// layer, so a commit is a pure function of its inputs and a test can assert an
/// exact object ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitSignature {
  pub name: String,
  pub email: String,
  /// Seconds since the Unix epoch.
  pub when_secs: i64,
  /// Offset from UTC in minutes, as Git records it.
  pub offset_minutes: i32,
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
  pub async fn run<T, F>(&self, op: F) -> Result<T, GfsError>
  where
    T: Send + 'static,
    F: FnOnce(&dyn GitRepository) -> Result<T, GfsError> + Send + 'static,
  {
    let permit = self
      .permits
      .clone()
      .acquire_owned()
      .await
      // Only fails if the semaphore was closed, which this code never does.
      .map_err(|_| GfsError::new(ErrorCode::Unavailable, "repository pool is shut down"))?;
    let inner = Arc::clone(&self.inner);
    tokio::task::spawn_blocking(move || {
      let out = op(inner.as_ref());
      drop(permit);
      out
    })
    .await
    .map_err(|e| {
      if e.is_cancelled() {
        GfsError::new(ErrorCode::Cancelled, "repository operation was cancelled")
      } else {
        // A panic inside libgit2 glue. Reported as Internal without the panic
        // payload, which can contain arbitrary text.
        GfsError::new(ErrorCode::Internal, "repository operation failed")
      }
    })?
  }

  pub async fn resolve(&self, selector: RevisionSelector) -> Result<ResolvedRevision, GfsError> {
    self.run(move |r| r.resolve(&selector)).await
  }

  pub async fn read_commit(&self, commit: ObjectId) -> Result<CommitMeta, GfsError> {
    self.run(move |r| r.read_commit(&commit)).await
  }

  pub async fn log(
    &self,
    commit: ObjectId,
    skip: usize,
    limit: usize,
  ) -> Result<(Vec<CommitMeta>, bool), GfsError> {
    self.run(move |r| r.log(&commit, skip, limit)).await
  }

  /// Every named entry under `root`, as `(path, mode)`.
  ///
  /// Collected rather than streamed, for the reason [`AsyncRepository::walk_tree`]
  /// gives: a streaming version would hold a blocking thread for as long as the
  /// consumer took, which defeats the pool's admission control.
  pub async fn walk_paths(
    &self,
    commit: ObjectId,
    root: BytePath,
  ) -> Result<Vec<(BytePath, u32)>, GfsError> {
    self
      .run(move |r| {
        let mut out = Vec::new();
        r.walk_paths(&commit, &root, &mut |path, mode| {
          out.push((path, mode));
          Ok(())
        })?;
        Ok(out)
      })
      .await
  }

  pub async fn entry(&self, commit: ObjectId, path: BytePath) -> EntryLookup {
    self.run(move |r| r.entry(&commit, &path)).await
  }

  pub async fn batch_entries(
    &self,
    commit: ObjectId,
    paths: Vec<BytePath>,
  ) -> Result<Vec<EntryLookup>, GfsError> {
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
  ) -> Result<DirectoryPage, GfsError> {
    self
      .run(move |r| r.list_directory(&commit, &path, after.as_deref(), limit))
      .await
  }

  pub async fn read_blob(&self, blob: ObjectId) -> Result<Vec<u8>, GfsError> {
    self.run(move |r| r.read_blob(&blob)).await
  }

  /// Collect a subtree's searchable files.
  ///
  /// Returns a `Vec` rather than streaming, because the value of this call is
  /// that a caller can issue several of them **concurrently** — one per top-level
  /// directory — and have the pool's admission control decide how many libgit2
  /// handles are busy at once. A streaming version would hold a blocking thread
  /// for as long as the consumer took, which defeats the bound.
  pub async fn walk_tree(
    &self,
    commit: ObjectId,
    root: BytePath,
  ) -> Result<Vec<WalkEntry>, GfsError> {
    self
      .run(move |r| {
        let mut out = Vec::new();
        r.walk_tree(&commit, &root, &mut |entry| {
          out.push(entry);
          Ok(())
        })?;
        Ok(out)
      })
      .await
  }

  pub async fn diff_commits(
    &self,
    from: ObjectId,
    to: ObjectId,
  ) -> Result<Vec<TreeDelta>, GfsError> {
    self.run(move |r| r.diff_commits(&from, &to)).await
  }

  pub async fn visible_refs(&self) -> Result<Vec<(String, ObjectId)>, GfsError> {
    self.run(move |r| r.visible_refs()).await
  }

  pub async fn is_visible(&self, commit: ObjectId) -> Result<bool, GfsError> {
    self.run(move |r| r.is_visible(&commit)).await
  }

  pub async fn write_tree(
    &self,
    base: Option<ObjectId>,
    changes: Vec<TreeChange>,
  ) -> Result<ObjectId, GfsError> {
    self
      .run(move |r| r.write_tree(base.as_ref(), &changes))
      .await
  }

  pub async fn create_commit(
    &self,
    tree: ObjectId,
    parents: Vec<ObjectId>,
    author: CommitSignature,
    committer: CommitSignature,
    message: String,
  ) -> Result<ObjectId, GfsError> {
    self
      .run(move |r| r.create_commit(&tree, &parents, &author, &committer, &message))
      .await
  }

  pub async fn update_work_ref(
    &self,
    name: String,
    new: ObjectId,
    expected: Option<ObjectId>,
  ) -> Result<(), GfsError> {
    self
      .run(move |r| r.update_work_ref(&name, &new, expected.as_ref()))
      .await
  }

  pub async fn read_ref(&self, name: String) -> Result<Option<ObjectId>, GfsError> {
    self.run(move |r| r.read_ref(&name)).await
  }
}
