//! The `fuser::Filesystem` implementation.
//!
//! # The dispatch rule is the architecture
//!
//! ADR 0003 measured what happens when a FUSE callback blocks: 64 files, 16
//! reader threads, 20 ms of origin latency.
//!
//! | Dispatch | Event-loop threads | Wall time | Peak concurrent fetches |
//! | --- | --- | ---: | ---: |
//! | Blocking | 1 | **1321 ms** | 1 |
//! | Blocking | 8 | 170 ms | 8 |
//! | Pooled (reply from a worker) | 1 | **123 ms** | 16 |
//!
//! 1321 ms is exactly 64 × 20 ms — one blocking callback thread turns a parallel
//! build into a sequential one. The ADR adopts **both** remedies, not either: more
//! than one event-loop thread *and* never blocking a callback. `n_threads` alone
//! caps concurrency at the thread count; it is the pooled model that reaches 16
//! concurrent fetches from a single event-loop thread.
//!
//! Every callback here that can touch the network or the disk therefore moves its
//! reply handle onto the tokio runtime and returns immediately. `fuser` 0.18 makes
//! this possible: the reply handles are `Send`, and the trait takes `&self`.
//!
//! The overlay is local SQLite rather than network, but it is still blocking
//! work, and a copy-up streams a whole blob. Every overlay mutation therefore
//! runs on a blocking pool through `Gfs::blocking`, which keeps the same rule
//! for the same reason.
//!
//! # Four worlds, routed by subtree
//!
//! ADR 0011's rule: the mount has exactly two subtree behaviours besides the
//! projected tree, and nothing merged. `.git/**` is pure passthrough to the
//! retained real directory; `.git/gfs/objects/**` is the pure object-store
//! projection; everything else is the merged overlay-over-base view. The
//! routing happens once per callback, on the parent's world, and inside the
//! merged view the resolution order lives in `Gfs::resolve_path` — the
//! overlay's answer *replaces* the base's, including the answer "this path is
//! gone" — so `readdir` and every mutation agree with `lookup` about what
//! exists.
//!
//! # Caching TTLs are long, and the overlay is what makes that need care
//!
//! The pinned commit is immutable, so a cached *base* attribute can never be
//! stale. ADR 0003 measured 1000 `stat(2)` calls on one path producing **zero**
//! `getattr` upcalls at a 60-second TTL. The overlay changes that for paths it
//! touches, so a mutation replies with the new attributes and the kernel's own
//! write path keeps the page cache coherent; entries the overlay has never seen
//! keep the long TTL.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use fuser::{
  AccessFlags, Errno, FileAttr, FileHandle, Filesystem, Generation, INodeNo, OpenAccMode,
  OpenFlags, Request,
};
use gfs_overlay::{
  BaseDescendant, BaseFacts, Overlay, OverlayEntry, OverlayError, OverlayKind, Resolution, Source,
};
use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::{BytePath, EntryKind, ObjectId, Timestamp, TreeEntryInfo};

use crate::attr::{attr_of, errno_of, errno_of_overlay, Ownership};
use crate::cache::{BlobCache, CacheStats};
use crate::inode::{InodeTable, Node, Record, ROOT_INO};
use crate::listing::Listing;
use crate::passthrough::{
  errno_io, git_rel, in_object_namespace, odb_rel, GitMeta, GitPassthrough, OdbNode, GIT_DIR_NAME,
};
use crate::source::SnapshotSource;

/// Inode numbers are never reused for a different path, so there is nothing for a
/// generation to disambiguate. See the module docs on [`crate::inode`].
const GENERATION: Generation = Generation(0);

#[derive(Clone, Debug)]
pub struct FsConfig {
  /// How long the kernel may cache attributes and directory entries.
  pub ttl: Duration,
  /// How long the kernel may cache the absence of a name.
  pub negative_ttl: Duration,
  /// How long the kernel may cache `.git` passthrough entries and attributes.
  ///
  /// Short, and deliberately so: the daemon rewrites the shadowed state from
  /// behind the mount on a repin (`HEAD`, refs, the index), so a long TTL
  /// here would serve those files stale. m05c measured that the attribute TTL
  /// is *not* the performance lever for the git subtree — negative dentries
  /// are — so shortness costs nothing measured.
  pub git_ttl: Duration,
  /// How long the kernel may cache the absence of a name in the **object
  /// namespace**: `.git/objects/**` and the `.git/gfs/objects/**` projection.
  ///
  /// This is ADR 0011's requirement, not an optimization: Git probes its
  /// primary loose-object directories before packs and alternates — 6,524
  /// ENOENT lookups for one linux `read-tree` (m05c) — and each probe is a
  /// FUSE round trip unless the kernel remembers the absence. Coherent by
  /// construction: the kernel drops a negative dentry when a name is created
  /// through the mount, nothing mutates the shadowed state behind the
  /// daemon's back, and the daemon itself never writes loose objects.
  pub object_negative_ttl: Duration,
  pub directory_page_size: u32,
  /// How many base directories the daemon keeps complete listings of.
  ///
  /// The pinned commit is immutable, so a complete listing answers every
  /// metadata question about its directory — children, attributes, and
  /// definitive negatives — for the life of the pin, with zero server round
  /// trips. Bounded so a monorepo walk cannot pin the whole tree's metadata
  /// in daemon memory; see [`crate::listing`].
  pub listing_cache_dirs: usize,
  /// How many *entries* those listings may hold in total. The bound that
  /// actually describes memory, since one directory can carry thousands.
  pub listing_cache_entries: usize,
  /// Listing misses inside a two-second window that make the daemon call a
  /// walk a walk and fetch the subtree in one request. Zero disables it.
  pub walk_prefetch_threshold: usize,
  /// Entries per `ListTree` page.
  pub tree_page_entries: u32,
  /// Entries one recognized walk may prefetch before it stops.
  pub tree_prefetch_max_entries: usize,
  /// Distinct files read from one directory that make the daemon fetch the
  /// rest of that directory's content. Zero disables it.
  pub read_prefetch_threshold: usize,
  /// Bytes one directory's content prefetch may move.
  pub read_prefetch_max_bytes: u64,
  /// The largest file a content prefetch will speculate on.
  pub read_prefetch_max_file_bytes: u64,
  /// Blob fetches in flight during a content prefetch.
  pub read_prefetch_concurrency: usize,
  /// The percentage of the hydration budget prefetching will not spend.
  pub prefetch_budget_reserve_percent: u64,
  /// Attempts for a retryable failure, including the first.
  pub attempts: u32,
  /// Bytes a job may hydrate from the server before reads are refused with
  /// `EDQUOT`. Zero is unlimited. See [`crate::budget`] for why this is
  /// mandatory rather than opt-in.
  pub hydration_budget_bytes: u64,
}

impl Default for FsConfig {
  fn default() -> Self {
    FsConfig {
      // One hour. The commit is immutable, so the only cost of a long TTL is
      // memory the kernel is free to reclaim, and the benefit is the metadata
      // sweep that never reaches the network.
      ttl: Duration::from_secs(3600),
      // Shorter than the positive TTL, and that asymmetry is deliberate: a
      // negative entry against an immutable commit is permanent, but a negative
      // entry the overlay could *create* is not, and the kernel invalidates on
      // its own creates but not on another process's.
      negative_ttl: Duration::from_secs(1),
      git_ttl: Duration::from_secs(1),
      // The m05c number: with 60 s every measured command lands within a few
      // ms of local disk on the worst-case repository.
      object_negative_ttl: Duration::from_secs(60),
      directory_page_size: gfs_types::limits::DEFAULT_DIRECTORY_PAGE_SIZE as u32,
      // Sized to hold a monorepo's whole directory structure, because that is
      // now a thing that happens in one operation: vscode is 4 318 directories
      // and 22 243 entries, and a walk prefetch fills all of them at once. The
      // entry bound is what caps memory — 150 000 entries is ~25 MB of paths
      // and object IDs, well past every measured corpus and still far from
      // pinning a million-entry tree, which is what ADR 0009 refused.
      listing_cache_dirs: 32_768,
      listing_cache_entries: 150_000,
      // Four misses inside two seconds. A job opening a handful of known files
      // never reaches it; a `git status` reaches it in the first few
      // directories and pays one `ListTree` for the rest of the tree.
      walk_prefetch_threshold: 4,
      tree_page_entries: gfs_types::limits::DEFAULT_TREE_PAGE_ENTRIES as u32,
      // Above every corpus measured (vscode 22 243, linux ~90 000) so a walk is
      // one prefetch, and far enough below a pathological tree that the daemon
      // stops rather than streaming a million entries into memory.
      tree_prefetch_max_entries: 200_000,
      // Three distinct files out of one directory is a read-through; two is a
      // build reading a header and its source.
      read_prefetch_threshold: 3,
      read_prefetch_max_bytes: 32 << 20,
      read_prefetch_max_file_bytes: 8 << 20,
      read_prefetch_concurrency: 4,
      prefetch_budget_reserve_percent: 25,
      attempts: 3,
      // 1 GiB, on by default, which is the point of ADR 0009: a budget that has
      // to be switched on is not enforcement. The number is chosen from
      // `spikes/reports/m05b-git-projection.md` to sit between the two behaviours
      // it has to tell apart -- a full re-hash of the Linux kernel's working tree
      // is 1 540 MiB and trips it, while every measured well-behaved command is
      // orders of magnitude below it. Retune from real jobs (PLAN.md M6.2); do not
      // turn it off to make a workload fit.
      hydration_budget_bytes: 1 << 30,
    }
  }
}

/// Counters DESIGN.md section 8.4 requires the client to keep.
#[derive(Clone, Copy, Default, Debug, serde::Serialize, serde::Deserialize)]
pub struct FsStats {
  pub lookups: u64,
  pub negative_lookups: u64,
  pub metadata_requests: u64,
  pub directory_pages: u64,
  /// Metadata questions answered from a cached base listing, server-free.
  pub listing_hits: u64,
  /// Recognized walks that were answered by a recursive subtree fetch.
  pub tree_prefetches: u64,
  /// `ListTree` pages received.
  pub tree_pages: u64,
  /// Directory listings filled by those pages, rather than one call each.
  pub prefetched_listings: u64,
  /// Directories whose remaining content was fetched on a read-through.
  pub content_prefetches: u64,
  pub prefetched_blobs: u64,
  pub prefetched_bytes: u64,
  pub opens: u64,
  pub reads: u64,
  pub read_bytes: u64,
  pub writes: u64,
  pub written_bytes: u64,
  /// Reads refused because the job's hydration budget was spent.
  pub hydration_refusals: u64,
  pub copy_ups: u64,
  pub copy_up_bytes: u64,
  pub errors: u64,
}

/// A resolved child in a directory listing.
#[derive(Clone, Debug)]
enum Child {
  Base(TreeEntryInfo),
  Overlay(Box<OverlayEntry>),
  Git { name: Vec<u8>, meta: GitMeta },
  Odb { name: Vec<u8>, node: OdbNode },
}

impl Child {
  fn name(&self) -> Vec<u8> {
    match self {
      Child::Base(entry) => entry.path.file_name().unwrap_or_default().to_vec(),
      Child::Overlay(entry) => entry.name(),
      Child::Git { name, .. } | Child::Odb { name, .. } => name.clone(),
    }
  }

  fn node(&self) -> Node {
    match self {
      Child::Base(entry) => Node::Base(entry.clone()),
      Child::Overlay(entry) => Node::Overlay(entry.clone()),
      Child::Git { meta, .. } => Node::Git(*meta),
      Child::Odb { node, .. } => Node::Odb(node.clone()),
    }
  }

  fn file_type(&self) -> fuser::FileType {
    match self {
      Child::Base(entry) => file_type(entry.kind),
      Child::Overlay(entry) => file_type(entry.kind.to_entry_kind()),
      Child::Git { meta, .. } => meta.kind,
      Child::Odb { node, .. } => {
        if node.is_dir() {
          fuser::FileType::Directory
        } else {
          fuser::FileType::RegularFile
        }
      }
    }
  }
}

#[derive(Debug)]
struct DirState {
  /// The directory's own path, used to build child paths.
  path: BytePath,
  children: Vec<Child>,
  /// Whether `children` is the whole merged listing. Filled by the first
  /// `fill_directory` call; base children always precede overlay extras, so a
  /// child's offset never moves for the life of the handle.
  complete: bool,
  /// A passthrough directory (git-relative path) whose real listing is still
  /// pending. Deferred out of `opendir` so the `read_dir` walk runs on a
  /// blocking worker inside `fill_directory`, never on the event loop.
  git_pending: Option<Vec<u8>>,
}

#[derive(Debug)]
enum FileState {
  /// A cached base blob, opened once and read with `pread`.
  Blob {
    oid: ObjectId,
    file: Arc<std::fs::File>,
  },
  /// A base blob held in memory for the life of the descriptor: what a source
  /// that serves from a local object store hands out instead of a cache file
  /// (local mode, ADR 0013).
  Memory { bytes: Arc<Vec<u8>> },
  /// Overlay content. Held open, which is what keeps an unlinked file readable
  /// through a descriptor that outlives its name — the overlay removes the
  /// content file, and the kernel keeps the inode alive until this closes.
  Local {
    content_id: u64,
    file: Arc<std::fs::File>,
    writable: bool,
  },
  /// A real file of the shadowed `.git`, held open through the passthrough.
  /// The handle keeps working across rename and unlink, exactly as a local
  /// descriptor would — because it is one.
  Git {
    file: Arc<std::fs::File>,
    writable: bool,
  },
  /// A projected object-store file, read block-wise from the shared store.
  Odb { path: String },
}

/// What `open_blob` produced: a descriptor on a verified cache file, or the
/// blob itself when the source serves from memory.
enum OpenedBlob {
  File(std::fs::File),
  Memory(Arc<Vec<u8>>),
}

impl OpenedBlob {
  fn into_state(self, oid: &ObjectId) -> FileState {
    match self {
      OpenedBlob::File(file) => FileState::Blob {
        oid: oid.clone(),
        file: Arc::new(file),
      },
      OpenedBlob::Memory(bytes) => FileState::Memory { bytes },
    }
  }
}

/// What a path is, and what the pinned base has there.
#[derive(Debug)]
struct Resolved {
  /// The merged view's answer. `None` means the path does not exist.
  node: Option<Node>,
  /// What the pinned commit holds at this path, whether or not it is visible.
  /// A mutation needs this to record where it diverged from.
  base: Option<BaseFacts>,
}

/// Everything about the pinned commit, swapped as one value.
///
/// One struct behind one lock rather than three locks, because these are only
/// meaningful together: a reader that got the new commit's client and the old
/// commit's overlay would answer about a tree neither describes. The `.git`
/// passthrough is deliberately *not* in here: a repin re-seeds the real git
/// dir on disk, but where the git dir lives never changes.
#[derive(Debug)]
pub struct Pinned {
  pub client: Arc<dyn SnapshotSource>,
  pub overlay: Arc<Overlay>,
  pub snapshot_time: Timestamp,
  /// Complete base listings for this pin. Living *inside* `Pinned` is the
  /// invalidation strategy: a repin swaps the whole struct, so the cache is
  /// born empty with the new commit, and a fetch still running against the
  /// old client can only ever insert into the old generation's cache.
  pub(crate) listings: crate::listing::ListingCache,
  /// Access-pattern detectors and the subtree fetches they started, for the
  /// same generation and for the same reason.
  pub(crate) prefetch: crate::prefetch::Prefetcher,
}

