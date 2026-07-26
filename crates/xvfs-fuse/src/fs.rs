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
//! # Caching TTLs are long on purpose
//!
//! The pinned commit is immutable, so a cached attribute can never be stale. ADR
//! 0003 measured 1000 `stat(2)` calls on one path producing **zero** `getattr`
//! upcalls at a 60-second TTL; the same reasoning permits much longer. This is
//! what makes `ls -l` over a monorepo affordable. Negative entries are cached the
//! same way and for the same reason: a path absent from an immutable commit stays
//! absent. M3's overlay is what will need explicit invalidation, and it is M3 that
//! will issue it.
//!
//! # Read-only means `EROFS`, not `ENOSYS`
//!
//! Every mutation is answered with `EROFS` (except `link`, which DESIGN.md section
//! 8.2 fixes at `EPERM` because Git has no hard links to model). `ENOSYS` would
//! tell the kernel the operation is unimplemented and let it cache that fact for
//! the whole filesystem, which is a different and less accurate claim than "this
//! filesystem is read-only".

use std::collections::HashMap;
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
use xvfs_types::error::{ErrorCode, XvfsError};
use xvfs_types::{BytePath, EntryKind, ObjectId, Timestamp, TreeEntryInfo};

use crate::attr::{attr_of, errno_of, Ownership};
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
  /// Reported by `statfs` as the total. DESIGN.md section 8.2 and PLAN.md M2.2
  /// require the overlay quota here rather than the host filesystem's totals: a
  /// build that reads `df` must see the budget it will actually be stopped by.
  pub overlay_quota_bytes: u64,
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
      negative_ttl: Duration::from_secs(3600),
      // 1 GiB. A placeholder until M3 has a real overlay to bound; it is reported
      // rather than enforced here, because there is nothing yet to write.
      overlay_quota_bytes: 1 << 30,
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
  pub errors: u64,
}

/// A resolved child in a directory listing.
#[derive(Clone, Debug)]
enum Child {
  Base(TreeEntryInfo),
  Synth { name: Vec<u8>, node: SynthNode },
}

impl Child {
  fn name(&self) -> Vec<u8> {
    match self {
      Child::Base(entry) => entry.path.file_name().unwrap_or_default().to_vec(),
      Child::Synth { name, .. } => name.clone(),
    }
  }
}

#[derive(Debug)]
struct DirState {
  /// The directory's own path, used to build child paths.
  path: BytePath,
  children: Vec<Child>,
  next_page_token: Vec<u8>,
  /// Whether every page has been fetched.
  complete: bool,
}

#[derive(Debug)]
enum FileState {
  /// A cached blob, opened once and read with `pread`.
  Blob {
    oid: ObjectId,
    file: Arc<std::fs::File>,
  },
  Synth(Arc<Vec<u8>>),
}

