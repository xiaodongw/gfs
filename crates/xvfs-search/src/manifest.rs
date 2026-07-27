//! The per-snapshot manifest: what one commit contains, in blob keys.
//!
//! ADR 0004 made this the number the whole representation was priced on —
//! **1.99 MiB per snapshot** on the Linux kernel, 0.39 GiB for 200 concurrently
//! retained snapshots — because manifest storage, not index build time, is what
//! decides whether searching an arbitrary commit is affordable.
//!
//! # What is stored, and what is derived
//!
//! Stored: a **front-coded path table**, sorted by path bytes, holding
//! `(path, mode, blob_key)` per searchable file.
//!
//! Derived at decode time: the **membership bitmap** (the set of blob keys) and
//! the **reverse table** (blob key → the path ordinals carrying it). PLAN.md
//! M4.2 says to store forward and reverse maps; both derived structures are
//! exact functions of the forward table, so storing them would inflate every
//! retained snapshot to hold data a single linear pass reconstructs — and would
//! introduce a way for the copies to disagree. The projection ADR 0004 made
//! therefore over-estimates what this actually costs, which is the safe
//! direction for a number that gated a go decision.
//!
//! The reverse table is not optional, though, and it is worth saying what it
//! buys: a blob that appears at forty paths is read **once** and reported at all
//! forty. That is ADR 0004's "repeated blobs are free" claim made real at query
//! time rather than only at index time.
//!
//! # The corpus is regular and executable files
//!
//! Symlinks, gitlinks, and unsupported modes are absent. `xvfs_git::WalkEntry`
//! carries the reasoning: `rg` does not follow symlinks by default, so including
//! them would make XVFS return matches `rg` does not.
//!
//! # Checksums and versions
//!
//! Every encoded manifest carries a format version and a SHA-256 over its body.
//! A manifest is derived data that survives process restarts and (in M7.1)
//! travels through object storage; a silently corrupted path table would produce
//! *wrong search results*, which M4's exit gate treats as the worst outcome
//! available.

use std::collections::BTreeMap;

use roaring::RoaringBitmap;
use sha2::{Digest, Sha256};
use xvfs_types::error::{ErrorCode, XvfsError};
use xvfs_types::{BytePath, ObjectId};

use crate::registry::BlobKey;

/// Bumped when the encoding changes incompatibly.
pub const MANIFEST_FORMAT_VERSION: u32 = 1;

const MAGIC: &[u8; 8] = b"XVFSMAN\x00";

/// One searchable file in a snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathEntry {
  pub path: BytePath,
  pub mode: u32,
  pub key: BlobKey,
}

/// One commit's searchable contents.
#[derive(Clone, Debug)]
pub struct Manifest {
  commit: ObjectId,
  /// Sorted by path bytes. The order is part of the format: front coding depends
  /// on it, and so does the deterministic ordering of query results.
  paths: Vec<PathEntry>,
  members: RoaringBitmap,
  reverse: BTreeMap<BlobKey, Vec<u32>>,
}

impl Manifest {
  /// Build from an unordered set of entries.
  ///
  /// Sorting happens here rather than being required of the caller, because the
  /// caller is a parallel tree walk whose results arrive per-subtree and in no
  /// particular order between subtrees.
  pub fn build(commit: ObjectId, mut entries: Vec<PathEntry>) -> Manifest {
    entries.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    entries.dedup_by(|a, b| a.path == b.path);
    Manifest::from_sorted(commit, entries)
  }

  fn from_sorted(commit: ObjectId, paths: Vec<PathEntry>) -> Manifest {
    let mut members = RoaringBitmap::new();
    let mut reverse: BTreeMap<BlobKey, Vec<u32>> = BTreeMap::new();
    for (ordinal, entry) in paths.iter().enumerate() {
      members.insert(entry.key);
      reverse.entry(entry.key).or_default().push(ordinal as u32);
    }
    Manifest {
      commit,
      paths,
      members,
      reverse,
    }
  }

  pub fn commit(&self) -> &ObjectId {
    &self.commit
  }

  pub fn paths(&self) -> &[PathEntry] {
    &self.paths
  }

  pub fn len(&self) -> usize {
    self.paths.len()
  }

  pub fn is_empty(&self) -> bool {
    self.paths.is_empty()
  }

  /// The blob keys this snapshot contains. Intersected with a posting list
  /// before any blob is read; that intersection is the whole design.
  pub fn members(&self) -> &RoaringBitmap {
    &self.members
  }

  /// The ordinals of the paths carrying a blob key.
  pub fn paths_for_key(&self, key: BlobKey) -> &[u32] {
    self.reverse.get(&key).map(|v| v.as_slice()).unwrap_or(&[])
  }

