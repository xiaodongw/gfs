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
//! runs on a blocking pool through [`Xvfs::blocking`], which keeps the same rule
//! for the same reason.
//!
//! # Three worlds, resolved in one order
//!
//! `lookup` consults, in order: the synthesized `.git` surface, the overlay, and
//! the pinned base. The order is not arbitrary — the overlay's answer *replaces*
//! the base's, including the answer "this path is gone" — and it is implemented
//! once, in [`Xvfs::resolve_path`], so `readdir` and every mutation agree with
//! `lookup` about what exists.
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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fuser::{
  AccessFlags, Errno, FileAttr, FileHandle, Filesystem, Generation, INodeNo, OpenAccMode,
  OpenFlags, Request,
};
use xvfs_overlay::{
  BaseDescendant, BaseFacts, Overlay, OverlayEntry, OverlayError, OverlayKind, Resolution, Source,
};
use xvfs_types::error::{ErrorCode, XvfsError};
use xvfs_types::{BytePath, EntryKind, ObjectId, Timestamp, TreeEntryInfo};

use crate::attr::{attr_of, errno_of, errno_of_overlay, Ownership};
use crate::cache::{BlobCache, CacheStats};
use crate::client::SnapshotClient;
use crate::gitdir::{GitDir, SynthNode, GIT_DIR};
use crate::inode::{InodeTable, Node, Record, ROOT_INO};

/// Inode numbers are never reused for a different path, so there is nothing for a
/// generation to disambiguate. See the module docs on [`crate::inode`].
const GENERATION: Generation = Generation(0);

#[derive(Clone, Debug)]
pub struct FsConfig {
  /// How long the kernel may cache attributes and directory entries.
  pub ttl: Duration,
  /// How long the kernel may cache the absence of a name.
  pub negative_ttl: Duration,
  pub directory_page_size: u32,
  /// Attempts for a retryable failure, including the first.
  pub attempts: u32,
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
      directory_page_size: xvfs_types::limits::DEFAULT_DIRECTORY_PAGE_SIZE as u32,
      attempts: 3,
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
  pub opens: u64,
  pub reads: u64,
  pub read_bytes: u64,
  pub writes: u64,
  pub written_bytes: u64,
  pub copy_ups: u64,
  pub copy_up_bytes: u64,
  pub errors: u64,
}

/// A resolved child in a directory listing.
#[derive(Clone, Debug)]
enum Child {
  Base(TreeEntryInfo),
  Overlay(Box<OverlayEntry>),
  Synth { name: Vec<u8>, node: SynthNode },
}

impl Child {
  fn name(&self) -> Vec<u8> {
    match self {
      Child::Base(entry) => entry.path.file_name().unwrap_or_default().to_vec(),
      Child::Overlay(entry) => entry.name(),
      Child::Synth { name, .. } => name.clone(),
    }
  }

  fn node(&self) -> Node {
    match self {
      Child::Base(entry) => Node::Base(entry.clone()),
      Child::Overlay(entry) => Node::Overlay(entry.clone()),
      Child::Synth { node, .. } => Node::Synth(node.clone()),
    }
  }