/// The filesystem. Shared behind an `Arc` so a callback can hand it to a worker.
pub struct Gfs {
  /// Replaced wholesale by [`Gfs::repin`]. Read through [`Gfs::pinned`], which
  /// clones the `Arc` and releases the lock — a guard held across an `await`
  /// would let a re-pin block every reader on the mount.
  pinned: RwLock<Arc<Pinned>>,
  /// The `.git` passthrough and the embedded object projection (ADR 0011).
  /// Constant across repins.
  git: Arc<GitPassthrough>,
  cache: Arc<BlobCache>,
  config: FsConfig,
  owner: Ownership,
  inodes: Mutex<InodeTable>,
  dirs: Mutex<HashMap<u64, Arc<tokio::sync::Mutex<DirState>>>>,
  files: Mutex<HashMap<u64, Arc<FileState>>>,
  next_handle: AtomicU64,
  /// Shared rather than owned: a prefetch runs after the call that triggered it
  /// has returned, and what it fetched still belongs in this mount's counters.
  stats: Arc<Mutex<FsStats>>,
  /// Deliberately not inside [`Pinned`]: a re-pin changes which commit the job is
  /// looking at and does not refund what it has already spent.
  budget: Arc<crate::budget::HydrationBudget>,
}

impl std::fmt::Debug for Gfs {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Gfs")
      .field("config", &self.config)
      .field("snapshot_time", &self.snapshot_time())
      .finish_non_exhaustive()
  }
}

impl Gfs {
  pub fn new(
    client: Arc<dyn SnapshotSource>,
    cache: Arc<BlobCache>,
    git: Arc<GitPassthrough>,
    overlay: Arc<Overlay>,
    root: TreeEntryInfo,
    config: FsConfig,
  ) -> Arc<Self> {
    let snapshot_time = client.binding().snapshot_time;
    let budget = crate::budget::HydrationBudget::new(config.hydration_budget_bytes);
    let mut inodes = InodeTable::new(root);
    // The numbers a previous process handed out, before any new one is issued.
    inodes.seed(&overlay.entries());
    Arc::new(Gfs {
      pinned: RwLock::new(Arc::new(Pinned {
        client,
        overlay,
        snapshot_time,
        listings: crate::listing::ListingCache::new(
          config.listing_cache_dirs,
          config.listing_cache_entries,
        ),
        prefetch: crate::prefetch::Prefetcher::new(config.walk_prefetch_threshold),
      })),
      git,
      cache,
      config,
      owner: Ownership::current(),
      inodes: Mutex::new(inodes),
      dirs: Mutex::new(HashMap::new()),
      files: Mutex::new(HashMap::new()),
      next_handle: AtomicU64::new(1),
      stats: Arc::new(Mutex::new(FsStats::default())),
      budget: Arc::new(budget),
    })
  }

  /// The commit this mount is currently looking at, as one consistent value.
  pub fn pinned(&self) -> Arc<Pinned> {
    Arc::clone(&self.pinned.read().expect("pinned commit"))
  }

  /// Look at a different commit, without ending the mount.
  ///
  /// This is `gfs switch`, and the shape is the one `git switch` has: the
  /// working tree a job is standing in changes underneath it, and the path it is
  /// standing in does not. Returns the paths whose kernel dentries the caller
  /// must now invalidate — see [`InodeTable::repin`] for why the caller does
  /// that rather than this.
  ///
  /// Open descriptors are deliberately untouched. A `FileState` holds an open
  /// handle on a materialized cache file or an overlay content file, so a
  /// descriptor obtained before the re-pin goes on reading the bytes it opened,
  /// owing nothing to the old commit's lease. That is what `git switch` leaves
  /// behind too: replacing a file does not reach into a reader's descriptor.
  pub fn repin(
    &self,
    client: Arc<dyn SnapshotSource>,
    overlay: Arc<Overlay>,
    snapshot_time: Timestamp,
    root: TreeEntryInfo,
  ) -> Vec<crate::inode::StaleEntry> {
    // Assembled here rather than by the caller so the listing cache is always
    // born empty and sized from this mount's config.
    let pinned = Pinned {
      client,
      overlay,
      snapshot_time,
      listings: crate::listing::ListingCache::new(
        self.config.listing_cache_dirs,
        self.config.listing_cache_entries,
      ),
      prefetch: crate::prefetch::Prefetcher::new(self.config.walk_prefetch_threshold),
    };
    // The inode table first, and under both locks, so no lookup can resolve
    // against the new commit and then be recorded against the old root.
    let mut inodes = self.inodes.lock().expect("inode table");
    let mut slot = self.pinned.write().expect("pinned commit");
    inodes.seed(&pinned.overlay.entries());
    *slot = Arc::new(pinned);
    inodes.repin(root)
  }

  pub fn stats(&self) -> FsStats {
    *self.stats.lock().expect("fs stats")
  }

  /// What the hydration budget has admitted and refused.
  pub fn budget_report(&self) -> crate::budget::BudgetReport {
    self.budget.report()
  }

  /// The stable sanitized time every base entry reports.
  pub fn snapshot_time(&self) -> Timestamp {
    self.pinned().snapshot_time
  }

  pub fn cache_stats(&self) -> CacheStats {
    self.cache.stats()
  }

  /// The snapshot client, for callers that search rather than read files.
  ///
  /// Handed out rather than duplicated because the client holds the *mount
  /// capability*, which is refreshed by every heartbeat renewal. A second client
  /// built for search would hold a copy that goes stale, and its first read
  /// after a force push -- the exact moment a mount must not break -- would fail.
  pub fn client(&self) -> Arc<dyn SnapshotSource> {
    Arc::clone(&self.pinned().client)
  }

  pub fn overlay(&self) -> Arc<Overlay> {
    Arc::clone(&self.pinned().overlay)
  }

  /// The `.git` passthrough, for the daemon's odb attribution report.
  pub fn git(&self) -> &Arc<GitPassthrough> {
    &self.git
  }

  /// Live inodes and distinct paths ever numbered. Reported by `gfs inspect`.
  pub fn inode_counts(&self) -> (usize, usize) {
    let table = self.inodes.lock().expect("inode table");
    (table.live(), table.assigned())
  }

  /// Open file and directory handles.
  ///
  /// `gfs refresh` reads this: PLAN.md M2.1 requires the old mount generation
  /// and its lease to survive until every handle opened through it has closed,
  /// so that no reader ever observes a mixture of two generations.
  pub fn open_handles(&self) -> usize {
    self.files.lock().expect("file handles").len() + self.dirs.lock().expect("dir handles").len()
  }

  fn bump(&self, f: impl FnOnce(&mut FsStats)) {
    f(&mut self.stats.lock().expect("fs stats"));
  }

  fn attr(&self, record: &Record) -> FileAttr {
    let mut attr = attr_of(record, self.snapshot_time(), self.owner);
    // The root is the one directory with no journal row to carry its times (see
    // `Overlay::touch_root`), so they are applied here instead. Skipping this
    // is what left a workspace whose root reported the pinned commit's snapshot
    // time however many files a job created in it — and Git's untracked cache
    // keys a directory's extent on exactly that stat data.
    if record.path.is_empty() {
      if let Some((mtime, ctime)) = self.overlay().root_times() {
        attr.mtime = crate::attr::to_system_time(mtime);
        attr.ctime = crate::attr::to_system_time(ctime);
        attr.atime = attr.mtime;
      }
    }
    attr
  }

  fn record(&self, ino: u64) -> Option<Record> {
    self.inodes.lock().expect("inode table").get(ino).cloned()
  }

  /// The number the inode table will report for a path, assigned now.
  ///
  /// Taken before the journal row is written so the row can record it: a row
  /// whose number disagreed with the live table would make the path change
  /// identity after a daemon restart for no reason.
  fn number_for(&self, path: &BytePath) -> u64 {
    self
      .inodes
      .lock()
      .expect("inode table")
      .number_for_path(path)
  }

  fn new_handle(&self) -> u64 {
    self.next_handle.fetch_add(1, Ordering::Relaxed)
  }

  /// Run blocking overlay work off the event loop. See the module docs.
  async fn blocking<T, F>(f: F) -> Result<T, OverlayError>
  where
    F: FnOnce() -> Result<T, OverlayError> + Send + 'static,
    T: Send + 'static,
  {
    match tokio::task::spawn_blocking(f).await {
      Ok(result) => result,
      Err(e) => Err(OverlayError::io(format!("the overlay task failed: {e}"))),
    }
  }

  /// Retry a retryable service failure.
  ///
  /// Bounded and short. The mount is in a job with a deadline, and a read that
  /// hangs behind an unbounded retry is worse for the job than a read that fails:
  /// ADR 0006's failure policy makes an uncached read during a server outage a
  /// retryable `EIO`, not an indefinite stall.
  async fn retrying<T, F, Fut>(&self, mut operation: F) -> Result<T, GfsError>
  where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, GfsError>>,
  {
    let mut delay = Duration::from_millis(20);
    let mut attempt = 1;
    loop {
      match operation().await {
        Ok(value) => return Ok(value),
        Err(e) if e.is_retryable() && attempt < self.config.attempts => {
          tokio::time::sleep(delay).await;
          delay *= 2;
          attempt += 1;
        }
        Err(e) => return Err(e),
      }
    }
  }

  /// One base lookup, with retries and accounting.
  ///
  /// This reaches the server. Metadata questions go through [`Gfs::base_node`]
  /// and the listing cache instead; what is left here is the root (which has
  /// no parent listing) and [`Gfs::open_blob`]'s ticket request, because a
  /// blob ticket is short-lived authorization state, not metadata.
  async fn base_entry(
    &self,
    path: &BytePath,
    want_ticket: bool,
  ) -> Result<Option<TreeEntryInfo>, GfsError> {
    path.validate()?;
    self.bump(|s| s.metadata_requests += 1);
    let client = self.client();
    self.retrying(|| client.get_entry(path, want_ticket)).await
  }

  /// The complete base listing of a directory: cached, or paged to the end
  /// and then cached.
  ///
  /// The pinned commit is immutable, so a complete listing is the permanent
  /// answer to every metadata question about its directory — children,
  /// attributes, and definitive negatives. See [`crate::listing`] for the
  /// lifetime and bounds.
  async fn base_listing(&self, dir: &BytePath) -> Result<Arc<Listing>, GfsError> {
    // One `Pinned` for the whole operation: the client that fetches and the
    // cache that stores must belong to the same generation.
    let pinned = self.pinned();
    if let Some(listing) = pinned.listings.get(dir) {
      self.bump(|s| s.listing_hits += 1);
      return Ok(listing);
    }

    // A recursive fetch may already be on its way to this directory. Waiting for
    // it is the whole point: the walk that missed here will ask for thousands
    // more directories, and one traversal answers all of them.
    if let Some(listing) = crate::prefetch::await_subtree(&pinned, dir).await {
      self.bump(|s| s.listing_hits += 1);
      return Ok(listing);
    }

    // Not covered, so this miss is evidence in its own right.
    if self.config.walk_prefetch_threshold > 0 {
      if let Some(root) = pinned.prefetch.note_listing_miss(dir) {
        crate::prefetch::spawn_subtree(
          Arc::clone(&pinned),
          root,
          self.prefetch_limits(),
          Arc::clone(&self.stats),
        );
      }
    }

    let client = Arc::clone(&pinned.client);
    let mut entries = Vec::new();
    let mut token = Vec::new();
    loop {
      let page = self
        .retrying(|| {
          client.list_directory(dir, token.clone(), self.config.directory_page_size, false)
        })
        .await?;
      self.bump(|s| s.directory_pages += 1);
      entries.extend(page.entries);
      if page.next_page_token.is_empty() {
        break;
      }
      token = page.next_page_token;
    }
    let listing = Arc::new(Listing::new(entries));
    pinned.listings.insert(dir, Arc::clone(&listing));
    Ok(listing)
  }

  /// One base entry, answered from its parent's cached listing.
  ///
  /// A cold deep lookup pays one listing per ancestor — the count the
  /// kernel's component walk forces anyway — and every later question about
  /// those directories, including the absence of any name they lack, is
  /// answered without the server.
  async fn base_node(&self, path: &BytePath) -> Result<Option<TreeEntryInfo>, GfsError> {
    path.validate()?;
    let Some(name) = path.file_name().map(<[u8]>::to_vec) else {
      // The root has no parent listing; its entry came with the pin.
      return self.base_entry(path, false).await;
    };
    let listing = self.base_listing(&parent_of(path)).await?;
    Ok(listing.get(&name).cloned())
  }

  /// Resolve a path across all three worlds. The single place the order lives.
  async fn resolve_path(&self, path: &BytePath) -> Result<Resolved, GfsError> {
    match self.pinned().overlay.resolve(path) {
      Resolution::Overlay(entry) => {
        let base = entry.base.clone();
        Ok(Resolved {
          node: Some(Node::Overlay(entry)),
          base,
        })
      }
      // Deleted or masked. The base is *not* consulted -- that is the whole point
      // of a whiteout -- but the row still remembers what it hid, which is what a
      // re-creation at the same path needs in order to be reported as a
      // replacement rather than an addition.
      Resolution::Absent => Ok(Resolved {
        node: None,
        base: self.pinned().overlay.get(path).and_then(|entry| entry.base),
      }),
      Resolution::Base => {
        let entry = self.base_node(path).await?;
        let base = entry.as_ref().and_then(base_facts);
        Ok(Resolved {
          node: entry.map(Node::Base),
          base,
        })
      }
    }
  }

  /// Whether a lookup goes to the `.git` passthrough or the projection.
  ///
  /// True for the root's `.git` entry and for anything under a git or odb
  /// parent. Everything else is the merged overlay-over-base view.
  fn routes_to_git(parent: &Record, name: &[u8]) -> bool {
    (parent.ino == ROOT_INO && name == GIT_DIR_NAME.as_bytes())
      || parent.node.is_git()
      || parent.node.is_odb()
  }

  /// Resolve one name inside the git subtree, and pick the negative TTL the
  /// caller must use when the answer is "absent".
  ///
  /// The stat runs on a blocking worker: it is local disk, but ADR 0003's rule
  /// is checked in one place and this keeps it checkable.
  async fn lookup_git(&self, path: &BytePath) -> Result<Option<Node>, fuser::Errno> {
    let Some(rel) = git_rel(path) else {
      return Err(fuser::Errno::ENOENT);
    };
    // The projection boundary: `.git/gfs/objects` and everything below it is
    // the manifest's tree, never the disk — the on-disk `gfs/` has no
    // `objects`, and a real one would be skipped rather than merged.
    if let Some(orel) = odb_rel(rel) {
      return Ok(self.git.odb_node(orel).map(Node::Odb));
    }
    let git = Arc::clone(&self.git);
    let rel = rel.to_vec();
    let stat = tokio::task::spawn_blocking(move || git.stat(&rel))
      .await
      .map_err(|_| fuser::Errno::EIO)?;
    match stat {
      Ok(meta) => Ok(Some(Node::Git(meta))),
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
      Err(e) => Err(errno_io(&e)),
    }
  }