  /// The half-open ordinal range covered by a path prefix.
  ///
  /// A binary search rather than a scan, which is the reason the table is
  /// sorted: a query scoped to one directory of a monorepo must cost that
  /// directory, not the monorepo.
  ///
  /// The prefix is matched at a path boundary. `src` selects `src/main.rs` but
  /// not `srcutil/x.rs`, because a scope that silently widened would report
  /// coverage for files the caller never asked about.
  pub fn scope(&self, prefix: &[u8]) -> std::ops::Range<usize> {
    if prefix.is_empty() {
      return 0..self.paths.len();
    }
    let mut with_sep = prefix.to_vec();
    if with_sep.last() != Some(&b'/') {
      with_sep.push(b'/');
    }
    let start = self
      .paths
      .partition_point(|e| e.path.as_bytes() < with_sep.as_slice());
    // The upper bound is the first path that does not share the prefix. Formed
    // by incrementing the last byte, which works on bytes and needs no
    // assumption about encoding.
    let mut upper = with_sep.clone();
    let end = loop {
      match upper.pop() {
        Some(255) => continue,
        Some(b) => {
          upper.push(b + 1);
          break self
            .paths
            .partition_point(|e| e.path.as_bytes() < upper.as_slice());
        }
        None => break self.paths.len(),
      }
    };

    // An exact path (a file, not a directory) is also a legal scope.
    if start >= end {
      let exact = self.paths.partition_point(|e| e.path.as_bytes() < prefix);
      if exact < self.paths.len() && self.paths[exact].path.as_bytes() == prefix {
        return exact..exact + 1;
      }
    }
    start..end
  }

  /// Apply a first-parent delta, producing the child's manifest.
  ///
  /// This is what makes preparing the next commit on a branch cost its diff.
  /// ADR 0004 measured successive commits adding ~4 (vscode) to ~39 (linux) new
  /// blobs, so a full walk per commit would re-derive ~94 000 unchanged entries
  /// to learn nothing.
  pub fn apply(&self, commit: ObjectId, deltas: &[ManifestDelta]) -> Manifest {
    let mut table: BTreeMap<Vec<u8>, (u32, BlobKey)> = self
      .paths
      .iter()
      .map(|e| (e.path.as_bytes().to_vec(), (e.mode, e.key)))
      .collect();
    for delta in deltas {
      match delta {
        ManifestDelta::Removed { path } => {
          table.remove(path.as_bytes());
        }
        ManifestDelta::Upserted { path, mode, key } => {
          table.insert(path.as_bytes().to_vec(), (*mode, *key));
        }
      }
    }
    let paths = table
      .into_iter()
      .map(|(path, (mode, key))| PathEntry {
        path: BytePath::new(path),
        mode,
        key,
      })
      .collect();
    // A `BTreeMap` iterates in key order, which is exactly the sort the format
    // requires, so `from_sorted` rather than `build`.
    Manifest::from_sorted(commit, paths)
  }

  /// Encode with a format version and a checksum.
  pub fn encode(&self) -> Vec<u8> {
    let mut body = Vec::with_capacity(self.paths.len() * 24);
    body.extend_from_slice(MAGIC);
    body.extend_from_slice(&MANIFEST_FORMAT_VERSION.to_le_bytes());
    let commit = self.commit.to_qualified();
    put_varint(&mut body, commit.len() as u64);
    body.extend_from_slice(commit.as_bytes());
    put_varint(&mut body, self.paths.len() as u64);

    let mut previous: &[u8] = b"";
    for entry in &self.paths {
      let bytes = entry.path.as_bytes();
      let shared = bytes
        .iter()
        .zip(previous.iter())
        .take_while(|(a, b)| a == b)
        .count();
      put_varint(&mut body, shared as u64);
      put_varint(&mut body, (bytes.len() - shared) as u64);
      body.extend_from_slice(&bytes[shared..]);
      put_varint(&mut body, entry.mode as u64);
      put_varint(&mut body, entry.key as u64);
      previous = bytes;
    }

    let digest = Sha256::digest(&body);
    body.extend_from_slice(&digest);
    body
  }