  fn file_type(&self) -> fuser::FileType {
    match self {
      Child::Base(entry) => file_type(entry.kind),
      Child::Overlay(entry) => file_type(entry.kind.to_entry_kind()),
      Child::Synth { node, .. } => {
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
  next_page_token: Vec<u8>,
  /// Whether the base listing has been exhausted.
  base_done: bool,
  /// Whether the overlay's extra children have been appended. Always after the
  /// base listing, so a child's offset never moves.
  complete: bool,
  /// Names the base listing produced, which is what decides whether an overlay
  /// child still has to be appended.
  base_names: HashSet<Vec<u8>>,
}

#[derive(Debug)]
enum FileState {
  /// A cached base blob, opened once and read with `pread`.
  Blob {
    oid: ObjectId,
    file: Arc<std::fs::File>,
  },
  /// Overlay content. Held open, which is what keeps an unlinked file readable
  /// through a descriptor that outlives its name — the overlay removes the
  /// content file, and the kernel keeps the inode alive until this closes.
  Local {
    content_id: u64,
    file: Arc<std::fs::File>,
    writable: bool,
  },
  Synth(Arc<Vec<u8>>),
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

/// The filesystem. Shared behind an `Arc` so a callback can hand it to a worker.
pub struct Xvfs {
  client: Arc<SnapshotClient>,
  cache: Arc<BlobCache>,
  gitdir: Arc<GitDir>,
  overlay: Arc<Overlay>,
  config: FsConfig,
  owner: Ownership,
  snapshot_time: Timestamp,
  inodes: Mutex<InodeTable>,
  dirs: Mutex<HashMap<u64, Arc<tokio::sync::Mutex<DirState>>>>,
  files: Mutex<HashMap<u64, Arc<FileState>>>,
  next_handle: AtomicU64,
  stats: Mutex<FsStats>,
}

impl std::fmt::Debug for Xvfs {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Xvfs")
      .field("config", &self.config)
      .field("snapshot_time", &self.snapshot_time)
      .finish_non_exhaustive()
  }
}

impl Xvfs {
  pub fn new(
    client: Arc<SnapshotClient>,
    cache: Arc<BlobCache>,
    gitdir: Arc<GitDir>,
    overlay: Arc<Overlay>,
    root: TreeEntryInfo,
    config: FsConfig,
  ) -> Arc<Self> {
    let snapshot_time = client.binding().snapshot_time;
    let mut inodes = InodeTable::new(root);
    // The numbers a previous process handed out, before any new one is issued.
    inodes.seed(&overlay.entries());
    Arc::new(Xvfs {
      client,
      cache,
      gitdir,
      overlay,
      config,
      owner: Ownership::current(),
      snapshot_time,
      inodes: Mutex::new(inodes),
      dirs: Mutex::new(HashMap::new()),
      files: Mutex::new(HashMap::new()),
      next_handle: AtomicU64::new(1),
      stats: Mutex::new(FsStats::default()),
    })
  }

  pub fn stats(&self) -> FsStats {
    *self.stats.lock().expect("fs stats")
  }

  /// The stable sanitized time every base entry reports.
  pub fn snapshot_time(&self) -> Timestamp {
    self.snapshot_time
  }

  pub fn cache_stats(&self) -> CacheStats {
    self.cache.stats()
  }

  pub fn overlay(&self) -> &Arc<Overlay> {
    &self.overlay
  }

  /// Live inodes and distinct paths ever numbered. Reported by `xvfs inspect`.
  pub fn inode_counts(&self) -> (usize, usize) {
    let table = self.inodes.lock().expect("inode table");
    (table.live(), table.assigned())
  }

  /// Open file and directory handles.
  ///
  /// `xvfs refresh` reads this: PLAN.md M2.1 requires the old mount generation
  /// and its lease to survive until every handle opened through it has closed,
  /// so that no reader ever observes a mixture of two generations.
  pub fn open_handles(&self) -> usize {
    self.files.lock().expect("file handles").len() + self.dirs.lock().expect("dir handles").len()
  }

  fn bump(&self, f: impl FnOnce(&mut FsStats)) {
    f(&mut self.stats.lock().expect("fs stats"));
  }

  fn attr(&self, record: &Record) -> FileAttr {
    attr_of(record, self.snapshot_time, self.owner)
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
  async fn retrying<T, F, Fut>(&self, mut operation: F) -> Result<T, XvfsError>
  where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, XvfsError>>,
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
  async fn base_entry(
    &self,
    path: &BytePath,
    want_ticket: bool,
  ) -> Result<Option<TreeEntryInfo>, XvfsError> {
    path.validate()?;
    self.bump(|s| s.metadata_requests += 1);
    self
      .retrying(|| self.client.get_entry(path, want_ticket))
      .await
  }

  /// Resolve a path across all three worlds. The single place the order lives.
  async fn resolve_path(&self, path: &BytePath) -> Result<Resolved, XvfsError> {
    match self.overlay.resolve(path) {
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
        base: self.overlay.get(path).and_then(|entry| entry.base),
      }),
      Resolution::Base => {
        let entry = self.base_entry(path, false).await?;
        let base = entry.as_ref().and_then(base_facts);
        Ok(Resolved {
          node: entry.map(Node::Base),
          base,
        })
      }
    }
  }

  /// Resolve one name under a parent.
  async fn lookup_child(&self, parent: &Record, name: &[u8]) -> Result<Option<Node>, XvfsError> {
    // The synthesized surface first, so a repository that somehow contained a
    // `.git` entry could not shadow it. Git refuses to record one, but the
    // ordering costs nothing and removes the question.
    if parent.ino == ROOT_INO && name == GIT_DIR {
      return Ok(
        self
          .gitdir
          .get(&BytePath::new(GIT_DIR.to_vec()))
          .map(Node::Synth),
      );
    }
    if parent.node.is_synth() {
      let path = parent.path.join(name);
      return Ok(self.gitdir.get(&path).map(Node::Synth));
    }
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
      Node::Overlay(_) | Node::Synth(_) => None,
    }
  }

  /// Every name the base has in a directory, paged to the end.
  async fn base_child_names(&self, dir: &BytePath) -> Result<Vec<Vec<u8>>, XvfsError> {
    if self.overlay.masks_base(dir) {
      return Ok(Vec::new());
    }
    let mut names = Vec::new();
    let mut token = Vec::new();
    loop {
      let page = self
        .retrying(|| {
          self
            .client
            .list_directory(dir, token.clone(), self.config.directory_page_size, false)
        })
        .await?;
      self.bump(|s| s.directory_pages += 1);
      names.extend(
        page
          .entries
          .iter()
          .map(|entry| entry.path.file_name().unwrap_or_default().to_vec()),
      );
      if page.next_page_token.is_empty() {
        return Ok(names);
      }
      token = page.next_page_token;
    }
  }

  /// Whether a directory is empty in the merged view.
  async fn merged_dir_is_empty(&self, dir: &BytePath) -> Result<bool, XvfsError> {
    let names = self.base_child_names(dir).await?;
    Ok(self.overlay.merged_dir_is_empty(dir, &names))
  }

  /// Every base descendant of a directory, for a rename that has to materialize
  /// the subtree as metadata.
  ///
  /// Bounded by the overlay's own rename limit so that a `mv` of a monorepo root
  /// fails before it has fetched a million directory pages rather than after.
  async fn base_descendants(&self, dir: &BytePath) -> Result<Vec<BaseDescendant>, XvfsError> {
    let mut out = Vec::new();
    if self.overlay.masks_base(dir) {
      return Ok(out);
    }
    let limit = self.overlay.config().max_rename_entries;
    let mut queue = vec![BytePath::root()];
    while let Some(relative) = queue.pop() {
      let absolute = if relative.is_empty() {
        dir.clone()
      } else {
        BytePath::new(join(dir.as_bytes(), relative.as_bytes()))
      };
      let mut token = Vec::new();
      loop {
        let page = self
          .retrying(|| {
            self.client.list_directory(
              &absolute,
              token.clone(),
              self.config.directory_page_size,
              false,
            )
          })
          .await?;
        self.bump(|s| s.directory_pages += 1);
        for entry in page.entries {
          let name = entry.path.file_name().unwrap_or_default().to_vec();
          let child = BytePath::new(if relative.is_empty() {
            name
          } else {
            join(relative.as_bytes(), &name)
          });
          if entry.kind == EntryKind::Directory {
            queue.push(child.clone());
          }
          let Some(facts) = base_facts(&entry) else {
            continue;
          };
          out.push(BaseDescendant {
            relative: child,
            facts,
            symlink_target: entry.symlink_target.clone(),
          });
          if out.len() > limit {
            return Err(XvfsError::new(
              ErrorCode::ResourceLimit,
              format!(
                "renaming {} would move more than {limit} entries",
                dir.escaped()
              ),
            ));
          }
        }
        if page.next_page_token.is_empty() {
          break;
        }
        token = page.next_page_token;
      }
    }
    Ok(out)
  }

  /// Fetch and merge directory pages until `wanted` children are known.
  async fn fill_directory(&self, state: &mut DirState, wanted: usize) -> Result<(), XvfsError> {
    while !state.complete && state.children.len() < wanted {
      if state.base_done {
        // The base listing is exhausted, so anything the overlay holds that the
        // base never named is appended now. Always last, so a child's offset
        // cannot move between two `readdir` calls on one handle.
        for entry in self.overlay.extra_children(&state.path, &state.base_names) {
          state.children.push(Child::Overlay(Box::new(entry)));
        }
        state.complete = true;
        break;
      }
      let token = std::mem::take(&mut state.next_page_token);
      let page = self
        .retrying(|| {
          self.client.list_directory(
            &state.path,
            token.clone(),
            self.config.directory_page_size,
            false,
          )
        })
        .await?;
      self.bump(|s| s.directory_pages += 1);
      for entry in page.entries {
        let name = entry.path.file_name().unwrap_or_default().to_vec();
        state.base_names.insert(name.clone());
        match self.overlay.resolve(&state.path.join(&name)) {
          Resolution::Absent => {}
          Resolution::Overlay(row) => state.children.push(Child::Overlay(row)),
          Resolution::Base => state.children.push(Child::Base(entry)),
        }
      }
      if page.next_page_token.is_empty() {
        state.base_done = true;
      } else {
        state.next_page_token = page.next_page_token;
      }
    }
    Ok(())
  }

  /// Open a base blob for reading, fetching and verifying it if the cache lacks
  /// it.
  ///
  /// The ticket is minted here rather than at lookup time. A blob ticket is
  /// short-lived authorization state (the server issues it for five minutes), so
  /// one attached to every metadata lookup would usually expire unused; and when
  /// the blob is already cached, no ticket -- and no round trip -- is needed at
  /// all, which is what makes a warm reopen free.
  async fn open_blob(&self, path: &BytePath, oid: &ObjectId) -> Result<std::fs::File, XvfsError> {
    if !self.cache.contains(oid) {
      let fresh = self
        .base_entry(path, true)
        .await?
        .ok_or_else(|| XvfsError::not_found("the path vanished from a pinned commit"))?;
      if fresh.oid != *oid {
        // Impossible against an immutable commit, and therefore worth refusing
        // loudly rather than serving: it means the server answered about a
        // different snapshot than the one this mount is pinned to.
        return Err(XvfsError::internal(
          "the server returned a different blob for a pinned path",
        ));
      }
      let ticket = fresh.blob_ticket.unwrap_or_default();
      let (cached, _) = self.cache.open_blob(&self.client, oid, &ticket).await?;
      return open_cached(&cached);
    }
    let (cached, _) = self.cache.open_blob(&self.client, oid, "").await?;
    open_cached(&cached)
  }

  /// Give a path local content, fetching the base blob only when the caller will
  /// actually read it.
  ///
  /// `truncating` is the `O_TRUNC` case PLAN.md M3.2 calls out: a caller
  /// replacing a whole file must not pay to download the version it is throwing
  /// away.
  async fn copy_up(&self, record: &Record, truncating: bool) -> Result<OverlayEntry, XvfsError> {
    let path = record.path.clone();
    let overlay = Arc::clone(&self.overlay);
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
      Node::Synth(_) => return Err(XvfsError::invalid("the .git surface is read-only")),
    };

    if truncating {
      let target = path.clone();
      return Self::blocking(move || overlay.materialize(&target, base, ino, Source::Empty))
        .await
        .map_err(overlay_as_service_error);
    }

    let Some(oid) = source_oid else {
      return Err(XvfsError::internal("nothing to copy up from"));
    };
    let blob = self.open_blob(&blob_path(record), &oid).await?;
    let size = blob.metadata().map(|m| m.len()).unwrap_or(0);
    self.bump(|s| {
      s.copy_ups += 1;
      s.copy_up_bytes += size;
    });
    let target = path.clone();
    Self::blocking(move || {
      let mut reader = std::io::BufReader::new(blob);
      overlay.materialize(&target, base, ino, Source::Reader(&mut reader))
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

fn join(prefix: &[u8], suffix: &[u8]) -> Vec<u8> {
  let mut out = prefix.to_vec();
  if !out.is_empty() {
    out.push(b'/');
  }
  out.extend_from_slice(suffix);
  out
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
pub fn overlay_as_service_error(e: OverlayError) -> XvfsError {
  XvfsError::new(
    match e.condition {
      xvfs_overlay::Condition::QuotaExceeded => ErrorCode::ResourceLimit,
      xvfs_overlay::Condition::NoEntry => ErrorCode::NotFound,
      xvfs_overlay::Condition::Io => ErrorCode::Internal,
      _ => ErrorCode::InvalidArgument,
    },
    e.message,
  )
}

fn open_cached(path: &std::path::Path) -> Result<std::fs::File, XvfsError> {
  std::fs::File::open(path).map_err(|e| {
    XvfsError::new(
      ErrorCode::Unavailable,
      format!("opening a cached blob: {}", e.kind()),
    )
  })
}

/// The `fuser` adapter: an `Arc<Xvfs>` plus the runtime its callbacks dispatch to.
pub struct XvfsFilesystem {
  fs: Arc<Xvfs>,
  runtime: tokio::runtime::Handle,
}

impl std::fmt::Debug for XvfsFilesystem {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("XvfsFilesystem").finish_non_exhaustive()
  }
}

impl XvfsFilesystem {
  pub fn new(fs: Arc<Xvfs>, runtime: tokio::runtime::Handle) -> Self {
    XvfsFilesystem { fs, runtime }
  }

  /// Run a fallible operation on the runtime and reply from there.
  ///
  /// The one place the ADR 0003 rule is implemented, so there is exactly one
  /// place to check that no callback blocks.
  fn spawn<F>(&self, future: F)
  where
    F: std::future::Future<Output = ()> + Send + 'static,
  {
    self.runtime.spawn(future);
  }
}

impl Filesystem for XvfsFilesystem {
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
    Ok(())
  }

  fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: fuser::ReplyEntry) {
    let fs = Arc::clone(&self.fs);
    let name = name.as_bytes().to_vec();
    self.spawn(async move {
      let Some(parent) = fs.record(parent.0) else {
        return reply.error(Errno::ESTALE);
      };
      match fs.lookup_child(&parent, &name).await {
        Ok(Some(node)) => {
          let path = match &node {
            Node::Base(entry) => entry.path.clone(),
            Node::Overlay(entry) => entry.path.clone(),
            Node::Synth(_) => parent.path.join(&name),
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
    // Answered inline: it never touches the network. Everything `getattr` needs
    // was recorded by the `lookup` that made the kernel aware of the inode, and a
    // mutation replies with the attributes it produced.
    match self.fs.record(ino.0) {
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
        Node::Synth(_) => return reply.error(Errno::EINVAL),
      };
      match fs.open_blob(&blob_path(&record), &oid).await {
        Ok(file) => match read_all(&file, expected) {
          Ok(bytes) => reply.data(&bytes),
          Err(e) => reply.error(errno_of(&e)),
        },
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
      if writable && record.node.is_synth() {
        // ADR 0005 fixes the synthesized surface as read-only. It is not overlay
        // content and there is nothing to copy it up into.
        return reply.error(Errno::EROFS);
      }
      let state = match &record.node {
        Node::Synth(SynthNode::File(bytes)) => FileState::Synth(Arc::clone(bytes)),
        Node::Synth(SynthNode::Dir) => return reply.error(Errno::EISDIR),
        Node::Overlay(entry) if entry.kind.is_dir() => return reply.error(Errno::EISDIR),
        Node::Overlay(entry) if entry.kind == OverlayKind::Symlink => {
          return reply.error(Errno::ELOOP)
        }
        Node::Base(entry) if entry.kind == EntryKind::Symlink => return reply.error(Errno::ELOOP),
        Node::Base(entry) if entry.kind.is_dir_like() => return reply.error(Errno::EISDIR),
        // Predictable rather than approximated: DESIGN.md section 8.2 refuses to
        // present a mode XVFS does not model as the nearest one it does.
        Node::Base(entry) if matches!(entry.kind, EntryKind::Unsupported(_)) => {
          return reply.error(Errno::EIO)
        }
        _ if writable || truncating => match fs.copy_up(&record, truncating).await {
          Ok(entry) => {
            let Some(id) = entry.content.local_id() else {
              return reply.error(Errno::EIO);
            };
            match fs.overlay.content_store().open_write(id) {
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
          Some(id) => match fs.overlay.content_store().open_read(id) {
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
              Ok(file) => FileState::Blob {
                oid: oid.clone(),
                file: Arc::new(file),
              },
              Err(e) => {
                fs.bump(|s| s.errors += 1);
                return reply.error(errno_of(&e));
              }
            },
            None => return reply.error(Errno::EIO),
          },
        },
        Node::Base(entry) => match fs.open_blob(&record.path, &entry.oid).await {
          Ok(file) => FileState::Blob {
            oid: entry.oid.clone(),
            file: Arc::new(file),
          },
          Err(e) => {
            fs.bump(|s| s.errors += 1);
            return reply.error(errno_of(&e));
          }
        },
      };
      let handle = fs.new_handle();
      fs.inodes.lock().expect("inode table").open(ino.0);
      fs.files
        .lock()
        .expect("file handles")
        .insert(handle, Arc::new(state));
      fs.bump(|s| s.opens += 1);
      reply.opened(FileHandle(handle), fuser::FopenFlags::empty());
    });
  }

  fn create(
    &self,
    _req: &Request,
    parent: INodeNo,
    name: &OsStr,
    mode: u32,
    umask: u32,
    _flags: i32,
    reply: fuser::ReplyCreate,
  ) {
    let fs = Arc::clone(&self.fs);
    let name = name.as_bytes().to_vec();
    self.spawn(async move {
      let Some(parent) = fs.record(parent.0) else {
        return reply.error(Errno::ESTALE);
      };
      if parent.node.is_synth() {
        return reply.error(Errno::EROFS);
      }
      let path = parent.path.join(&name);
      let resolved = match fs.resolve_path(&path).await {
        Ok(resolved) => resolved,
        Err(e) => return reply.error(errno_of(&e)),
      };
      if resolved.node.is_some() {
        return reply.error(Errno::EEXIST);
      }
      // Git records exactly one permission bit, so that is the one this reads.
      let executable = mode & !umask & 0o111 != 0;
      let overlay = Arc::clone(&fs.overlay);
      let parent_base = Xvfs::parent_base(&parent);
      let target = path.clone();
      let ino = fs.number_for(&path);
      let created = Xvfs::blocking(move || {
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
      let file = match fs.overlay.content_store().open_write(id) {
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
      reply.created(
        &fs.config.ttl,
        &attr,
        GENERATION,
        FileHandle(handle),
        fuser::FopenFlags::empty(),
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
        FileState::Synth(bytes) => {
          let start = (offset as usize).min(bytes.len());
          let end = start.saturating_add(size as usize).min(bytes.len());
          fs.bump(|s| {
            s.reads += 1;
            s.read_bytes += (end - start) as u64;
          });
          return reply.data(&bytes[start..end]);
        }
        FileState::Blob { file, .. } | FileState::Local { file, .. } => Arc::clone(file),
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
      let (content_id, file) = match &*state {
        FileState::Local {
          content_id,
          file,
          writable: true,
        } => (*content_id, Arc::clone(file)),
        // Not `EROFS`: the filesystem is writable, this descriptor is not.
        _ => return reply.error(Errno::EBADF),
      };
      let overlay = Arc::clone(&fs.overlay);
      let written =
        Xvfs::blocking(move || overlay.write_content(content_id, &file, offset, &data)).await;
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
            match fs.overlay.get(&record.path) {
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
      // Both halves, and in this order: the content file, then the journal that
      // names it. The store's invariant is that a committed row's content
      // exists, and syncing the journal first would invert it for exactly the
      // window a power loss could land in.
      let file = match state.as_deref() {
        Some(FileState::Local { file, .. }) => Some(Arc::clone(file)),
        _ => None,
      };
      let overlay = Arc::clone(&fs.overlay);
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
    _ino: INodeNo,
    _fh: FileHandle,
    _datasync: bool,
    reply: fuser::ReplyEmpty,
  ) {
    let fs = Arc::clone(&self.fs);
    self.spawn(async move {
      let overlay = Arc::clone(&fs.overlay);
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
      Node::Synth(node) => node.size(),
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
      next_page_token: Vec::new(),
      base_done: false,
      complete: false,
      base_names: HashSet::new(),
    };
    match &record.node {
      Node::Synth(SynthNode::Dir) => {
        state.children = self
          .fs
          .gitdir
          .children(&record.path)
          .into_iter()
          .map(|(name, node)| Child::Synth { name, node })
          .collect();
        state.complete = true;
      }
      Node::Synth(SynthNode::File(_)) => return reply.error(Errno::ENOTDIR),
      Node::Overlay(entry) if entry.kind.is_dir() => {
        // A created directory shadows the base, so there is nothing to page.
        state.base_done = self.fs.overlay.masks_base(&record.path);
      }
      Node::Overlay(_) => return reply.error(Errno::ENOTDIR),
      Node::Base(entry) => match entry.kind {
        // A submodule is an empty directory that lists successfully rather than
        // erroring, which is what DESIGN.md section 8.2 specifies.
        EntryKind::Gitlink => state.complete = true,
        EntryKind::Directory => {
          // The root carries the synthesized `.git` as its first child, so the
          // offset of every other entry stays the same no matter how the listing
          // pages. Appending it last would require exhausting the listing before
          // the first entry could be emitted.
          if ino.0 == ROOT_INO {
            if let Some(node) = self.fs.gitdir.get(&BytePath::new(GIT_DIR.to_vec())) {
              state.children.push(Child::Synth {
                name: GIT_DIR.to_vec(),
                node,
              });
            }
          }
        }
        _ => return reply.error(Errno::ENOTDIR),
      },
    }

    let handle = self.fs.new_handle();
    self.fs.inodes.lock().expect("inode table").open(ino.0);
    self
      .fs
      .dirs
      .lock()
      .expect("dir handles")
      .insert(handle, Arc::new(tokio::sync::Mutex::new(state)));
    reply.opened(FileHandle(handle), fuser::FopenFlags::empty());
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
        let wanted = (index - 1) as usize;
        if let Err(e) = fs.fill_directory(&mut state, wanted).await {
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
        let wanted = (index - 1) as usize;
        if let Err(e) = fs.fill_directory(&mut state, wanted).await {
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
        if reply.add(
          INodeNo(record.ino),
          index + 1,
          OsStr::from_bytes(&name),
          &fs.config.ttl,
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
    if mask.contains(AccessFlags::W_OK) && record.node.is_synth() {
      // `EROFS` for the synthesized surface only. The rest of the mount is
      // writable through the overlay, and POSIX distinguishes "you may not" from
      // "nobody may, because this is read-only".
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
    let stats = self.fs.overlay.stats();
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
      xvfs_types::limits::MAX_PATH_BYTES as u32,
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
    _uid: Option<u32>,
    _gid: Option<u32>,
    size: Option<u64>,
    _atime: Option<fuser::TimeOrNow>,
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
      if record.node.is_synth() {
        return reply.error(Errno::EROFS);
      }
      let base = match &record.node {
        Node::Overlay(entry) => entry.base.clone(),
        Node::Base(entry) => base_facts(entry),
        Node::Synth(_) => None,
      };

      if let Some(size) = size {
        // Truncating to zero replaces the whole file, so the old bytes are never
        // fetched. Any other size needs them.
        match fs.copy_up(&record, size == 0).await {
          Ok(_) => {}
          Err(e) => return reply.error(errno_of(&e)),
        }
        let overlay = Arc::clone(&fs.overlay);
        let path = record.path.clone();
        if let Err(e) = Xvfs::blocking(move || overlay.truncate(&path, size)).await {
          return reply.error(errno_of_overlay(&e));
        }
      }

      if let Some(mode) = mode {
        let overlay = Arc::clone(&fs.overlay);
        let path = record.path.clone();
        let base = base.clone();
        let ino = record.ino;
        let executable = mode & 0o111 != 0;
        if let Err(e) =
          Xvfs::blocking(move || overlay.set_executable(&path, base, ino, executable)).await
        {
          return reply.error(errno_of_overlay(&e));
        }
      }

      if let Some(mtime) = mtime {
        let requested = match mtime {
          fuser::TimeOrNow::SpecificTime(t) => Some(Timestamp::from_system_time(t)),
          fuser::TimeOrNow::Now => None,
        };
        let overlay = Arc::clone(&fs.overlay);
        let path = record.path.clone();
        let base = base.clone();
        let ino = record.ino;
        if let Err(e) = Xvfs::blocking(move || overlay.set_times(&path, base, ino, requested)).await
        {
          return reply.error(errno_of_overlay(&e));
        }
      }

      // Nothing asked for: `truncate`-less `utimensat`-less `chmod`-less
      // `setattr` still has to answer with the current attributes.
      match fs.overlay.get(&record.path) {
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
      if parent.node.is_synth() {
        return reply.error(Errno::EROFS);
      }
      let path = parent.path.join(&name);
      let resolved = match fs.resolve_path(&path).await {
        Ok(resolved) => resolved,
        Err(e) => return reply.error(errno_of(&e)),
      };
      let overlay = Arc::clone(&fs.overlay);
      let parent_base = Xvfs::parent_base(&parent);
      let target = path.clone();
      let ino = fs.number_for(&path);
      match Xvfs::blocking(move || overlay.mkdir(&target, resolved.base, parent_base, ino)).await {
        Ok(entry) => {
          let record = fs
            .inodes
            .lock()
            .expect("inode table")
            .insert_lookup(path, Node::Overlay(Box::new(entry)));
          fs.bump(|s| s.lookups += 1);
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
      if parent.node.is_synth() {
        return reply.error(Errno::EROFS);
      }
      let path = parent.path.join(&name);
      let resolved = match fs.resolve_path(&path).await {
        Ok(resolved) => resolved,
        Err(e) => return reply.error(errno_of(&e)),
      };
      let overlay = Arc::clone(&fs.overlay);
      let parent_base = Xvfs::parent_base(&parent);
      let link_path = path.clone();
      let ino = fs.number_for(&path);
      match Xvfs::blocking(move || {
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
    _ino: INodeNo,
    _newparent: INodeNo,
    _newname: &OsStr,
    reply: fuser::ReplyEntry,
  ) {
    // `EPERM`, not `EROFS`: DESIGN.md section 8.2 fixes hard links as unsupported
    // for the life of the MVP because Git has no hard links and the overlay does
    // not model them. Reporting it as a consequence of read-only would be
    // misleading now that the mount is writable.
    reply.error(Errno::EPERM);
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
      if record.node.is_synth() {
        return reply.error(Errno::EROFS);
      }
      if let Err(e) = fs.copy_up(&record, false).await {
        return reply.error(errno_of(&e));
      }
      let wanted = offset.saturating_add(length);
      let current = fs.overlay.get(&record.path).map(|e| e.size).unwrap_or(0);
      if wanted <= current {
        return reply.ok();
      }
      let overlay = Arc::clone(&fs.overlay);
      let path = record.path.clone();
      match Xvfs::blocking(move || overlay.truncate(&path, wanted)).await {
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

impl Xvfs {
  /// `unlink` and `rmdir`, which differ only in what they expect to find.
  async fn remove_child(&self, parent: u64, name: &[u8], expect_dir: bool) -> Result<(), Errno> {
    let parent = self.record(parent).ok_or(Errno::ESTALE)?;
    if parent.node.is_synth() {
      return Err(Errno::EROFS);
    }
    let path = parent.path.join(name);
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
    let overlay = Arc::clone(&self.overlay);
    let target = path.clone();
    Self::blocking(move || overlay.remove(&target, resolved.base, expect_dir, empty))
      .await
      .map_err(|e| errno_of_overlay(&e))
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
    if from_parent.node.is_synth() || to_parent.node.is_synth() {
      return Err(Errno::EROFS);
    }
    let from = from_parent.path.join(name);
    let to = to_parent.path.join(newname);

    let source = self.resolve_path(&from).await.map_err(|e| errno_of(&e))?;
    let Some(source_node) = &source.node else {
      return Err(Errno::ENOENT);
    };
    let target = self.resolve_path(&to).await.map_err(|e| errno_of(&e))?;

    let source_is_dir = match source_node {
      Node::Base(entry) => entry.kind.is_dir_like(),
      Node::Overlay(entry) => entry.kind.is_dir(),
      Node::Synth(_) => return Err(Errno::EROFS),
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
      Some(Node::Synth(node)) => node.is_dir(),
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

    let overlay = Arc::clone(&self.overlay);
    let to_parent_base = Xvfs::parent_base(&to_parent);
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
      if let Some(entry) = self.overlay.get(&path) {
        self
          .inodes
          .lock()
          .expect("inode table")
          .refresh(ino, Node::Overlay(Box::new(entry)));
      }
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
    mode: xvfs_types::mode::DIRECTORY,
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

fn read_all(file: &std::fs::File, size: u64) -> Result<Vec<u8>, XvfsError> {
  let mut buffer = vec![0u8; size as usize];
  let read = file.read_at(&mut buffer, 0).map_err(|e| {
    XvfsError::new(
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
      oid: ObjectId::from_raw(xvfs_types::HashAlgorithm::Sha1, &[1; 20]).unwrap(),
      size: 0,
      symlink_target: None,
      blob_ticket: None,
    };
    assert!(base_facts(&entry(EntryKind::Regular)).is_some());
    assert!(base_facts(&entry(EntryKind::Gitlink)).is_none());
    assert!(base_facts(&entry(EntryKind::Unsupported(0o120_755))).is_none());
  }
}
