//! The projected object database: one read-only mount per repository per host.
//!
//! ADR 0009's workspace has a real `.git` on local disk whose
//! `objects/info/alternates` points here. This module is the client half of that
//! projection: a manifest of the gateway's `objects/` tree, a 64 KiB block cache
//! under it, and a small FUSE filesystem serving the two to every workspace of
//! the repository on this host.
//!
//! # Why blocks, and why this size
//!
//! Pinning the store's metadata per node (632 MiB for linux) would trade away
//! the fast mount; fetching whole files is worse. Pack access is sparse and
//! random — a binary search in the `.idx`, a delta chain walked through the
//! `.pack` — and `spikes/reports/m05b-git-projection.md` measured what chunk
//! sizes do to that: 8 MiB amplifies `git log --oneline -20` by 68×, while
//! 64 KiB costs 1.0–1.4× and matches FUSE's maximum read, so one block serves
//! one request. [`gfs_types::limits::ODB_BLOCK_BYTES`] is that number, shared
//! with the server's accounting.
//!
//! # Why per repository rather than per mount
//!
//! Every file here is immutable by *name* — a pack's name is its own checksum, a
//! loose object's path is its digest — so two mounts of the same repository can
//! never disagree about a block's content, only about whether it is present yet.
//! That is ADR 0008's blob-cache argument applied to the object store, and it is
//! also why blocks are keyed by `(file path, block index)`: staleness is
//! impossible by construction, not by discipline. A file can only *stop
//! existing* (a gateway repack), which surfaces as `ESTALE` and a message to
//! remount, per ADR 0009's retention policy.
//!
//! # What is deliberately not here yet
//!
//! Eviction. The store on disk is sparse — blocks land at their true offsets and
//! unread ranges occupy no disk — and the residency-budget policy (evict and
//! re-fetch rather than refuse) needs the re-fetch/unique ratio this module
//! already counts. Per-job attribution of odb traffic is likewise not possible
//! at this layer, because the projection is shared; the per-job budget guards
//! the working tree, where per-job identity exists.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use fuser::{
  Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner,
  OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen, Request,
};
use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::{limits, RepositoryId};

const BLOCK: u64 = limits::ODB_BLOCK_BYTES;

/// One file the gateway's manifest lists.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct OdbFile {
  pub path: String,
  pub size: u64,
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

/// The HTTP half: manifest and ranges, bearer-authenticated.
///
/// Separate from [`crate::client::SnapshotClient`] because that client is bound
/// to a mount's pinned commit and capability, while the projection is
/// per-repository and outlives any one mount.
#[derive(Clone)]
pub struct OdbClient {
  http: hyper_util::client::legacy::Client<
    hyper_util::client::legacy::connect::HttpConnector,
    http_body_util::Empty<bytes::Bytes>,
  >,
  endpoint: String,
  token: String,
  repository_id: RepositoryId,
}

impl std::fmt::Debug for OdbClient {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("OdbClient")
      .field("endpoint", &self.endpoint)
      .field("repository_id", &self.repository_id)
      .finish_non_exhaustive()
  }
}

impl OdbClient {
  pub fn new(http_endpoint: &str, token: &str, repository_id: RepositoryId) -> Self {
    OdbClient {
      http: hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http(),
      endpoint: http_endpoint.trim_end_matches('/').to_owned(),
      token: token.to_owned(),
      repository_id,
    }
  }

  async fn get(&self, url: &str, range: Option<(u64, u64)>) -> Result<Vec<u8>, GfsError> {
    use http_body_util::BodyExt;
    let uri: http::Uri = url
      .parse()
      .map_err(|_| GfsError::invalid("invalid odb URL"))?;
    let mut builder = http::Request::builder().uri(uri);
    if !self.token.is_empty() {
      builder = builder.header(
        http::header::AUTHORIZATION,
        format!("Bearer {}", self.token),
      );
    }
    if let Some((start, end)) = range {
      builder = builder.header(http::header::RANGE, format!("bytes={start}-{end}"));
    }
    let request = builder
      .body(http_body_util::Empty::new())
      .map_err(|e| GfsError::internal(format!("building an odb request: {e}")))?;
    let response = self.http.request(request).await.map_err(|e| {
      GfsError::new(
        ErrorCode::Unavailable,
        format!("odb request did not complete: {e}"),
      )
    })?;
    let status = response.status();
    let body = response
      .into_body()
      .collect()
      .await
      .map_err(|e| {
        GfsError::new(
          ErrorCode::Unavailable,
          format!("odb body did not complete: {e}"),
        )
      })?
      .to_bytes();
    if !status.is_success() {
      // FAILED_PRECONDITION from the server means the file was repacked away;
      // the caller maps that to ESTALE, ADR 0009's "missing, never wrong".
      return Err(crate::client::http_error(status, &body));
    }
    Ok(body.to_vec())
  }

