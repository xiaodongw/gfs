//! PLAN.md M2.3's mmap bullet, measured.
//!
//! The bullet asks for two things: implement `mmap` behaviour "as supported by
//! FUSE/kernel", and *determine whether writable `MAP_SHARED` is available in the
//! target deployment and whether enabling the writeback cache to get it is
//! acceptable* — recording the answer as a compatibility boundary either way.
//!
//! # Why the write side needs a second filesystem
//!
//! When this was written the GFS mount was read-only, so a writable mapping
//! failed at `open(2)` and never reached `mmap(2)`. The question it had to answer
//! is a property of *FUSE* rather than of GFS: does the kernel permit a shared
//! writable mapping of a FUSE file, and does that depend on
//! `FUSE_WRITEBACK_CACHE`? A one-file probe answers it in isolation, and the
//! answer -- yes, and no -- is what let ADR 0006 decline the writeback cache.
//!
//! M3 made the mount writable, so the same case can now be measured against
//! GFS itself as well, and it is: see
//! `a_writable_shared_mapping_of_a_mounted_file_reaches_the_overlay`. The probe
//! stays, because it is what isolates the kernel's behaviour from ours.
//!
//! `Probe` below is the smallest filesystem that can answer it — one file, held
//! in memory, mounted twice: once with the capability requested and once without.
//! It is a measurement harness, not GFS code, and it exists so the M3 decision
//! rests on this kernel's behaviour rather than on recollection of FUSE's.
//!
//! Run with `--nocapture`; `docs/reports/m2-completion.md` quotes the result.

use std::ffi::{c_void, OsStr};
use std::io;

use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use fuser::{
  FileAttr, FileHandle, FileType, Filesystem, INodeNo, InitFlags, KernelConfig, OpenFlags, Request,
};
use gfs_test::mount::{on_fs, Backend, Mount};

const TTL: Duration = Duration::from_secs(1);
const FILE_INO: u64 = 2;
const FILE_NAME: &str = "f";
const CONTENT: &[u8] = b"0123456789abcdef";

// ---------------------------------------------------------------------------
// mmap helpers
// ---------------------------------------------------------------------------

/// `mmap(2)`, returning the errno on failure.
///
/// The workspace denies `unsafe_code` as a deny rather than a forbid so a call
/// like this can opt out with a reason: there is no safe wrapper for `mmap` in
/// `std`, and the whole point of this file is to observe what the kernel does
/// with one. Every mapping created here is unmapped before the test returns.
#[allow(unsafe_code)]
fn map(file: &std::fs::File, len: usize, prot: i32, flags: i32) -> Result<*mut c_void, i32> {
  use std::os::fd::AsRawFd;
  let pointer = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, flags, file.as_raw_fd(), 0) };
  if pointer == libc::MAP_FAILED {
    Err(io::Error::last_os_error().raw_os_error().unwrap_or(0))
  } else {
    Ok(pointer)
  }
}

#[allow(unsafe_code)]
fn read_mapping(pointer: *mut c_void, len: usize) -> Vec<u8> {
  unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), len) }.to_vec()
}

#[allow(unsafe_code)]
fn write_mapping(pointer: *mut c_void, bytes: &[u8]) {
  unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast::<u8>(), bytes.len()) };
}

#[allow(unsafe_code)]
fn sync_and_unmap(pointer: *mut c_void, len: usize) {
  unsafe {
    libc::msync(pointer, len, libc::MS_SYNC);
    libc::munmap(pointer, len);
  }
}

