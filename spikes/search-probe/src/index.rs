//! Blob registry, trigram postings, and snapshot manifests.
//!
//! The representation from DESIGN.md section 6.5: every unique searchable blob
//! in a repository gets a stable numeric `blob_key` and is indexed once; a
//! snapshot supplies a path table, a reverse table, and a Roaring bitmap of the
//! blob keys it contains. Literal queries intersect trigram postings with that
//! bitmap before any bytes are read.
//!
//! The number this spike exists to produce is not index build time. It is
//! *manifest bytes per retained snapshot*, because that is what decides whether
//! searching an arbitrary commit is affordable when many snapshots are retained
//! at once (PLAN.md M0.4).

use anyhow::{Context, Result};
use roaring::RoaringBitmap;
use std::collections::HashMap;
use std::time::Instant;

/// Why a blob is not in the searchable corpus. Recorded per blob, because the
/// completion contract in DESIGN.md section 7.5 has to report coverage
/// exclusions by reason and scope rather than silently dropping them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Exclusion {
  Binary,
  Oversized,
  /// Reserved: not produced by the current classifier, which follows
  /// ripgrep's NUL-byte rule. Kept because the coverage contract must be able
  /// to name this reason if a future classifier adds it.
  #[allow(dead_code)]
  InvalidUtf8,
}

pub const MAX_INDEXED_BYTES: usize = 8 * 1024 * 1024;

/// Classify a blob the way the server index must, before it is indexed.
///
/// Deliberately close to what ripgrep does, because `xvfs-rg` has to agree with
/// `rg` or the substitution is a lie: a NUL byte in the first block means
/// binary.
pub fn classify(content: &[u8]) -> Option<Exclusion> {
  if content.len() > MAX_INDEXED_BYTES {
    return Some(Exclusion::Oversized);
  }
  let probe = &content[..content.len().min(8192)];
  if memchr::memchr(0, probe).is_some() {
    return Some(Exclusion::Binary);
  }
  None
}

/// Trigrams of a blob, deduplicated.
///
/// Byte trigrams, not character trigrams: paths and contents are bytes, and a
/// UTF-8-aware split would make the index disagree with a byte-oriented matcher
/// on exactly the inputs that matter.
pub fn trigrams(content: &[u8]) -> Vec<u32> {
  if content.len() < 3 {
    return Vec::new();
  }
  let mut v: Vec<u32> = content
    .windows(3)
    .map(|w| ((w[0] as u32) << 16) | ((w[1] as u32) << 8) | w[2] as u32)
    .collect();
  v.sort_unstable();
  v.dedup();
  v
}

/// A repository-scoped registry mapping object IDs to stable blob keys.
#[derive(Default)]
pub struct BlobRegistry {
  pub key_by_oid: HashMap<[u8; 20], u32>,
  pub oid_by_key: Vec<[u8; 20]>,
  pub size_by_key: Vec<u32>,
  pub excluded: HashMap<u32, Exclusion>,
}

impl BlobRegistry {
  /// Assign a key, or return the existing one. Indexing is idempotent per OID,
  /// which is what makes repeated blobs across snapshots free.
  pub fn intern(&mut self, oid: &[u8; 20], size: u32) -> (u32, bool) {
    if let Some(k) = self.key_by_oid.get(oid) {
      return (*k, false);
    }
    let key = self.oid_by_key.len() as u32;
    self.key_by_oid.insert(*oid, key);
    self.oid_by_key.push(*oid);
    self.size_by_key.push(size);
    (key, true)
  }

  pub fn len(&self) -> usize {
    self.oid_by_key.len()
  }
}

/// Trigram -> blob keys.
#[derive(Default)]
pub struct TrigramIndex {
  pub postings: HashMap<u32, RoaringBitmap>,
}

impl TrigramIndex {
  pub fn add(&mut self, key: u32, content: &[u8]) {
    for t in trigrams(content) {
      self.postings.entry(t).or_default().insert(key);
    }
  }

