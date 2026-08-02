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
  BlameHunk, BytePath, CommitMeta, DiffFileChange, DiffFormat, HashAlgorithm, ObjectId,
  ResolvedRevision, RevisionSelector,
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

/// One detected LFS entry at a revision (ADR 0012).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LfsEntry {
  pub path: BytePath,
  /// The pointer blob as Git stores it — the identity trees reference and the
  /// search index interns by, never the expanded content's key.
  pub blob_oid: ObjectId,
  /// The pointer blob's own size (~130 bytes), from the object header.
  pub blob_size: u64,
  /// What the pointer names: the expanded content's `lfs-sha256:` key and size.
  pub pointer: crate::lfs::LfsPointer,
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

/// What to diff, and how much of it to render.
///
/// Rendering happens **on the server**, not in the client, and that is the point
/// of the whole facility: the object database is here, so one round trip returns
/// Git's own byte format instead of a client walking trees and fetching blobs to
/// approximate it. See [`GitRepository::diff`].
#[derive(Clone, Debug)]
pub struct DiffRequest {
  /// `None` diffs against the empty tree, which is what a root commit needs.
  pub from: Option<ObjectId>,
  pub to: ObjectId,
  /// Path prefixes to limit the diff to. Empty means the whole tree.
  pub paths: Vec<BytePath>,
  pub format: DiffFormat,
  pub context_lines: u32,
  /// Ceiling on the rendered bytes. Reaching it sets `truncated` rather than
  /// failing: a caller reviewing a large commit is better served by the first
  /// megabyte and a flag than by an error.
  pub max_bytes: usize,
}

#[derive(Clone, Debug, Default)]
pub struct DiffOutput {
  pub rendered: Vec<u8>,
  /// Always populated, whatever the format, so a caller can act on the change
  /// set without parsing the rendering.
  pub files: Vec<DiffFileChange>,
  pub truncated: bool,
}

/// What a revwalk should cover.
#[derive(Clone, Debug, Default)]
pub struct LogOptions {
  pub skip: usize,
  pub limit: usize,
  /// Follow only first parents, so a merge's side branch is not walked. What
  /// `git log --first-parent` does, and the only way to read a history whose
  /// merges outnumber its commits.
  pub first_parent: bool,
  /// Show only commits that changed one of these paths, comparing each commit
  /// against its first parent. `git log -- <path>` without the rename
  /// following, which is a separate and much more expensive question.
  pub paths: Vec<BytePath>,
}

/// Blame hunks together with the bytes they describe.
///
/// The content travels with the hunks because a blame is useless without it and
/// the alternative — hunks now, blob over the ticketed HTTP path afterwards —
/// costs a second round trip and a ticket for one bounded file.
#[derive(Clone, Debug)]
pub struct BlameOutput {
  pub hunks: Vec<BlameHunk>,
  pub content: Vec<u8>,
  /// True when the file was too large to return, in which case `content` is
  /// empty and only the hunks are present.
  pub truncated: bool,
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

  /// Apply a parsed `~n`/`^n` chain by walking parent pointers.
  ///
  /// Separate from [`GitRepository::resolve`] and deliberately *not* implemented
  /// with `revparse`: the steps arrive already parsed
  /// ([`gfs_types::AncestryStep`]), so the only thing that happens here is
  /// `commit.parent(n)`, and there is no path by which `main^{tree}` becomes a
  /// tree OID in a commit-shaped field.
  fn walk_ancestry(
    &self,
    commit: &ObjectId,
    steps: &[gfs_types::AncestryStep],
  ) -> Result<ObjectId, GfsError>;

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
    options: &LogOptions,
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

  /// The same two trees, rendered for a reader rather than for a manifest.
  ///
  /// Distinct from [`GitRepository::diff_commits`] in what it is *for*, which is
  /// why the two exist side by side: that one answers "what must the search
  /// index write", so it has two cases and no rename detection; this one answers
  /// "what did this commit change", so it has Git's five statuses, finds
  /// renames, and produces bytes.
  fn diff(&self, request: &DiffRequest) -> Result<DiffOutput, GfsError>;

  /// Line-by-line attribution for one path at one commit, with the bytes.
  fn blame(
    &self,
    commit: &ObjectId,
    path: &BytePath,
    max_content_bytes: u64,
  ) -> Result<BlameOutput, GfsError>;

  /// Serialize a commit's tree as a Git index file (ADR 0009).
  ///
  /// The gateway builds this once per commit and ships it to every mount,
  /// because a client building it would walk the whole tree through the
  /// snapshot API — the sweep the projection exists to avoid. Entry stat data
  /// records `snapshot_time`, which is what makes one file valid on every host;
  /// see [`crate::index`] for the format and the racy-clean reasoning.
  fn index_for_commit(
    &self,
    commit: &ObjectId,
    snapshot_time: gfs_types::Timestamp,
  ) -> Result<Vec<u8>, GfsError>;

