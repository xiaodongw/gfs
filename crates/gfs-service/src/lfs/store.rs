//! Filesystem layout and write discipline for the LFS store.

use std::io::Write;
use std::path::{Path, PathBuf};

use gfs_types::error::GfsError;
use gfs_types::{HashAlgorithm, ObjectId, RepositoryId};
use sha2::{Digest, Sha256};

/// A content-addressed store of expanded LFS objects, sharded per repository.
///
/// Layout: `<root>/<repository_id>/<hex[0..2]>/<hex>`. The two-byte fan-out is
/// the same shape the blob cache and Git's own loose objects use, for the same
/// reason: a repository can hold tens of thousands of LFS objects and one flat
/// directory would make every lookup a linear scan on some filesystems.
#[derive(Debug)]
pub struct LfsStore {
  root: PathBuf,
}

impl LfsStore {
  pub fn open(root: impl Into<PathBuf>) -> Result<Self, GfsError> {
    let root = root.into();
    std::fs::create_dir_all(&root)
      .map_err(|e| GfsError::internal(format!("creating LFS store {}: {e}", root.display())))?;
    Ok(LfsStore { root })
  }

  pub fn root(&self) -> &Path {
    &self.root
  }

  fn object_path(&self, repository: &RepositoryId, oid: &ObjectId) -> PathBuf {
    let hex = oid.to_hex();
    self
      .root
      .join(repository.as_str())
      .join(&hex[..2])
      .join(&hex)
  }

  /// Whether the store holds this object. This is the gate entry-metadata
  /// substitution checks: present means "expanded", absent means the entry
  /// degrades to its pointer.
  pub fn contains(&self, repository: &RepositoryId, oid: &ObjectId) -> bool {
    oid.algorithm() == HashAlgorithm::LfsSha256
      && self.object_path(repository, oid).is_file()
  }

  /// The on-disk path of a stored object, or `None` when it is not here.
  /// What the batch uploader hands to curl, so a multi-gigabyte upload never
  /// passes through this process's memory.
  pub fn object_path_for(&self, repository: &RepositoryId, oid: &ObjectId) -> Option<PathBuf> {
    let path = self.object_path(repository, oid);
    (oid.algorithm() == HashAlgorithm::LfsSha256 && path.is_file()).then_some(path)
  }

  /// Read an object's bytes, or `None` when the store does not hold it.
  ///
  /// The bytes are returned as stored, not re-hashed: publication verified
  /// them against their address, and the client's own cache verifies again on
  /// download, so a re-read here would only double the cost of every serve.
  pub fn read(
    &self,
    repository: &RepositoryId,
    oid: &ObjectId,
  ) -> Result<Option<Vec<u8>>, GfsError> {
    require_lfs_key(oid)?;
    match std::fs::read(self.object_path(repository, oid)) {
      Ok(bytes) => Ok(Some(bytes)),
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
      Err(e) => Err(GfsError::internal(format!(
        "reading LFS object {oid}: {e}"
      ))),
    }
  }

  /// Verify `content` against `oid` and publish it.
  ///
  /// Refuses bytes whose hash is not the address — a batch download that was
  /// truncated or corrupted must not become servable. Publishing an object
  /// that already exists is a no-op: content addressing makes the second copy
  /// byte-identical by construction.
  pub fn put(
    &self,
    repository: &RepositoryId,
    oid: &ObjectId,
    content: &[u8],
  ) -> Result<(), GfsError> {
    require_lfs_key(oid)?;
    let actual = Sha256::digest(content);
    if actual.as_slice() != oid.as_bytes() {
      return Err(GfsError::invalid(format!(
        "LFS content hashes to {}, not the claimed {}",
        hex(&actual),
        oid.to_hex()
      )));
    }

    let path = self.object_path(repository, oid);
    if path.is_file() {
      return Ok(());
    }
    let dir = path.parent().expect("object path always has a parent");
    std::fs::create_dir_all(dir)
      .map_err(|e| GfsError::internal(format!("creating {}: {e}", dir.display())))?;

    // Temp file in the destination directory: a rename across filesystems is
    // not a rename at all, and a crash mid-write must leave nothing servable.
    let tmp = dir.join(format!(".tmp-{}-{}", oid.to_hex(), std::process::id()));
    let result = (|| -> std::io::Result<()> {
      let mut f = std::fs::File::create(&tmp)?;
      f.write_all(content)?;
      f.sync_all()?;
      std::fs::rename(&tmp, &path)?;
      // The rename is durable only once the directory entry is. This is the
      // catalog's posture, not the blob cache's: cached data may be refetched
      // from the server after a crash, but this store may have no upstream
      // credential to refetch with.
      std::fs::File::open(dir)?.sync_all()?;
      Ok(())
    })();
    if result.is_err() {
      let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(|e| GfsError::internal(format!("publishing LFS object {oid}: {e}")))
  }
}

fn require_lfs_key(oid: &ObjectId) -> Result<(), GfsError> {
  if oid.algorithm() != HashAlgorithm::LfsSha256 {
    return Err(GfsError::invalid(format!(
      "the LFS store is keyed by lfs-sha256, not {}",
      oid.algorithm()
    )));
  }
  Ok(())
}

fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn oid_of(content: &[u8]) -> ObjectId {
    ObjectId::from_raw(HashAlgorithm::LfsSha256, &Sha256::digest(content)).unwrap()
  }

  #[test]
  fn put_verifies_publishes_and_read_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let store = LfsStore::open(tmp.path()).unwrap();
    let repo = RepositoryId::parse("r-lfs").unwrap();
    let content = b"expanded model weights";
    let oid = oid_of(content);

    assert!(!store.contains(&repo, &oid));
    assert_eq!(store.read(&repo, &oid).unwrap(), None);

    store.put(&repo, &oid, content).unwrap();
    assert!(store.contains(&repo, &oid));
    assert_eq!(store.read(&repo, &oid).unwrap().as_deref(), Some(&content[..]));

    // Idempotent republish.
    store.put(&repo, &oid, content).unwrap();
  }

  #[test]
  fn corrupt_content_is_refused_and_nothing_becomes_servable() {
    let tmp = tempfile::tempdir().unwrap();
    let store = LfsStore::open(tmp.path()).unwrap();
    let repo = RepositoryId::parse("r-lfs").unwrap();
    let oid = oid_of(b"the real bytes");

    let err = store.put(&repo, &oid, b"tampered bytes").unwrap_err();
    assert!(err.to_string().contains("hashes to"));
    assert!(!store.contains(&repo, &oid));
  }

  #[test]
  fn a_git_object_id_is_refused_as_a_store_key() {
    let tmp = tempfile::tempdir().unwrap();
    let store = LfsStore::open(tmp.path()).unwrap();
    let repo = RepositoryId::parse("r-lfs").unwrap();
    let sha1 = ObjectId::from_hex(HashAlgorithm::Sha1, &"ab".repeat(20)).unwrap();
    assert!(store.put(&repo, &sha1, b"x").is_err());
    assert!(!store.contains(&repo, &sha1));
  }
}