  /// Candidate blob keys for a set of required trigrams, intersected with the
  /// snapshot's membership bitmap.
  ///
  /// The intersection happens before any blob is read, which is the entire
  /// reason this representation exists: a query over one commit does not read
  /// content belonging to other commits.
  pub fn candidates(&self, required: &[u32], snapshot: &RoaringBitmap) -> Option<RoaringBitmap> {
    if required.is_empty() {
      return None; // no usable literal: caller must fall back to a scan
    }
    // Rarest posting list first, so the intersection shrinks fastest.
    let mut lists: Vec<&RoaringBitmap> = Vec::with_capacity(required.len());
    for t in required {
      match self.postings.get(t) {
        Some(p) => lists.push(p),
        // A trigram absent from the index cannot match anything.
        None => return Some(RoaringBitmap::new()),
      }
    }
    lists.sort_by_key(|l| l.len());
    let mut acc = lists[0] & snapshot;
    for l in &lists[1..] {
      if acc.is_empty() {
        break;
      }
      acc &= *l;
    }
    Some(acc)
  }

  /// Serialized size of all posting lists, as they would land on disk.
  pub fn serialized_bytes(&self) -> u64 {
    self
      .postings
      .values()
      .map(|b| b.serialized_size() as u64 + 4)
      .sum()
  }
}

/// One snapshot's manifest: the per-commit data that must be retained while any
/// mount or job pins that commit.
pub struct Manifest {
  pub commit: String,
  /// Sorted `path -> (mode, blob_key)`.
  pub paths: Vec<(Vec<u8>, u32, u32)>,
  /// Blob keys reachable from this snapshot.
  pub members: RoaringBitmap,
  /// Reverse table: blob key -> the paths that carry it.
  pub reverse: HashMap<u32, Vec<u32>>,
}

