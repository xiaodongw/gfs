//! A read-only passthrough FUSE filesystem that counts what its caller asks for.
//!
//! This is an *instrument*, not a prototype. The question this spike asks is
//! "what does raw `git` demand of a projected filesystem, and how much of it",
//! which is a property of Git, not of GFS's implementation. So this deliberately
//! forwards every operation to a local directory and gets fast, honest answers
//! about **counts and bytes** rather than plausible ones about latency:
//!
//! * one `lookup` here is one snapshot-API round trip or cache hit in a real
//!   mount, which is the unit ADR 0005's 94 850-lookup objection is denominated
//!   in;
//! * `read_bytes` under `objects/pack/` is exactly what a real gateway would
//!   have to ship for the command that caused it.
//!
//! Two choices matter for the numbers to mean anything.
//!
//! **Long TTLs.** 60 s entry and attribute TTLs, matching DESIGN.md section
//! 8.2's rule for an immutable base and M0.2's finding that the kernel then
//! absorbs repeated `stat`s entirely. Without them every measurement would be
//! about the kernel's default caching rather than about Git.
//!
//! **Path classification.** Counters are bucketed by what the path *is* —
//! worktree, packfile, loose object, other — because the whole design question
//! is whether the expensive traffic lands on the tree (which GFS must avoid) or
//! on the object store (which it can serve cheaply and share between mounts).

use fuser::{
  Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner,
  OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
  Request,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const ROOT_INO: u64 = 1;

/// What a path is, for accounting. The design question is which bucket the
/// expensive traffic lands in, so the buckets are the design's own categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
  /// The projected working tree. Every byte and every lookup here is what GFS
  /// exists to avoid.
  Worktree,
  /// `objects/pack/*.pack` — the object data itself. The only class whose size
  /// scales with what the command actually needed.
  PackData,
  /// `objects/pack/*.idx`, `*.bitmap`, `*.rev` — the pack's lookup structures.
  /// Small, immutable, and identical for every mount of the repository, so a
  /// gateway can prefetch them once per node instead of serving them per job.
  PackIdx,
  /// `objects/??/*` — a loose object, also immutable by name.
  LooseObject,
  /// Anything else in the projected git directory: `packed-refs`, `HEAD`,
  /// `commit-graph`, `info/`.
  GitMeta,
}

impl Class {
  pub fn name(self) -> &'static str {
    match self {
      Class::Worktree => "worktree",
      Class::PackData => "pack_data",
      Class::PackIdx => "pack_idx",
      Class::LooseObject => "loose_object",
      Class::GitMeta => "git_meta",
    }
  }

  /// Classify a path relative to the projection root.
  ///
  /// The staging layout the driver script builds is `tree/` for the checkout and
  /// `objects/` for the object database, which is what makes this a prefix test
  /// rather than a heuristic.
  fn of(rel: &Path) -> Class {
    let s = rel.to_string_lossy();
    if s.starts_with("objects/pack") {
      // The split matters for the design conclusion, not just the report: pack
      // data is what a command genuinely needed, while the lookup structures are
      // the same bytes for every mount and can be pinned once per node.
      if s.ends_with(".pack") {
        Class::PackData
      } else {
        Class::PackIdx
      }
    } else if s.starts_with("objects") {
      // `objects/info/*` is metadata, not an object.
      if s.starts_with("objects/info") {
        Class::GitMeta
      } else {
        Class::LooseObject
      }
    } else if s.starts_with("tree") {
      Class::Worktree
    } else {
      Class::GitMeta
    }
  }
}

/// The granularity a chunked fetcher would work in, smallest first. A read of a
/// few bytes forces its whole enclosing chunk to be fetched, so the design
/// question "what chunk size?" is answered by how *scattered* the reads are, not
/// by how many bytes they total.
pub const CHUNK_SIZES: [u64; 4] = [64 << 10, 1 << 20, 8 << 20, 32 << 20];
/// Touched regions are recorded at the finest granularity and every coarser
/// figure is derived from it by division, so one pass answers every chunk size.
const BLOCK: u64 = CHUNK_SIZES[0];