  /// The negative-entry TTL for an absent name under a git-subtree parent.
  fn git_negative_ttl(&self, parent_rel: &[u8]) -> Duration {
    if in_object_namespace(parent_rel) {
      self.config.object_negative_ttl
    } else {
      // A repin creates names under `.git` (refs, the overlay's journal
      // sidecars) from behind the mount, which the kernel cannot see — so
      // absences outside the object namespace expire on the short TTL.
      self.config.negative_ttl
    }
  }

  /// Resolve one name under a parent in the merged view.
  async fn lookup_child(&self, parent: &Record, name: &[u8]) -> Result<Option<Node>, GfsError> {
    Ok(self.resolve_path(&parent.path.join(name)).await?.node)
  }

  /// What the pinned base holds at a directory that is about to be mutated in.
  ///
  /// Taken from the parent's own record rather than fetched: the kernel resolved
  /// the parent inode before issuing the mutation, so the answer is already here.
  fn parent_base(parent: &Record) -> Option<BaseFacts> {
    match &parent.node {
      Node::Base(entry) => base_facts(entry),
      // The overlay resolves the parent itself in that case, so the value is
      // unused -- but a wrong value would be worse than an absent one.
      Node::Overlay(_) | Node::Git(_) | Node::Odb(_) => None,
    }
  }

  /// Every name the base has in a directory.
  async fn base_child_names(&self, dir: &BytePath) -> Result<Vec<Vec<u8>>, GfsError> {
    if self.pinned().overlay.masks_base(dir) {
      return Ok(Vec::new());
    }
    let listing = self.base_listing(dir).await?;
    Ok(
      listing
        .entries()
        .iter()
        .map(|entry| entry.path.file_name().unwrap_or_default().to_vec())
        .collect(),
    )
  }

  /// Whether a directory is empty in the merged view.
  async fn merged_dir_is_empty(&self, dir: &BytePath) -> Result<bool, GfsError> {
    let names = self.base_child_names(dir).await?;
    Ok(self.pinned().overlay.merged_dir_is_empty(dir, &names))
  }

  /// Every base descendant of a directory, for a rename that has to materialize
  /// the subtree as metadata.
  ///
  /// Bounded by the overlay's own rename limit so that a `mv` of a monorepo root
  /// fails before it has fetched a million directory pages rather than after.
  async fn base_descendants(&self, dir: &BytePath) -> Result<Vec<BaseDescendant>, GfsError> {
    let mut out = Vec::new();
    if self.pinned().overlay.masks_base(dir) {
      return Ok(out);
    }
    let limit = self.pinned().overlay.config().max_rename_entries;
    let mut queue = vec![BytePath::root()];
    while let Some(relative) = queue.pop() {
      let absolute = if relative.is_empty() {
        dir.clone()
      } else {
        BytePath::new(join(dir.as_bytes(), relative.as_bytes()))
      };
      let listing = self.base_listing(&absolute).await?;
      for entry in listing.entries() {
        let name = entry.path.file_name().unwrap_or_default().to_vec();
        let child = BytePath::new(if relative.is_empty() {
          name
        } else {
          join(relative.as_bytes(), &name)
        });
        if entry.kind == EntryKind::Directory {
          queue.push(child.clone());
        }
        let Some(facts) = base_facts(entry) else {
          continue;
        };
        out.push(BaseDescendant {
          relative: child,
          facts,
          symlink_target: entry.symlink_target.clone(),
        });
        if out.len() > limit {
          return Err(GfsError::new(
            ErrorCode::ResourceLimit,
            format!(
              "renaming {} would move more than {limit} entries",
              dir.escaped()
            ),
          ));
        }
      }
    }
    Ok(out)
  }

  /// Resolve a directory's full merged listing into its handle state.
  ///
  /// Filled to completion on the first call: the base half is one cached
  /// listing (see [`Gfs::base_listing`]), so there are no pages left to
  /// stream, and a complete `children` is what keeps every child's offset
  /// fixed for the life of the handle.
  async fn fill_directory(&self, state: &mut DirState) -> Result<(), GfsError> {
    // A passthrough directory lists once, from the real disk, on a blocking
    // worker. `.git` directories are small; there is nothing to page.
    if let Some(rel) = state.git_pending.take() {
      let git = Arc::clone(&self.git);
      let listing_rel = rel.clone();
      let listed = tokio::task::spawn_blocking(move || git.list(&listing_rel))
        .await
        .map_err(|e| GfsError::internal(format!("the git listing task failed: {e}")))?
        .map_err(|e| match e.kind() {
          std::io::ErrorKind::NotFound => GfsError::not_found("the git directory vanished"),
          _ => GfsError::internal(format!("listing a git directory: {e}")),
        })?;
      state.children = listed
        .into_iter()
        .map(|(name, meta)| Child::Git { name, meta })
        .collect();
      // The projection appears at `.git/gfs/objects` — injected as an Odb
      // child, so the record behind its inode routes to the projection and
      // never to a disk stat of a name the disk does not have.
      if self.git.has_projection() && rel == crate::passthrough::STATE_SUBDIR.as_bytes() {
        state.children.push(Child::Odb {
          name: b"objects".to_vec(),
          node: OdbNode::Dir,
        });
      }
      state.complete = true;
    }
    if !state.complete {
      let mut base_names = HashSet::new();
      if !self.pinned().overlay.masks_base(&state.path) {
        let listing = self.base_listing(&state.path).await?;
        for entry in listing.entries() {
          let name = entry.path.file_name().unwrap_or_default().to_vec();
          base_names.insert(name.clone());
          match self.pinned().overlay.resolve(&state.path.join(&name)) {
            Resolution::Absent => {}
            Resolution::Overlay(row) => state.children.push(Child::Overlay(row)),
            Resolution::Base => state.children.push(Child::Base(entry.clone())),
          }
        }
      }
      // Anything the overlay holds that the base never named, appended after
      // the base children. Always last, so a child's offset cannot move
      // between two `readdir` calls on one handle.
      for entry in self
        .pinned()
        .overlay
        .extra_children(&state.path, &base_names)
      {
        state.children.push(Child::Overlay(Box::new(entry)));
      }
      state.complete = true;
    }
    Ok(())
  }

  /// The bounds every prefetch runs inside, from this mount's configuration.
  fn prefetch_limits(&self) -> crate::prefetch::PrefetchLimits {
    crate::prefetch::PrefetchLimits {
      tree_page_entries: self.config.tree_page_entries,
      tree_max_entries: self.config.tree_prefetch_max_entries,
      content_max_bytes: self.config.read_prefetch_max_bytes,
      content_max_file_bytes: self.config.read_prefetch_max_file_bytes,
      content_concurrency: self.config.read_prefetch_concurrency,
      budget_reserve_percent: self.config.prefetch_budget_reserve_percent,
    }
  }

  /// Note that a base file was read, and fetch the rest of its directory once
  /// enough of them have been.
  fn note_read(&self, path: &BytePath) {
    if self.config.read_prefetch_threshold == 0 {
      return;
    }
    let pinned = self.pinned();
    let Some(dir) = pinned
      .prefetch
      .note_read(path, self.config.read_prefetch_threshold)
    else {
      return;
    };
    crate::prefetch::spawn_content(
      Arc::clone(&pinned),
      dir,
      self.prefetch_limits(),
      Arc::clone(&self.cache),
      Arc::clone(&self.budget),
      Arc::clone(&self.stats),
    );
  }

  /// Open a base blob for reading, fetching and verifying it if the cache lacks
  /// it.
  ///
  /// The ticket is minted here rather than at lookup time. A blob ticket is
  /// short-lived authorization state (the server issues it for five minutes), so
  /// one attached to every metadata lookup would usually expire unused; and when
  /// the blob is already cached, no ticket -- and no round trip -- is needed at
  /// all, which is what makes a warm reopen free.
  async fn open_blob(&self, path: &BytePath, oid: &ObjectId) -> Result<OpenedBlob, GfsError> {
    // Evidence for the read detector, taken whether or not the blob is cached:
    // what makes a directory look read-through is which files a job asked for,
    // not which of them happened to be local already.
    self.note_read(path);
    let source = self.client();
    if source.serves_blobs_in_memory() {
      // No cache file, no hash, no budget: the bytes are already on this
      // machine, and the source hands out the inflated blob it holds.
      return Ok(OpenedBlob::Memory(source.read_blob_shared(oid, "").await?));
    }
    if !self.cache.contains(oid) {
      let fresh = self
        .base_entry(path, true)
        .await?
        .ok_or_else(|| GfsError::not_found("the path vanished from a pinned commit"))?;
      if fresh.oid != *oid {
        // Impossible against an immutable commit, and therefore worth refusing
        // loudly rather than serving: it means the server answered about a
        // different snapshot than the one this mount is pinned to.
        return Err(GfsError::internal(
          "the server returned a different blob for a pinned path",
        ));
      }
      // The budget is charged here and nowhere else, because this is the only
      // place bytes cross the network for a base blob -- and it runs inside
      // `open`, which is where ADR 0009 requires the refusal to land. Admitting
      // *before* the fetch is the whole point: an `EDQUOT` issued after the
      // download has already spent what it was meant to protect.
      if let Err(e) = self.budget.admit(oid, fresh.size) {
        self.bump(|s| s.hydration_refusals += 1);
        tracing::warn!(
          path = ?path,
          size = fresh.size,
          "hydration budget refused a read: {e}"
        );
        return Err(e);
      }
      let ticket = fresh.blob_ticket.unwrap_or_default();
      let (cached, _) = self
        .cache
        .open_blob(&self.pinned().client, oid, &ticket)
        .await?;
      return open_cached(&cached).map(OpenedBlob::File);
    }
    let (cached, _) = self.cache.open_blob(&self.pinned().client, oid, "").await?;
    open_cached(&cached).map(OpenedBlob::File)
  }

  /// Give a path local content, fetching the base blob only when the caller will
  /// actually read it.
  ///
  /// `truncating` is the `O_TRUNC` case PLAN.md M3.2 calls out: a caller
  /// replacing a whole file must not pay to download the version it is throwing
  /// away.
  async fn copy_up(&self, record: &Record, truncating: bool) -> Result<OverlayEntry, GfsError> {
    let path = record.path.clone();
    let overlay = self.overlay();
    let ino = record.ino;

    // Already local: nothing to do, and nothing to fetch to find that out.
    if let Node::Overlay(entry) = &record.node {
      if entry.content.local_id().is_some() {
        if truncating && entry.size > 0 {
          let target = path.clone();
          return Self::blocking(move || overlay.truncate(&target, 0))
            .await
            .map_err(overlay_as_service_error);
        }
        return Ok((**entry).clone());
      }
    }

    let (base, source_oid) = match &record.node {
      Node::Overlay(entry) => (entry.base.clone(), entry.content.base_oid().cloned()),
      Node::Base(entry) => (base_facts(entry), Some(entry.oid.clone())),
      // Passthrough and projection entries never copy up: `.git` writes go to
      // the real disk, and the projection is read-only.
      Node::Git(_) | Node::Odb(_) => {
        return Err(GfsError::invalid("the .git subtree has no overlay"))
      }
    };

    if truncating {
      let target = path.clone();
      return Self::blocking(move || overlay.materialize(&target, base, ino, Source::Empty))
        .await
        .map_err(overlay_as_service_error);
    }

    let Some(oid) = source_oid else {
      return Err(GfsError::internal("nothing to copy up from"));
    };
    let blob = self.open_blob(&blob_path(record), &oid).await?;
    let size = match &blob {
      OpenedBlob::File(file) => file.metadata().map(|m| m.len()).unwrap_or(0),
      OpenedBlob::Memory(bytes) => bytes.len() as u64,
    };
    self.bump(|s| {
      s.copy_ups += 1;
      s.copy_up_bytes += size;
    });
    let target = path.clone();
    Self::blocking(move || match blob {
      OpenedBlob::File(file) => {
        let mut reader = std::io::BufReader::new(file);
        overlay.materialize(&target, base, ino, Source::Reader(&mut reader))
      }
      OpenedBlob::Memory(bytes) => {
        let mut reader = std::io::Cursor::new(bytes.as_slice());
        overlay.materialize(&target, base, ino, Source::Reader(&mut reader))
      }
    })
    .await
    .map_err(overlay_as_service_error)
  }

  /// Refresh a record from the overlay and return its attributes.
  fn republish(&self, path: &BytePath, entry: OverlayEntry) -> FileAttr {
    let record = self
      .inodes
      .lock()
      .expect("inode table")
      .insert_lookup(path.clone(), Node::Overlay(Box::new(entry)));
    let attr = self.attr(&record);
    // The lookup taken above is bookkeeping, not a kernel reference: a mutation
    // reply that also incremented the lookup count would leak a record per write.
    self
      .inodes
      .lock()
      .expect("inode table")
      .forget(record.ino, 1);
    attr
  }
}

/// Refuse a component longer than `NAME_MAX` on the way *in*.
///
/// The kernel does not enforce this for FUSE -- its own cap is 1024 bytes -- and
/// `f_namemax` from `statfs` is advisory, so the filesystem has to. The asymmetry
/// is deliberate: a base path that Git already holds with an over-long name stays
/// readable, because refusing to read what the commit contains would be worse.
/// What is refused is *creating* one, since the resulting tree could not be
/// checked out on any ordinary filesystem and the failure would surface on
/// someone else's machine rather than at the write that caused it.
fn check_name(name: &[u8]) -> Result<(), Errno> {
  if name.len() > gfs_types::limits::MAX_NAME_BYTES {
    return Err(Errno::ENAMETOOLONG);
  }
  Ok(())
}

fn join(prefix: &[u8], suffix: &[u8]) -> Vec<u8> {
  let mut out = prefix.to_vec();
  if !out.is_empty() {
    out.push(b'/');
  }
  out.extend_from_slice(suffix);
  out
}

/// The directory a validated path sits in; the root for a top-level name.
fn parent_of(path: &BytePath) -> BytePath {
  let bytes = path.as_bytes();
  match bytes.iter().rposition(|b| *b == b'/') {
    Some(slash) => BytePath::new(bytes[..slash].to_vec()),
    None => BytePath::root(),
  }
}

/// Where a row's base blob still lives in the pinned commit.
///
/// Almost always the row's own path. Not so after a rename: the blob is still in
/// the commit under the name it had there, and asking the server for the *new*
/// path gets an honest `ENOENT` for a file the workspace can plainly see. This is
/// what `renamed_from` is for on a `Content::Base` row.
fn blob_path(record: &Record) -> BytePath {
  match &record.node {
    Node::Overlay(entry) if entry.content.base_oid().is_some() => entry
      .renamed_from
      .clone()
      .unwrap_or_else(|| record.path.clone()),
    _ => record.path.clone(),
  }
}

