//! Canonical content hashing for overlay content.
//!
//! Two verifiable forms, matching the client's blob cache: the Git blob frame
//! `blob <size>\0<content>` under SHA-1, and ADR 0012's LFS key — SHA-256 over
//! the raw bytes, no frame — for content whose base entry is an expanded LFS
//! object. Status needs both to answer a question the journal cannot: a file
//! that was written and then written back to its original bytes has a journal
//! row, but it is *not* a change, and reporting it as one would put a spurious
//! entry in every `git status` an agent runs after an aborted edit.
//!
//! Git's own SHA-256 object format stays refused, because ADR 0001 measured
//! that SHA-256 repositories are unreachable through the pinned `git2-rs`
//! build and are rejected at ingest. The algorithm is still taken from the
//! caller rather than assumed, so the day that changes this fails loudly
//! instead of producing wrong object IDs.

use std::io::Read;

use gfs_types::{HashAlgorithm, ObjectId};
use sha1::{Digest, Sha1};
use sha2::Sha256;

use crate::error::{OverlayError, Result};

/// Hash bytes as a Git blob.
pub fn blob_oid(algorithm: HashAlgorithm, bytes: &[u8]) -> Result<ObjectId> {
  let mut hasher = start(algorithm, bytes.len() as u64)?;
  hasher.update(bytes);
  finish(algorithm, hasher)
}

/// Hash a file as a Git blob without holding it in memory.
pub fn blob_oid_of_file(
  algorithm: HashAlgorithm,
  file: &mut std::fs::File,
  size: u64,
) -> Result<ObjectId> {
  let mut hasher = start(algorithm, size)?;
  let mut buffer = vec![0u8; 64 * 1024];
  loop {
    let read = file
      .read(&mut buffer)
      .map_err(|e| OverlayError::io(format!("hashing overlay content: {e}")))?;
    if read == 0 {
      break;
    }
    hasher.update(&buffer[..read]);
  }
  finish(algorithm, hasher)
}

enum Hasher {
  Git(Sha1),
  Lfs(Sha256),
}

impl Hasher {
  fn update(&mut self, bytes: impl AsRef<[u8]>) {
    match self {
      Hasher::Git(h) => h.update(bytes),
      Hasher::Lfs(h) => h.update(bytes),
    }
  }
}

fn start(algorithm: HashAlgorithm, size: u64) -> Result<Hasher> {
  match algorithm {
    HashAlgorithm::Sha1 => {
      let mut hasher = Sha1::new();
      hasher.update(b"blob ");
      hasher.update(size.to_string().as_bytes());
      hasher.update([0u8]);
      Ok(Hasher::Git(hasher))
    }
    // The LFS oid is the raw content hash: no frame, no size prefix.
    HashAlgorithm::LfsSha256 => Ok(Hasher::Lfs(Sha256::new())),
    HashAlgorithm::Sha256 => Err(OverlayError::io(format!(
      "{algorithm} object hashing is not available in this build (ADR 0001)"
    ))),
  }
}

fn finish(algorithm: HashAlgorithm, hasher: Hasher) -> Result<ObjectId> {
  let digest = match hasher {
    Hasher::Git(h) => h.finalize().to_vec(),
    Hasher::Lfs(h) => h.finalize().to_vec(),
  };
  ObjectId::from_raw(algorithm, &digest)
    .map_err(|e| OverlayError::io(format!("hashing overlay content: {e}")))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_empty_blob_matches_gits_well_known_object_id() {
    // `git hash-object -t blob /dev/null`. A constant rather than a computed
    // expectation on purpose: it is the one value that proves the framing, not
    // just the digest, is right.
    assert_eq!(
      blob_oid(HashAlgorithm::Sha1, b"").unwrap().to_hex(),
      "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
    );
  }

  #[test]
  fn a_known_blob_matches_git_hash_object() {
    // `printf 'hello\n' | git hash-object --stdin`
    assert_eq!(
      blob_oid(HashAlgorithm::Sha1, b"hello\n").unwrap().to_hex(),
      "ce013625030ba8dba906f756967f9e9ca394464a"
    );
  }
}
