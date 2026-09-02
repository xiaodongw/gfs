//! The projected object database: one block store per repository per host.
//!
//! ADR 0009's workspace has a real `.git` on local disk whose
//! `objects/info/alternates` points at a projection of the gateway's `objects/`
//! tree. This module is the client half of that projection: a manifest of the
//! gateway's tree and a 64 KiB block cache under it. ADR 0011 moved the serving
//! half *into the workspace filesystem* — the projection appears at
//! `.git/gfs/objects/` inside the one workspace mount ([`crate::passthrough`]) —
//! so there is no separate FUSE session here any more, only the store that every
//! workspace of the repository shares.
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
//! # The residency budget
//!
//! ADR 0009's second budget: bytes *held* rather than bytes fetched — evict and
//! re-fetch, never refuse. Off by default (`limit = 0`), because its purpose is
//! to let a small disk work a monorepo slowly rather than not at all, and a
//! host with disk to spare should not pay re-fetches for a limit it does not
//! need.
//!
//! The policy is SLRU (probation/protected) rather than plain LRU, because the
//! ADR requires scan resistance: a `blame` streaming hundreds of MiB of pack
//! must not flush the blocks a build is re-reading. A block enters probation on
//! first fetch and is promoted to protected only by a re-read while resident;
//! victims come from probation first. A one-pass scan never promotes anything,
//! so the most it can flush is probation — the re-read working set survives a
//! scan of any size. Protected is capped at half the limit and demotes back to
//! probation, so a working set that drifts still turns over.
//!
//! Residency mode's risk is not a stall but an unbounded re-fetch multiplier
//! (a cyclic scan larger than the cache re-fetches forever), which presents as
//! a slow job rather than an error. So re-fetched blocks and bytes are counted
//! apart from first-time fetches — the refetch/unique ratio in [`OdbStats`] is
//! the thrash signal, surfaced in `gfs status`.
//!
//! Blocks of an in-flight read are pinned. Presence-marking and eviction share
//! one lock, and serving a read from the sparse file is only sound while its
//! blocks cannot be evicted underneath it — a punched hole reads as zeros,
//! which is the "bitmap that lied" hazard in eviction clothing.
//!
//! # Per-job attribution
//!
//! The store's counters are per repository — the projection is shared, so
//! per-job identity does not exist at the store. Attribution lives one layer
//! up: every workspace serves the shared store through its own mount with its
//! own [`ViewCounters`], so *which mount was read through* is the job identity.
//! That is unforgeable and free, where a pid lookup at read time is racy and
//! was rejected by the spike. The blocks stay shared; only the counting is per
//! view.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

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
}

// ---------------------------------------------------------------------------
// The block store
// ---------------------------------------------------------------------------

/// What the store has done, for telemetry and `gfs status`.
#[derive(Clone, Copy, Default, Debug, serde::Serialize, serde::Deserialize)]
pub struct OdbStats {
  pub blocks_fetched: u64,
  pub bytes_fetched: u64,
  pub block_hits: u64,
  /// Bytes currently held on local disk. Bounded by the residency limit when
  /// one is set, give or take the blocks an in-flight read has pinned.
  #[serde(default)]
  pub resident_bytes: u64,
  #[serde(default)]
  pub residency_limit_bytes: u64,
  #[serde(default)]
  pub evicted_blocks: u64,
  /// Fetches of blocks the store had held before and evicted. The ratio of
  /// `refetched_bytes` to first-time bytes is residency mode's thrash signal:
  /// its failure mode is not an error but a job slowed by re-fetching.
  #[serde(default)]
  pub refetched_blocks: u64,
  #[serde(default)]
  pub refetched_bytes: u64,
}

/// What one read cost, for per-view attribution.
///
/// The store's own counters are per repository — the projection is shared — so
/// per-job attribution cannot live there. It lives one layer up instead: each
/// workspace serves the store through its own [`ViewCounters`], every read
/// returns its cost, and the view accumulates. Identity comes from *which
/// mount was read through*, which is unforgeable, instead of from a racy pid
/// lookup (rejected in `spikes/reports/m05b-git-projection.md`).
#[derive(Clone, Copy, Default, Debug)]
pub struct ReadCost {
  pub blocks_fetched: u64,
  pub bytes_fetched: u64,
  pub block_hits: u64,
}