// ---------------------------------------------------------------------------
// The read side, against the real GFS mount
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_private_mapping_of_a_mounted_file_reads_correctly() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let path = mount.join("README.md");

  let bytes = on_fs(move || {
    let file = std::fs::File::open(&path).unwrap();
    let len = file.metadata().unwrap().len() as usize;
    let pointer = map(&file, len, libc::PROT_READ, libc::MAP_PRIVATE)
      .expect("MAP_PRIVATE PROT_READ must work: it needs only `read`");
    let bytes = read_mapping(pointer, len);
    sync_and_unmap(pointer, len);
    bytes
  })
  .await;

  assert_eq!(bytes, b"# basic\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_only_shared_mapping_of_a_mounted_file_reads_correctly() {
  // The case a language server or a compiler that mmaps its inputs relies on.
  let backend = Backend::start("content").await;
  let mount = Mount::new(&backend, "main").await;
  let path = mount.join("huge-line.txt");

  let (len, first, last) = on_fs(move || {
    let file = std::fs::File::open(&path).unwrap();
    let len = file.metadata().unwrap().len() as usize;
    let pointer =
      map(&file, len, libc::PROT_READ, libc::MAP_SHARED).expect("read-only MAP_SHARED must work");
    let bytes = read_mapping(pointer, len);
    let result = (bytes.len(), bytes[0], bytes[bytes.len() - 1]);
    sync_and_unmap(pointer, len);
    result
  })
  .await;

  // A 4 MiB file, so the mapping spans many pages and the kernel faults them in
  // through `read` rather than serving one cached page.
  assert_eq!(len, 4 * 1024 * 1024);
  assert_eq!((first, last), (b'x', b'x'));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_writable_shared_mapping_of_a_mounted_file_reaches_the_overlay() {
  // The measurement ADR 0006's amendment rests on, now made against GFS rather
  // than against the probe: a shared writable mapping of a base file works
  // *without* `FUSE_WRITEBACK_CACHE`, and the bytes land in the overlay rather
  // than anywhere near the pinned commit.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let path = mount.join("README.md");
  let read_back = path.clone();

  on_fs(move || {
    let file = std::fs::OpenOptions::new()
      .read(true)
      .write(true)
      .open(&path)
      .expect("a read-write open copies the blob up");
    let len = file.metadata().unwrap().len() as usize;
    let pointer = map(
      &file,
      len,
      libc::PROT_READ | libc::PROT_WRITE,
      libc::MAP_SHARED,
    )
    .expect("writable MAP_SHARED must work without the writeback cache");
    write_mapping(pointer, b"# EDITED");
    sync_and_unmap(pointer, len);
  })
  .await;

  let bytes = on_fs(move || std::fs::read(&read_back).unwrap()).await;
  assert_eq!(bytes, b"# EDITED", "the mapping's writes are visible");
  assert_eq!(
    mount.overlay.stats().entries,
    1,
    "exactly one path diverged from the base"
  );
}

// ---------------------------------------------------------------------------
// The write side, against a throwaway probe filesystem
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Observed {
  content: Arc<Mutex<Vec<u8>>>,
  writes: Arc<Mutex<u32>>,
  writeback_granted: Arc<Mutex<bool>>,
}

struct Probe {
  observed: Observed,
  request_writeback: bool,
}

impl Probe {
  fn attr(&self) -> FileAttr {
    let size = self.observed.content.lock().expect("probe content").len() as u64;
    FileAttr {
      ino: INodeNo(FILE_INO),
      size,
      blocks: size.div_ceil(512),
      atime: UNIX_EPOCH,
      mtime: UNIX_EPOCH,
      ctime: UNIX_EPOCH,
      crtime: UNIX_EPOCH,
      kind: FileType::RegularFile,
      perm: 0o644,
      nlink: 1,
      uid: gfs_mount::attr::Ownership::current().uid,
      gid: gfs_mount::attr::Ownership::current().gid,
      rdev: 0,
      blksize: 4096,
      flags: 0,
    }
  }

  fn root_attr(&self) -> FileAttr {
    let mut attr = self.attr();
    attr.ino = INodeNo(1);
    attr.kind = FileType::Directory;
    attr.perm = 0o755;
    attr.size = 0;
    attr.nlink = 2;
    attr
  }
}

impl Filesystem for Probe {
  fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
    if self.request_writeback {
      let granted = config
        .add_capabilities(InitFlags::FUSE_WRITEBACK_CACHE)
        .is_ok();
      *self
        .observed
        .writeback_granted
        .lock()
        .expect("writeback flag") = granted;
    }
    Ok(())
  }

  fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: fuser::ReplyEntry) {
    if parent.0 == 1 && name == FILE_NAME {
      reply.entry(&TTL, &self.attr(), fuser::Generation(0));
    } else {
      reply.error(fuser::Errno::ENOENT);
    }
  }

  fn getattr(
    &self,
    _req: &Request,
    ino: INodeNo,
    _fh: Option<FileHandle>,
    reply: fuser::ReplyAttr,
  ) {
    match ino.0 {
      1 => reply.attr(&TTL, &self.root_attr()),
      FILE_INO => reply.attr(&TTL, &self.attr()),
      _ => reply.error(fuser::Errno::ENOENT),
    }
  }

  fn setattr(
    &self,
    _req: &Request,
    _ino: INodeNo,
    _mode: Option<u32>,
    _uid: Option<u32>,
    _gid: Option<u32>,
    size: Option<u64>,
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
    if let Some(size) = size {
      self
        .observed
        .content
        .lock()
        .expect("probe content")
        .resize(size as usize, 0);
    }
    reply.attr(&TTL, &self.attr());
  }

  fn open(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: fuser::ReplyOpen) {
    // No `direct_io`: the page-cache path is the one that can support a shared
    // mapping at all, so requesting direct I/O would predetermine the answer.
    reply.opened(FileHandle(1), fuser::FopenFlags::empty());
  }

  fn read(
    &self,
    _req: &Request,
    _ino: INodeNo,
    _fh: FileHandle,
    offset: u64,
    size: u32,
    _flags: OpenFlags,
    _lock_owner: Option<fuser::LockOwner>,
    reply: fuser::ReplyData,
  ) {
    let content = self.observed.content.lock().expect("probe content");
    let start = (offset as usize).min(content.len());
    let end = start.saturating_add(size as usize).min(content.len());
    reply.data(&content[start..end]);
  }

  fn write(
    &self,
    _req: &Request,
    _ino: INodeNo,
    _fh: FileHandle,
    offset: u64,
    data: &[u8],
    _write_flags: fuser::WriteFlags,
    _flags: OpenFlags,
    _lock_owner: Option<fuser::LockOwner>,
    reply: fuser::ReplyWrite,
  ) {
    let mut content = self.observed.content.lock().expect("probe content");
    let end = offset as usize + data.len();
    if content.len() < end {
      content.resize(end, 0);
    }
    content[offset as usize..end].copy_from_slice(data);
    *self.observed.writes.lock().expect("probe writes") += 1;
    reply.written(data.len() as u32);
  }

  fn flush(
    &self,
    _req: &Request,
    _ino: INodeNo,
    _fh: FileHandle,
    _lock_owner: fuser::LockOwner,
    reply: fuser::ReplyEmpty,
  ) {
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
}

