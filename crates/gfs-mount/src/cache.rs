//! The verified, quota-bounded blob cache.
//!
//! DESIGN.md section 8.3 and ADR 0006's threat model fix the mechanics: entries
//! are keyed by `(repository_id, algorithm, oid)`, written to a temporary file,
//! hashed as a canonical Git blob, then atomically renamed; eviction is
//! quota-based LRU and never removes an open entry; directories are mode 0700.
//!
//! # Why verification is not optional
//!
//! The whole point of content addressing is that the client can check the server's
//! work. A truncated response, a corrupted disk, or a proxy that rewrote the body
//! all produce bytes whose hash is not the object ID, and every one of them would
//! otherwise reach a compiler as a silently wrong source file. The canonical form
//! is `blob <size>\0<content>` -- the header is part of the hashed bytes, so a
//! blob whose content happens to collide with a tree's content still hashes
//! differently.
//!
//! # Why publication is a rename
//!
//! Two daemons, or one daemon and a crash, must never let a reader observe a
//! half-written file. `rename(2)` within a filesystem is atomic, so a reader sees
//! either no file or a complete verified one. The temporary file lives in the same
//! directory as its destination, because a rename across filesystems is not a
//! rename at all.
//!
//! # Single-flight
//!
//! A parallel build opens the same header from twenty compiler processes at once.
//! Without deduplication that is twenty downloads of the same bytes; with it, one
//! download and nineteen waiters. The in-flight map is keyed by object ID and
//! holds a broadcast receiver rather than the bytes, so a waiter that gives up
//! (a cancelled FUSE request) does not keep the download alive on its own.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::{HashAlgorithm, ObjectId, RepositoryId};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use tokio::sync::broadcast;

use crate::client::SnapshotClient;

/// Compute the content key of a blob's bytes under the given algorithm.
///
/// Two verifiable forms exist: the Git SHA-1 object hash over
/// `blob <size>\0<content>`, and the ADR 0012 LFS key, which is SHA-256 over
/// the raw bytes with no header — the LFS oid *is* the content hash, so the
/// same no-unverified-bytes rule covers both. Git's own SHA-256 object format
/// stays unimplementable: ADR 0001 measured that it is unreachable through
/// `git2-rs`, and ADR 0006 moved it out of scope. The `match` is still written
/// as a match so that adding an algorithm is a compile error at every call
/// site that assumed one.
pub fn hash_blob(algorithm: HashAlgorithm, content: &[u8]) -> Result<ObjectId, GfsError> {
  match algorithm {
    HashAlgorithm::Sha1 => {
      let mut hasher = Sha1::new();
      hasher.update(format!("blob {}\0", content.len()).as_bytes());
      hasher.update(content);
      ObjectId::from_raw(HashAlgorithm::Sha1, &hasher.finalize())
        .map_err(|e| GfsError::internal(format!("hashing a blob: {e}")))
    }
    HashAlgorithm::LfsSha256 => {
      let mut hasher = Sha256::new();
      hasher.update(content);
      ObjectId::from_raw(HashAlgorithm::LfsSha256, &hasher.finalize())
        .map_err(|e| GfsError::internal(format!("hashing an LFS object: {e}")))
    }
    HashAlgorithm::Sha256 => Err(GfsError::new(
      ErrorCode::UnsupportedRepositoryFormat,
      "SHA-256 repositories are out of scope (ADR 0001)",
    )),
  }
}

/// What the cache did to satisfy a read. Feeds the hydration accounting DESIGN.md
/// section 8.4 requires.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hydration {
  /// Served from the on-disk cache; no bytes crossed the network.
  Hit,
  /// Downloaded and published by this call.
  Fetched,
  /// Another in-flight fetch for the same object was joined.
  Coalesced,
}

#[derive(Clone, Copy, Default, Debug, serde::Serialize, serde::Deserialize)]
pub struct CacheStats {
  pub hits: u64,
  pub fetches: u64,
  pub coalesced: u64,
  pub bytes_fetched: u64,
  pub evictions: u64,
  pub verification_failures: u64,
}