/// The base facts a mutation records, from a tree entry.
///
/// `None` for kinds the overlay refuses to model — a gitlink or an unsupported
/// Git mode — so a mutation against one is refused rather than approximated.
fn base_facts(entry: &TreeEntryInfo) -> Option<BaseFacts> {
  OverlayKind::from_entry_kind(entry.kind)?;
  Some(BaseFacts {
    oid: entry.oid.clone(),
    kind: entry.kind,
    size: entry.size,
  })
}

/// An overlay refusal seen from a path that speaks the service vocabulary.
pub fn overlay_as_service_error(e: OverlayError) -> GfsError {
  GfsError::new(
    match e.condition {
      gfs_overlay::Condition::QuotaExceeded => ErrorCode::ResourceLimit,
      gfs_overlay::Condition::NoEntry => ErrorCode::NotFound,
      gfs_overlay::Condition::Io => ErrorCode::Internal,
      _ => ErrorCode::InvalidArgument,
    },
    e.message,
  )
}

fn open_cached(path: &std::path::Path) -> Result<std::fs::File, GfsError> {
  std::fs::File::open(path).map_err(|e| {
    GfsError::new(
      ErrorCode::Unavailable,
      format!("opening a cached blob: {}", e.kind()),
    )
  })
}

/// What the kernel may keep for a descriptor.
///
/// `NOFLUSH` for every file: nothing is buffered above the host filesystem (a
/// write reaches its content file inside the callback that carried it, see
/// `flush`), so the `FUSE_FLUSH` the kernel would otherwise send on every
/// `close` is a round trip that answers nothing. `KEEP_CACHE` for a base
/// blob: the bytes behind a pinned commit never change under one inode, so
/// the page cache may outlive the descriptor and the next open of the same
/// file reads from memory without asking. A write goes through the kernel,
/// which keeps its own cache coherent; a re-pin invalidates the inodes it
/// moves; and a path re-created after a delete announces a new size, which
/// the kernel treats as a reason to drop what it held.
fn fopen_flags(state: &FileState) -> fuser::FopenFlags {
  let mut flags = fuser::FopenFlags::FOPEN_NOFLUSH;
  if matches!(state, FileState::Memory { .. } | FileState::Blob { .. }) {
    flags |= fuser::FopenFlags::FOPEN_KEEP_CACHE;
  }
  flags
}

/// The `fuser` adapter: an `Arc<Gfs>` plus the runtime its callbacks dispatch to.
pub struct GfsFilesystem {
  fs: Arc<Gfs>,
  runtime: tokio::runtime::Handle,
}

impl std::fmt::Debug for GfsFilesystem {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("GfsFilesystem").finish_non_exhaustive()
  }
}

impl GfsFilesystem {
  pub fn new(fs: Arc<Gfs>, runtime: tokio::runtime::Handle) -> Self {
    GfsFilesystem { fs, runtime }
  }

  /// Run an operation and reply from wherever it finishes.
  ///
  /// The one place the ADR 0003 rule is implemented, so there is exactly one
  /// place to check that no callback blocks. The future is polled once right
  /// here, on the FUSE thread, inside the runtime's context: an operation the
  /// caches can answer -- a lookup in a listed directory, an open of a blob
  /// the source holds in memory, a read of that blob, a page of a directory
  /// already assembled -- completes without leaving this thread, and the
  /// reply is written before the next request is read. Only a future that
  /// has to wait (a fetch, a `spawn_blocking`, a lock someone holds) is handed
  /// to the runtime, which polls it again the moment a worker picks it up.
  ///
  /// Measured on the local-mode workspace: the hand-off alone was a third of
  /// the daemon's per-request wall time, and for a workload that opens and
  /// reads one small file after another every request is a cache hit.
  ///
  /// The first poll uses a waker that does nothing. That is sound because a
  /// spawned task is polled once unconditionally, and every primitive the
  /// futures here wait on re-registers whichever waker the current poll
  /// carries -- the `Future` contract, not a property of one crate.
  fn spawn<F>(&self, future: F)
  where
    F: std::future::Future<Output = ()> + Send + 'static,
  {
    let mut future = Box::pin(future);
    let _runtime = self.runtime.enter();
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    if future.as_mut().poll(&mut context).is_pending() {
      self.runtime.spawn(future);
    }
  }
}

impl Filesystem for GfsFilesystem {
  fn init(&mut self, _req: &Request, config: &mut fuser::KernelConfig) -> std::io::Result<()> {
    // Without `ATOMIC_O_TRUNC` the kernel splits `open(O_TRUNC)` into an open
    // followed by `setattr(size = 0)`. The open then copies the whole blob up
    // before the truncate throws it away, which is exactly the hydration PLAN.md
    // M3.2's `O_TRUNC` bullet exists to avoid -- and `std::fs::File::create` is
    // the single most common way an agent replaces a file.
    //
    // Requested rather than required: a kernel that refuses it still works, and
    // the `setattr(size = 0)` path below is careful not to fetch either.
    if config
      .add_capabilities(fuser::InitFlags::FUSE_ATOMIC_O_TRUNC)
      .is_err()
    {
      tracing::warn!(
        "the kernel refused FUSE_ATOMIC_O_TRUNC; replacing a file will copy its \
         old contents up before discarding them"
      );
    }
    // Everything below trades daemon round trips for kernel-side state, and
    // each one is safe here for a reason worth writing down:
    // * `PARALLEL_DIROPS`: the kernel otherwise serializes lookups and
    //   listings inside one directory. Every handler here is already
    //   concurrent across directories; nothing in the listing cache or the
    //   overlay assumes one caller per parent.
    // * `DO_READDIRPLUS` + `READDIRPLUS_AUTO`: `readdirplus` has always been
    //   implemented below but never advertised, so the kernel never used it.
    //   With `AUTO` the kernel asks for it only after seeing a listing followed
    //   by lookups of the listed names (`ls -l`, a walk that stats), and keeps
    //   plain `readdir` for a walk that does not.
    // Symlink caching is deliberately absent: a path keeps its inode number
    // across delete and re-create, and the kernel does not drop a cached link
    // target for a symlink whose inode it already holds.
    let wanted = fuser::InitFlags::FUSE_PARALLEL_DIROPS;
    let offered = wanted & config.capabilities();
    if let Err(refused) = config.add_capabilities(offered) {
      tracing::warn!(?refused, "the kernel refused directory capabilities it offered");
    }
    if offered != wanted {
      tracing::info!(missing = ?(wanted - offered), "the kernel does not offer every directory capability");
    }
    Ok(())
  }

  fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: fuser::ReplyEntry) {
    let fs = Arc::clone(&self.fs);
    let name = name.as_bytes().to_vec();
    self.spawn(async move {
      let Some(parent) = fs.record(parent.0) else {
        return reply.error(Errno::ESTALE);
      };

      // The `.git` subtree first, so a repository that somehow contained a
      // `.git` entry could not shadow it. Git refuses to record one, but the
      // ordering costs nothing and removes the question.
      if Gfs::routes_to_git(&parent, &name) {
        let path = parent.path.join(&name);
        let parent_rel = git_rel(&parent.path).unwrap_or_default().to_vec();
        match fs.lookup_git(&path).await {
          Ok(Some(node)) => {
            let record = fs
              .inodes
              .lock()
              .expect("inode table")
              .insert_lookup(path, node);
            fs.bump(|s| s.lookups += 1);
            reply.entry(&fs.config.git_ttl, &fs.attr(&record), GENERATION);
          }
          Ok(None) => {
            fs.bump(|s| s.negative_lookups += 1);
            // The negative dentry ADR 0011 requires on the object namespace:
            // Git probes thousands of absent loose objects per command, and
            // this is what keeps the repeats inside the kernel.
            reply.entry(
              &fs.git_negative_ttl(&parent_rel),
              &negative_attr(),
              GENERATION,
            );
          }
          Err(errno) => {
            fs.bump(|s| s.errors += 1);
            reply.error(errno);
          }
        }
        return;
      }

      match fs.lookup_child(&parent, &name).await {
        Ok(Some(node)) => {
          let path = match &node {
            Node::Base(entry) => entry.path.clone(),
            Node::Overlay(entry) => entry.path.clone(),
            Node::Git(_) | Node::Odb(_) => parent.path.join(&name),
          };
          let record = fs
            .inodes
            .lock()
            .expect("inode table")
            .insert_lookup(path, node);
          fs.bump(|s| s.lookups += 1);
          reply.entry(&fs.config.ttl, &fs.attr(&record), GENERATION);
        }
        Ok(None) => {
          fs.bump(|s| s.negative_lookups += 1);
          // A negative entry with a TTL, which is what an immutable commit
          // permits: the kernel stops asking. Signalled by inode zero, the
          // low-level FUSE convention -- `reply.error(ENOENT)` would be correct
          // but would force an upcall for every repeated miss, and a compiler
          // searching an include path produces thousands of them.
          reply.entry(&fs.config.negative_ttl, &negative_attr(), GENERATION);
        }
        Err(e) => {
          fs.bump(|s| s.errors += 1);
          reply.error(errno_of(&e));
        }
      }
    });
  }

  fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
    self
      .fs
      .inodes
      .lock()
      .expect("inode table")
      .forget(ino.0, nlookup);
  }

  fn getattr(
    &self,
    _req: &Request,
    ino: INodeNo,
    _fh: Option<FileHandle>,
    reply: fuser::ReplyAttr,
  ) {
    // Answered inline for the merged view: it never touches the network.
    // Everything `getattr` needs was recorded by the `lookup` that made the
    // kernel aware of the inode, and a mutation replies with the attributes it
    // produced. Git passthrough entries re-stat instead — the real file's
    // size and mtime move under lockfile churn, and Git compares them.
    match self.fs.record(ino.0) {
      Some(record) if record.node.is_git() => {
        let fs = Arc::clone(&self.fs);
        self.spawn(async move {
          let Some(rel) = git_rel(&record.path).map(<[u8]>::to_vec) else {
            return reply.error(Errno::ESTALE);
          };
          let git = Arc::clone(&fs.git);
          match tokio::task::spawn_blocking(move || git.stat(&rel)).await {
            Ok(Ok(meta)) => {
              let refreshed = {
                let mut table = fs.inodes.lock().expect("inode table");
                table.refresh(record.ino, Node::Git(meta));
                table.get(record.ino).cloned()
              };
              match refreshed {
                Some(record) => reply.attr(&fs.config.git_ttl, &fs.attr(&record)),
                None => reply.error(Errno::ESTALE),
              }
            }
            Ok(Err(e)) => reply.error(errno_io(&e)),
            Err(_) => reply.error(Errno::EIO),
          }
        });
      }
      Some(record) => reply.attr(&self.fs.config.ttl, &self.fs.attr(&record)),
      None => reply.error(Errno::ESTALE),
    }
  }

  fn readlink(&self, _req: &Request, ino: INodeNo, reply: fuser::ReplyData) {
    let fs = Arc::clone(&self.fs);
    self.spawn(async move {
      let Some(record) = fs.record(ino.0) else {
        return reply.error(Errno::ESTALE);
      };
      if let Node::Git(meta) = &record.node {
        if meta.kind != fuser::FileType::Symlink {
          return reply.error(Errno::EINVAL);
        }
        let git = Arc::clone(&fs.git);
        let Some(rel) = git_rel(&record.path).map(<[u8]>::to_vec) else {
          return reply.error(Errno::ESTALE);
        };
        let target = tokio::task::spawn_blocking(move || std::fs::read_link(git.real(&rel))).await;
        return match target {
          Ok(Ok(target)) => reply.data(target.as_os_str().as_bytes()),
          Ok(Err(e)) => reply.error(errno_io(&e)),
          Err(_) => reply.error(Errno::EIO),
        };
      }
      let (oid, expected) = match &record.node {
        Node::Overlay(entry) => {
          if entry.kind != OverlayKind::Symlink {
            return reply.error(Errno::EINVAL);
          }
          // A created or moved symlink keeps its target in the row, so this
          // costs nothing. A symlink moved from the base keeps the base's blob.
          if let Some(target) = &entry.symlink_target {
            return reply.data(target);
          }
          match entry.content.base_oid() {
            Some(oid) => (oid.clone(), entry.size),
            None => return reply.error(Errno::EIO),
          }
        }
        Node::Base(entry) => {
          if entry.kind != EntryKind::Symlink {
            return reply.error(Errno::EINVAL);
          }
          // The server returns the target with the entry, so an `ls -l` of a
          // directory full of symlinks resolves every one without fetching a
          // blob.
          if let Some(target) = &entry.symlink_target {
            return reply.data(target);
          }
          (entry.oid.clone(), entry.size)
        }
        Node::Git(_) | Node::Odb(_) => return reply.error(Errno::EINVAL),
      };
      match fs.open_blob(&blob_path(&record), &oid).await {
        Ok(OpenedBlob::File(file)) => match read_all(&file, expected) {
          Ok(bytes) => reply.data(&bytes),
          Err(e) => reply.error(errno_of(&e)),
        },
        Ok(OpenedBlob::Memory(bytes)) => reply.data(&bytes),
        Err(e) => reply.error(errno_of(&e)),
      }
    });
  }

  fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: fuser::ReplyOpen) {
    let fs = Arc::clone(&self.fs);
    let writable = flags.acc_mode() != OpenAccMode::O_RDONLY;
    let truncating = flags.0 & libc::O_TRUNC != 0;
    self.spawn(async move {
      let Some(record) = fs.record(ino.0) else {
        return reply.error(Errno::ESTALE);
      };
      if writable && record.node.is_odb() {
        // The projection is read-only by construction: its files are immutable
        // by name, and a write here could only corrupt the shared store.
        return reply.error(Errno::EROFS);
      }
      let state = match &record.node {
        Node::Git(meta) => match meta.kind {
          fuser::FileType::Directory => return reply.error(Errno::EISDIR),
          fuser::FileType::Symlink => return reply.error(Errno::ELOOP),
          _ => {
            let Some(rel) = git_rel(&record.path).map(<[u8]>::to_vec) else {
              return reply.error(Errno::ESTALE);
            };
            let git = Arc::clone(&fs.git);
            let acc = flags.0 & libc::O_ACCMODE;
            // O_APPEND is deliberately not forwarded: the kernel resolves
            // append offsets before they reach FUSE, and a double-append
            // would corrupt whatever Git was appending to (m05c's note).
            let opened = tokio::task::spawn_blocking(move || {
              std::fs::OpenOptions::new()
                .read(acc != libc::O_WRONLY)
                .write(acc != libc::O_RDONLY)
                .truncate(truncating && acc != libc::O_RDONLY)
                .open(git.real(&rel))
            })
            .await;
            match opened {
              Ok(Ok(file)) => FileState::Git {
                file: Arc::new(file),
                writable,
              },
              Ok(Err(e)) => return reply.error(errno_io(&e)),
              Err(_) => return reply.error(Errno::EIO),
            }
          }
        },
        Node::Odb(OdbNode::Dir) => return reply.error(Errno::EISDIR),
        Node::Odb(OdbNode::File { path, .. }) => FileState::Odb { path: path.clone() },
        Node::Overlay(entry) if entry.kind.is_dir() => return reply.error(Errno::EISDIR),
        Node::Overlay(entry) if entry.kind == OverlayKind::Symlink => {
          return reply.error(Errno::ELOOP)
        }
        Node::Base(entry) if entry.kind == EntryKind::Symlink => return reply.error(Errno::ELOOP),
        Node::Base(entry) if entry.kind.is_dir_like() => return reply.error(Errno::EISDIR),
        // Predictable rather than approximated: DESIGN.md section 8.2 refuses to
        // present a mode GFS does not model as the nearest one it does.
        Node::Base(entry) if matches!(entry.kind, EntryKind::Unsupported(_)) => {
          return reply.error(Errno::EIO)
        }
        _ if writable || truncating => match fs.copy_up(&record, truncating).await {
          Ok(entry) => {
            let Some(id) = entry.content.local_id() else {
              return reply.error(Errno::EIO);
            };
            match fs.overlay().content_store().open_write(id) {
              Ok(file) => {
                fs.republish(&record.path, entry);
                FileState::Local {
                  content_id: id,
                  file: Arc::new(file),
                  writable: true,
                }
              }
              Err(e) => return reply.error(errno_of_overlay(&e)),
            }
          }
          Err(e) => {
            fs.bump(|s| s.errors += 1);
            return reply.error(errno_of(&e));
          }
        },
        Node::Overlay(entry) => match entry.content.local_id() {
          Some(id) => match fs.overlay().content_store().open_read(id) {
            Ok(file) => FileState::Local {
              content_id: id,
              file: Arc::new(file),
              writable: false,
            },
            Err(e) => return reply.error(errno_of_overlay(&e)),
          },
          // A row whose bytes are still the base's: a mode change or a rename.
          None => match entry.content.base_oid() {
            Some(oid) => match fs.open_blob(&blob_path(&record), oid).await {
              Ok(opened) => opened.into_state(oid),
              Err(e) => {
                fs.bump(|s| s.errors += 1);
                return reply.error(errno_of(&e));
              }
            },
            None => return reply.error(Errno::EIO),
          },
        },
        Node::Base(entry) => match fs.open_blob(&record.path, &entry.oid).await {
          Ok(opened) => opened.into_state(&entry.oid),
          Err(e) => {
            fs.bump(|s| s.errors += 1);
            return reply.error(errno_of(&e));
          }
        },
      };
      let flags = fopen_flags(&state);
      let handle = fs.new_handle();
      fs.inodes.lock().expect("inode table").open(ino.0);
      fs.files
        .lock()
        .expect("file handles")
        .insert(handle, Arc::new(state));
      fs.bump(|s| s.opens += 1);
      reply.opened(FileHandle(handle), flags);
    });
  }

  fn create(
    &self,
    _req: &Request,
    parent: INodeNo,
    name: &OsStr,
    mode: u32,
    umask: u32,
    flags: i32,
    reply: fuser::ReplyCreate,
  ) {
    let fs = Arc::clone(&self.fs);
    let name = name.as_bytes().to_vec();
    self.spawn(async move {
      let Some(parent) = fs.record(parent.0) else {
        return reply.error(Errno::ESTALE);
      };
      if parent.node.is_odb() {
        return reply.error(Errno::EROFS);
      }
      if let Err(e) = check_name(&name) {
        return reply.error(e);
      }
      let path = parent.path.join(&name);
      // The passthrough create. O_EXCL is forwarded faithfully — it is Git's
      // lockfile protocol, and mapping it to a plain create would make every
      // lock race invisible.
      if parent.node.is_git() {
        let Some(rel) = git_rel(&path).map(<[u8]>::to_vec) else {
          return reply.error(Errno::EROFS);
        };
        if odb_rel(&rel).is_some() {
          return reply.error(Errno::EROFS);
        }
        let git = Arc::clone(&fs.git);
        let created = tokio::task::spawn_blocking(move || {
          use std::os::unix::fs::OpenOptionsExt;
          let mut opts = std::fs::OpenOptions::new();
          let acc = flags & libc::O_ACCMODE;
          opts
            .read(acc != libc::O_WRONLY)
            .write(true)
            .truncate(flags & libc::O_TRUNC != 0)
            .mode(mode & !umask & 0o7777);
          if flags & libc::O_EXCL != 0 {
            opts.create_new(true);
          } else {
            opts.create(true);
          }
          let file = opts.open(git.real(&rel))?;
          let meta = GitMeta::of(&file.metadata()?);
          Ok::<_, std::io::Error>((file, meta))
        })
        .await;
        return match created {
          Ok(Ok((file, meta))) => {
            let record = fs
              .inodes
              .lock()
              .expect("inode table")
              .insert_lookup(path, Node::Git(meta));
            let attr = fs.attr(&record);
            let handle = fs.new_handle();
            fs.inodes.lock().expect("inode table").open(record.ino);
            fs.files.lock().expect("file handles").insert(
              handle,
              Arc::new(FileState::Git {
                file: Arc::new(file),
                writable: true,
              }),
            );
            fs.bump(|s| {
              s.lookups += 1;
              s.opens += 1;
            });
            reply.created(
              &fs.config.git_ttl,
              &attr,
              GENERATION,
              FileHandle(handle),
              fuser::FopenFlags::FOPEN_NOFLUSH,
            )
          }
          Ok(Err(e)) => {
            fs.bump(|s| s.errors += 1);
            reply.error(errno_io(&e))
          }
          Err(_) => reply.error(Errno::EIO),
        };
      }
      let resolved = match fs.resolve_path(&path).await {
        Ok(resolved) => resolved,
        Err(e) => return reply.error(errno_of(&e)),
      };
      if resolved.node.is_some() {
        return reply.error(Errno::EEXIST);
      }
      // Git records exactly one permission bit, so that is the one this reads.
      let executable = mode & !umask & 0o111 != 0;
      let overlay = fs.overlay();
      let parent_base = Gfs::parent_base(&parent);
      let target = path.clone();
      let ino = fs.number_for(&path);
      let created = Gfs::blocking(move || {
        overlay.create_file(&target, resolved.base, parent_base, ino, executable)
      })
      .await;
      let entry = match created {
        Ok(entry) => entry,
        Err(e) => {
          fs.bump(|s| s.errors += 1);
          return reply.error(errno_of_overlay(&e));
        }
      };
      let Some(id) = entry.content.local_id() else {
        return reply.error(Errno::EIO);
      };
      let file = match fs.overlay().content_store().open_write(id) {
        Ok(file) => file,
        Err(e) => return reply.error(errno_of_overlay(&e)),
      };

      let record = fs
        .inodes
        .lock()
        .expect("inode table")
        .insert_lookup(path, Node::Overlay(Box::new(entry)));
      let attr = fs.attr(&record);
      let handle = fs.new_handle();
      fs.inodes.lock().expect("inode table").open(record.ino);
      fs.files.lock().expect("file handles").insert(
        handle,
        Arc::new(FileState::Local {
          content_id: id,
          file: Arc::new(file),
          writable: true,
        }),
      );
      fs.bump(|s| {
        s.lookups += 1;
        s.opens += 1;
      });
      fs.touch_parent(&parent).await;
      reply.created(
        &fs.config.ttl,
        &attr,
        GENERATION,
        FileHandle(handle),
        fuser::FopenFlags::FOPEN_NOFLUSH,
      );
    });
  }

  fn read(
    &self,
    _req: &Request,
    _ino: INodeNo,
    fh: FileHandle,
    offset: u64,
    size: u32,
    _flags: OpenFlags,
    _lock_owner: Option<fuser::LockOwner>,
    reply: fuser::ReplyData,
  ) {
    let fs = Arc::clone(&self.fs);
    let state = fs.files.lock().expect("file handles").get(&fh.0).cloned();
    let Some(state) = state else {
      return reply.error(Errno::EBADF);
    };
    self.spawn(async move {
      let file = match &*state {
        FileState::Odb { path } => {
          // The projection read: absent blocks are fetched, resident ones
          // served, and the cost attributed to this workspace's view.
          let path = path.clone();
          let Some(store) = fs.git.store().cloned() else {
            return reply.error(Errno::EIO);
          };
          match store.read(&path, offset, size).await {
            Ok((bytes, cost)) => {
              fs.git.counters().add(&cost);
              fs.bump(|s| {
                s.reads += 1;
                s.read_bytes += bytes.len() as u64;
              });
              return reply.data(&bytes);
            }
            Err(e) => {
              // "Repacked away" is the one expected failure, and ESTALE is
              // its errno: the name is gone, not wrong (ADR 0009's retention
              // policy). Everything else is EIO.
              let errno = if e.code == ErrorCode::FailedPrecondition {
                Errno::ESTALE
              } else {
                Errno::EIO
              };
              tracing::warn!(path, offset, "odb read failed: {e}");
              fs.bump(|s| s.errors += 1);
              return reply.error(errno);
            }
          }
        }
        FileState::Memory { bytes, .. } => {
          // A slice of the inflated blob. Nothing blocks and nothing is copied
          // beyond the reply itself.
          let start = (offset as usize).min(bytes.len());
          let end = start.saturating_add(size as usize).min(bytes.len());
          fs.bump(|s| {
            s.reads += 1;
            s.read_bytes += (end - start) as u64;
          });
          return reply.data(&bytes[start..end]);
        }
        FileState::Blob { file, .. }
        | FileState::Local { file, .. }
        | FileState::Git { file, .. } => Arc::clone(file),
      };
      // Even a page-cache hit is a blocking syscall, and ADR 0003's measurement
      // is about what one blocked worker costs the whole mount.
      let result = tokio::task::spawn_blocking(move || {
        let mut buffer = vec![0u8; size as usize];
        let read = file.read_at(&mut buffer, offset)?;
        buffer.truncate(read);
        Ok::<_, std::io::Error>(buffer)
      })
      .await;
      match result {
        Ok(Ok(bytes)) => {
          fs.bump(|s| {
            s.reads += 1;
            s.read_bytes += bytes.len() as u64;
          });
          reply.data(&bytes);
        }
        Ok(Err(_)) | Err(_) => {
          fs.bump(|s| s.errors += 1);
          reply.error(Errno::EIO);
        }
      }
    });
  }

  fn write(
    &self,
    _req: &Request,
    ino: INodeNo,
    fh: FileHandle,
    offset: u64,
    data: &[u8],
    _write_flags: fuser::WriteFlags,
    _flags: OpenFlags,
    _lock_owner: Option<fuser::LockOwner>,
    reply: fuser::ReplyWrite,
  ) {
    let fs = Arc::clone(&self.fs);
    let state = fs.files.lock().expect("file handles").get(&fh.0).cloned();
    let Some(state) = state else {
      return reply.error(Errno::EBADF);
    };
    let data = data.to_vec();
    self.spawn(async move {
      // The passthrough write: straight to the real file. No overlay row, no
      // journal — `.git` is Git's own state and the disk is its journal.
      if let FileState::Git { file, writable } = &*state {
        if !writable {
          return reply.error(Errno::EBADF);
        }
        let file = Arc::clone(file);
        let written = tokio::task::spawn_blocking(move || file.write_at(&data, offset)).await;
        return match written {
          Ok(Ok(n)) => {
            fs.bump(|s| {
              s.writes += 1;
              s.written_bytes += n as u64;
            });
            reply.written(n as u32)
          }
          Ok(Err(e)) => {
            fs.bump(|s| s.errors += 1);
            reply.error(errno_io(&e))
          }
          Err(_) => reply.error(Errno::EIO),
        };
      }
      let (content_id, file) = match &*state {
        FileState::Local {
          content_id,
          file,
          writable: true,
        } => (*content_id, Arc::clone(file)),
        // Not `EROFS`: the filesystem is writable, this descriptor is not.
        _ => return reply.error(Errno::EBADF),
      };
      let overlay = fs.overlay();
      let written =
        Gfs::blocking(move || overlay.write_content(content_id, &file, offset, &data)).await;
      match written {
        Ok(written) => {
          fs.bump(|s| {
            s.writes += 1;
            s.written_bytes += written as u64;
          });
          // The record the kernel will consult for `size` on the next `getattr`.
          // An unlinked-but-open file has no row left, so its record is updated
          // in place instead -- otherwise `getattr` keeps reporting the size the
          // file had when its name was removed, and a read through the same
          // descriptor stops short.
          if let Some(record) = fs.record(ino.0) {
            match fs.overlay().get(&record.path) {
              Some(entry) => {
                fs.republish(&record.path, entry);
              }
              None => {
                if let Node::Overlay(entry) = &record.node {
                  let grown = OverlayEntry {
                    size: entry.size.max(offset.saturating_add(written as u64)),
                    ..(**entry).clone()
                  };
                  fs.inodes
                    .lock()
                    .expect("inode table")
                    .refresh(ino.0, Node::Overlay(Box::new(grown)));
                }
              }
            }
          }
          reply.written(written as u32);
        }
        Err(e) => {
          fs.bump(|s| s.errors += 1);
          reply.error(errno_of_overlay(&e));
        }
      }
    });
  }

  fn release(
    &self,
    _req: &Request,
    ino: INodeNo,
    fh: FileHandle,
    _flags: OpenFlags,
    _lock_owner: Option<fuser::LockOwner>,
    _flush: bool,
    reply: fuser::ReplyEmpty,
  ) {
    let state = self.fs.files.lock().expect("file handles").remove(&fh.0);
    if let Some(state) = state {
      if let FileState::Blob { oid, .. } = &*state {
        // Unpin only when this was the last reference to the state, which it is:
        // the map owned the only other `Arc`, and a concurrent `read` holding one
        // keeps the file open through its own clone.
        self.fs.cache.release_blob(oid);
      }
    }
    self.fs.inodes.lock().expect("inode table").close(ino.0);
    reply.ok();
  }

  fn flush(
    &self,
    _req: &Request,
    _ino: INodeNo,
    _fh: FileHandle,
    _lock_owner: fuser::LockOwner,
    reply: fuser::ReplyEmpty,
  ) {
    // Nothing is buffered above the host filesystem: a write reaches the content
    // file inside the callback that carried it, so there is nothing to flush.
    // Durability is `fsync`'s job, below.
    reply.ok();
  }

  fn fsync(
    &self,
    _req: &Request,
    _ino: INodeNo,
    fh: FileHandle,
    _datasync: bool,
    reply: fuser::ReplyEmpty,
  ) {
    let fs = Arc::clone(&self.fs);
    let state = fs.files.lock().expect("file handles").get(&fh.0).cloned();
    self.spawn(async move {
      // A passthrough descriptor syncs the real file and nothing else: Git's
      // durability protocol (lockfile, fsync, rename) is talking to its own
      // directory, and the overlay journal has no part in it.
      if let Some(FileState::Git { file, .. }) = state.as_deref() {
        let file = Arc::clone(file);
        return match tokio::task::spawn_blocking(move || file.sync_all()).await {
          Ok(Ok(())) => reply.ok(),
          Ok(Err(e)) => reply.error(errno_io(&e)),
          Err(_) => reply.error(Errno::EIO),
        };
      }
      // A projected file has nothing local to make durable, and neither does a
      // blob held in memory.
      if let Some(FileState::Odb { .. } | FileState::Memory { .. }) = state.as_deref() {
        return reply.ok();
      }
      // Both halves, and in this order: the content file, then the journal that
      // names it. The store's invariant is that a committed row's content
      // exists, and syncing the journal first would invert it for exactly the
      // window a power loss could land in.
      let file = match state.as_deref() {
        Some(FileState::Local { file, .. }) => Some(Arc::clone(file)),
        _ => None,
      };
      let overlay = fs.overlay();
      let result = tokio::task::spawn_blocking(move || {
        if let Some(file) = file {
          file
            .sync_all()
            .map_err(|e| OverlayError::io(format!("syncing overlay content: {e}")))?;
        }
        overlay.sync()
      })
      .await;
      match result {
        Ok(Ok(())) => reply.ok(),
        Ok(Err(e)) => reply.error(errno_of_overlay(&e)),
        Err(_) => reply.error(Errno::EIO),
      }
    });
  }

  fn fsyncdir(
    &self,
    _req: &Request,
    ino: INodeNo,
    _fh: FileHandle,
    _datasync: bool,
    reply: fuser::ReplyEmpty,
  ) {
    let fs = Arc::clone(&self.fs);
    self.spawn(async move {
      // Routed by subtree, and this routing is load-bearing rather than tidy:
      // SQLite fsyncs the journal's own *directory* by its canonicalized
      // on-disk path — which resolves through this very mount — on some
      // commits. That request must sync the real directory and nothing else;
      // reaching `overlay.sync()` here would try to take the overlay lock the
      // committing thread already holds, and the mount would deadlock against
      // its own journal (found live, on the first `echo >>` into a workspace).
      if let Some(record) = fs.record(ino.0) {
        if record.node.is_git() {
          let git = Arc::clone(&fs.git);
          let Some(rel) = git_rel(&record.path).map(<[u8]>::to_vec) else {
            return reply.error(Errno::ESTALE);
          };
          let synced =
            tokio::task::spawn_blocking(move || std::fs::File::open(git.real(&rel))?.sync_all())
              .await;
          return match synced {
            Ok(Ok(())) => reply.ok(),
            Ok(Err(e)) => reply.error(errno_io(&e)),
            Err(_) => reply.error(Errno::EIO),
          };
        }
        if record.node.is_odb() {
          return reply.ok();
        }
      }
      let overlay = fs.overlay();
      match tokio::task::spawn_blocking(move || overlay.sync()).await {
        Ok(Ok(())) => reply.ok(),
        Ok(Err(e)) => reply.error(errno_of_overlay(&e)),
        Err(_) => reply.error(Errno::EIO),
      }
    });
  }

  fn lseek(
    &self,
    _req: &Request,
    ino: INodeNo,
    _fh: FileHandle,
    offset: i64,
    whence: i32,
    reply: fuser::ReplyLseek,
  ) {
    // Only `SEEK_DATA` and `SEEK_HOLE` reach a filesystem; ordinary seeks are the
    // kernel's business. Neither a Git blob nor an overlay file is sparse, so the
    // whole file is data and the only hole is at the end.
    let Some(record) = self.fs.record(ino.0) else {
      return reply.error(Errno::ESTALE);
    };
    let size = match &record.node {
      Node::Base(entry) => entry.size,
      Node::Overlay(entry) => entry.size,
      Node::Git(meta) => meta.size,
      Node::Odb(node) => node.size(),
    } as i64;
    if offset < 0 || offset >= size {
      return reply.error(Errno::ENXIO);
    }
    match whence {
      libc::SEEK_DATA => reply.offset(offset),
      libc::SEEK_HOLE => reply.offset(size),
      _ => reply.error(Errno::EINVAL),
    }
  }

  fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: fuser::ReplyOpen) {
    let Some(record) = self.fs.record(ino.0) else {
      return reply.error(Errno::ESTALE);
    };
    let mut state = DirState {
      path: record.path.clone(),
      children: Vec::new(),
      complete: false,
      git_pending: None,
    };
    match &record.node {
      Node::Git(meta) => {
        if !meta.is_dir() {
          return reply.error(Errno::ENOTDIR);
        }
        // The real listing is deferred to `fill_directory`, where it runs on
        // a blocking worker rather than on this event-loop thread.
        let Some(rel) = git_rel(&record.path).map(<[u8]>::to_vec) else {
          return reply.error(Errno::ESTALE);
        };
        state.git_pending = Some(rel);
      }
      Node::Odb(node) => {
        if !node.is_dir() {
          return reply.error(Errno::ENOTDIR);
        }
        // The projection's tree is interned in memory, so this is a map walk,
        // not disk.
        let rel = git_rel(&record.path)
          .and_then(odb_rel)
          .unwrap_or_default()
          .to_vec();
        state.children = self
          .fs
          .git
          .odb_children(&rel)
          .into_iter()
          .map(|(name, node)| Child::Odb { name, node })
          .collect();
        state.complete = true;
      }
      // A created directory shadows the base; `fill_directory` checks the
      // mask itself before touching the base listing.
      Node::Overlay(entry) if entry.kind.is_dir() => {}
      Node::Overlay(_) => return reply.error(Errno::ENOTDIR),
      Node::Base(entry) => match entry.kind {
        // A submodule is an empty directory that lists successfully rather than
        // erroring, which is what DESIGN.md section 8.2 specifies.
        EntryKind::Gitlink => state.complete = true,
        EntryKind::Directory => {
          // The root carries `.git` as its first child, so the offset of
          // every other entry stays the same no matter how the listing pages.
          // Appending it last would require exhausting the listing before the
          // first entry could be emitted. The meta is a placeholder a
          // directory always satisfies; lookup and getattr re-stat.
          if ino.0 == ROOT_INO {
            state.children.push(Child::Git {
              name: GIT_DIR_NAME.as_bytes().to_vec(),
              meta: GitMeta {
                kind: fuser::FileType::Directory,
                size: 0,
                perm: 0o755,
                nlink: 2,
                mtime: std::time::UNIX_EPOCH,
                ctime: std::time::UNIX_EPOCH,
              },
            });
          }
        }
        _ => return reply.error(Errno::ENOTDIR),
      },
    }

    // The kernel may keep the listing of a merged-view directory in its page
    // cache across `opendir` calls: the base never changes under one pin, every
    // overlay mutation goes through the kernel (which bumps the directory's
    // version itself), and a re-pin invalidates the inodes it moves. The two
    // passthrough trees are excluded because the daemon writes into them
    // behind the kernel's back -- the index, `packed-refs`, the projection.
    //
    // `CACHE_DIR` alone is not enough: `fuse_dir_open` drops a directory's
    // pages on every open unless `KEEP_CACHE` is set too, and the listing
    // cache lives in those pages. Without both, the kernel refilled the cache
    // on every `opendir` and never once read from it.
    let flags = if record.node.is_git() || record.node.is_odb() {
      fuser::FopenFlags::empty()
    } else {
      fuser::FopenFlags::FOPEN_CACHE_DIR | fuser::FopenFlags::FOPEN_KEEP_CACHE
    };
    let handle = self.fs.new_handle();
    self.fs.inodes.lock().expect("inode table").open(ino.0);
    self
      .fs
      .dirs
      .lock()
      .expect("dir handles")
      .insert(handle, Arc::new(tokio::sync::Mutex::new(state)));
    reply.opened(FileHandle(handle), flags);
  }

  fn readdir(
    &self,
    _req: &Request,
    ino: INodeNo,
    fh: FileHandle,
    offset: u64,
    mut reply: fuser::ReplyDirectory,
  ) {
    let fs = Arc::clone(&self.fs);
    let state = fs.dirs.lock().expect("dir handles").get(&fh.0).cloned();
    let Some(state) = state else {
      return reply.error(Errno::EBADF);
    };
    self.spawn(async move {
      let mut state = state.lock().await;
      let mut index = offset;
      loop {
        // Two synthetic entries first. Not optional: `find`, `rm -r`, and any
        // `readdir` loop that checks for them treat their absence as a broken
        // directory.
        if index == 0 {
          if reply.add(INodeNo(ino.0), 1, fuser::FileType::Directory, ".") {
            break;
          }
          index = 1;
          continue;
        }
        if index == 1 {
          if reply.add(INodeNo(ino.0), 2, fuser::FileType::Directory, "..") {
            break;
          }
          index = 2;
          continue;
        }
        if let Err(e) = fs.fill_directory(&mut state).await {
          fs.bump(|s| s.errors += 1);
          return reply.error(errno_of(&e));
        }
        let Some(child) = state.children.get((index - 2) as usize).cloned() else {
          break;
        };
        let name = child.name();
        // Inodes are still assigned here, because the caller of `readdir` will
        // almost always `stat` what it found, and assigning now keeps that
        // `lookup` from having to allocate under a different lock.
        let child_ino = {
          let path = state.path.join(&name);
          let mut table = fs.inodes.lock().expect("inode table");
          let record = table.insert_lookup(path, child.node());
          // `readdir` (unlike `readdirplus`) does *not* take a kernel reference,
          // so the lookup taken above is released immediately.
          table.forget(record.ino, 1);
          record.ino
        };
        if reply.add(
          INodeNo(child_ino),
          index + 1,
          child.file_type(),
          OsStr::from_bytes(&name),
        ) {
          break;
        }
        index += 1;
      }
      reply.ok();
    });
  }

  fn readdirplus(
    &self,
    _req: &Request,
    ino: INodeNo,
    fh: FileHandle,
    offset: u64,
    mut reply: fuser::ReplyDirectoryPlus,
  ) {
    let fs = Arc::clone(&self.fs);
    let state = fs.dirs.lock().expect("dir handles").get(&fh.0).cloned();
    let Some(state) = state else {
      return reply.error(Errno::EBADF);
    };
    let Some(self_record) = fs.record(ino.0) else {
      return reply.error(Errno::ESTALE);
    };
    self.spawn(async move {
      let mut state = state.lock().await;
      let self_attr = fs.attr(&self_record);
      let mut index = offset;
      loop {
        // `.` and `..` carry attributes but no kernel reference: the protocol
        // special-cases them, and counting them as lookups would leak a record
        // for every directory the kernel ever reads.
        if index == 0 {
          if reply.add(
            INodeNo(ino.0),
            1,
            ".",
            &fs.config.ttl,
            &self_attr,
            GENERATION,
          ) {
            break;
          }
          index = 1;
          continue;
        }
        if index == 1 {
          if reply.add(
            INodeNo(ino.0),
            2,
            "..",
            &fs.config.ttl,
            &self_attr,
            GENERATION,
          ) {
            break;
          }
          index = 2;
          continue;
        }
        if let Err(e) = fs.fill_directory(&mut state).await {
          fs.bump(|s| s.errors += 1);
          return reply.error(errno_of(&e));
        }
        let Some(child) = state.children.get((index - 2) as usize).cloned() else {
          break;
        };
        let name = child.name();
        let path = state.path.join(&name);
        let record = fs
          .inodes
          .lock()
          .expect("inode table")
          .insert_readdirplus(path, child.node());
        let attr = fs.attr(&record);
        // Passthrough entries take the short TTL for the reason lookup does:
        // the daemon rewrites the shadowed state from behind the mount.
        let ttl = if record.node.is_git() {
          fs.config.git_ttl
        } else {
          fs.config.ttl
        };
        if reply.add(
          INodeNo(record.ino),
          index + 1,
          OsStr::from_bytes(&name),
          &ttl,
          &attr,
          GENERATION,
        ) {
          // The kernel did not take this entry, so the reference taken above
          // must be released or it leaks a record per full directory read.
          fs.inodes.lock().expect("inode table").forget(record.ino, 1);
          break;
        }
        fs.bump(|s| s.lookups += 1);
        index += 1;
      }
      reply.ok();
    });
  }

  fn releasedir(
    &self,
    _req: &Request,
    ino: INodeNo,
    fh: FileHandle,
    _flags: OpenFlags,
    reply: fuser::ReplyEmpty,
  ) {
    self.fs.dirs.lock().expect("dir handles").remove(&fh.0);
    self.fs.inodes.lock().expect("inode table").close(ino.0);
    reply.ok();
  }

  fn access(&self, _req: &Request, ino: INodeNo, mask: AccessFlags, reply: fuser::ReplyEmpty) {
    let Some(record) = self.fs.record(ino.0) else {
      return reply.error(Errno::ESTALE);
    };
    let attr = self.fs.attr(&record);
    if mask.contains(AccessFlags::W_OK) && record.node.is_odb() {
      // `EROFS` for the projection only. The rest of the mount — the overlay
      // view and the real `.git` — is writable, and POSIX distinguishes "you
      // may not" from "nobody may, because this is read-only".
      return reply.error(Errno::EROFS);
    }
    if mask.contains(AccessFlags::X_OK) && attr.perm & 0o111 == 0 {
      return reply.error(Errno::EACCES);
    }
    if mask.contains(AccessFlags::R_OK) && attr.perm & 0o444 == 0 {
      return reply.error(Errno::EACCES);
    }
    reply.ok();
  }

  fn statfs(&self, _req: &Request, _ino: INodeNo, reply: fuser::ReplyStatfs) {
    // The overlay quota, not the host filesystem (DESIGN.md section 8.2, PLAN.md
    // M2.2). Reporting the host's free space would tell a build it has hundreds
    // of gigabytes when its real budget is the per-job quota, and the failure
    // would arrive as a surprise `EDQUOT` in the middle of a link step.
    const BLOCK: u64 = 4096;
    let stats = self.fs.overlay().stats();
    let blocks = stats.quota_bytes / BLOCK;
    let used = stats.local_bytes.div_ceil(BLOCK).min(blocks);
    let free = blocks - used;
    reply.statfs(
      blocks,
      free,
      free,
      // Inode counts are notional: there is no inode table to exhaust, and
      // reporting zero free makes some tools refuse to write before trying.
      1 << 20,
      (1 << 20) - stats.entries.min(1 << 20),
      BLOCK as u32,
      // `f_namemax`, not the path limit: this is what the kernel checks a
      // single component against, and reporting 4096 here let a 300-byte name
      // through into a tree nothing else could check out.
      gfs_types::limits::MAX_NAME_BYTES as u32,
      BLOCK as u32,
    );
  }

  // -------------------------------------------------------------------------
  // Mutations
  // -------------------------------------------------------------------------

  #[allow(clippy::too_many_arguments)]
  fn setattr(
    &self,
    _req: &Request,
    ino: INodeNo,
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    size: Option<u64>,
    // Accepted and discarded, deliberately. The mount is `noatime` (see
    // `session.rs`) and the overlay models one modification time per entry, so
    // there is no atime to set: `getattr` reports mtime for it, which is what a
    // `noatime` mount looks like, and Git could not record an atime an export
    // would have to reproduce.
    //
    // Discarded rather than refused, which is the part worth stating. `cp -p`,
    // `tar -x`, `rsync -a` and `touch` all set atime and mtime in one
    // `utimensat`, so returning an error whenever atime is requested would fail
    // every one of them — a far worse outcome than a timestamp that tracks mtime.
    // pjdfstest's `utimensat/02`, `/04`, `/05`, `/08` and `/09` fail against this
    // by design; `docs/reports/posix-conformance.md` records it as scope.
    atime: Option<fuser::TimeOrNow>,
    mtime: Option<fuser::TimeOrNow>,
    _ctime: Option<std::time::SystemTime>,
    _fh: Option<FileHandle>,
    _crtime: Option<std::time::SystemTime>,
    _chgtime: Option<std::time::SystemTime>,
    _bkuptime: Option<std::time::SystemTime>,
    _flags: Option<fuser::BsdFileFlags>,
    reply: fuser::ReplyAttr,
  ) {
    let fs = Arc::clone(&self.fs);
    self.spawn(async move {
      let Some(record) = fs.record(ino.0) else {
        return reply.error(Errno::ESTALE);
      };
      if record.node.is_odb() {
        // A `utimensat` on the projection is Git *freshening* a pack: before
        // writing an object it already has, `freshen_packed_object()` touches
        // the containing pack so `gc` will not prune it, and reads a failed
        // touch as "cannot vouch for this object" -- so it writes a duplicate
        // loose copy instead. Refusing this cost the first commit in a
        // workspace thousands of redundant objects.
        //
        // Accepted as a no-op rather than refused, because there is nothing here
        // for it to change and nothing for `gc` to prune: the projection's
        // mtimes are synthetic and its packs are the server's. Anything that
        // would really alter the projection -- a mode, a size, an owner -- is
        // still EROFS.
        let times_only = mode.is_none() && size.is_none() && uid.is_none() && gid.is_none();
        if times_only && (atime.is_some() || mtime.is_some()) {
          return reply.attr(&fs.config.ttl, &fs.attr(&record));
        }
        return reply.error(Errno::EROFS);
      }
      // The passthrough setattr: chmod, truncate, and utimens land on the real
      // file, exactly as they would without the mount in between. `tar -x` and
      // `cp -a` over a `.git` are the callers that need the times.
      if record.node.is_git() {
        let Some(rel) = git_rel(&record.path).map(<[u8]>::to_vec) else {
          return reply.error(Errno::ESTALE);
        };
        let git = Arc::clone(&fs.git);
        let requested_mtime = mtime.map(|m| match m {
          fuser::TimeOrNow::SpecificTime(t) => t,
          fuser::TimeOrNow::Now => std::time::SystemTime::now(),
        });
        let outcome = tokio::task::spawn_blocking(move || {
          let real = git.real(&rel);
          if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&real, std::fs::Permissions::from_mode(mode & 0o7777))?;
          }
          if let Some(size) = size {
            std::fs::OpenOptions::new()
              .write(true)
              .open(&real)
              .and_then(|f| f.set_len(size))?;
          }
          if requested_mtime.is_some() {
            crate::passthrough::set_times(&real, None, requested_mtime)?;
          }
          git.stat(&rel)
        })
        .await;
        return match outcome {
          Ok(Ok(meta)) => {
            let refreshed = {
              let mut table = fs.inodes.lock().expect("inode table");
              table.refresh(record.ino, Node::Git(meta));
              table.get(record.ino).cloned()
            };
            match refreshed {
              Some(record) => reply.attr(&fs.config.git_ttl, &fs.attr(&record)),
              None => reply.error(Errno::ESTALE),
            }
          }
          Ok(Err(e)) => reply.error(errno_io(&e)),
          Err(_) => reply.error(Errno::EIO),
        };
      }
      let base = match &record.node {
        Node::Overlay(entry) => entry.base.clone(),
        Node::Base(entry) => base_facts(entry),
        Node::Git(_) | Node::Odb(_) => None,
      };

      if let Some(size) = size {
        // Truncating to zero replaces the whole file, so the old bytes are never
        // fetched. Any other size needs them.
        match fs.copy_up(&record, size == 0).await {
          Ok(_) => {}
          Err(e) => return reply.error(errno_of(&e)),
        }
        let overlay = fs.overlay();
        let path = record.path.clone();
        if let Err(e) = Gfs::blocking(move || overlay.truncate(&path, size)).await {
          return reply.error(errno_of_overlay(&e));
        }
      }

      if let Some(mode) = mode {
        let overlay = fs.overlay();
        let path = record.path.clone();
        let base = base.clone();
        let ino = record.ino;
        let executable = mode & 0o111 != 0;
        if let Err(e) =
          Gfs::blocking(move || overlay.set_executable(&path, base, ino, executable)).await
        {
          return reply.error(errno_of_overlay(&e));
        }
      }

      if let Some(mtime) = mtime {
        let requested = match mtime {
          fuser::TimeOrNow::SpecificTime(t) => Some(Timestamp::from_system_time(t)),
          fuser::TimeOrNow::Now => None,
        };
        let overlay = fs.overlay();
        let path = record.path.clone();
        let base = base.clone();
        let ino = record.ino;
        // `touch .` on the workspace root: the root keeps its times in meta
        // rather than in a row, so it cannot go through `set_times`.
        let outcome = if path.is_empty() {
          Gfs::blocking(move || overlay.touch_root(requested).map(|_| ())).await
        } else {
          Gfs::blocking(move || overlay.set_times(&path, base, ino, requested).map(|_| ())).await
        };
        if let Err(e) = outcome {
          return reply.error(errno_of_overlay(&e));
        }
      }

      // Nothing asked for: `truncate`-less `utimensat`-less `chmod`-less
      // `setattr` still has to answer with the current attributes.
      match fs.overlay().get(&record.path) {
        Some(entry) => {
          let attr = fs.republish(&record.path, entry);
          reply.attr(&fs.config.ttl, &attr);
        }
        None => reply.attr(&fs.config.ttl, &fs.attr(&record)),
      }
    });
  }

  fn mkdir(
    &self,
    _req: &Request,
    parent: INodeNo,
    name: &OsStr,
    _mode: u32,
    _umask: u32,
    reply: fuser::ReplyEntry,
  ) {
    let fs = Arc::clone(&self.fs);
    let name = name.as_bytes().to_vec();
    self.spawn(async move {
      let Some(parent) = fs.record(parent.0) else {
        return reply.error(Errno::ESTALE);
      };
      if parent.node.is_odb() {
        return reply.error(Errno::EROFS);
      }
      if let Err(e) = check_name(&name) {
        return reply.error(e);
      }
      let path = parent.path.join(&name);
      if parent.node.is_git() {
        // The passthrough mkdir: loose-object fan-out directories
        // (`objects/ab/`) are the common caller.
        return match fs
          .git_mutate_entry(&path, |real| std::fs::create_dir(real))
          .await
        {
          Ok(record) => reply.entry(&fs.config.git_ttl, &fs.attr(&record), GENERATION),
          Err(errno) => {
            fs.bump(|s| s.errors += 1);
            reply.error(errno)
          }
        };
      }
      let resolved = match fs.resolve_path(&path).await {
        Ok(resolved) => resolved,
        Err(e) => return reply.error(errno_of(&e)),
      };
      let overlay = fs.overlay();
      let parent_base = Gfs::parent_base(&parent);
      let target = path.clone();
      let ino = fs.number_for(&path);
      match Gfs::blocking(move || overlay.mkdir(&target, resolved.base, parent_base, ino)).await {
        Ok(entry) => {
          let record = fs
            .inodes
            .lock()
            .expect("inode table")
            .insert_lookup(path, Node::Overlay(Box::new(entry)));
          fs.bump(|s| s.lookups += 1);
          fs.touch_parent(&parent).await;
          reply.entry(&fs.config.ttl, &fs.attr(&record), GENERATION);
        }
        Err(e) => {
          fs.bump(|s| s.errors += 1);
          reply.error(errno_of_overlay(&e));
        }
      }
    });
  }

  fn symlink(
    &self,
    _req: &Request,
    parent: INodeNo,
    name: &OsStr,
    link: &std::path::Path,
    reply: fuser::ReplyEntry,
  ) {
    let fs = Arc::clone(&self.fs);
    let name = name.as_bytes().to_vec();
    let target = link.as_os_str().as_bytes().to_vec();
    self.spawn(async move {
      let Some(parent) = fs.record(parent.0) else {
        return reply.error(Errno::ESTALE);
      };
      if parent.node.is_odb() {
        return reply.error(Errno::EROFS);
      }
      if let Err(e) = check_name(&name) {
        return reply.error(e);
      }
      let path = parent.path.join(&name);
      if parent.node.is_git() {
        let target = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&target).to_os_string());
        return match fs
          .git_mutate_entry(&path, move |real| std::os::unix::fs::symlink(&target, real))
          .await
        {
          Ok(record) => reply.entry(&fs.config.git_ttl, &fs.attr(&record), GENERATION),
          Err(errno) => {
            fs.bump(|s| s.errors += 1);
            reply.error(errno)
          }
        };
      }
      let resolved = match fs.resolve_path(&path).await {
        Ok(resolved) => resolved,
        Err(e) => return reply.error(errno_of(&e)),
      };
      let overlay = fs.overlay();
      let parent_base = Gfs::parent_base(&parent);
      let link_path = path.clone();
      let ino = fs.number_for(&path);
      match Gfs::blocking(move || {
        overlay.symlink(&link_path, &target, resolved.base, parent_base, ino)
      })
      .await
      {
        Ok(entry) => {
          let record = fs
            .inodes
            .lock()
            .expect("inode table")
            .insert_lookup(path, Node::Overlay(Box::new(entry)));
          fs.bump(|s| s.lookups += 1);
          fs.touch_parent(&parent).await;
          reply.entry(&fs.config.ttl, &fs.attr(&record), GENERATION);
        }
        Err(e) => {
          fs.bump(|s| s.errors += 1);
          reply.error(errno_of_overlay(&e));
        }
      }
    });
  }

  fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: fuser::ReplyEmpty) {
    let fs = Arc::clone(&self.fs);
    let name = name.as_bytes().to_vec();
    self.spawn(async move {
      match fs.remove_child(parent.0, &name, false).await {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(e),
      }
    });
  }

  fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: fuser::ReplyEmpty) {
    let fs = Arc::clone(&self.fs);
    let name = name.as_bytes().to_vec();
    self.spawn(async move {
      match fs.remove_child(parent.0, &name, true).await {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(e),
      }
    });
  }

  fn rename(
    &self,
    _req: &Request,
    parent: INodeNo,
    name: &OsStr,
    newparent: INodeNo,
    newname: &OsStr,
    flags: fuser::RenameFlags,
    reply: fuser::ReplyEmpty,
  ) {
    let fs = Arc::clone(&self.fs);
    let name = name.as_bytes().to_vec();
    let newname = newname.as_bytes().to_vec();
    self.spawn(async move {
      match fs
        .rename_child(parent.0, &name, newparent.0, &newname, flags)
        .await
      {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(e),
      }
    });
  }

  fn link(
    &self,
    _req: &Request,
    ino: INodeNo,
    newparent: INodeNo,
    newname: &OsStr,
    reply: fuser::ReplyEntry,
  ) {
    // Inside `.git`, hard links are real: `git clone --local` and pack
    // plumbing use them, and the passthrough forwards to a filesystem that
    // has them. Everywhere else `EPERM` stands — not `EROFS` — because
    // DESIGN.md section 8.2 fixes hard links as unsupported in the tree: Git
    // has no hard links to model and the overlay does not model them either.
    let fs = Arc::clone(&self.fs);
    let newname = newname.as_bytes().to_vec();
    self.spawn(async move {
      let (Some(source), Some(parent)) = (fs.record(ino.0), fs.record(newparent.0)) else {
        return reply.error(Errno::ESTALE);
      };
      if !(source.node.is_git() && parent.node.is_git()) {
        return reply.error(Errno::EPERM);
      }
      if let Err(e) = check_name(&newname) {
        return reply.error(e);
      }
      let Some(source_rel) = git_rel(&source.path).map(<[u8]>::to_vec) else {
        return reply.error(Errno::EPERM);
      };
      if odb_rel(&source_rel).is_some() {
        return reply.error(Errno::EROFS);
      }
      let path = parent.path.join(&newname);
      let git = Arc::clone(&fs.git);
      match fs
        .git_mutate_entry(&path, move |real| {
          std::fs::hard_link(git.real(&source_rel), real)
        })
        .await
      {
        Ok(record) => reply.entry(&fs.config.git_ttl, &fs.attr(&record), GENERATION),
        Err(errno) => {
          fs.bump(|s| s.errors += 1);
          reply.error(errno)
        }
      }
    });
  }

  fn mknod(
    &self,
    _req: &Request,
    _parent: INodeNo,
    _name: &OsStr,
    mode: u32,
    _umask: u32,
    _rdev: u32,
    reply: fuser::ReplyEntry,
  ) {
    // Device nodes, FIFOs, and sockets are documented unsupported (DESIGN.md
    // section 8.2): none of them can be exported as a Git tree entry. A plain
    // file through `mknod` rather than `create` is rare but legal, and it is the
    // one form that has a representation, so it is refused with `EPERM` rather
    // than silently created as something else.
    let _ = mode;
    reply.error(Errno::EPERM);
  }

  fn fallocate(
    &self,
    _req: &Request,
    ino: INodeNo,
    _fh: FileHandle,
    offset: u64,
    length: u64,
    mode: i32,
    reply: fuser::ReplyEmpty,
  ) {
    // Only plain allocation. `FALLOC_FL_PUNCH_HOLE` and friends would need sparse
    // overlay files, which have no Git representation, so they are refused rather
    // than silently written as zeroes.
    if mode != 0 {
      return reply.error(Errno::EOPNOTSUPP);
    }
    let fs = Arc::clone(&self.fs);
    self.spawn(async move {
      let Some(record) = fs.record(ino.0) else {
        return reply.error(Errno::ESTALE);
      };
      if record.node.is_odb() {
        return reply.error(Errno::EROFS);
      }
      if record.node.is_git() {
        // Plain allocation on a real file: extend it, matching the overlay's
        // semantics below. Git itself never calls this; `pjdfstest` does.
        let wanted = offset.saturating_add(length);
        return match fs
          .git_mutate(&record.path, move |real| {
            let file = std::fs::OpenOptions::new().write(true).open(real)?;
            if file.metadata()?.len() < wanted {
              file.set_len(wanted)?;
            }
            Ok(())
          })
          .await
        {
          Ok(()) => reply.ok(),
          Err(errno) => reply.error(errno),
        };
      }
      if let Err(e) = fs.copy_up(&record, false).await {
        return reply.error(errno_of(&e));
      }
      let wanted = offset.saturating_add(length);
      let current = fs.overlay().get(&record.path).map(|e| e.size).unwrap_or(0);
      if wanted <= current {
        return reply.ok();
      }
      let overlay = fs.overlay();
      let path = record.path.clone();
      match Gfs::blocking(move || overlay.truncate(&path, wanted)).await {
        Ok(entry) => {
          fs.republish(&record.path, entry);
          reply.ok();
        }
        Err(e) => reply.error(errno_of_overlay(&e)),
      }
    });
  }

  fn setxattr(
    &self,
    _req: &Request,
    _ino: INodeNo,
    _name: &OsStr,
    _value: &[u8],
    _flags: i32,
    _position: u32,
    reply: fuser::ReplyEmpty,
  ) {
    reply.error(Errno::ENOTSUP);
  }

  fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: fuser::ReplyEmpty) {
    reply.error(Errno::ENOTSUP);
  }

  fn getxattr(
    &self,
    _req: &Request,
    _ino: INodeNo,
    _name: &OsStr,
    _size: u32,
    reply: fuser::ReplyXattr,
  ) {
    // Documented unsupported in the MVP (DESIGN.md section 8.2). `ENOTSUP` rather
    // than `ENOSYS` so tools that probe for xattrs -- `cp -a`, `tar`, `rsync` --
    // treat it as "this filesystem has none" and continue.
    reply.error(Errno::ENOTSUP);
  }

  fn listxattr(&self, _req: &Request, _ino: INodeNo, _size: u32, reply: fuser::ReplyXattr) {
    reply.error(Errno::ENOTSUP);
  }
}