impl Manifest {
  /// Bytes this manifest would occupy on disk, per component.
  ///
  /// This is the projection the M0.4 exit gate turns on, so it is computed
  /// from the actual data rather than estimated. Path bytes are counted as
  /// stored with a 2-byte length prefix and front-coding against the previous
  /// path, which is what a real implementation would do for a sorted table.
  pub fn storage(&self) -> ManifestStorage {
    let mut path_bytes = 0u64;
    let mut prev: &[u8] = b"";
    for (p, _, _) in &self.paths {
      let shared = p
        .iter()
        .zip(prev.iter())
        .take_while(|(a, b)| a == b)
        .count()
        .min(255);
      // 1 byte shared-prefix length + suffix + 2 byte mode + 4 byte key
      path_bytes += 1 + (p.len() - shared) as u64 + 2 + 4;
      prev = p;
    }
    // The reverse table stores, per key, a count plus path ordinals.
    let reverse_bytes: u64 = self.reverse.values().map(|v| 2 + 4 * v.len() as u64).sum();
    ManifestStorage {
      path_table: path_bytes,
      bitmap: self.members.serialized_size() as u64,
      reverse_table: reverse_bytes,
    }
  }
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct ManifestStorage {
  pub path_table: u64,
  pub bitmap: u64,
  pub reverse_table: u64,
}

impl ManifestStorage {
  pub fn total(&self) -> u64 {
    self.path_table + self.bitmap + self.reverse_table
  }
}

pub struct BuildStats {
  pub walk_ms: f64,
  pub index_ms: f64,
  pub entries: usize,
  pub new_blobs: usize,
  pub indexed_blobs: usize,
  pub indexed_bytes: u64,
  pub excluded: HashMap<Exclusion, usize>,
  pub excluded_bytes: u64,
}

/// Build (or extend) the registry and index for one commit, and return its
/// manifest.
pub fn build_snapshot(
  repo: &git2::Repository,
  commit_oid: git2::Oid,
  registry: &mut BlobRegistry,
  index: &mut TrigramIndex,
  index_content: bool,
) -> Result<(Manifest, BuildStats)> {
  let t0 = Instant::now();
  let commit = repo.find_commit(commit_oid)?;
  let tree = commit.tree()?;

  // Collect the full path list first. A tree walk that also reads blobs
  // interleaves cheap metadata work with expensive inflation and makes the
  // two costs impossible to separate in the numbers.
  let mut entries: Vec<(Vec<u8>, u32, git2::Oid)> = Vec::new();
  tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
    if entry.filemode() == 0o040000 {
      return git2::TreeWalkResult::Ok;
    }
    // Only regular files carry searchable content.
    if entry.filemode() == 0o100644 || entry.filemode() == 0o100755 {
      let mut path = dir.as_bytes().to_vec();
      path.extend_from_slice(entry.name_bytes());
      entries.push((path, entry.filemode() as u32, entry.id()));
    }
    git2::TreeWalkResult::Ok
  })?;
  entries.sort_by(|a, b| a.0.cmp(&b.0));
  let walk_ms = t0.elapsed().as_secs_f64() * 1000.0;

  let t1 = Instant::now();
  let odb = repo.odb()?;
  let mut members = RoaringBitmap::new();
  let mut reverse: HashMap<u32, Vec<u32>> = HashMap::new();
  let mut paths = Vec::with_capacity(entries.len());
  let mut stats = BuildStats {
    walk_ms,
    index_ms: 0.0,
    entries: entries.len(),
    new_blobs: 0,
    indexed_blobs: 0,
    indexed_bytes: 0,
    excluded: HashMap::new(),
    excluded_bytes: 0,
  };

  for (ordinal, (path, mode, oid)) in entries.into_iter().enumerate() {
    let mut raw = [0u8; 20];
    raw.copy_from_slice(oid.as_bytes());
    // read_header gives the size without inflating, so a blob that will be
    // excluded for size never costs an inflate.
    let size = odb.read_header(oid).map(|(s, _)| s as u32).unwrap_or(0);
    let (key, is_new) = registry.intern(&raw, size);

    members.insert(key);
    reverse.entry(key).or_default().push(ordinal as u32);
    paths.push((path, mode, key));

    if !is_new {
      continue;
    }
    stats.new_blobs += 1;

    if size as usize > MAX_INDEXED_BYTES {
      registry.excluded.insert(key, Exclusion::Oversized);
      *stats.excluded.entry(Exclusion::Oversized).or_default() += 1;
      stats.excluded_bytes += size as u64;
      continue;
    }
    let blob = odb
      .read(oid)
      .with_context(|| format!("reading blob {oid}"))?;
    let content = blob.data();
    match classify(content) {
      Some(reason) => {
        registry.excluded.insert(key, reason);
        *stats.excluded.entry(reason).or_default() += 1;
        stats.excluded_bytes += content.len() as u64;
      }
      None => {
        stats.indexed_blobs += 1;
        stats.indexed_bytes += content.len() as u64;
        if index_content {
          index.add(key, content);
        }
      }
    }
  }
  stats.index_ms = t1.elapsed().as_secs_f64() * 1000.0;

  Ok((
    Manifest {
      commit: commit_oid.to_string(),
      paths,
      members,
      reverse,
    },
    stats,
  ))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn trigrams_are_deduplicated_and_byte_oriented() {
    assert!(trigrams(b"ab").is_empty());
    assert_eq!(trigrams(b"aaaa"), vec![0x616161]);
    // A non-UTF-8 byte still produces trigrams rather than being skipped.
    assert_eq!(trigrams(b"\xff\xfe\xfd").len(), 1);
  }

  #[test]
  fn classification_matches_the_documented_corpus() {
    assert_eq!(classify(b"hello\n"), None);
    assert_eq!(classify(b"a\0b"), Some(Exclusion::Binary));
    assert_eq!(
      classify(&vec![b'x'; MAX_INDEXED_BYTES + 1]),
      Some(Exclusion::Oversized)
    );
    // A NUL beyond the probe window is not seen, matching ripgrep.
    let mut late = vec![b'x'; 9000];
    late[8500] = 0;
    assert_eq!(classify(&late), None);
  }

  #[test]
  fn missing_trigram_yields_no_candidates_without_reading_blobs() {
    let idx = TrigramIndex::default();
    let mut snap = RoaringBitmap::new();
    snap.insert(1);
    let got = idx.candidates(&[0x616161], &snap).unwrap();
    assert!(got.is_empty());
  }

  #[test]
  fn no_required_trigram_reports_that_it_cannot_help() {
    let idx = TrigramIndex::default();
    assert!(idx.candidates(&[], &RoaringBitmap::new()).is_none());
  }
}
