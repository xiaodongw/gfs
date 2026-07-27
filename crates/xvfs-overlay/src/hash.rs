//! Canonical Git object hashing for overlay content.
//!
//! `blob <size>\0<content>`, the same framing the client's blob cache verifies
//! against. Status needs it to answer a question the journal cannot: a file that
//! was written and then written back to its original bytes has a journal row, but
//! it is *not* a change, and reporting it as one would put a spurious entry in
//! every `git status` an agent runs after an aborted edit.
//!
//! SHA-1 only, because ADR 0001 measured that SHA-256 repositories are
//! unreachable through the pinned `git2-rs` build and are rejected at ingest. The
//! algorithm is still taken from the repository rather than assumed, so the day
//! that changes this fails loudly instead of producing wrong object IDs.

use std::io::Read;

use sha1::{Digest, Sha1};
use xvfs_types::{HashAlgorithm, ObjectId};

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

fn start(algorithm: HashAlgorithm, size: u64) -> Result<Sha1> {
  if algorithm != HashAlgorithm::Sha1 {
    return Err(OverlayError::io(format!(
      "{algorithm} object hashing is not available in this build (ADR 0001)"
    )));
  }
  let mut hasher = Sha1::new();
  hasher.update(b"blob ");
  hasher.update(size.to_string().as_bytes());
  hasher.update([0u8]);
  Ok(hasher)
}

fn finish(algorithm: HashAlgorithm, hasher: Sha1) -> Result<ObjectId> {
  ObjectId::from_raw(algorithm, &hasher.finalize())
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