struct ProbeMount {
  path: std::path::PathBuf,
  observed: Observed,
  session: Option<fuser::BackgroundSession>,
  _tmp: tempfile::TempDir,
}

impl ProbeMount {
  fn start(request_writeback: bool) -> ProbeMount {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("probe");
    std::fs::create_dir(&path).unwrap();

    let observed = Observed {
      content: Arc::new(Mutex::new(CONTENT.to_vec())),
      ..Observed::default()
    };
    let mut config = fuser::Config::default();
    // Writable, unlike the GFS mount: that is the whole point of the probe.
    config.mount_options = vec![
      fuser::MountOption::FSName("gfs-mmap-probe".to_owned()),
      fuser::MountOption::RW,
    ];
    config.n_threads = Some(2);

    let session = fuser::spawn_mount(
      Probe {
        observed: observed.clone(),
        request_writeback,
      },
      &path,
      &config,
    )
    .expect("the probe filesystem must mount");

    ProbeMount {
      path,
      observed,
      session: Some(session),
      _tmp: tmp,
    }
  }
}

impl Drop for ProbeMount {
  fn drop(&mut self) {
    if let Some(session) = self.session.take() {
      let _ = session.umount_and_join();
    }
  }
}

/// What one probe run observed.
struct MmapOutcome {
  writeback_granted: bool,
  mmap_errno: Option<i32>,
  content_after: Vec<u8>,
  writes: u32,
}

fn probe_shared_writable_mmap(request_writeback: bool) -> MmapOutcome {
  let probe = ProbeMount::start(request_writeback);
  let file_path = probe.path.join(FILE_NAME);

  let file = std::fs::OpenOptions::new()
    .read(true)
    .write(true)
    .open(&file_path)
    .expect("the probe accepts a read-write open");
  let len = CONTENT.len();

  let mmap_errno = match map(
    &file,
    len,
    libc::PROT_READ | libc::PROT_WRITE,
    libc::MAP_SHARED,
  ) {
    Ok(pointer) => {
      write_mapping(pointer, b"WRITTEN");
      sync_and_unmap(pointer, len);
      None
    }
    Err(errno) => Some(errno),
  };
  drop(file);

  // Read out before `probe` is dropped: dropping it unmounts, and a `MutexGuard`
  // living into that drop would borrow the thing being torn down.
  let outcome = MmapOutcome {
    writeback_granted: *probe.observed.writeback_granted.lock().unwrap(),
    mmap_errno,
    content_after: probe.observed.content.lock().unwrap().clone(),
    writes: *probe.observed.writes.lock().unwrap(),
  };
  outcome
}

#[test]
fn writable_shared_mmap_without_the_writeback_cache() {
  let outcome = probe_shared_writable_mmap(false);
  println!(
    "MAP_SHARED|PROT_WRITE, no FUSE_WRITEBACK_CACHE: mmap errno {:?}, {} write requests, content {:?}",
    outcome.mmap_errno,
    outcome.writes,
    String::from_utf8_lossy(&outcome.content_after)
  );
  assert!(!outcome.writeback_granted);

  // The measurement, not an expectation. Whatever this kernel does is what M3.2
  // has to design against; the assertion only pins the *observation* so a change
  // in kernel behaviour shows up as a failing test rather than as a surprise
  // during M3.
  match outcome.mmap_errno {
    None => assert!(
      outcome.content_after.starts_with(b"WRITTEN"),
      "the mapping was accepted, so the dirtied bytes must reach the filesystem"
    ),
    Some(errno) => panic!(
      "this kernel refused a shared writable mapping without the writeback \
       cache (errno {errno}); M3.2 must either enable FUSE_WRITEBACK_CACHE or \
       declare writable MAP_SHARED unsupported"
    ),
  }
}

#[test]
fn writable_shared_mmap_with_the_writeback_cache() {
  let outcome = probe_shared_writable_mmap(true);
  println!(
    "MAP_SHARED|PROT_WRITE, FUSE_WRITEBACK_CACHE granted={}: mmap errno {:?}, {} write requests, content {:?}",
    outcome.writeback_granted,
    outcome.mmap_errno,
    outcome.writes,
    String::from_utf8_lossy(&outcome.content_after)
  );
  assert!(
    outcome.writeback_granted,
    "this kernel does not offer FUSE_WRITEBACK_CACHE at all"
  );
  assert_eq!(outcome.mmap_errno, None);
  assert!(outcome.content_after.starts_with(b"WRITTEN"));
}