#[derive(Debug)]
struct Entry {
  size: u64,
  /// Monotonic use counter, not a wall clock: the LRU order must not change
  /// because the host's clock stepped.
  last_used: u64,
  /// Open descriptors referencing this blob. Never evicted above zero.
  pins: u32,
}

#[derive(Debug, Default)]
struct Index {
  entries: HashMap<String, Entry>,
  bytes: u64,
  clock: u64,
}

pub struct BlobCache {
  root: PathBuf,
  repository_id: RepositoryId,
  quota_bytes: u64,
  index: Mutex<Index>,
  inflight: Mutex<HashMap<String, broadcast::Sender<Result<u64, String>>>>,
  stats: Mutex<CacheStats>,
}

impl std::fmt::Debug for BlobCache {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("BlobCache")
      .field("root", &self.root)
      .field("quota_bytes", &self.quota_bytes)
      .finish_non_exhaustive()
  }
}

impl BlobCache {
  /// Open (creating if needed) the cache for one repository.
  ///
  /// The repository ID is part of the path, not merely of the key. ADR 0002 makes
  /// one bare repository one authorization domain, and ADR 0006 scopes
  /// deduplication to it -- a shared directory with a composite key would be one
  /// path-construction bug away from cross-repository content disclosure.
  pub fn open(
    cache_root: &Path,
    repository_id: &RepositoryId,
    algorithm: HashAlgorithm,
    quota_bytes: u64,
  ) -> Result<Arc<Self>, GfsError> {
    let root = cache_root
      .join("blobs")
      .join(repository_id.as_str())
      .join(algorithm.name());
    std::fs::create_dir_all(&root)
      .map_err(|e| GfsError::internal(format!("creating the blob cache directory: {e}")))?;
    // 0700 on every level the cache owns. Jobs reach content through the mount,
    // never by reading the daemon's cache directly (ADR 0006).
    restrict(cache_root)?;
    restrict(&cache_root.join("blobs"))?;
    restrict(&cache_root.join("blobs").join(repository_id.as_str()))?;
    restrict(&root)?;

    let cache = BlobCache {
      root,
      repository_id: repository_id.clone(),
      quota_bytes,
      index: Mutex::new(Index::default()),
      inflight: Mutex::new(HashMap::new()),
      stats: Mutex::new(CacheStats::default()),
    };
    cache.adopt_existing()?;
    Ok(Arc::new(cache))
  }

  /// Take ownership of whatever a previous daemon left behind.
  ///
  /// Existing files are content-addressed and were verified before publication,
  /// so they are adopted rather than discarded: a daemon restart that threw away
  /// a warm cache would re-download the whole working set, which is the cost the
  /// cache exists to avoid. Anything that is not a well-formed hex name is
  /// removed, because it can only be a partial file from an interrupted write.
  fn adopt_existing(&self) -> Result<(), GfsError> {
    let mut index = self.index.lock().expect("cache index");
    let iter = match std::fs::read_dir(&self.root) {
      Ok(iter) => iter,
      Err(_) => return Ok(()),
    };
    for shard in iter.flatten() {
      let prefix = shard.file_name().to_string_lossy().into_owned();
      if prefix.len() != 2 || !gfs_types::oid::is_hex(&prefix) {
        continue;
      }
      let Ok(files) = std::fs::read_dir(shard.path()) else {
        continue;
      };
      for file in files.flatten() {
        let name = file.file_name().to_string_lossy().into_owned();
        let Ok(meta) = file.metadata() else { continue };
        if !meta.is_file() || !gfs_types::oid::is_hex(&name) {
          let _ = std::fs::remove_file(file.path());
          continue;
        }
        // The index is keyed by the *whole* hex digest, and the on-disk name is
        // only its tail. Keying by the file name alone would make every adopted
        // entry unfindable, so a restarted daemon would silently re-download its
        // entire warm cache while reporting it as present on disk.
        index.clock += 1;
        let clock = index.clock;
        index.bytes += meta.len();
        index.entries.insert(
          format!("{prefix}{name}"),
          Entry {
            size: meta.len(),
            last_used: clock,
            pins: 0,
          },
        );
      }
    }
    Ok(())
  }