/// One view's accumulated attribution, for `gfs status` and telemetry.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OdbViewStats {
  pub blocks_fetched: u64,
  pub bytes_fetched: u64,
  pub block_hits: u64,
}

/// One workspace's share of the shared store's traffic.
#[derive(Debug, Default)]
pub struct ViewCounters {
  blocks_fetched: AtomicU64,
  bytes_fetched: AtomicU64,
  block_hits: AtomicU64,
}

impl ViewCounters {
  pub fn add(&self, cost: &ReadCost) {
    self
      .blocks_fetched
      .fetch_add(cost.blocks_fetched, Ordering::Relaxed);
    self
      .bytes_fetched
      .fetch_add(cost.bytes_fetched, Ordering::Relaxed);
    self
      .block_hits
      .fetch_add(cost.block_hits, Ordering::Relaxed);
  }

  pub fn snapshot(&self) -> OdbViewStats {
    OdbViewStats {
      blocks_fetched: self.blocks_fetched.load(Ordering::Relaxed),
      bytes_fetched: self.bytes_fetched.load(Ordering::Relaxed),
      block_hits: self.block_hits.load(Ordering::Relaxed),
    }
  }
}

struct FileState {
  /// Dense index into the manifest, so residency bookkeeping can key blocks by
  /// `(u32, u64)` instead of cloning path strings per block.
  id: u32,
  size: u64,
  /// One bit per block, set once the block's true bytes are at their offset in
  /// the local sparse file. In-memory only: on host restart the sparse file is
  /// discarded rather than trusted, because a bitmap that lied would serve
  /// zeros as pack bytes.
  present: Vec<u64>,
  /// One bit per block, set on first fetch and never cleared: it is what tells
  /// a re-fetch after eviction apart from a first fetch.
  fetched_ever: Vec<u64>,
  local: PathBuf,
}

fn bit_get(words: &[u64], block: u64) -> bool {
  let (word, bit) = ((block / 64) as usize, block % 64);
  words.get(word).is_some_and(|w| w & (1 << bit) != 0)
}

fn bit_set(words: &mut [u64], block: u64) {
  let (word, bit) = ((block / 64) as usize, block % 64);
  if let Some(w) = words.get_mut(word) {
    *w |= 1 << bit;
  }
}

fn bit_clear(words: &mut [u64], block: u64) {
  let (word, bit) = ((block / 64) as usize, block % 64);
  if let Some(w) = words.get_mut(word) {
    *w &= !(1 << bit);
  }
}

impl FileState {
  fn has(&self, block: u64) -> bool {
    bit_get(&self.present, block)
  }
  fn set(&mut self, block: u64) {
    bit_set(&mut self.present, block);
  }
  /// A block's true byte length: the last block of a file is usually partial.
  fn block_len(&self, block: u64) -> u64 {
    (self.size - block * BLOCK).min(BLOCK)
  }
}

/// One resident block, keyed by `(file id, block index)`.
type BlockKey = (u32, u64);

#[derive(Clone, Copy)]
struct BlockEntry {
  seq: u64,
  len: u64,
  protected: bool,
}

/// The SLRU bookkeeping. All of it lives under the store's one lock, and
/// `limit == 0` means every method is a no-op.
#[derive(Default)]
struct Residency {
  limit: u64,
  /// Monotonic recency counter; both queues are ordered by it.
  seq: u64,
  resident_bytes: u64,
  protected_bytes: u64,
  entries: HashMap<BlockKey, BlockEntry>,
  probation: std::collections::BTreeMap<u64, BlockKey>,
  protected: std::collections::BTreeMap<u64, BlockKey>,
  /// Blocks an in-flight read holds. A pinned block is never a victim.
  pins: HashMap<BlockKey, u32>,
}