impl Gfs {
  /// Run a passthrough mutation that creates `path`, stat what it made, and
  /// publish the record for the reply.
  ///
  /// The projection subtree is refused before the disk is touched: `.git/gfs/
  /// objects` is not backed by the real directory, and a mutation that landed
  /// there on disk would create the merged namespace ADR 0011 forbids.
  async fn git_mutate_entry<F>(&self, path: &BytePath, op: F) -> Result<Record, Errno>
  where
    F: FnOnce(&std::path::Path) -> std::io::Result<()> + Send + 'static,
  {
    let Some(rel) = git_rel(path).map(<[u8]>::to_vec) else {
      return Err(Errno::EROFS);
    };
    if odb_rel(&rel).is_some() {
      return Err(Errno::EROFS);
    }
    let git = Arc::clone(&self.git);
    let meta = tokio::task::spawn_blocking(move || {
      op(&git.real(&rel))?;
      git.stat(&rel)
    })
    .await
    .map_err(|_| Errno::EIO)?
    .map_err(|e| errno_io(&e))?;
    let record = self
      .inodes
      .lock()
      .expect("inode table")
      .insert_lookup(path.clone(), Node::Git(meta));
    self.bump(|s| s.lookups += 1);
    Ok(record)
  }

  /// A passthrough removal or rename: the operation itself, no record to
  /// publish.
  async fn git_mutate<F>(&self, path: &BytePath, op: F) -> Result<(), Errno>
  where
    F: FnOnce(&std::path::Path) -> std::io::Result<()> + Send + 'static,
  {
    let Some(rel) = git_rel(path).map(<[u8]>::to_vec) else {
      return Err(Errno::EROFS);
    };
    if odb_rel(&rel).is_some() {
      return Err(Errno::EROFS);
    }
    let git = Arc::clone(&self.git);
    tokio::task::spawn_blocking(move || op(&git.real(&rel)))
      .await
      .map_err(|_| Errno::EIO)?
      .map_err(|e| errno_io(&e))
  }