#[derive(Default, Debug)]
pub struct Counters {
  pub lookup: AtomicU64,
  pub lookup_enoent: AtomicU64,
  pub getattr: AtomicU64,
  pub open: AtomicU64,
  pub read: AtomicU64,
  pub read_bytes: AtomicU64,
  pub readdir: AtomicU64,
  pub readdir_entries: AtomicU64,
  pub readlink: AtomicU64,
  /// Distinct 64 KiB blocks touched, per file. Keyed by inode because chunking is
  /// per file and two files' offsets are unrelated.
  pub blocks: Mutex<HashMap<u64, std::collections::BTreeSet<u64>>>,
}

impl Counters {
  /// Record every block a read spans, so a one-byte read at a chunk boundary is
  /// charged to both chunks it straddles -- which is what a fetcher would pay.
  fn touch(&self, ino: u64, offset: u64, len: u64) {
    if len == 0 {
      return;
    }
    let mut blocks = self.blocks.lock().unwrap();
    let set = blocks.entry(ino).or_default();
    for b in (offset / BLOCK)..=((offset + len - 1) / BLOCK) {
      set.insert(b);
    }
  }

  /// Bytes a fetcher with this chunk size would download to satisfy the reads
  /// seen so far.
  fn chunked_bytes(&self, chunk: u64) -> u64 {
    let per_block = chunk / BLOCK;
    let blocks = self.blocks.lock().unwrap();
    blocks
      .values()
      .map(|set| {
        let mut chunks = std::collections::BTreeSet::new();
        for b in set {
          chunks.insert(b / per_block);
        }
        chunks.len() as u64 * chunk
      })
      .sum()
  }

  fn snapshot(&self) -> CounterReport {
    CounterReport {
      lookup: self.lookup.load(Ordering::Relaxed),
      lookup_enoent: self.lookup_enoent.load(Ordering::Relaxed),
      getattr: self.getattr.load(Ordering::Relaxed),
      open: self.open.load(Ordering::Relaxed),
      read: self.read.load(Ordering::Relaxed),
      read_bytes: self.read_bytes.load(Ordering::Relaxed),
      readdir: self.readdir.load(Ordering::Relaxed),
      readdir_entries: self.readdir_entries.load(Ordering::Relaxed),
      readlink: self.readlink.load(Ordering::Relaxed),
      chunked_bytes: CHUNK_SIZES.iter().map(|c| self.chunked_bytes(*c)).collect(),
    }
  }