impl Residency {
  fn next_seq(&mut self) -> u64 {
    self.seq += 1;
    self.seq
  }

  /// A block just became present: it enters probation.
  fn admit(&mut self, key: BlockKey, len: u64) {
    if self.limit == 0 {
      return;
    }
    let seq = self.next_seq();
    if let Some(old) = self.entries.insert(
      key,
      BlockEntry {
        seq,
        len,
        protected: false,
      },
    ) {
      // Should not happen -- admit is called on a presence transition -- but a
      // stale entry must not double-count.
      self.remove_queued(&old);
      self.resident_bytes -= old.len;
    }
    self.probation.insert(seq, key);
    self.resident_bytes += len;
  }

  /// A resident block was read again: promote it to protected.
  fn touch(&mut self, key: BlockKey) {
    if self.limit == 0 {
      return;
    }
    let Some(mut entry) = self.entries.get(&key).copied() else {
      return;
    };
    self.remove_queued(&entry);
    if !entry.protected {
      entry.protected = true;
      self.protected_bytes += entry.len;
    }
    entry.seq = self.next_seq();
    self.protected.insert(entry.seq, key);
    self.entries.insert(key, entry);
    // Protected is capped at half the limit so a shifted working set turns
    // over; the overflow demotes to probation's MRU end, not to the door.
    while self.protected_bytes > self.limit / 2 {
      let Some((&seq, &victim)) = self.protected.iter().next() else {
        break;
      };
      self.protected.remove(&seq);
      let Some(mut demoted) = self.entries.get(&victim).copied() else {
        continue;
      };
      demoted.protected = false;
      demoted.seq = self.next_seq();
      self.protected_bytes -= demoted.len;
      self.probation.insert(demoted.seq, victim);
      self.entries.insert(victim, demoted);
    }
  }

  fn remove_queued(&mut self, entry: &BlockEntry) {
    if entry.protected {
      self.protected.remove(&entry.seq);
    } else {
      self.probation.remove(&entry.seq);
    }
  }

  fn pin(&mut self, key: BlockKey) {
    *self.pins.entry(key).or_insert(0) += 1;
  }

  fn unpin(&mut self, key: BlockKey) {
    if let Some(count) = self.pins.get_mut(&key) {
      *count -= 1;
      if *count == 0 {
        self.pins.remove(&key);
      }
    }
  }

  /// The next victim: probation LRU first, then protected LRU, never a pinned
  /// block. `None` when everything left is pinned — the store then simply
  /// stays over the limit until the pins release, which is the same answer the
  /// blob cache gives for a fully pinned quota.
  fn victim(&self) -> Option<BlockKey> {
    self
      .probation
      .values()
      .chain(self.protected.values())
      .find(|key| !self.pins.contains_key(*key))
      .copied()
  }

  fn forget(&mut self, key: BlockKey) -> Option<BlockEntry> {
    let entry = self.entries.remove(&key)?;
    self.remove_queued(&entry);
    self.resident_bytes -= entry.len;
    if entry.protected {
      self.protected_bytes -= entry.len;
    }
    Some(entry)
  }

  fn over_limit(&self) -> bool {
    self.limit > 0 && self.resident_bytes > self.limit
  }
}

/// Everything under the store's one lock: the manifest's files and the
/// residency bookkeeping. One lock rather than two because eviction reads and
/// writes both — a victim is chosen from the residency queues and cleared from
/// its file's presence bitmap in the same critical section.
struct StoreState {
  files: HashMap<String, FileState>,
  /// `FileState::id` back to the manifest path, for eviction to find the file.
  paths: Vec<String>,
  residency: Residency,
}