  pub async fn manifest(&self) -> Result<Vec<OdbFile>, GfsError> {
    let url = format!(
      "{}/v1/repos/{}/odb",
      self.endpoint,
      self.repository_id.as_str()
    );
    let body = self.get(&url, None).await?;
    serde_json::from_slice(&body)
      .map_err(|e| GfsError::internal(format!("unparseable odb manifest: {e}")))
  }

  pub async fn read_range(&self, path: &str, start: u64, end: u64) -> Result<Vec<u8>, GfsError> {
    let url = format!(
      "{}/v1/repos/{}/odb/{path}",
      self.endpoint,
      self.repository_id.as_str()
    );
    self.get(&url, Some((start, end))).await
  }

  /// The shipped index for a pinned commit (ADR 0009's workspace seed).
  pub async fn commit_index(&self, commit: &gfs_types::ObjectId) -> Result<Vec<u8>, GfsError> {
    let url = format!(
      "{}/v1/repos/{}/index?commit={}",
      self.endpoint,
      self.repository_id.as_str(),
      commit.to_qualified()
    );
    self.get(&url, None).await
  }
}

// ---------------------------------------------------------------------------
// The block store
// ---------------------------------------------------------------------------

/// What the store has done, for telemetry and the residency policy to come.
#[derive(Clone, Copy, Default, Debug, serde::Serialize, serde::Deserialize)]
pub struct OdbStats {
  pub blocks_fetched: u64,
  pub bytes_fetched: u64,
  pub block_hits: u64,
}

struct FileState {
  size: u64,
  /// One bit per block, set once the block's true bytes are at their offset in
  /// the local sparse file. In-memory only: on host restart the sparse file is
  /// discarded rather than trusted, because a bitmap that lied would serve
  /// zeros as pack bytes.
  present: Vec<u64>,
  local: PathBuf,
}

impl FileState {
  fn has(&self, block: u64) -> bool {
    let (word, bit) = ((block / 64) as usize, block % 64);
    self.present.get(word).is_some_and(|w| w & (1 << bit) != 0)
  }
  fn set(&mut self, block: u64) {
    let (word, bit) = ((block / 64) as usize, block % 64);
    if let Some(w) = self.present.get_mut(word) {
      *w |= 1 << bit;
    }
  }
}

/// The per-repository store: manifest tree plus locally cached blocks.
pub struct BlockStore {
  client: OdbClient,
  root: PathBuf,
  files: Mutex<HashMap<String, FileState>>,
  blocks_fetched: AtomicU64,
  bytes_fetched: AtomicU64,
  block_hits: AtomicU64,
}

impl std::fmt::Debug for BlockStore {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("BlockStore")
      .field("root", &self.root)
      .finish_non_exhaustive()
  }
}