  fn reset(&self) {
    for c in [
      &self.lookup,
      &self.lookup_enoent,
      &self.getattr,
      &self.open,
      &self.read,
      &self.read_bytes,
      &self.readdir,
      &self.readdir_entries,
      &self.readlink,
    ] {
      c.store(0, Ordering::Relaxed);
    }
    self.blocks.lock().unwrap().clear();
  }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CounterReport {
  pub lookup: u64,
  pub lookup_enoent: u64,
  pub getattr: u64,
  pub open: u64,
  pub read: u64,
  pub read_bytes: u64,
  pub readdir: u64,
  pub readdir_entries: u64,
  pub readlink: u64,
  /// Bytes a chunked fetcher would download, one entry per [`CHUNK_SIZES`].
  pub chunked_bytes: Vec<u64>,
}

pub struct Shared {
  lower: PathBuf,
  /// `ino -> path relative to `lower``. Grow-only: the base is immutable, so an
  /// inode never needs to be forgotten within one mount, and stable inodes are
  /// what let the kernel cache at all (DESIGN.md section 8.2).
  paths: Mutex<Vec<PathBuf>>,
  by_path: Mutex<HashMap<PathBuf, u64>>,
  /// Open lower-file handles, so a sequential read of a packfile does not
  /// reopen it per 128 KiB request.
  files: Mutex<HashMap<u64, Arc<File>>>,
  next_fh: AtomicU64,
  counters: HashMap<Class, Counters>,
  ttl: Duration,
  /// Uniform timestamp for every projected entry, the way a real mount reports
  /// `snapshot_time`. Load-bearing: it is what a shipped index's stat data has
  /// to match under `core.checkStat=minimal`.
  snapshot_time: Option<SystemTime>,
  /// Worktree bytes after which reads fail, modelling DESIGN.md section 8.4's
  /// hard hydration budget. Zero means unlimited.
  ///
  /// It exists to answer a question the protocol constrains: a FUSE reply is data
  /// or a negative errno, with no room for prose, so the only thing a tool's own
  /// stderr can show is `strerror` of whatever errno is chosen. This makes the
  /// choice measurable instead of assumed.
  worktree_budget: u64,
}

impl Shared {
  pub fn new(
    lower: PathBuf,
    ttl_secs: u64,
    snapshot_time: Option<SystemTime>,
    worktree_budget: u64,
  ) -> Self {
    let mut counters = HashMap::new();
    for c in [
      Class::Worktree,
      Class::PackData,
      Class::PackIdx,
      Class::LooseObject,
      Class::GitMeta,
    ] {
      counters.insert(c, Counters::default());
    }
    let mut by_path = HashMap::new();
    by_path.insert(PathBuf::new(), ROOT_INO);
    Shared {
      lower,
      paths: Mutex::new(vec![PathBuf::new(), PathBuf::new()]),
      by_path: Mutex::new(by_path),
      files: Mutex::new(HashMap::new()),
      next_fh: AtomicU64::new(1),
      counters,
      ttl: Duration::from_secs(ttl_secs),
      snapshot_time,
      worktree_budget,
    }
  }

  pub fn report(&self) -> HashMap<String, CounterReport> {
    self
      .counters
      .iter()
      .map(|(c, v)| (c.name().to_owned(), v.snapshot()))
      .collect()
  }

  pub fn reset(&self) {
    for v in self.counters.values() {
      v.reset();
    }
  }

  fn count(&self, class: Class) -> &Counters {
    // Every variant is inserted in `new`, so this cannot fail.
    &self.counters[&class]
  }

  fn rel(&self, ino: u64) -> Option<PathBuf> {
    self.paths.lock().unwrap().get(ino as usize).cloned()
  }

  fn full(&self, rel: &Path) -> PathBuf {
    self.lower.join(rel)
  }

  fn intern(&self, rel: PathBuf) -> u64 {
    let mut by_path = self.by_path.lock().unwrap();
    if let Some(ino) = by_path.get(&rel) {
      return *ino;
    }
    let mut paths = self.paths.lock().unwrap();
    let ino = paths.len() as u64;
    paths.push(rel.clone());
    by_path.insert(rel, ino);
    ino
  }