  pub fn stats(&self) -> CacheStats {
    *self.stats.lock().expect("cache stats")
  }

  pub fn bytes_on_disk(&self) -> u64 {
    self.index.lock().expect("cache index").bytes
  }

  fn path_for(&self, oid: &ObjectId) -> PathBuf {
    self.shard_path(&oid.to_hex())
  }

  /// Two-character shards, as Git does.
  ///
  /// Not for lookup speed but for directory size: a monorepo's working set is
  /// hundreds of thousands of objects, and some filesystems degrade badly past a
  /// few tens of thousands of entries in one directory.
  ///
  /// `split_at` rather than range indexing because the hex string is produced by
  /// [`ObjectId::to_hex`] and is therefore always at least 40 ASCII characters;
  /// the split point cannot fall inside a multi-byte sequence.
  fn shard_path(&self, hex: &str) -> PathBuf {
    let (prefix, rest) = hex.split_at(2);
    self.root.join(prefix).join(rest)
  }

  /// Whether the object is already published locally.
  pub fn contains(&self, oid: &ObjectId) -> bool {
    self
      .index
      .lock()
      .expect("cache index")
      .entries
      .contains_key(&oid.to_hex())
  }

  /// Ensure the blob is present, then pin it for the life of an open handle.
  ///
  /// Pinning happens inside the same lock acquisition that records the hit, so an
  /// eviction cannot run between "it is present" and "it is pinned" -- the window
  /// in which a concurrent open would otherwise return a path that no longer
  /// exists.
  pub async fn open_blob(
    &self,
    client: &SnapshotClient,
    oid: &ObjectId,
    ticket: &str,
  ) -> Result<(PathBuf, Hydration), GfsError> {
    let hydration = self.ensure(client, oid, ticket).await?;
    let hex = oid.to_hex();
    let mut index = self.index.lock().expect("cache index");
    let clock = {
      index.clock += 1;
      index.clock
    };
    match index.entries.get_mut(&hex) {
      Some(entry) => {
        entry.pins += 1;
        entry.last_used = clock;
      }
      // Evicted between `ensure` and here. Rare, and recoverable by the caller
      // retrying; reported as retryable rather than as a missing file.
      None => {
        return Err(GfsError::new(
          ErrorCode::Unavailable,
          "the cached blob was evicted before it could be opened",
        ))
      }
    }
    Ok((self.path_for(oid), hydration))
  }

  /// Release an open handle's pin.
  pub fn release_blob(&self, oid: &ObjectId) {
    let mut index = self.index.lock().expect("cache index");
    if let Some(entry) = index.entries.get_mut(&oid.to_hex()) {
      entry.pins = entry.pins.saturating_sub(1);
    }
  }

  /// Fetch a blob into the cache without pinning it.
  ///
  /// For prefetching, where nothing holds a descriptor: an unread speculative
  /// blob must stay evictable, or a wrong guess would hold cache space against
  /// the reads that were right.
  pub async fn ensure_cached(
    &self,
    client: &SnapshotClient,
    oid: &ObjectId,
    ticket: &str,
  ) -> Result<(), GfsError> {
    self.ensure(client, oid, ticket).await.map(|_| ())
  }

  /// Read a whole blob, going through the cache.
  pub async fn read(
    &self,
    client: &SnapshotClient,
    oid: &ObjectId,
    ticket: &str,
  ) -> Result<Vec<u8>, GfsError> {
    self.ensure(client, oid, ticket).await?;
    std::fs::read(self.path_for(oid)).map_err(|e| {
      GfsError::new(
        ErrorCode::Unavailable,
        format!("reading a cached blob: {}", e.kind()),
      )
    })
  }