impl BlockStore {
  /// Open the store and take the manifest.
  ///
  /// The local directory is cleared first: block presence is tracked in memory
  /// only, and sparse files from a previous run would otherwise be
  /// indistinguishable from files full of unfetched zeros.
  pub async fn open(client: OdbClient, root: PathBuf) -> Result<Self, GfsError> {
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root)
      .map_err(|e| GfsError::internal(format!("creating the odb store: {e}")))?;
    let manifest = client.manifest().await?;
    let mut files = HashMap::new();
    for f in &manifest {
      let blocks = f.size.div_ceil(BLOCK);
      let words = (blocks as usize).div_ceil(64);
      let local = root.join(&f.path);
      if let Some(parent) = local.parent() {
        std::fs::create_dir_all(parent)
          .map_err(|e| GfsError::internal(format!("creating the odb store: {e}")))?;
      }
      let file = std::fs::File::create(&local)
        .map_err(|e| GfsError::internal(format!("creating a sparse block file: {e}")))?;
      file
        .set_len(f.size)
        .map_err(|e| GfsError::internal(format!("sizing a sparse block file: {e}")))?;
      files.insert(
        f.path.clone(),
        FileState {
          size: f.size,
          present: vec![0; words],
          local,
        },
      );
    }
    Ok(BlockStore {
      client,
      root,
      files: Mutex::new(files),
      blocks_fetched: AtomicU64::new(0),
      bytes_fetched: AtomicU64::new(0),
      block_hits: AtomicU64::new(0),
    })
  }

  pub fn stats(&self) -> OdbStats {
    OdbStats {
      blocks_fetched: self.blocks_fetched.load(Ordering::Relaxed),
      bytes_fetched: self.bytes_fetched.load(Ordering::Relaxed),
      block_hits: self.block_hits.load(Ordering::Relaxed),
    }
  }

  /// The manifest's view of the tree, for the filesystem to build inodes from.
  pub fn listing(&self) -> Vec<(String, u64)> {
    self
      .files
      .lock()
      .expect("odb files")
      .iter()
      .map(|(path, state)| (path.clone(), state.size))
      .collect()
  }

  /// Read `size` bytes at `offset`, fetching any absent blocks first.
  ///
  /// Fetches are one HTTP range per *contiguous run* of absent blocks, so a
  /// sequential scan costs one round trip per run rather than one per block,
  /// while a sparse binary search still fetches only what it touches.
  pub async fn read(&self, path: &str, offset: u64, size: u32) -> Result<Vec<u8>, GfsError> {
    let (local, total) = {
      let files = self.files.lock().expect("odb files");
      let state = files.get(path).ok_or_else(|| {
        GfsError::new(
          ErrorCode::FailedPrecondition,
          "the object store was repacked; remount",
        )
      })?;
      (state.local.clone(), state.size)
    };
    if offset >= total {
      return Ok(Vec::new());
    }
    let end = (offset + size as u64).min(total);
    let first = offset / BLOCK;
    let last = (end - 1) / BLOCK;

    // Collect the absent runs under the lock, fetch them outside it.
    let mut runs: Vec<(u64, u64)> = Vec::new();
    {
      let files = self.files.lock().expect("odb files");
      let state = files.get(path).expect("checked above");
      let mut run_start: Option<u64> = None;
      for block in first..=last {
        if state.has(block) {
          if let Some(s) = run_start.take() {
            runs.push((s, block - 1));
          }
          self.block_hits.fetch_add(1, Ordering::Relaxed);
        } else if run_start.is_none() {
          run_start = Some(block);
        }
      }
      if let Some(s) = run_start {
        runs.push((s, last));
      }
    }

    for (from, to) in runs {
      let byte_start = from * BLOCK;
      let byte_end = ((to + 1) * BLOCK).min(total) - 1;
      let bytes = self.client.read_range(path, byte_start, byte_end).await?;
      if bytes.len() as u64 != byte_end - byte_start + 1 {
        return Err(GfsError::internal(format!(
          "short odb range: wanted {} got {}",
          byte_end - byte_start + 1,
          bytes.len()
        )));
      }
      // Write at the true offset, then mark. Two fetches of one block race
      // benignly: both write identical bytes, because the name is the content.
      let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(&local)
        .map_err(|e| GfsError::internal(format!("opening the block file: {e}")))?;
      file
        .seek(SeekFrom::Start(byte_start))
        .and_then(|_| file.write_all(&bytes))
        .map_err(|e| GfsError::internal(format!("writing blocks: {e}")))?;
      let mut files = self.files.lock().expect("odb files");
      if let Some(state) = files.get_mut(path) {
        for block in from..=to {
          state.set(block);
        }
      }
      self
        .blocks_fetched
        .fetch_add(to - from + 1, Ordering::Relaxed);
      self
        .bytes_fetched
        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
    }

    let mut buf = vec![0u8; (end - offset) as usize];
    let mut file = std::fs::File::open(&local)
      .map_err(|e| GfsError::internal(format!("opening the block file: {e}")))?;
    file
      .seek(SeekFrom::Start(offset))
      .and_then(|_| file.read_exact(&mut buf))
      .map_err(|e| GfsError::internal(format!("reading cached blocks: {e}")))?;
    Ok(buf)
  }
}

// ---------------------------------------------------------------------------
// The filesystem
// ---------------------------------------------------------------------------

const TTL: Duration = Duration::from_secs(3600);

#[derive(Debug)]
enum Node {
  Dir { children: Vec<(String, u64)> },
  File { path: String, size: u64 },
}

/// The projection: a read-only tree of exactly the manifest's files.
pub struct OdbFs {
  store: Arc<BlockStore>,
  nodes: Vec<Node>,
  by_name: HashMap<(u64, String), u64>,
  runtime: tokio::runtime::Handle,
}