  /// `unlink` and `rmdir`, which differ only in what they expect to find.
  async fn remove_child(&self, parent: u64, name: &[u8], expect_dir: bool) -> Result<(), Errno> {
    let parent = self.record(parent).ok_or(Errno::ESTALE)?;
    if parent.node.is_odb() {
      return Err(Errno::EROFS);
    }
    let path = parent.path.join(name);
    if parent.node.is_git() {
      return self
        .git_mutate(&path, move |real| {
          if expect_dir {
            std::fs::remove_dir(real)
          } else {
            std::fs::remove_file(real)
          }
        })
        .await;
    }
    let resolved = self.resolve_path(&path).await.map_err(|e| errno_of(&e))?;
    if resolved.node.is_none() {
      return Err(Errno::ENOENT);
    }
    let empty = if expect_dir {
      self
        .merged_dir_is_empty(&path)
        .await
        .map_err(|e| errno_of(&e))?
    } else {
      true
    };
    let overlay = self.overlay();
    let target = path.clone();
    Self::blocking(move || overlay.remove(&target, resolved.base, expect_dir, empty))
      .await
      .map_err(|e| errno_of_overlay(&e))?;
    self.touch_parent(&parent).await;
    Ok(())
  }

  /// Advance a directory's mtime and ctime after an entry appeared in it or left.
  ///
  /// POSIX requires it and pjdfstest checks it (`rmdir/00`, `symlink/00`).
  /// Without it a directory reported the pinned commit's snapshot time forever,
  /// and a build system or watcher keyed on directory mtime — the ordinary way to
  /// notice that something appeared in a directory — saw nothing change.
  ///
  /// Best-effort on purpose. The mutation itself has already committed and
  /// succeeded; failing the syscall because its parent's timestamp could not be
  /// recorded would turn a cosmetic loss into a broken write. A crash between the
  /// two commits leaves the child present with a stale parent time, which is the
  /// same thing every filesystem that does not journal the two together does.
  ///
  /// The mount root goes through [`gfs_overlay::Overlay::touch_root`] rather
  /// than through a row: it has no base facts to adopt from, and giving the
  /// empty path an overlay row would create a second spelling of the root for
  /// every resolver that walks ancestors. Its times live in the journal's meta
  /// table and [`Gfs::attr`] applies them.
  async fn touch_parent(&self, parent: &Record) {
    if parent.node.is_git() || parent.node.is_odb() {
      // A real directory's mtime advances by itself; the projection has none.
      return;
    }
    if parent.path.is_empty() {
      let overlay = self.overlay();
      if let Err(e) = Self::blocking(move || overlay.touch_root(None)).await {
        tracing::debug!(error = %e, "could not advance the mount root's timestamps");
      }
      return;
    }
    let overlay = self.overlay();
    let path = parent.path.clone();
    let base = Gfs::parent_base(parent);
    let ino = parent.ino;
    match Self::blocking(move || overlay.touch_directory(&path, base, ino)).await {
      // Republished, not merely committed. `getattr` is answered inline from the
      // inode table and never consults the overlay, so a row the table does not
      // know about is a timestamp nothing will ever report -- which is what made
      // the first attempt at this look like it had done nothing at all.
      Ok(entry) => {
        self.republish(&parent.path, entry);
      }
      Err(e) => tracing::debug!(
        path = %parent.path.escaped(),
        error = %e,
        "could not advance the parent directory's timestamps"
      ),
    }
  }