  async fn ensure(
    &self,
    client: &SnapshotClient,
    oid: &ObjectId,
    ticket: &str,
  ) -> Result<Hydration, GfsError> {
    let hex = oid.to_hex();
    if self.touch(&hex) {
      self.stats.lock().expect("cache stats").hits += 1;
      return Ok(Hydration::Hit);
    }

    // Either become the fetcher or join the one already running.
    let receiver = {
      let mut inflight = self.inflight.lock().expect("inflight map");
      match inflight.get(&hex) {
        Some(sender) => Some(sender.subscribe()),
        None => {
          let (sender, _) = broadcast::channel(1);
          inflight.insert(hex.clone(), sender);
          None
        }
      }
    };

    if let Some(mut receiver) = receiver {
      self.stats.lock().expect("cache stats").coalesced += 1;
      return match receiver.recv().await {
        Ok(Ok(_)) => Ok(Hydration::Coalesced),
        Ok(Err(message)) => Err(GfsError::new(ErrorCode::Unavailable, message)),
        // The fetcher died without sending. Retryable: the next attempt becomes
        // the fetcher itself.
        Err(_) => Err(GfsError::new(
          ErrorCode::Unavailable,
          "the in-flight blob fetch did not complete",
        )),
      };
    }

    let result = self.fetch_and_publish(client, oid, ticket).await;
    let sender = self
      .inflight
      .lock()
      .expect("inflight map")
      .remove(&hex)
      .expect("this task installed the in-flight sender");
    // `send` fails when nobody is waiting, which is the common case.
    let _ = sender.send(
      result
        .as_ref()
        .map(|size| *size)
        .map_err(|e| e.message.clone()),
    );
    result.map(|_| Hydration::Fetched)
  }

  async fn fetch_and_publish(
    &self,
    client: &SnapshotClient,
    oid: &ObjectId,
    ticket: &str,
  ) -> Result<u64, GfsError> {
    let bytes = client.read_blob(oid, ticket).await?;

    // Hashed under the *key's* algorithm, not the cache's: one cache holds the
    // repository's git blobs (sha1, `blob <size>\0`-prefixed) and its expanded
    // LFS objects (`lfs-sha256:`, raw bytes) side by side, and each key form
    // names its own verification rule (ADR 0012). `hash_blob` still refuses
    // algorithms with no rule, so this cannot silently weaken.
    let computed = hash_blob(oid.algorithm(), &bytes)?;
    if &computed != oid {
      self
        .stats
        .lock()
        .expect("cache stats")
        .verification_failures += 1;
      // The object ID is safe to log: it is a hash, not content. The bytes are
      // not, and are deliberately discarded rather than quarantined.
      return Err(GfsError::new(
        ErrorCode::Internal,
        format!(
          "blob {} did not verify: the server returned content hashing to {}",
          oid.to_qualified(),
          computed.to_qualified()
        ),
      ));
    }

    let destination = self.path_for(oid);
    let directory = destination
      .parent()
      .ok_or_else(|| GfsError::internal("cache path has no parent"))?;
    std::fs::create_dir_all(directory)
      .map_err(|e| GfsError::internal(format!("creating a cache shard: {e}")))?;
    restrict(directory)?;

    // In the destination directory, so the publication below is a rename within
    // one filesystem and therefore atomic.
    let temporary = directory.join(format!(".{}.{}", oid.to_hex(), std::process::id()));
    {
      let mut file = std::fs::File::create(&temporary)
        .map_err(|e| GfsError::internal(format!("creating a cache temporary: {e}")))?;
      file
        .write_all(&bytes)
        .map_err(|e| GfsError::internal(format!("writing a cache temporary: {e}")))?;
      // Durable before it is visible. Without this a crash can publish a name
      // whose content the filesystem has not yet written, and the next daemon
      // adopts a file that verifies as present but reads as zeros.
      file
        .sync_all()
        .map_err(|e| GfsError::internal(format!("syncing a cache temporary: {e}")))?;
      let mut permissions = file
        .metadata()
        .map_err(|e| GfsError::internal(format!("stat on a cache temporary: {e}")))?
        .permissions();
      permissions.set_mode(0o600);
      let _ = file.set_permissions(permissions);
    }
    std::fs::rename(&temporary, &destination).map_err(|e| {
      let _ = std::fs::remove_file(&temporary);
      GfsError::internal(format!("publishing a cached blob: {e}"))
    })?;

    let size = bytes.len() as u64;
    {
      let mut index = self.index.lock().expect("cache index");
      index.clock += 1;
      let clock = index.clock;
      if let Some(previous) = index.entries.insert(
        oid.to_hex(),
        Entry {
          size,
          last_used: clock,
          pins: 0,
        },
      ) {
        index.bytes -= previous.size;
      }
      index.bytes += size;
    }
    {
      let mut stats = self.stats.lock().expect("cache stats");
      stats.fetches += 1;
      stats.bytes_fetched += size;
    }
    self.evict_to_quota();
    Ok(size)
  }