/// The filesystem. Shared behind an `Arc` so a callback can hand it to a worker.
pub struct Xvfs {
  client: Arc<SnapshotClient>,
  cache: Arc<BlobCache>,
  gitdir: Arc<GitDir>,
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
    root: TreeEntryInfo,
    config: FsConfig,
  ) -> Arc<Self> {
    let snapshot_time = client.binding().snapshot_time;
    Arc::new(Xvfs {
      client,
      cache,
      gitdir,
      config,
      owner: Ownership::current(),
      snapshot_time,
      inodes: Mutex::new(InodeTable::new(root)),
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

  fn new_handle(&self) -> u64 {
    self.next_handle.fetch_add(1, Ordering::Relaxed)
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

  /// Resolve one name under a parent, from either world.
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

    let path = parent.path.join(name);
    path.validate()?;
    self.bump(|s| s.metadata_requests += 1);
    let entry = self
      .retrying(|| self.client.get_entry(&path, false))
      .await?;
    Ok(entry.map(Node::Base))
  }

  /// Fetch directory pages until `wanted` children are known or the listing ends.
  async fn fill_directory(&self, state: &mut DirState, wanted: usize) -> Result<(), XvfsError> {
    while !state.complete && state.children.len() < wanted {
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
      state
        .children
        .extend(page.entries.into_iter().map(Child::Base));
      if page.next_page_token.is_empty() {
        state.complete = true;
      } else {
        state.next_page_token = page.next_page_token;
      }
    }
    Ok(())
  }

  /// Open a blob for reading, fetching and verifying it if the cache lacks it.
  ///
  /// The ticket is minted here rather than at lookup time. A blob ticket is
  /// short-lived authorization state (the server issues it for five minutes), so
  /// one attached to every metadata lookup would usually expire unused; and when
  /// the blob is already cached, no ticket -- and no round trip -- is needed at
  /// all, which is what makes a warm reopen free.
  async fn open_blob(&self, entry: &TreeEntryInfo) -> Result<std::fs::File, XvfsError> {
    if !self.cache.contains(&entry.oid) {
      let path = entry.path.clone();
      self.bump(|s| s.metadata_requests += 1);
      let fresh = self
        .retrying(|| self.client.get_entry(&path, true))
        .await?
        .ok_or_else(|| XvfsError::not_found("the path vanished from a pinned commit"))?;
      if fresh.oid != entry.oid {
        // Impossible against an immutable commit, and therefore worth refusing
        // loudly rather than serving: it means the server answered about a
        // different snapshot than the one this mount is pinned to.
        return Err(XvfsError::internal(
          "the server returned a different blob for a pinned path",
        ));
      }
      let ticket = fresh.blob_ticket.unwrap_or_default();
      let (path, _) = self
        .cache
        .open_blob(&self.client, &entry.oid, &ticket)
        .await?;
      return open_cached(&path);
    }
    let (path, _) = self.cache.open_blob(&self.client, &entry.oid, "").await?;
    open_cached(&path)
  }
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
    // was recorded by the `lookup` that made the kernel aware of the inode, and
    // the commit is immutable so it cannot have changed since.
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
      let Node::Base(entry) = &record.node else {
        return reply.error(Errno::EINVAL);
      };
      if entry.kind != EntryKind::Symlink {
        return reply.error(Errno::EINVAL);
      }
      // The server returns the target with the entry, so an `ls -l` of a
      // directory full of symlinks resolves every one without fetching a blob.
      if let Some(target) = &entry.symlink_target {
        return reply.data(target);
      }
      match fs.open_blob(entry).await {
        Ok(file) => match read_all(&file, entry.size) {
          Ok(bytes) => reply.data(&bytes),
          Err(e) => reply.error(errno_of(&e)),
        },
        Err(e) => reply.error(errno_of(&e)),
      }
    });
  }

  fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: fuser::ReplyOpen) {
    if flags.acc_mode() != OpenAccMode::O_RDONLY {
      return reply.error(Errno::EROFS);
    }
    let fs = Arc::clone(&self.fs);
    self.spawn(async move {
      let Some(record) = fs.record(ino.0) else {
        return reply.error(Errno::ESTALE);
      };
      let state = match &record.node {
        Node::Synth(SynthNode::File(bytes)) => FileState::Synth(Arc::clone(bytes)),
        Node::Synth(SynthNode::Dir) => return reply.error(Errno::EISDIR),
        Node::Base(entry) => match entry.kind {
          EntryKind::Regular | EntryKind::Executable => match fs.open_blob(entry).await {
            Ok(file) => FileState::Blob {
              oid: entry.oid.clone(),
              file: Arc::new(file),
            },
            Err(e) => {
              fs.bump(|s| s.errors += 1);
              return reply.error(errno_of(&e));
            }
          },
          EntryKind::Directory | EntryKind::Gitlink => return reply.error(Errno::EISDIR),
          EntryKind::Symlink => return reply.error(Errno::ELOOP),
          // Predictable rather than approximated: DESIGN.md section 8.2 refuses
          // to present a mode XVFS does not model as the nearest one it does.
          EntryKind::Unsupported(_) => return reply.error(Errno::EIO),
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
      match &*state {
        FileState::Synth(bytes) => {
          let start = (offset as usize).min(bytes.len());
          let end = start.saturating_add(size as usize).min(bytes.len());
          fs.bump(|s| {
            s.reads += 1;
            s.read_bytes += (end - start) as u64;
          });
          reply.data(&bytes[start..end]);
        }
        FileState::Blob { file, .. } => {
          let file = Arc::clone(file);
          // Even a page-cache hit is a blocking syscall, and ADR 0003's
          // measurement is about what one blocked worker costs the whole mount.
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
    // Nothing is buffered for a read-only base, so there is nothing to flush.
    reply.ok();
  }

  fn fsync(
    &self,
    _req: &Request,
    _ino: INodeNo,
    _fh: FileHandle,
    _datasync: bool,
    reply: fuser::ReplyEmpty,
  ) {
    reply.ok();
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
    // kernel's business. A Git blob is never sparse, so the whole file is data
    // and the only hole is at the end.
    let Some(record) = self.fs.record(ino.0) else {
      return reply.error(Errno::ESTALE);
    };
    let size = match &record.node {
      Node::Base(entry) => entry.size,
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
    let state = match &record.node {
      Node::Synth(SynthNode::Dir) => DirState {
        path: record.path.clone(),
        children: self
          .fs
          .gitdir
          .children(&record.path)
          .into_iter()
          .map(|(name, node)| Child::Synth { name, node })
          .collect(),
        next_page_token: Vec::new(),
        complete: true,
      },
      Node::Synth(SynthNode::File(_)) => return reply.error(Errno::ENOTDIR),
      Node::Base(entry) => match entry.kind {
        // A submodule is an empty directory that lists successfully rather than
        // erroring, which is what DESIGN.md section 8.2 specifies.
        EntryKind::Gitlink => DirState {
          path: record.path.clone(),
          children: Vec::new(),
          next_page_token: Vec::new(),
          complete: true,
        },
        EntryKind::Directory => DirState {
          path: record.path.clone(),
          // The root carries the synthesized `.git` as its first child, so the
          // offset of every base entry stays the same no matter how the listing
          // pages. Appending it last would require exhausting the listing before
          // the first entry could be emitted.
          children: if ino.0 == ROOT_INO {
            self
              .fs
              .gitdir
              .get(&BytePath::new(GIT_DIR.to_vec()))
              .map(|node| {
                vec![Child::Synth {
                  name: GIT_DIR.to_vec(),
                  node,
                }]
              })
              .unwrap_or_default()
          } else {
            Vec::new()
          },
          next_page_token: Vec::new(),
          complete: false,
        },
        _ => return reply.error(Errno::ENOTDIR),
      },
    };

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
        let Some(child) = state.children.get((index - 2) as usize) else {
          break;
        };
        let (name, kind) = match child {
          Child::Base(entry) => (
            entry.path.file_name().unwrap_or_default().to_vec(),
            file_type(entry.kind),
          ),
          Child::Synth { name, node } => (
            name.clone(),
            if node.is_dir() {
              fuser::FileType::Directory
            } else {
              fuser::FileType::RegularFile
            },
          ),
        };
        // Inodes are still assigned here, because the caller of `readdir` will
        // almost always `stat` what it found, and assigning now keeps that
        // `lookup` from having to allocate under a different lock.
        let child_ino = {
          let path = state.path.join(&name);
          let node = match child {
            Child::Base(entry) => Node::Base(entry.clone()),
            Child::Synth { node, .. } => Node::Synth(node.clone()),
          };
          let mut table = fs.inodes.lock().expect("inode table");
          let record = table.insert_lookup(path, node);
          // `readdir` (unlike `readdirplus`) does *not* take a kernel reference,
          // so the lookup taken above is released immediately.
          table.forget(record.ino, 1);
          record.ino
        };
        if reply.add(
          INodeNo(child_ino),
          index + 1,
          kind,
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
        let node = match &child {
          Child::Base(entry) => Node::Base(entry.clone()),
          Child::Synth { node, .. } => Node::Synth(node.clone()),
        };
        let record = fs
          .inodes
          .lock()
          .expect("inode table")
          .insert_readdirplus(path, node);
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
    if mask.contains(AccessFlags::W_OK) {
      // `EROFS`, not `EACCES`: POSIX distinguishes "you may not" from "nobody
      // may, because the filesystem is read-only", and a build that sees the
      // former may retry as another user.
      return reply.error(Errno::EROFS);
    }
    let attr = self.fs.attr(&record);
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
    let blocks = self.fs.config.overlay_quota_bytes / BLOCK;
    // M2 is read-only, so nothing of the quota is consumed yet. M3 subtracts the
    // overlay's own usage here.
    let free = blocks;
    reply.statfs(
      blocks,
      free,
      free,
      // Inode counts are notional: there is no inode table to exhaust, and
      // reporting zero free makes some tools refuse to write before trying.
      1 << 20,
      1 << 20,
      BLOCK as u32,
      xvfs_types::limits::MAX_PATH_BYTES as u32,
      BLOCK as u32,
    );
  }

  // -------------------------------------------------------------------------
  // Mutations. Read-only in M2; M3 replaces these with the overlay.
  // -------------------------------------------------------------------------

  fn setattr(
    &self,
    _req: &Request,
    _ino: INodeNo,
    _mode: Option<u32>,
    _uid: Option<u32>,
    _gid: Option<u32>,
    _size: Option<u64>,
    _atime: Option<fuser::TimeOrNow>,
    _mtime: Option<fuser::TimeOrNow>,
    _ctime: Option<std::time::SystemTime>,
    _fh: Option<FileHandle>,
    _crtime: Option<std::time::SystemTime>,
    _chgtime: Option<std::time::SystemTime>,
    _bkuptime: Option<std::time::SystemTime>,
    _flags: Option<fuser::BsdFileFlags>,
    reply: fuser::ReplyAttr,
  ) {
    reply.error(Errno::EROFS);
  }

  fn mknod(
    &self,
    _req: &Request,
    _parent: INodeNo,
    _name: &OsStr,
    _mode: u32,
    _umask: u32,
    _rdev: u32,
    reply: fuser::ReplyEntry,
  ) {
    reply.error(Errno::EROFS);
  }

  fn mkdir(
    &self,
    _req: &Request,
    _parent: INodeNo,
    _name: &OsStr,
    _mode: u32,
    _umask: u32,
    reply: fuser::ReplyEntry,
  ) {
    reply.error(Errno::EROFS);
  }

  fn unlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: fuser::ReplyEmpty) {
    reply.error(Errno::EROFS);
  }

  fn rmdir(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: fuser::ReplyEmpty) {
    reply.error(Errno::EROFS);
  }

  fn symlink(
    &self,
    _req: &Request,
    _parent: INodeNo,
    _name: &OsStr,
    _link: &std::path::Path,
    reply: fuser::ReplyEntry,
  ) {
    reply.error(Errno::EROFS);
  }

  fn rename(
    &self,
    _req: &Request,
    _parent: INodeNo,
    _name: &OsStr,
    _newparent: INodeNo,
    _newname: &OsStr,
    _flags: fuser::RenameFlags,
    reply: fuser::ReplyEmpty,
  ) {
    reply.error(Errno::EROFS);
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
    // not model them. M3 will not make this work, so reporting it as a
    // consequence of read-only would be misleading.
    reply.error(Errno::EPERM);
  }

  fn create(
    &self,
    _req: &Request,
    _parent: INodeNo,
    _name: &OsStr,
    _mode: u32,
    _umask: u32,
    _flags: i32,
    reply: fuser::ReplyCreate,
  ) {
    reply.error(Errno::EROFS);
  }

  fn write(
    &self,
    _req: &Request,
    _ino: INodeNo,
    _fh: FileHandle,
    _offset: u64,
    _data: &[u8],
    _write_flags: fuser::WriteFlags,
    _flags: OpenFlags,
    _lock_owner: Option<fuser::LockOwner>,
    reply: fuser::ReplyWrite,
  ) {
    reply.error(Errno::EROFS);
  }

  fn fallocate(
    &self,
    _req: &Request,
    _ino: INodeNo,
    _fh: FileHandle,
    _offset: u64,
    _length: u64,
    _mode: i32,
    reply: fuser::ReplyEmpty,
  ) {
    reply.error(Errno::EROFS);
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
    reply.error(Errno::EROFS);
  }

  fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: fuser::ReplyEmpty) {
    reply.error(Errno::EROFS);
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
}