  /// Every LFS pointer reachable from the commit's tree: `filter=lfs` paths
  /// whose blobs parse as spec v1 pointers (ADR 0012). Detection only — the
  /// result is independent of whether any expanded object is available, which
  /// is what the store-population path needs to know what to fetch and what
  /// lets the search indexer key LFS paths by their pointer blobs.
  fn lfs_pointers(&self, commit: &ObjectId) -> Result<Vec<LfsEntry>, GfsError>;

  /// Whether `.gitattributes` at this revision says `filter=lfs` for `path`.
  /// This is the write path's question (ADR 0012): content committed to such
  /// a path must be re-cleaned into a pointer, whatever the content is.
  fn lfs_filtered(&self, commit: &ObjectId, path: &BytePath) -> Result<bool, GfsError>;

  /// Read a whole blob, verifying its object ID against its contents.
  fn read_blob(&self, blob: &ObjectId) -> Result<Vec<u8>, GfsError>;

  fn blob_size(&self, blob: &ObjectId) -> Result<u64, GfsError>;

  /// Every ref a caller may see, with annotated tags peeled. Excludes the
  /// reserved internal namespace.
  fn visible_ref_targets(&self) -> Result<Vec<gfs_types::RefTarget>, GfsError>;

  /// The same refs as names and direct targets, for the callers that reconcile
  /// or compare rather than serve.
  ///
  /// A default over [`Self::visible_ref_targets`] rather than a second
  /// implementation, so the reserved-namespace filter has exactly one home.
  fn visible_refs(&self) -> Result<Vec<(String, ObjectId)>, GfsError> {
    Ok(
      self
        .visible_ref_targets()?
        .into_iter()
        .map(|r| (r.name, r.target))
        .collect(),
    )
  }

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

  pub async fn lfs_filtered(&self, commit: ObjectId, path: BytePath) -> Result<bool, GfsError> {
    self.run(move |r| r.lfs_filtered(&commit, &path)).await
  }

  pub async fn lfs_pointers(&self, commit: ObjectId) -> Result<Vec<LfsEntry>, GfsError> {
    self.run(move |r| r.lfs_pointers(&commit)).await
  }

  /// Resolve a selector and then walk its ancestry, in one blocking operation.
  ///
  /// One operation rather than two so a `~50` walk does not take fifty trips
  /// through the semaphore and the blocking pool, and so the base commit and its
  /// ancestors are read through the same handle.
  pub async fn resolve_expression(
    &self,
    expression: gfs_types::RevisionExpression,
  ) -> Result<ResolvedRevision, GfsError> {
    self
      .run(move |r| {
        let mut resolved = r.resolve(&expression.base)?;
        if expression.steps.is_empty() {
          return Ok(resolved);
        }
        let walked = r.walk_ancestry(&resolved.commit, &expression.steps)?;
        let meta = r.read_commit(&walked)?;
        resolved.commit = meta.commit;
        resolved.tree = meta.tree;
        // The ref named the *base*, not the ancestor this walked to, and
        // reporting it here would tell a caller that `main~3` is where `main`
        // points. Dropped rather than kept, for the same reason `%d` is not a
        // supported log verb.
        resolved.ref_name = None;
        resolved.snapshot_time = meta.snapshot_time;
        Ok(resolved)
      })
      .await
  }

  pub async fn log(
    &self,
    commit: ObjectId,
    options: LogOptions,
  ) -> Result<(Vec<CommitMeta>, bool), GfsError> {
    self.run(move |r| r.log(&commit, &options)).await
  }

  pub async fn diff(&self, request: DiffRequest) -> Result<DiffOutput, GfsError> {
    self.run(move |r| r.diff(&request)).await
  }

  pub async fn blame(
    &self,
    commit: ObjectId,
    path: BytePath,
    max_content_bytes: u64,
  ) -> Result<BlameOutput, GfsError> {
    self
      .run(move |r| r.blame(&commit, &path, max_content_bytes))
      .await
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

  pub async fn index_for_commit(
    &self,
    commit: ObjectId,
    snapshot_time: gfs_types::Timestamp,
  ) -> Result<Vec<u8>, GfsError> {
    self
      .run(move |r| r.index_for_commit(&commit, snapshot_time))
      .await
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

  pub async fn visible_ref_targets(&self) -> Result<Vec<gfs_types::RefTarget>, GfsError> {
    self.run(move |r| r.visible_ref_targets()).await
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

  /// Every named path under `root`, collected.
  ///
  /// A directory that is absent in the base is *not* an error here, unlike
  /// [`GitRepository::walk_paths`]: this exists to expand a deletion, and a
  /// workspace that created a directory and then deleted it has nothing in the
  /// base to remove. Returning an empty set is the right answer.
  pub async fn walk_paths_collect(
    &self,
    commit: ObjectId,
    root: BytePath,
    out: &mut Vec<BytePath>,
  ) -> Result<(), GfsError> {
    let collected = self
      .run(move |r| {
        let mut paths = Vec::new();
        match r.walk_paths(&commit, &root, &mut |path, _mode| {
          paths.push(path);
          Ok(())
        }) {
          Ok(()) => Ok(paths),
          Err(e) if e.code == ErrorCode::NotFound => Ok(Vec::new()),
          Err(e) => Err(e),
        }
      })
      .await?;
    out.extend(collected);
    Ok(())
  }
}
