//! The seam between the filesystem and wherever a pinned commit's bytes come
//! from.
//!
//! The mount layer asks one set of questions about a commit — what is at this
//! path, what is in this directory, what are these blob's bytes, what does the
//! history look like, what matches this pattern — and nothing in how it asks
//! depends on whether the answer crosses a network. [`SnapshotSource`] is that
//! set of questions, and there are two answers:
//!
//! * [`crate::client::SnapshotClient`], gRPC and HTTP against `gfs-server`,
//!   which is where a job in a container gets its commit from; and
//! * [`crate::local::LocalSource`], libgit2 over a clone that is already on
//!   this machine, for a developer who wants a workspace per change without a
//!   `git worktree add` copying the tree each time.
//!
//! # Every call still names the commit
//!
//! A source is constructed *around* a [`MountBinding`] and the filesystem only
//! ever asks about that commit. The `_at` forms take a commit because history
//! commands (`gfs show HEAD~1`) legitimately read other commits; the filesystem
//! never calls them with anything but the pin. The rule in `client.rs` — a
//! branch name is a selector, resolved once, before the mount exists — holds
//! for every implementation, which is why `resolve` is documented as a history
//! helper and not as a way to move a mount.
//!
//! # What a source may decline
//!
//! Three capability hooks let an implementation say what it does not do rather
//! than fake it. A local source has no lease, so it says so and the heartbeat
//! is never spawned instead of failing every interval; it holds no capability,
//! so `mount.json` records none; and it serves blob bytes from memory, so the
//! filesystem does not write a second copy of every file it reads onto the disk
//! the clone is already on.

use gfs_types::error::GfsError;
use gfs_types::{
  BytePath, CommitMeta, HashAlgorithm, MountId, ObjectId, RepositoryId, Timestamp, TreeEntryInfo,
};

/// Everything a source needs that does not change for the life of a pin.
#[derive(Clone, Debug)]
pub struct MountBinding {
  pub repository_id: RepositoryId,
  pub commit: ObjectId,
  pub algorithm: HashAlgorithm,
  pub snapshot_time: Timestamp,
}

/// What a revwalk should cover. The wire's `LogRequest` in domain terms.
#[derive(Clone, Debug, Default)]
pub struct LogQuery {
  pub skip: u32,
  pub limit: u32,
  pub first_parent: bool,
  pub paths: Vec<BytePath>,
}

/// What to diff and how to render it.
#[derive(Clone, Debug, Default)]
pub struct DiffQuery {
  pub paths: Vec<BytePath>,
  pub format: gfs_types::DiffFormat,
  /// `None` takes the server's default of 3. `Some(0)` genuinely means no
  /// context, which is why this is an `Option` rather than a bare count.
  pub context_lines: Option<u32>,
  /// Zero takes the server's default.
  pub max_bytes: u64,
}

/// A rendered commit-to-commit diff.
#[derive(Clone, Debug)]
pub struct RevDiff {
  pub rendered: Vec<u8>,
  pub files: Vec<gfs_types::DiffFileChange>,
  pub truncated: bool,
}

/// Blame hunks and the bytes they describe.
#[derive(Clone, Debug)]
pub struct Blame {
  pub hunks: Vec<gfs_types::BlameHunk>,
  pub content: Vec<u8>,
  pub truncated: bool,
}

/// One page of a directory listing.
#[derive(Clone, Debug)]
pub struct DirectoryPage {
  pub entries: Vec<TreeEntryInfo>,
  /// Empty when this was the last page.
  pub next_page_token: Vec<u8>,
}

/// One page of a recursive listing: whole directories, never a partial one.
#[derive(Debug, Default)]
pub struct TreePage {
  /// Every entry of every directory in `directories`, in walk order.
  pub entries: Vec<TreeEntryInfo>,
  /// The directories this page describes completely, the walk root included.
  /// An empty directory appears here with no entries, which is the only way to
  /// tell "listed, and empty" from "not listed".
  pub directories: Vec<BytePath>,
  /// Empty when the walk reached the end of the subtree.
  pub next_page_token: Vec<u8>,
}

/// Where a pinned commit's tree, content, history, and search come from.
///
/// Every method that can take time is `async` and must not block the caller's
/// thread: the filesystem invokes these from tokio workers with a FUSE reply
/// waiting (ADR 0003). An implementation over a blocking library dispatches to
/// `spawn_blocking`; one over a network awaits the socket.
#[async_trait::async_trait]
pub trait SnapshotSource: Send + Sync + std::fmt::Debug {
  fn binding(&self) -> &MountBinding;

  /// Whether the pin is held by a lease something has to keep renewing.
  ///
  /// `false` means [`SnapshotSource::renew_mount`] is never called and the
  /// lease health is reported as healthy for the life of the mount.
  fn leased(&self) -> bool {
    true
  }

  /// Whether base blobs should be served straight from
  /// [`SnapshotSource::read_blob`]'s bytes rather than through the verified
  /// on-disk cache.
  ///
  /// The cache exists to make a network fetch happen once and to verify what
  /// arrived. A source whose bytes are already on this machine's disk gains
  /// nothing from a second copy and pays a write, a hash, and a rename for it.
  fn serves_blobs_in_memory(&self) -> bool {
    false
  }

  /// The credential to record in `mount.json`, if the source holds one.
  fn capability_for_persistence(&self) -> String {
    String::new()
  }