  /// Mark an entry used, returning whether it exists.
  fn touch(&self, hex: &str) -> bool {
    let mut index = self.index.lock().expect("cache index");
    index.clock += 1;
    let clock = index.clock;
    match index.entries.get_mut(hex) {
      Some(entry) => {
        entry.last_used = clock;
        true
      }
      None => false,
    }
  }

  /// Evict least-recently-used unpinned entries until the quota is met.
  ///
  /// A cache that is entirely pinned simply stays over quota. That is the correct
  /// direction: breaking a live descriptor to satisfy a byte budget would turn a
  /// disk-space policy into data loss, and the hydration budget in DESIGN.md
  /// section 8.4 is the mechanism for bounding demand.
  fn evict_to_quota(&self) {
    let mut index = self.index.lock().expect("cache index");
    if index.bytes <= self.quota_bytes {
      return;
    }
    let mut candidates: Vec<(String, u64, u64)> = index
      .entries
      .iter()
      .filter(|(_, e)| e.pins == 0)
      .map(|(hex, e)| (hex.clone(), e.last_used, e.size))
      .collect();
    candidates.sort_by_key(|(_, last_used, _)| *last_used);

    let mut evicted = 0u64;
    for (hex, _, size) in candidates {
      if index.bytes <= self.quota_bytes {
        break;
      }
      let path = self.shard_path(&hex);
      if std::fs::remove_file(&path).is_ok() || !path.exists() {
        index.entries.remove(&hex);
        index.bytes = index.bytes.saturating_sub(size);
        evicted += 1;
      }
    }
    drop(index);
    if evicted > 0 {
      self.stats.lock().expect("cache stats").evictions += evicted;
    }
  }

  pub fn repository_id(&self) -> &RepositoryId {
    &self.repository_id
  }
}