  fn attr(&self, ino: u64, md: &std::fs::Metadata) -> FileAttr {
    let kind = if md.is_dir() {
      FileType::Directory
    } else if md.file_type().is_symlink() {
      FileType::Symlink
    } else {
      FileType::RegularFile
    };
    // A uniform time when asked for one, otherwise the lower file's own. The
    // first models GFS as designed; the second models a projection that passes
    // the gateway checkout's timestamps through. Which one is used decides
    // whether a shipped index refreshes clean, so it is a flag rather than a
    // constant.
    let t = self
      .snapshot_time
      .unwrap_or_else(|| UNIX_EPOCH + Duration::from_secs(md.mtime().max(0) as u64));
    FileAttr {
      ino: INodeNo(ino),
      size: md.size(),
      blocks: md.size().div_ceil(512),
      atime: t,
      mtime: t,
      ctime: t,
      crtime: t,
      kind,
      // Read-only, but the executable bit is preserved: Git tracks it and a
      // `status` that disagreed about mode would report every such file as
      // modified.
      perm: if md.is_dir() || md.mode() & 0o111 != 0 {
        0o555
      } else {
        0o444
      },
      nlink: if md.is_dir() { 2 } else { 1 },
      uid: unsafe { libc::getuid() },
      gid: unsafe { libc::getgid() },
      rdev: 0,
      blksize: 4096,
      flags: 0,
    }
  }
}

pub struct PassthroughFs {
  pub shared: Arc<Shared>,
}

fn errno(e: &std::io::Error) -> Errno {
  match e.raw_os_error() {
    Some(libc::ENOENT) => Errno::ENOENT,
    Some(libc::EACCES) => Errno::EACCES,
    Some(libc::ENOTDIR) => Errno::ENOTDIR,
    Some(libc::EISDIR) => Errno::EISDIR,
    _ => Errno::EIO,
  }
}

impl Filesystem for PassthroughFs {
  fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
    let Some(prel) = self.shared.rel(parent.0) else {
      reply.error(Errno::ENOENT);
      return;
    };
    let rel = prel.join(name);
    let class = Class::of(&rel);
    self
      .shared
      .count(class)
      .lookup
      .fetch_add(1, Ordering::Relaxed);
    // `symlink_metadata`, not `metadata`: Git stores symlinks and a projection
    // that followed them would both lie about the mode and escape the tree.
    match std::fs::symlink_metadata(self.shared.full(&rel)) {
      Ok(md) => {
        let ino = self.shared.intern(rel);
        reply.entry(&self.shared.ttl, &self.shared.attr(ino, &md), Generation(0));
      }
      Err(e) => {
        // Counted separately because Git's negative lookups are a real cost of
        // their own: it probes for `.gitattributes`, `.gitignore`, and
        // `objects/<oid>` on paths that mostly do not exist.
        self
          .shared
          .count(class)
          .lookup_enoent
          .fetch_add(1, Ordering::Relaxed);
        reply.error(errno(&e));
      }
    }
  }

  fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
    let Some(rel) = self.shared.rel(ino.0) else {
      reply.error(Errno::ENOENT);
      return;
    };
    self
      .shared
      .count(Class::of(&rel))
      .getattr
      .fetch_add(1, Ordering::Relaxed);
    match std::fs::symlink_metadata(self.shared.full(&rel)) {
      Ok(md) => reply.attr(&self.shared.ttl, &self.shared.attr(ino.0, &md)),
      Err(e) => reply.error(errno(&e)),
    }
  }

  fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
    let Some(rel) = self.shared.rel(ino.0) else {
      reply.error(Errno::ENOENT);
      return;
    };
    self
      .shared
      .count(Class::of(&rel))
      .open
      .fetch_add(1, Ordering::Relaxed);
    match File::open(self.shared.full(&rel)) {
      Ok(f) => {
        let fh = self.shared.next_fh.fetch_add(1, Ordering::Relaxed);
        self.shared.files.lock().unwrap().insert(fh, Arc::new(f));
        // KEEP_CACHE because the projection is immutable, which is what makes a
        // second read of the same packfile page free. Removing it would measure
        // the kernel's pessimism rather than Git's demand.
        reply.opened(FileHandle(fh), FopenFlags::FOPEN_KEEP_CACHE);
      }
      Err(e) => reply.error(errno(&e)),
    }
  }

  #[allow(clippy::too_many_arguments)]
  fn read(
    &self,
    _req: &Request,
    ino: INodeNo,
    fh: FileHandle,
    offset: u64,
    size: u32,
    _flags: OpenFlags,
    _lock_owner: Option<LockOwner>,
    reply: ReplyData,
  ) {
    let Some(rel) = self.shared.rel(ino.0) else {
      reply.error(Errno::ENOENT);
      return;
    };
    let class = Class::of(&rel);
    let file = self.shared.files.lock().unwrap().get(&fh.0).cloned();
    let Some(file) = file else {
      reply.error(Errno::EBADF);
      return;
    };
    // The budget is checked before the read is served, and only on the working
    // tree: hydrating the tree is what GFS exists to avoid, while object-store
    // traffic is bounded by the command rather than by the repository's size.
    if class == Class::Worktree
      && self.shared.worktree_budget > 0
      && self.shared.count(class).read_bytes.load(Ordering::Relaxed) >= self.shared.worktree_budget
    {
      reply.error(Errno::EDQUOT);
      return;
    }
    let mut buf = vec![0u8; size as usize];
    match file.read_at(&mut buf, offset) {
      Ok(n) => {
        let counters = self.shared.count(class);
        counters.read.fetch_add(1, Ordering::Relaxed);
        // Bytes actually handed back, not bytes requested: the kernel asks for
        // full 128 KiB windows past the end of a file and counting the request
        // would overstate what a gateway must ship.
        counters.read_bytes.fetch_add(n as u64, Ordering::Relaxed);
        counters.touch(ino.0, offset, n as u64);
        reply.data(&buf[..n]);
      }
      Err(e) => reply.error(errno(&e)),
    }
  }

  fn release(
    &self,
    _req: &Request,
    _ino: INodeNo,
    fh: FileHandle,
    _flags: OpenFlags,
    _lock_owner: Option<LockOwner>,
    _flush: bool,
    reply: ReplyEmpty,
  ) {
    self.shared.files.lock().unwrap().remove(&fh.0);
    reply.ok();
  }

  fn readdir(
    &self,
    _req: &Request,
    ino: INodeNo,
    _fh: FileHandle,
    offset: u64,
    mut reply: ReplyDirectory,
  ) {
    let Some(rel) = self.shared.rel(ino.0) else {
      reply.error(Errno::ENOENT);
      return;
    };
    let class = Class::of(&rel);
    if offset == 0 {
      self
        .shared
        .count(class)
        .readdir
        .fetch_add(1, Ordering::Relaxed);
    }
    let dir = self.shared.full(&rel);
    let mut listing: Vec<(u64, FileType, String)> = vec![
      (ino.0, FileType::Directory, ".".into()),
      (ino.0, FileType::Directory, "..".into()),
    ];
    let entries = match std::fs::read_dir(&dir) {
      Ok(e) => e,
      Err(e) => {
        reply.error(errno(&e));
        return;
      }
    };
    for entry in entries.flatten() {
      let name = entry.file_name().to_string_lossy().into_owned();
      let child_rel = rel.join(&name);
      let kind = match entry.file_type() {
        Ok(ft) if ft.is_dir() => FileType::Directory,
        Ok(ft) if ft.is_symlink() => FileType::Symlink,
        Ok(_) => FileType::RegularFile,
        Err(_) => continue,
      };
      listing.push((self.shared.intern(child_rel), kind, name));
    }
    let total = listing.len().saturating_sub(2) as u64;
    if offset == 0 {
      self
        .shared
        .count(class)
        .readdir_entries
        .fetch_add(total, Ordering::Relaxed);
    }
    for (i, (child, kind, name)) in listing.into_iter().enumerate().skip(offset as usize) {
      if reply.add(INodeNo(child), (i + 1) as u64, kind, name) {
        break;
      }
    }
    reply.ok();
  }

  fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
    let Some(rel) = self.shared.rel(ino.0) else {
      reply.error(Errno::ENOENT);
      return;
    };
    self
      .shared
      .count(Class::of(&rel))
      .readlink
      .fetch_add(1, Ordering::Relaxed);
    match std::fs::read_link(self.shared.full(&rel)) {
      Ok(target) => reply.data(target.as_os_str().as_encoded_bytes()),
      Err(e) => reply.error(errno(&e)),
    }
  }

  fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
    reply.statfs(1 << 20, 0, 0, 1 << 20, 0, 4096, 255, 4096);
  }
}
