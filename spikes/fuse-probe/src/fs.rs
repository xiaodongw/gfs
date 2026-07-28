//! A minimal read-only FUSE filesystem with remote-backed files.
//!
//! Small on purpose. The M0.2 question is whether a mount is possible in the
//! target environment and what it costs, not whether GFS's eventual inode model
//! is right. What this does model faithfully is the one structural decision
//! M2 cannot walk back: a `read` has to wait on the network, and where that wait
//! happens decides whether the whole mount serializes.
//!
//! Two dispatch modes exist so the difference is measurable rather than assumed:
//!
//! * `Blocking` — fetch and reply inside the FUSE callback, the obvious
//!   implementation.
//! * `Pooled` — hand the `ReplyData` to a bounded worker pool and return
//!   immediately, which is what DESIGN.md section 8.2 requires.

use crate::origin::OriginClient;
use fuser::{
  Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner,
  OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
  Request,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const ROOT_INO: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
  Blocking,
  Pooled,
}

#[derive(Default, Debug, serde::Serialize)]
pub struct OpCounters {
  pub lookup: AtomicU64,
  pub getattr: AtomicU64,
  pub open: AtomicU64,
  pub read: AtomicU64,
  pub readdir: AtomicU64,
  pub readlink: AtomicU64,
  pub statfs: AtomicU64,
  pub release: AtomicU64,
  /// Reads served from the in-process blob cache rather than the origin.
  pub read_cache_hit: AtomicU64,
  /// Peak number of reads in flight at once. The whole point of `Pooled`.
  pub peak_concurrent_reads: AtomicU64,
}

pub struct Entry {
  pub ino: u64,
  pub name: String,
  pub kind: FileType,
  pub size: u64,
  /// Origin blob name for regular files, link target for symlinks.
  pub backing: String,
  pub parent: u64,
}

pub struct Shared {
  pub entries: Vec<Entry>,
  pub by_parent_name: HashMap<(u64, String), u64>,
  pub origin: OriginClient,
  pub counters: OpCounters,
  /// Whole-blob cache, as in DESIGN.md section 7.4's MVP decision.
  pub cache: Mutex<HashMap<u64, Arc<Vec<u8>>>>,
  pub snapshot_time: SystemTime,
  pub attr_ttl: Duration,
  pub entry_ttl: Duration,
  pub in_flight: AtomicU64,
}

impl Shared {
  pub fn entry(&self, ino: u64) -> Option<&Entry> {
    self.entries.iter().find(|e| e.ino == ino)
  }

  fn attr(&self, e: &Entry) -> FileAttr {
    // Every base entry reports the same sanitized snapshot time, which is
    // the rule in DESIGN.md section 8.2. Nothing here derives a timestamp
    // from the host clock.
    FileAttr {
      ino: INodeNo(e.ino),
      size: e.size,
      blocks: e.size.div_ceil(512),
      atime: self.snapshot_time,
      mtime: self.snapshot_time,
      ctime: self.snapshot_time,
      crtime: self.snapshot_time,
      kind: e.kind,
      perm: match e.kind {
        FileType::Directory => 0o555,
        FileType::Symlink => 0o777,
        _ => 0o444,
      },
      nlink: if e.kind == FileType::Directory { 2 } else { 1 },
      uid: unsafe { libc::getuid() },
      gid: unsafe { libc::getgid() },
      rdev: 0,
      blksize: 4096,
      flags: 0,
    }
  }

  /// Fetch a blob, recording concurrency and cache behaviour.
  fn fetch(&self, ino: u64, backing: &str) -> Result<Arc<Vec<u8>>, Errno> {
    if let Some(hit) = self.cache.lock().unwrap().get(&ino) {
      self.counters.read_cache_hit.fetch_add(1, Ordering::Relaxed);
      return Ok(Arc::clone(hit));
    }

    let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    self
      .counters
      .peak_concurrent_reads
      .fetch_max(now, Ordering::SeqCst);
    let result = self.origin.fetch(backing);
    self.in_flight.fetch_sub(1, Ordering::SeqCst);

    match result {
      Ok(data) => {
        let arc = Arc::new(data);
        self.cache.lock().unwrap().insert(ino, Arc::clone(&arc));
        Ok(arc)
      }
      // A lost origin is a retryable I/O error for uncached base data,
      // per DESIGN.md section 9 — never ENOENT, which would tell the
      // caller the file does not exist.
      Err(_) => Err(Errno::EIO),
    }
  }
}