/// The per-repository store: manifest tree plus locally cached blocks.
pub struct BlockStore {
  client: OdbClient,
  root: PathBuf,
  state: Mutex<StoreState>,
  blocks_fetched: AtomicU64,
  bytes_fetched: AtomicU64,
  block_hits: AtomicU64,
  evicted_blocks: AtomicU64,
  refetched_blocks: AtomicU64,
  refetched_bytes: AtomicU64,
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
  ///
  /// `residency_limit` bounds bytes held on local disk; zero means unbounded,
  /// the same convention as the hydration budget and for the same reason.
  pub async fn open(
    client: OdbClient,
    root: PathBuf,
    residency_limit: u64,
  ) -> Result<Self, GfsError> {
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root)
      .map_err(|e| GfsError::internal(format!("creating the odb store: {e}")))?;
    let manifest = client.manifest().await?;
    let mut files = HashMap::new();
    let mut paths = Vec::new();
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
          id: paths.len() as u32,
          size: f.size,
          present: vec![0; words],
          fetched_ever: vec![0; words],
          local,
        },
      );
      paths.push(f.path.clone());
    }
    Ok(BlockStore {
      client,
      root,
      state: Mutex::new(StoreState {
        files,
        paths,
        residency: Residency {
          limit: residency_limit,
          ..Residency::default()
        },
      }),
      blocks_fetched: AtomicU64::new(0),
      bytes_fetched: AtomicU64::new(0),
      block_hits: AtomicU64::new(0),
      evicted_blocks: AtomicU64::new(0),
      refetched_blocks: AtomicU64::new(0),
      refetched_bytes: AtomicU64::new(0),
    })
  }

  pub fn stats(&self) -> OdbStats {
    let (resident_bytes, residency_limit_bytes) = {
      let state = self.state.lock().expect("odb state");
      (state.residency.resident_bytes, state.residency.limit)
    };
    OdbStats {
      blocks_fetched: self.blocks_fetched.load(Ordering::Relaxed),
      bytes_fetched: self.bytes_fetched.load(Ordering::Relaxed),
      block_hits: self.block_hits.load(Ordering::Relaxed),
      resident_bytes,
      residency_limit_bytes,
      evicted_blocks: self.evicted_blocks.load(Ordering::Relaxed),
      refetched_blocks: self.refetched_blocks.load(Ordering::Relaxed),
      refetched_bytes: self.refetched_bytes.load(Ordering::Relaxed),
    }
  }

  /// The manifest's view of the tree, for the filesystem to build inodes from.
  pub fn listing(&self) -> Vec<(String, u64)> {
    self
      .state
      .lock()
      .expect("odb state")
      .files
      .iter()
      .map(|(path, state)| (path.clone(), state.size))
      .collect()
  }

  /// Read `size` bytes at `offset`, fetching any absent blocks first.
  ///
  /// Fetches are one HTTP range per *contiguous run* of absent blocks, so a
  /// sequential scan costs one round trip per run rather than one per block,
  /// while a sparse binary search still fetches only what it touches. The
  /// returned [`ReadCost`] is this read's share of the store's counters, so a
  /// view can attribute it to the workspace that asked.
  pub async fn read(
    &self,
    path: &str,
    offset: u64,
    size: u32,
  ) -> Result<(Vec<u8>, ReadCost), GfsError> {
    let mut cost = ReadCost::default();
    // Resolve, pin, and classify in one lock acquisition. The pins are what
    // make the rest of this method sound: from here until the guard drops,
    // nothing in `first..=last` can be evicted, so a block marked present
    // stays truly on disk until it has been served.
    let (local, total, file_id, first, last, runs) = {
      let mut state = self.state.lock().expect("odb state");
      let StoreState {
        files, residency, ..
      } = &mut *state;
      let file = files.get(path).ok_or_else(|| {
        GfsError::new(
          ErrorCode::FailedPrecondition,
          "the object store was repacked; remount",
        )
      })?;
      let (local, total, file_id) = (file.local.clone(), file.size, file.id);
      if offset >= total {
        return Ok((Vec::new(), cost));
      }
      let end = (offset + size as u64).min(total);
      let first = offset / BLOCK;
      let last = (end - 1) / BLOCK;
      let mut runs: Vec<(u64, u64)> = Vec::new();
      let mut run_start: Option<u64> = None;
      for block in first..=last {
        residency.pin((file_id, block));
        if file.has(block) {
          if let Some(s) = run_start.take() {
            runs.push((s, block - 1));
          }
          self.block_hits.fetch_add(1, Ordering::Relaxed);
          cost.block_hits += 1;
          residency.touch((file_id, block));
        } else if run_start.is_none() {
          run_start = Some(block);
        }
      }
      if let Some(s) = run_start {
        runs.push((s, last));
      }
      (local, total, file_id, first, last, runs)
    };
    let _unpin = UnpinGuard {
      store: self,
      file_id,
      first,
      last,
    };
    let end = (offset + size as u64).min(total);

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
      // The write happens outside the lock — the blocks are pinned and not yet
      // present, so eviction cannot touch the range being written.
      let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(&local)
        .map_err(|e| GfsError::internal(format!("opening the block file: {e}")))?;
      file
        .seek(SeekFrom::Start(byte_start))
        .and_then(|_| file.write_all(&bytes))
        .map_err(|e| GfsError::internal(format!("writing blocks: {e}")))?;
      let mut state = self.state.lock().expect("odb state");
      let StoreState {
        files, residency, ..
      } = &mut *state;
      if let Some(file) = files.get_mut(path) {
        for block in from..=to {
          // Only a presence *transition* admits the block, so the benign
          // double-fetch race cannot double-count residency.
          if !file.has(block) {
            file.set(block);
            let len = file.block_len(block);
            if bit_get(&file.fetched_ever, block) {
              self.refetched_blocks.fetch_add(1, Ordering::Relaxed);
              self.refetched_bytes.fetch_add(len, Ordering::Relaxed);
            } else {
              bit_set(&mut file.fetched_ever, block);
            }
            residency.admit((file.id, block), len);
          }
        }
      }
      self
        .blocks_fetched
        .fetch_add(to - from + 1, Ordering::Relaxed);
      self
        .bytes_fetched
        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
      cost.blocks_fetched += to - from + 1;
      cost.bytes_fetched += bytes.len() as u64;
      self.evict_to_limit(&mut state);
    }

    let mut buf = vec![0u8; (end - offset) as usize];
    let mut file = std::fs::File::open(&local)
      .map_err(|e| GfsError::internal(format!("opening the block file: {e}")))?;
    file
      .seek(SeekFrom::Start(offset))
      .and_then(|_| file.read_exact(&mut buf))
      .map_err(|e| GfsError::internal(format!("reading cached blocks: {e}")))?;
    Ok((buf, cost))
  }

  /// Evict until the residency limit is met, or everything left is pinned.
  ///
  /// Eviction is a presence-bit clear plus a punched hole, in that order and
  /// under the lock: a reader consulting the bitmap after this ran sees the
  /// block absent and re-fetches, so the hole can never be served as content.
  fn evict_to_limit(&self, state: &mut StoreState) {
    let StoreState {
      files,
      paths,
      residency,
    } = state;
    while residency.over_limit() {
      let Some(key) = residency.victim() else {
        break;
      };
      let Some(entry) = residency.forget(key) else {
        break;
      };
      let (file_id, block) = key;
      let Some(file) = paths
        .get(file_id as usize)
        .and_then(|path| files.get_mut(path))
      else {
        continue;
      };
      bit_clear(&mut file.present, block);
      if let Err(e) = punch_hole(&file.local, block * BLOCK, entry.len) {
        // The block is forgotten either way — the bitmap is the truth — but
        // the disk was not returned, which is worth a line in the log.
        tracing::warn!(block, "punching an evicted odb block: {e}");
      }
      self.evicted_blocks.fetch_add(1, Ordering::Relaxed);
    }
  }
}