impl OdbFs {
  pub fn new(store: Arc<BlockStore>, runtime: tokio::runtime::Handle) -> Self {
    // Inode 0 unused, 1 is the root. Directories are interned as paths appear;
    // the manifest is small (dozens of entries) so this is all cheap.
    let mut nodes = vec![
      Node::Dir { children: vec![] }, // 0: unused
      Node::Dir { children: vec![] }, // 1: root
    ];
    let mut dirs: HashMap<String, u64> = HashMap::new();
    dirs.insert(String::new(), 1);
    let mut by_name = HashMap::new();

    let mut listing = store.listing();
    listing.sort();
    for (path, size) in listing {
      let mut parent = 1u64;
      let mut walked = String::new();
      let components: Vec<&str> = path.split('/').collect();
      for dir in &components[..components.len() - 1] {
        if !walked.is_empty() {
          walked.push('/');
        }
        walked.push_str(dir);
        parent = match dirs.get(&walked) {
          Some(ino) => *ino,
          None => {
            let ino = nodes.len() as u64;
            nodes.push(Node::Dir { children: vec![] });
            if let Node::Dir { children } = &mut nodes[parent as usize] {
              children.push(((*dir).to_owned(), ino));
            }
            by_name.insert((parent, (*dir).to_owned()), ino);
            dirs.insert(walked.clone(), ino);
            ino
          }
        };
      }
      let name = components[components.len() - 1].to_owned();
      let ino = nodes.len() as u64;
      nodes.push(Node::File {
        path: path.clone(),
        size,
      });
      if let Node::Dir { children } = &mut nodes[parent as usize] {
        children.push((name.clone(), ino));
      }
      by_name.insert((parent, name), ino);
    }
    OdbFs {
      store,
      nodes,
      by_name,
      runtime,
    }
  }

  fn attr(&self, ino: u64) -> Option<FileAttr> {
    let node = self.nodes.get(ino as usize)?;
    let owner = crate::attr::Ownership::current();
    // A fixed timestamp: nothing reads a pack's mtime for correctness, and a
    // stable value keeps two mounts of one store identical.
    let t = UNIX_EPOCH + Duration::from_secs(1);
    let (kind, size, perm) = match node {
      Node::Dir { .. } => (FileType::Directory, 0, 0o555),
      Node::File { size, .. } => (FileType::RegularFile, *size, 0o444),
    };
    Some(FileAttr {
      ino: INodeNo(ino),
      size,
      blocks: size.div_ceil(512),
      atime: t,
      mtime: t,
      ctime: t,
      crtime: t,
      kind,
      perm,
      nlink: 1,
      uid: owner.uid,
      gid: owner.gid,
      rdev: 0,
      blksize: BLOCK as u32,
      flags: 0,
    })
  }
}

impl std::fmt::Debug for OdbFs {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("OdbFs").finish_non_exhaustive()
  }
}

impl Filesystem for OdbFs {
  fn lookup(&self, _req: &Request, parent: INodeNo, name: &std::ffi::OsStr, reply: ReplyEntry) {
    let key = (parent.0, name.to_string_lossy().into_owned());
    match self.by_name.get(&key).and_then(|ino| self.attr(*ino)) {
      Some(attr) => reply.entry(&TTL, &attr, Generation(0)),
      None => reply.error(Errno::ENOENT),
    }
  }

  fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
    match self.attr(ino.0) {
      Some(attr) => reply.attr(&TTL, &attr),
      None => reply.error(Errno::ENOENT),
    }
  }

  fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
    if flags.acc_mode() != fuser::OpenAccMode::O_RDONLY {
      return reply.error(Errno::EROFS);
    }
    match self.nodes.get(ino.0 as usize) {
      Some(Node::File { .. }) => reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE),
      Some(Node::Dir { .. }) => reply.error(Errno::EISDIR),
      None => reply.error(Errno::ENOENT),
    }
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
    let Some(Node::File { path, .. }) = self.nodes.get(ino.0 as usize) else {
      return reply.error(Errno::EBADF);
    };
    let path = path.clone();
    let store = Arc::clone(&self.store);
    // Handed to the runtime rather than awaited on the FUSE thread: a block
    // fetch is a network wait, and one blocking callback serializes the mount
    // (M0.2's measured rule).
    self.runtime.spawn(async move {
      match store.read(&path, offset, size).await {
        Ok(bytes) => reply.data(&bytes),
        Err(e) => {
          // "Repacked away" is the one expected failure, and ESTALE is its
          // errno: the name is gone, not wrong. Everything else is EIO.
          let errno = if e.code == ErrorCode::FailedPrecondition {
            Errno::ESTALE
          } else {
            Errno::EIO
          };
          tracing::warn!(path, offset, "odb read failed: {e}");
          reply.error(errno);
        }
      }
    });
  }

  fn readdir(
    &self,
    _req: &Request,
    ino: INodeNo,
    _fh: FileHandle,
    offset: u64,
    mut reply: ReplyDirectory,
  ) {
    let Some(Node::Dir { children }) = self.nodes.get(ino.0 as usize) else {
      return reply.error(Errno::ENOTDIR);
    };
    let mut listing: Vec<(u64, FileType, String)> = vec![
      (ino.0, FileType::Directory, ".".into()),
      (ino.0, FileType::Directory, "..".into()),
    ];
    for (name, child) in children {
      let kind = match self.nodes.get(*child as usize) {
        Some(Node::Dir { .. }) => FileType::Directory,
        _ => FileType::RegularFile,
      };
      listing.push((*child, kind, name.clone()));
    }
    for (i, (child, kind, name)) in listing.into_iter().enumerate().skip(offset as usize) {
      if reply.add(INodeNo(child), (i + 1) as u64, kind, name) {
        break;
      }
    }
    reply.ok();
  }
}

// ---------------------------------------------------------------------------
// The projection handle
// ---------------------------------------------------------------------------

/// One repository's projection: the store, the mount, and its lifetime.
pub struct OdbProjection {
  pub store: Arc<BlockStore>,
  /// A handle for the workspace seed: fetching the shipped index for a pinned
  /// commit goes to the same endpoint the blocks come from.
  pub client: OdbClient,
  /// Where the projection is mounted; what a workspace's alternates points at.
  pub mountpoint: PathBuf,
  /// Dropping the session unmounts, so this handle's lifetime *is* the mount's.
  session: Mutex<Option<fuser::BackgroundSession>>,
}

impl std::fmt::Debug for OdbProjection {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("OdbProjection")
      .field("mountpoint", &self.mountpoint)
      .finish_non_exhaustive()
  }
}

impl OdbProjection {
  /// Mount the projection for one repository.
  ///
  /// `root` holds both the block store (`blocks/`) and the mountpoint
  /// (`mnt/`), so one directory per repository owns everything the projection
  /// touches on disk.
  pub async fn mount(client: OdbClient, root: &Path) -> Result<Arc<Self>, GfsError> {
    let mountpoint = root.join("mnt");
    // A previous process may have died with the mount half-attached; unmounting
    // a directory that exists but is not mounted is harmless and quiet with -z.
    if mountpoint.exists() {
      let _ = std::process::Command::new("fusermount3")
        .args(["-uzq"])
        .arg(&mountpoint)
        .output();
    }
    std::fs::create_dir_all(&mountpoint)
      .map_err(|e| GfsError::internal(format!("creating the odb mountpoint: {e}")))?;

    let seed_client = client.clone();
    let store = Arc::new(BlockStore::open(client, root.join("blocks")).await?);
    let fs = OdbFs::new(Arc::clone(&store), tokio::runtime::Handle::current());

    let mut config = fuser::Config::default();
    config.mount_options = vec![
      fuser::MountOption::FSName("gfs-odb".into()),
      fuser::MountOption::RO,
      fuser::MountOption::NoSuid,
      fuser::MountOption::NoDev,
      fuser::MountOption::NoAtime,
    ];
    config.acl = fuser::SessionACL::Owner;
    // Git reads packs from several threads; more than one event loop keeps a
    // slow block fetch from serializing its neighbours.
    config.n_threads = Some(4);
    config.clone_fd = true;
    let session = fuser::spawn_mount(fs, &mountpoint, &config)
      .map_err(|e| GfsError::internal(format!("mounting the odb projection: {e}")))?;

    Ok(Arc::new(OdbProjection {
      store,
      client: seed_client,
      mountpoint,
      session: Mutex::new(Some(session)),
    }))
  }

  /// Unmount explicitly. Also happens on drop, but an unmount that can fail
  /// loudly belongs on the shutdown path, not in a destructor.
  pub fn unmount(&self) {
    if let Some(session) = self.session.lock().expect("odb session").take() {
      let _ = session.umount_and_join();
    }
  }
}

impl Drop for OdbProjection {
  fn drop(&mut self) {
    self.unmount();
  }
}