fn restrict(path: &Path) -> Result<(), GfsError> {
  std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
    .map_err(|e| GfsError::internal(format!("restricting cache directory permissions: {e}")))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_canonical_blob_hash_matches_git() {
    // `printf '' | git hash-object --stdin` and the "hello\n" case, which are the
    // two values every Git implementation is checked against.
    assert_eq!(
      hash_blob(HashAlgorithm::Sha1, b"").unwrap().to_hex(),
      "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
    );
    assert_eq!(
      hash_blob(HashAlgorithm::Sha1, b"hello\n").unwrap().to_hex(),
      "ce013625030ba8dba906f756967f9e9ca394464a"
    );
  }

  #[test]
  fn the_header_is_part_of_the_hashed_bytes() {
    // If the length header were omitted, these two would collide with content
    // hashed under a different object type, which is the mistake the canonical
    // form exists to prevent.
    let a = hash_blob(HashAlgorithm::Sha1, b"ab").unwrap();
    let b = hash_blob(HashAlgorithm::Sha1, b"abc").unwrap();
    assert_ne!(a, b);
  }

  #[test]
  fn sha256_is_refused_rather_than_silently_hashed_as_sha1() {
    let e = hash_blob(HashAlgorithm::Sha256, b"x").unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedRepositoryFormat);
  }

  #[test]
  fn the_lfs_key_is_the_raw_sha256_with_no_header() {
    // `printf 'hello\n' | sha256sum` — the digest a spec v1 pointer's `oid`
    // line would carry for this content, which is what makes the download
    // verifiable without trusting the server.
    assert_eq!(
      hash_blob(HashAlgorithm::LfsSha256, b"hello\n")
        .unwrap()
        .to_qualified(),
      "lfs-sha256:5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03"
    );
    // And it must never equal the Git object hash of the same bytes.
    assert_ne!(
      hash_blob(HashAlgorithm::LfsSha256, b"hello\n").unwrap(),
      hash_blob(HashAlgorithm::Sha1, b"hello\n").unwrap()
    );
  }

  #[test]
  fn eviction_removes_the_least_recently_used_unpinned_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = RepositoryId::parse("r-cache").unwrap();
    let cache = BlobCache::open(tmp.path(), &repo, HashAlgorithm::Sha1, 8).unwrap();

    // Three 4-byte objects against an 8-byte quota, published directly so the
    // test does not need a server. One eviction brings 12 bytes back to 8.
    let mut oids = Vec::new();
    for content in [&b"aaaa"[..], b"bbbb", b"cccc"] {
      let oid = hash_blob(HashAlgorithm::Sha1, content).unwrap();
      let path = cache.path_for(&oid);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      std::fs::write(&path, content).unwrap();
      let mut index = cache.index.lock().unwrap();
      index.clock += 1;
      let clock = index.clock;
      index.bytes += 4;
      index.entries.insert(
        oid.to_hex(),
        Entry {
          size: 4,
          last_used: clock,
          pins: 0,
        },
      );
      oids.push(oid);
    }

    // Use the first, so the second becomes the least recently used.
    assert!(cache.touch(&oids[0].to_hex()));
    cache.evict_to_quota();

    assert!(cache.contains(&oids[0]), "recently used must survive");
    assert!(!cache.contains(&oids[1]), "least recently used must go");
    assert!(cache.contains(&oids[2]));
    assert!(cache.bytes_on_disk() <= 8);
  }

  #[test]
  fn a_pinned_entry_is_never_evicted_even_over_quota() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = RepositoryId::parse("r-pin").unwrap();
    let cache = BlobCache::open(tmp.path(), &repo, HashAlgorithm::Sha1, 1).unwrap();

    let oid = hash_blob(HashAlgorithm::Sha1, b"pinned").unwrap();
    let path = cache.path_for(&oid);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"pinned").unwrap();
    {
      let mut index = cache.index.lock().unwrap();
      index.bytes = 6;
      index.entries.insert(
        oid.to_hex(),
        Entry {
          size: 6,
          last_used: 1,
          pins: 1,
        },
      );
    }

    cache.evict_to_quota();
    assert!(
      cache.contains(&oid),
      "an open descriptor must outrank the byte quota"
    );

    cache.release_blob(&oid);
    cache.evict_to_quota();
    assert!(!cache.contains(&oid), "unpinned, it is evictable again");
  }

  #[test]
  fn a_partial_file_left_by_a_crash_is_discarded_on_adoption() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = RepositoryId::parse("r-adopt").unwrap();
    let cache = BlobCache::open(tmp.path(), &repo, HashAlgorithm::Sha1, 1 << 20).unwrap();

    let oid = hash_blob(HashAlgorithm::Sha1, b"kept").unwrap();
    let good = cache.path_for(&oid);
    std::fs::create_dir_all(good.parent().unwrap()).unwrap();
    std::fs::write(&good, b"kept").unwrap();
    let partial = good.parent().unwrap().join(".not-a-hex-name.12345");
    std::fs::write(&partial, b"half").unwrap();

    let reopened = BlobCache::open(tmp.path(), &repo, HashAlgorithm::Sha1, 1 << 20).unwrap();
    assert!(reopened.contains(&oid), "a verified file is adopted");
    assert!(!partial.exists(), "a temporary is removed");
    assert_eq!(reopened.bytes_on_disk(), 4);
  }
}