type Job = Box<dyn FnOnce() + Send>;

pub struct ProbeFs {
  shared: Arc<Shared>,
  dispatch: Dispatch,
  jobs: Option<Sender<Job>>,
}

impl ProbeFs {
  pub fn new(shared: Arc<Shared>, dispatch: Dispatch, workers: usize) -> Self {
    let jobs = if dispatch == Dispatch::Pooled {
      let (tx, rx) = mpsc::channel::<Job>();
      let rx = Arc::new(Mutex::new(rx));
      for _ in 0..workers.max(1) {
        let rx = Arc::clone(&rx);
        std::thread::spawn(move || loop {
          // The lock is held only to take a job, never while running
          // one, so workers do not serialize on each other.
          let job = { rx.lock().unwrap().recv() };
          match job {
            Ok(job) => job(),
            Err(_) => break,
          }
        });
      }
      Some(tx)
    } else {
      None
    };
    ProbeFs {
      shared,
      dispatch,
      jobs,
    }
  }
}

impl Filesystem for ProbeFs {
  fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
    self.shared.counters.lookup.fetch_add(1, Ordering::Relaxed);
    let key = (parent.0, name.to_string_lossy().into_owned());
    match self.shared.by_parent_name.get(&key) {
      Some(ino) => {
        let e = self.shared.entry(*ino).unwrap();
        reply.entry(&self.shared.entry_ttl, &self.shared.attr(e), Generation(0));
      }
      None => reply.error(Errno::ENOENT),
    }
  }

  fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
    self.shared.counters.getattr.fetch_add(1, Ordering::Relaxed);
    match self.shared.entry(ino.0) {
      Some(e) => reply.attr(&self.shared.attr_ttl, &self.shared.attr(e)),
      None => reply.error(Errno::ENOENT),
    }
  }

  fn open(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
    self.shared.counters.open.fetch_add(1, Ordering::Relaxed);
    // The base is immutable, so the kernel may keep its page cache across
    // opens. FOPEN_KEEP_CACHE is safe here in a way it would not be for a
    // writable overlay entry.
    reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE);
  }

  #[allow(clippy::too_many_arguments)]
  fn read(
    &self,
    _req: &Request,
    ino: INodeNo,
    _fh: FileHandle,
    offset: u64,
    size: u32,
    _flags: OpenFlags,
    _lock_owner: Option<LockOwner>,
    reply: ReplyData,
  ) {
    self.shared.counters.read.fetch_add(1, Ordering::Relaxed);
    let Some(e) = self.shared.entry(ino.0) else {
      reply.error(Errno::ENOENT);
      return;
    };
    let backing = e.backing.clone();
    let shared = Arc::clone(&self.shared);

    let serve = move || match shared.fetch(ino.0, &backing) {
      Ok(data) => {
        let start = (offset as usize).min(data.len());
        let end = (start + size as usize).min(data.len());
        reply.data(&data[start..end]);
      }
      Err(errno) => reply.error(errno),
    };

    match (&self.dispatch, &self.jobs) {
      // The naive implementation: the FUSE event-loop thread waits on the
      // network, so nothing else it would have dispatched progresses.
      (Dispatch::Blocking, _) | (_, None) => serve(),
      // The required one: the callback returns immediately and a bounded
      // pool replies later. The reply types are Send precisely for this.
      (Dispatch::Pooled, Some(tx)) => {
        let _ = tx.send(Box::new(serve));
      }
    }
  }

  fn readdir(
    &self,
    _req: &Request,
    ino: INodeNo,
    _fh: FileHandle,
    offset: u64,
    mut reply: ReplyDirectory,
  ) {
    self.shared.counters.readdir.fetch_add(1, Ordering::Relaxed);
    let Some(dir) = self.shared.entry(ino.0) else {
      reply.error(Errno::ENOENT);
      return;
    };
    if dir.kind != FileType::Directory {
      reply.error(Errno::ENOTDIR);
      return;
    }
    let mut listing: Vec<(u64, FileType, String)> = vec![
      (ino.0, FileType::Directory, ".".into()),
      (dir.parent, FileType::Directory, "..".into()),
    ];
    listing.extend(
      self
        .shared
        .entries
        .iter()
        .filter(|e| e.parent == ino.0 && e.ino != ino.0)
        .map(|e| (e.ino, e.kind, e.name.clone())),
    );

    for (i, (child, kind, name)) in listing.into_iter().enumerate().skip(offset as usize) {
      // `add` returns true when the reply buffer is full; the kernel then
      // asks again starting at the offset we handed back.
      if reply.add(INodeNo(child), (i + 1) as u64, kind, name) {
        break;
      }
    }
    reply.ok();
  }

  fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
    self
      .shared
      .counters
      .readlink
      .fetch_add(1, Ordering::Relaxed);
    match self.shared.entry(ino.0) {
      Some(e) if e.kind == FileType::Symlink => reply.data(e.backing.as_bytes()),
      Some(_) => reply.error(Errno::EINVAL),
      None => reply.error(Errno::ENOENT),
    }
  }

  #[allow(clippy::too_many_arguments)]
  fn release(
    &self,
    _req: &Request,
    _ino: INodeNo,
    _fh: FileHandle,
    _flags: OpenFlags,
    _lock_owner: Option<LockOwner>,
    _flush: bool,
    reply: ReplyEmpty,
  ) {
    self.shared.counters.release.fetch_add(1, Ordering::Relaxed);
    reply.ok();
  }

  fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
    self.shared.counters.statfs.fetch_add(1, Ordering::Relaxed);
    // Reported against the notional overlay quota, not the host filesystem,
    // per DESIGN.md section 8.2.
    let total_blocks = 1024 * 1024u64;
    reply.statfs(
      total_blocks,
      total_blocks / 2,
      total_blocks / 2,
      self.shared.entries.len() as u64,
      1 << 20,
      4096,
      255,
      4096,
    );
  }
}