  pub fn decode(bytes: &[u8]) -> Result<Manifest, XvfsError> {
    if bytes.len() < MAGIC.len() + 4 + 32 {
      return Err(corrupt("manifest is too short to be one"));
    }
    let (body, digest) = bytes.split_at(bytes.len() - 32);
    if Sha256::digest(body).as_slice() != digest {
      // Checked before anything is parsed, so a corrupted length field cannot
      // make the decoder allocate from garbage.
      return Err(corrupt("manifest checksum does not match its contents"));
    }

    let mut cursor = Cursor { bytes: body, at: 0 };
    if cursor.take(MAGIC.len())? != MAGIC {
      return Err(corrupt("manifest magic is wrong"));
    }
    let version = u32::from_le_bytes(
      cursor
        .take(4)?
        .try_into()
        .map_err(|_| corrupt("truncated format version"))?,
    );
    if version != MANIFEST_FORMAT_VERSION {
      return Err(XvfsError::new(
        ErrorCode::FailedPrecondition,
        format!(
          "manifest format version {version} is not the {MANIFEST_FORMAT_VERSION} \
           this build writes; rebuild the snapshot"
        ),
      ));
    }
    let commit_len = cursor.varint()? as usize;
    let commit = std::str::from_utf8(cursor.take(commit_len)?)
      .map_err(|_| corrupt("commit id is not text"))?;
    let commit =
      ObjectId::parse_qualified(commit).map_err(|e| corrupt(format!("commit id: {e}")))?;

    let count = cursor.varint()? as usize;
    let mut paths = Vec::with_capacity(count.min(1 << 20));
    let mut previous: Vec<u8> = Vec::new();
    for _ in 0..count {
      let shared = cursor.varint()? as usize;
      let suffix_len = cursor.varint()? as usize;
      if shared > previous.len() {
        return Err(corrupt(
          "front-coded prefix is longer than the previous path",
        ));
      }
      let mut path = previous[..shared].to_vec();
      path.extend_from_slice(cursor.take(suffix_len)?);
      let mode = cursor.varint()? as u32;
      let key = u32::try_from(cursor.varint()?).map_err(|_| corrupt("blob key out of range"))?;
      previous = path.clone();
      paths.push(PathEntry {
        path: BytePath::new(path),
        mode,
        key,
      });
    }

    Ok(Manifest::from_sorted(commit, paths))
  }

  /// The checksum an encoded manifest carries, as hex.
  pub fn checksum(encoded: &[u8]) -> String {
    let digest = &encoded[encoded.len().saturating_sub(32)..];
    digest.iter().map(|b| format!("{b:02x}")).collect()
  }
}

/// A change to a manifest, with the blob key already resolved.
///
/// Distinct from `xvfs_git::TreeDelta`, which carries an object ID: interning
/// the ID into a key is the caller's job, and doing it in the caller keeps this
/// crate free of the registry lookup order that would otherwise be implied here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestDelta {
  Removed {
    path: BytePath,
  },
  Upserted {
    path: BytePath,
    mode: u32,
    key: BlobKey,
  },
}

struct Cursor<'a> {
  bytes: &'a [u8],
  at: usize,
}

impl<'a> Cursor<'a> {
  fn take(&mut self, n: usize) -> Result<&'a [u8], XvfsError> {
    let end = self
      .at
      .checked_add(n)
      .filter(|e| *e <= self.bytes.len())
      .ok_or_else(|| corrupt("manifest ended in the middle of a field"))?;
    let out = &self.bytes[self.at..end];
    self.at = end;
    Ok(out)
  }

  fn varint(&mut self) -> Result<u64, XvfsError> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
      let byte = *self
        .bytes
        .get(self.at)
        .ok_or_else(|| corrupt("manifest ended in the middle of a varint"))?;
      self.at += 1;
      value |= u64::from(byte & 0x7f) << shift;
      if byte & 0x80 == 0 {
        return Ok(value);
      }
      shift += 7;
      if shift >= 64 {
        return Err(corrupt("varint is too long"));
      }
    }
  }
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
  loop {
    let byte = (value & 0x7f) as u8;
    value >>= 7;
    if value == 0 {
      out.push(byte);
      return;
    }
    out.push(byte | 0x80);
  }
}

fn corrupt(message: impl Into<String>) -> XvfsError {
  XvfsError::new(ErrorCode::Internal, message)
}

#[cfg(test)]
mod tests {
  use super::*;
  use xvfs_types::HashAlgorithm;

  fn commit() -> ObjectId {
    ObjectId::from_raw(HashAlgorithm::Sha1, &[7u8; 20]).unwrap()
  }

  fn entry(path: &str, key: BlobKey) -> PathEntry {
    PathEntry {
      path: BytePath::new(path.as_bytes().to_vec()),
      mode: xvfs_types::mode::REGULAR,
      key,
    }
  }

  fn manifest(entries: Vec<PathEntry>) -> Manifest {
    Manifest::build(commit(), entries)
  }

  #[test]
  fn a_round_trip_preserves_every_entry() {
    let m = manifest(vec![
      entry("src/main.rs", 3),
      entry("README.md", 1),
      entry("src/lib.rs", 2),
    ]);
    let encoded = m.encode();
    let back = Manifest::decode(&encoded).unwrap();
    assert_eq!(back.paths(), m.paths());
    assert_eq!(back.commit(), m.commit());
    assert_eq!(back.members(), m.members());
  }