/// Deallocate a block's bytes without changing the file's size.
///
/// This is what the workspace-level deny (rather than forbid) of unsafe code
/// exists for: `fallocate(FALLOC_FL_PUNCH_HOLE)` has no wrapper in `std`, and
/// the alternatives either do not return the disk (overwriting with zeros
/// *allocates*) or cost a subprocess per evicted block. The call reads no
/// memory: it takes a descriptor owned by this scope and two integers.
#[allow(unsafe_code)]
fn punch_hole(local: &Path, offset: u64, len: u64) -> std::io::Result<()> {
  use std::os::fd::AsRawFd;
  let file = std::fs::OpenOptions::new().write(true).open(local)?;
  let rc = unsafe {
    libc::fallocate(
      file.as_raw_fd(),
      libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
      offset as libc::off_t,
      len as libc::off_t,
    )
  };
  if rc != 0 {
    return Err(std::io::Error::last_os_error());
  }
  Ok(())
}

/// Releases a read's pins on drop, so every exit path — including an error
/// return from a fetch — releases them exactly once.
struct UnpinGuard<'a> {
  store: &'a BlockStore,
  file_id: u32,
  first: u64,
  last: u64,
}

impl Drop for UnpinGuard<'_> {
  fn drop(&mut self) {
    let mut state = self.store.state.lock().expect("odb state");
    for block in self.first..=self.last {
      state.residency.unpin((self.file_id, block));
    }
  }
}