/// Build the tree the probe serves: `n` remote-backed files, a subdirectory,
/// and a symlink.
pub fn build_tree(n: usize, file_size: usize) -> (Vec<Entry>, HashMap<String, Vec<u8>>) {
  let mut entries = vec![Entry {
    ino: ROOT_INO,
    name: "/".into(),
    kind: FileType::Directory,
    size: 0,
    backing: String::new(),
    parent: ROOT_INO,
  }];
  let mut blobs = HashMap::new();

  let sub_ino = 2;
  entries.push(Entry {
    ino: sub_ino,
    name: "sub".into(),
    kind: FileType::Directory,
    size: 0,
    backing: String::new(),
    parent: ROOT_INO,
  });

  let mut ino = 3;
  for i in 0..n {
    let name = format!("file-{i:04}");
    // Distinct content per file so a wrong-blob bug cannot pass unnoticed.
    let mut content = format!("gfs fuse probe blob {i}\n").into_bytes();
    content.resize(file_size, b'.');
    blobs.insert(name.clone(), content);
    entries.push(Entry {
      ino,
      name,
      kind: FileType::RegularFile,
      size: file_size as u64,
      backing: format!("file-{i:04}"),
      parent: if i % 8 == 7 { sub_ino } else { ROOT_INO },
    });
    ino += 1;
  }

  entries.push(Entry {
    ino,
    name: "link-to-first".into(),
    kind: FileType::Symlink,
    size: "file-0000".len() as u64,
    backing: "file-0000".into(),
    parent: ROOT_INO,
  });

  (entries, blobs)
}

pub fn index(entries: &[Entry]) -> HashMap<(u64, String), u64> {
  entries
    .iter()
    .filter(|e| e.ino != ROOT_INO)
    .map(|e| ((e.parent, e.name.clone()), e.ino))
    .collect()
}

pub fn epoch(secs: u64) -> SystemTime {
  UNIX_EPOCH + Duration::from_secs(secs)
}