  #[test]
  fn non_utf8_paths_survive_the_encoding() {
    // Front coding is a byte operation, and this is the case that would break if
    // anything in the pipeline reached for `str`.
    let mut e = entry("x", 1);
    e.path = BytePath::new(b"src/caf\xe9.rs".to_vec());
    let m = manifest(vec![e]);
    let back = Manifest::decode(&m.encode()).unwrap();
    assert_eq!(back.paths()[0].path.as_bytes(), b"src/caf\xe9.rs");
  }

  #[test]
  fn a_corrupted_byte_is_caught_rather_than_producing_wrong_results() {
    let m = manifest(vec![entry("a.rs", 1), entry("b.rs", 2)]);
    let mut encoded = m.encode();
    let middle = encoded.len() / 2;
    encoded[middle] ^= 0xff;
    let err = Manifest::decode(&encoded).unwrap_err();
    assert!(
      err.message.contains("checksum"),
      "a corrupted manifest must fail loudly, not return a different tree: {}",
      err.message
    );
  }

  #[test]
  fn a_future_format_version_is_refused() {
    let m = manifest(vec![entry("a.rs", 1)]);
    let mut encoded = m.encode();
    encoded[8] = 99;
    // Re-checksum so the version check is what fires, not the checksum.
    let len = encoded.len();
    let digest = Sha256::digest(&encoded[..len - 32]);
    encoded[len - 32..].copy_from_slice(&digest);
    let err = Manifest::decode(&encoded).unwrap_err();
    assert_eq!(err.code, ErrorCode::FailedPrecondition);
  }

  #[test]
  fn repeated_blobs_map_to_every_path_that_carries_them() {
    // The property that makes a query read one blob and report forty matches.
    let m = manifest(vec![
      entry("a/LICENSE", 5),
      entry("b/LICENSE", 5),
      entry("c/other", 6),
    ]);
    assert_eq!(m.paths_for_key(5).len(), 2);
    assert_eq!(m.members().len(), 2);
  }

  #[test]
  fn a_scope_stops_at_a_path_boundary() {
    let m = manifest(vec![
      entry("src/main.rs", 1),
      entry("src/util/x.rs", 2),
      entry("srcutil/y.rs", 3),
      entry("tests/a.rs", 4),
    ]);
    let range = m.scope(b"src");
    let selected: Vec<&[u8]> = m.paths()[range].iter().map(|e| e.path.as_bytes()).collect();
    assert_eq!(selected, vec![&b"src/main.rs"[..], &b"src/util/x.rs"[..]]);
  }

  #[test]
  fn an_exact_file_path_is_a_legal_scope() {
    let m = manifest(vec![entry("src/main.rs", 1), entry("src/lib.rs", 2)]);
    let range = m.scope(b"src/main.rs");
    assert_eq!(range.len(), 1);
    assert_eq!(m.paths()[range.start].path.as_bytes(), b"src/main.rs");
  }

  #[test]
  fn an_empty_scope_is_the_whole_snapshot() {
    let m = manifest(vec![entry("a", 1), entry("b", 2)]);
    assert_eq!(m.scope(b""), 0..2);
  }

  #[test]
  fn a_first_parent_delta_costs_the_diff_rather_than_the_tree() {
    let parent = manifest(vec![
      entry("a.rs", 1),
      entry("b.rs", 2),
      entry("dir/c.rs", 3),
    ]);
    let child = parent.apply(
      commit(),
      &[
        ManifestDelta::Removed {
          path: BytePath::new(b"b.rs".to_vec()),
        },
        ManifestDelta::Upserted {
          path: BytePath::new(b"a.rs".to_vec()),
          mode: xvfs_types::mode::EXECUTABLE,
          key: 9,
        },
        ManifestDelta::Upserted {
          path: BytePath::new(b"dir/d.rs".to_vec()),
          mode: xvfs_types::mode::REGULAR,
          key: 10,
        },
      ],
    );
    let paths: Vec<&[u8]> = child.paths().iter().map(|e| e.path.as_bytes()).collect();
    assert_eq!(
      paths,
      vec![&b"a.rs"[..], &b"dir/c.rs"[..], &b"dir/d.rs"[..]]
    );
    assert_eq!(child.paths()[0].key, 9);
    assert_eq!(child.paths()[0].mode, xvfs_types::mode::EXECUTABLE);
    assert!(!child.members().contains(2), "the removed blob is gone");
    // And the result is identical to a full build of the same content, which is
    // the invariant that makes incremental construction safe to trust.
    let full = manifest(vec![
      PathEntry {
        path: BytePath::new(b"a.rs".to_vec()),
        mode: xvfs_types::mode::EXECUTABLE,
        key: 9,
      },
      entry("dir/c.rs", 3),
      entry("dir/d.rs", 10),
    ]);
    assert_eq!(child.encode(), full.encode());
  }
}