  /// One path's metadata in the pinned commit, or `None` when there is no such
  /// path. The `None` is what the filesystem caches as a negative lookup.
  async fn get_entry(
    &self,
    path: &BytePath,
    want_blob_ticket: bool,
  ) -> Result<Option<TreeEntryInfo>, GfsError> {
    let commit = self.binding().commit.clone();
    self.get_entry_at(&commit, path, want_blob_ticket).await
  }

  /// The same, in any commit this source may read.
  async fn get_entry_at(
    &self,
    commit: &ObjectId,
    path: &BytePath,
    want_blob_ticket: bool,
  ) -> Result<Option<TreeEntryInfo>, GfsError>;

  async fn list_directory(
    &self,
    path: &BytePath,
    page_token: Vec<u8>,
    page_size: u32,
    want_blob_tickets: bool,
  ) -> Result<DirectoryPage, GfsError> {
    let commit = self.binding().commit.clone();
    self
      .list_directory_at(&commit, path, page_token, page_size, want_blob_tickets)
      .await
  }

  async fn list_directory_at(
    &self,
    commit: &ObjectId,
    path: &BytePath,
    page_token: Vec<u8>,
    page_size: u32,
    want_blob_tickets: bool,
  ) -> Result<DirectoryPage, GfsError>;

  /// A whole subtree's directories in one call. Every directory named in the
  /// page is complete, so each can be cached as an authoritative listing.
  async fn list_tree(
    &self,
    root: &BytePath,
    page_token: Vec<u8>,
    max_entries: u32,
  ) -> Result<TreePage, GfsError>;

  /// Many paths at once. A path that fails individually is `None`: the caller
  /// is a prefetch, and a batch is an optimisation whose partial failure must
  /// degrade to an individual lookup rather than to an error.
  async fn batch_get_entry(
    &self,
    paths: &[BytePath],
    want_blob_tickets: bool,
  ) -> Result<Vec<Option<TreeEntryInfo>>, GfsError>;

  /// A whole blob's bytes. `ticket` is whatever `get_entry` attached to the
  /// entry when asked for one; a source that needs no ticket ignores it.
  async fn read_blob(&self, oid: &ObjectId, ticket: &str) -> Result<Vec<u8>, GfsError>;

  /// The same bytes, shared. A source that already holds the blob in memory
  /// hands out its own `Arc` rather than copying; the default copies once.
  /// The daemon no longer needs its copy of a blob: the kernel holds the
  /// bytes (a passthrough memfd). A source that keeps blobs in memory drops
  /// them; every other source has nothing to drop.
  fn forget_blob(&self, _oid: &ObjectId) {}

  async fn read_blob_shared(
    &self,
    oid: &ObjectId,
    ticket: &str,
  ) -> Result<std::sync::Arc<Vec<u8>>, GfsError> {
    Ok(std::sync::Arc::new(self.read_blob(oid, ticket).await?))
  }

  /// The `.git/index` for a commit, with ADR 0009's stat data and cache tree.
  async fn commit_index(&self, commit: &ObjectId) -> Result<Vec<u8>, GfsError>;

  async fn get_commit(&self) -> Result<CommitMeta, GfsError> {
    let commit = self.binding().commit.clone();
    self.get_commit_at(&commit).await
  }

  async fn get_commit_at(&self, commit: &ObjectId) -> Result<CommitMeta, GfsError>;

  /// The ancestry of `from`, newest first. `None` walks the pinned commit.
  /// Returns the page and whether ancestry remains beyond it.
  async fn log(
    &self,
    from: Option<&ObjectId>,
    options: &LogQuery,
  ) -> Result<(Vec<CommitMeta>, bool), GfsError>;

  /// Resolve a revision expression. Answers "what commit does `HEAD~1` mean"
  /// for history commands; the filesystem never calls it and no implementation
  /// re-pins anything through it.
  async fn resolve(&self, selector: &str) -> Result<ObjectId, GfsError>;

  /// Every ref this source shows, with tags peeled. Called once per pin; the
  /// seed turns it into `packed-refs`.
  async fn list_refs(&self) -> Result<Vec<gfs_types::RefTarget>, GfsError>;

  /// What changed between two commits, rendered. `from` is `None` for a root
  /// commit, which is diffed against the empty tree.
  async fn diff_commits(
    &self,
    from: Option<&ObjectId>,
    to: &ObjectId,
    query: &DiffQuery,
  ) -> Result<RevDiff, GfsError>;

  /// Line attribution for one path at one commit, with the file's bytes.
  async fn blame(&self, commit: &ObjectId, path: &BytePath) -> Result<Blame, GfsError>;

  /// Make the pinned commit searchable. Returns whether it is searchable *now*;
  /// `false` means a build is in progress and a later search may succeed.
  async fn prepare_snapshot(&self) -> Result<bool, GfsError>;

  /// Search the pinned commit's tree. Never returns an empty result for a
  /// search that did not complete: that is what the `FailedBeforeCompletion`
  /// outcome is for.
  async fn search(
    &self,
    query: &gfs_search::Query,
    max_results: u32,
  ) -> Result<gfs_search::SearchOutcome, GfsError>;

  /// Extend the lease. Only called when [`SnapshotSource::leased`] is true.
  async fn renew_mount(&self, mount_id: &MountId) -> Result<Timestamp, GfsError>;

  /// Release the pin: the lease, the anchor, whatever held the commit.
  async fn release_mount(&self, mount_id: &MountId) -> Result<(), GfsError>;
}