// ---------------------------------------------------------------------------
// The projection handle
// ---------------------------------------------------------------------------

/// One repository's projection: the shared block store and the client that
/// feeds it.
///
/// No FUSE session lives here any more. ADR 0011 made the workspace mount
/// itself serve the store at `.git/gfs/objects/`, so what the host shares per
/// repository is exactly the state that cannot disagree between workspaces:
/// the manifest and the blocks.
pub struct OdbProjection {
  pub store: Arc<BlockStore>,
  /// A handle for the workspace seed: fetching the shipped index for a pinned
  /// commit goes to the same endpoint the blocks come from.
  pub client: OdbClient,
}

impl std::fmt::Debug for OdbProjection {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("OdbProjection")
      .field("root", &self.store.root)
      .finish_non_exhaustive()
  }
}

impl OdbProjection {
  /// Open the projection for one repository.
  ///
  /// `root` holds the block store (`blocks/`); `residency_limit` bounds the
  /// store's bytes held, zero is unbounded.
  pub async fn open(
    client: OdbClient,
    root: &Path,
    residency_limit: u64,
  ) -> Result<Arc<Self>, GfsError> {
    let seed_client = client.clone();
    let store = Arc::new(BlockStore::open(client, root.join("blocks"), residency_limit).await?);
    Ok(Arc::new(OdbProjection {
      store,
      client: seed_client,
    }))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  // The one property of the SLRU that is policy rather than plumbing, and that
  // ADR 0009 states as a requirement: a streaming scan must not flush the
  // re-read working set. Everything else about the store is exercised through
  // real Git in `tests/odb.rs`.
  #[test]
  fn a_one_pass_scan_cannot_flush_the_reread_working_set() {
    let mut r = Residency {
      limit: 10 * BLOCK,
      ..Residency::default()
    };
    // A working set of three blocks, re-read so it is promoted to protected.
    for block in 0..3 {
      r.admit((0, block), BLOCK);
      r.touch((0, block));
    }
    // A scan far larger than the limit sweeps through, evicting as the store
    // would: admit, then evict to the limit. Single-touch blocks never promote.
    for block in 0..100 {
      r.admit((1, block), BLOCK);
      while r.over_limit() {
        let victim = r.victim().expect("nothing is pinned");
        assert_eq!(victim.0, 1, "the scan may only flush itself");
        r.forget(victim);
      }
    }
    for block in 0..3 {
      assert!(
        r.entries.contains_key(&(0, block)),
        "protected block {block} survived"
      );
    }
  }

  #[test]
  fn a_pinned_block_is_never_a_victim() {
    let mut r = Residency {
      limit: BLOCK,
      ..Residency::default()
    };
    r.admit((0, 0), BLOCK);
    r.pin((0, 0));
    r.admit((0, 1), BLOCK);
    // Over the limit, but the LRU block is pinned: the victim must be the
    // newer, unpinned one.
    assert_eq!(r.victim(), Some((0, 1)));
    r.unpin((0, 0));
    assert_eq!(r.victim(), Some((0, 0)));
  }
}