  async fn rename_child(
    &self,
    parent: u64,
    name: &[u8],
    newparent: u64,
    newname: &[u8],
    flags: fuser::RenameFlags,
  ) -> Result<(), Errno> {
    // `RENAME_EXCHANGE` would need two paths to swap atomically, which the
    // journal can express but nothing in the pilot's tooling uses. `EINVAL` is
    // what a filesystem returns for a flag it does not implement, and it is what
    // `renameat2` callers already handle.
    if flags.contains(fuser::RenameFlags::RENAME_EXCHANGE)
      || flags.contains(fuser::RenameFlags::RENAME_WHITEOUT)
    {
      return Err(Errno::EINVAL);
    }
    let no_replace = flags.contains(fuser::RenameFlags::RENAME_NOREPLACE);

    let from_parent = self.record(parent).ok_or(Errno::ESTALE)?;
    let to_parent = self.record(newparent).ok_or(Errno::ESTALE)?;
    if from_parent.node.is_odb() || to_parent.node.is_odb() {
      return Err(Errno::EROFS);
    }
    // A rename never crosses the passthrough boundary: `.git` and the tree
    // are different worlds with different backing, and `EXDEV` is what makes
    // `mv` fall back to copy+delete — the only correct translation.
    if from_parent.node.is_git() != to_parent.node.is_git() {
      return Err(Errno::EXDEV);
    }
    check_name(newname)?;
    let from = from_parent.path.join(name);
    let to = to_parent.path.join(newname);

    if from_parent.node.is_git() {
      let (Some(from_rel), Some(to_rel)) = (git_rel(&from), git_rel(&to)) else {
        return Err(Errno::EXDEV);
      };
      if odb_rel(from_rel).is_some() || odb_rel(to_rel).is_some() {
        return Err(Errno::EROFS);
      }
      let (from_rel, to_rel) = (from_rel.to_vec(), to_rel.to_vec());
      let git = Arc::clone(&self.git);
      tokio::task::spawn_blocking(move || {
        let target = git.real(&to_rel);
        // Emulated rather than `renameat2`: Git's own atomicity protocol is
        // `O_EXCL` create plus plain rename, so the small window here is not
        // one Git stands in.
        if no_replace && std::fs::symlink_metadata(&target).is_ok() {
          return Err(std::io::Error::from_raw_os_error(libc::EEXIST));
        }
        std::fs::rename(git.real(&from_rel), target)
      })
      .await
      .map_err(|_| Errno::EIO)?
      .map_err(|e| errno_io(&e))?;
      // The kernel relinks the dentry it has; the table must follow, exactly
      // as in the overlay case below. Git records carry no per-path state
      // beyond the meta snapshot, so no refresh pass is needed.
      let _ = self
        .inodes
        .lock()
        .expect("inode table")
        .rename_subtree(&from, &to);
      return Ok(());
    }

    let source = self.resolve_path(&from).await.map_err(|e| errno_of(&e))?;
    let Some(source_node) = &source.node else {
      return Err(Errno::ENOENT);
    };
    let target = self.resolve_path(&to).await.map_err(|e| errno_of(&e))?;

    let source_is_dir = match source_node {
      Node::Base(entry) => entry.kind.is_dir_like(),
      Node::Overlay(entry) => entry.kind.is_dir(),
      Node::Git(_) | Node::Odb(_) => return Err(Errno::EXDEV),
    };
    let descendants = if source_is_dir {
      self
        .base_descendants(&from)
        .await
        .map_err(|e| errno_of(&e))?
    } else {
      Vec::new()
    };
    // Only a directory can be "not empty", and asking the server to list a file
    // is an invalid request that surfaces as a confusing `EINVAL` on the rename.
    let target_is_dir = match &target.node {
      Some(Node::Base(entry)) => entry.kind.is_dir_like(),
      Some(Node::Overlay(entry)) => entry.kind.is_dir(),
      Some(Node::Git(meta)) => meta.is_dir(),
      Some(Node::Odb(node)) => node.is_dir(),
      None => false,
    };
    let to_empty = if target_is_dir {
      self
        .merged_dir_is_empty(&to)
        .await
        .map_err(|e| errno_of(&e))?
    } else {
      true
    };

    let overlay = self.overlay();
    let to_parent_base = Gfs::parent_base(&to_parent);
    let (from_path, to_path) = (from.clone(), to.clone());
    let from_base = source.base.clone();
    let to_base = target.base.clone();
    let from_ino = self.number_for(&from);
    Self::blocking(move || {
      overlay.rename(
        &from_path,
        from_ino,
        from_base,
        &to_path,
        to_base,
        to_parent_base,
        &descendants,
        to_empty,
        no_replace,
      )
    })
    .await
    .map_err(|e| errno_of_overlay(&e))?;

    // The kernel relinks the dentry it already has rather than looking the
    // destination up again, so every inode number under `from` is now the
    // kernel's number for the corresponding path under `to`. A table that did not
    // follow would answer the next request for the destination out of the
    // source's record -- which is exactly what a moved directory listing empty
    // looks like.
    let moved = self
      .inodes
      .lock()
      .expect("inode table")
      .rename_subtree(&from, &to);
    for (ino, path) in moved {
      if let Some(entry) = self.pinned().overlay.get(&path) {
        self
          .inodes
          .lock()
          .expect("inode table")
          .refresh(ino, Node::Overlay(Box::new(entry)));
      }
    }
    // Both ends: the entry left one directory and arrived in another. When the
    // rename is within a single directory they are the same record and the
    // second touch is a no-op update of the row the first one wrote.
    self.touch_parent(&from_parent).await;
    if to_parent.path != from_parent.path {
      self.touch_parent(&to_parent).await;
    }
    Ok(())
  }
}

/// The snapshot root as a tree entry.
///
/// Synthesized from the commit's tree OID rather than fetched. `CreateMount`
/// already returned it, and a round trip to be told that the root of a tree is a
/// directory would be a network call whose answer is known before it is made.
pub fn root_entry(tree: ObjectId) -> TreeEntryInfo {
  TreeEntryInfo {
    path: BytePath::root(),
    kind: EntryKind::Directory,
    mode: gfs_types::mode::DIRECTORY,
    oid: tree,
    size: 0,
    symlink_target: None,
    blob_ticket: None,
  }
}

/// The attribute block for a cached negative lookup. Only the inode number is
/// read by the kernel, and zero is what marks the entry negative.
fn negative_attr() -> FileAttr {
  FileAttr {
    ino: INodeNo(0),
    size: 0,
    blocks: 0,
    atime: std::time::UNIX_EPOCH,
    mtime: std::time::UNIX_EPOCH,
    ctime: std::time::UNIX_EPOCH,
    crtime: std::time::UNIX_EPOCH,
    kind: fuser::FileType::RegularFile,
    perm: 0,
    nlink: 0,
    uid: 0,
    gid: 0,
    rdev: 0,
    blksize: 4096,
    flags: 0,
  }
}

fn file_type(kind: EntryKind) -> fuser::FileType {
  match kind {
    EntryKind::Directory | EntryKind::Gitlink => fuser::FileType::Directory,
    EntryKind::Symlink => fuser::FileType::Symlink,
    _ => fuser::FileType::RegularFile,
  }
}

fn read_all(file: &std::fs::File, size: u64) -> Result<Vec<u8>, GfsError> {
  let mut buffer = vec![0u8; size as usize];
  let read = file.read_at(&mut buffer, 0).map_err(|e| {
    GfsError::new(
      ErrorCode::Unavailable,
      format!("reading a blob: {}", e.kind()),
    )
  })?;
  buffer.truncate(read);
  Ok(buffer)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_negative_entry_is_signalled_by_inode_zero() {
    // The whole mechanism: a nonzero TTL with inode zero is what tells the kernel
    // to remember the absence instead of asking again.
    assert_eq!(negative_attr().ino, INodeNo(0));
    assert!(FsConfig::default().negative_ttl > Duration::ZERO);
  }

  #[test]
  fn a_gitlink_reports_as_a_directory() {
    assert_eq!(file_type(EntryKind::Gitlink), fuser::FileType::Directory);
    assert_eq!(
      file_type(EntryKind::Unsupported(0o120_755)),
      fuser::FileType::RegularFile
    );
  }

  #[test]
  fn an_unmodelled_git_mode_yields_no_base_facts() {
    // A mutation against a gitlink or an unknown mode must be refused rather
    // than recorded against facts the overlay cannot represent.
    let entry = |kind| TreeEntryInfo {
      path: BytePath::new(b"x".to_vec()),
      kind,
      mode: 0,
      oid: ObjectId::from_raw(gfs_types::HashAlgorithm::Sha1, &[1; 20]).unwrap(),
      size: 0,
      symlink_target: None,
      blob_ticket: None,
    };
    assert!(base_facts(&entry(EntryKind::Regular)).is_some());
    assert!(base_facts(&entry(EntryKind::Gitlink)).is_none());
    assert!(base_facts(&entry(EntryKind::Unsupported(0o120_755))).is_none());
  }
}
