//! Build a Git index file for a commit, with stat data a projection satisfies.
//!
//! ADR 0009's workspace has a real `.git` on local disk whose index the gateway
//! ships, because building it client-side would mean walking the whole tree
//! through the snapshot API — the metadata sweep GFS exists to avoid. The server
//! walks its local object database instead and produces the file once per
//! commit, shared by every mount of it.
//!
//! The stat fields are the entire point. `git status` compares each entry's
//! recorded stat data against `lstat` of the working tree, and the spike
//! measured what happens when they disagree: with real values from another
//! filesystem, every entry is stat-dirty and Git re-hashes the tree — 1 615 MiB
//! on the Linux kernel. So every entry here records:
//!
//! * `mtime = snapshot_time` — the deterministic per-commit time DESIGN.md
//!   section 8.2 makes every projected entry report, identical on every host;
//! * `size` — the blob's true size, which the projection also reports;
//! * `dev`, `ino`, `uid`, `gid`, `ctime` — zero, which is why the workspace
//!   config must set `core.checkStat=minimal` and `core.trustctime=false`;
//!   those settings exclude exactly these fields from the comparison.
//!
//! One racy-clean subtlety: Git distrusts any entry whose mtime is not older
//! than the index file's own mtime, and re-hashes it. `snapshot_time` is
//! clamped below the commit's first-seen time (ADR 0006), and the index file is
//! written at mount time, so every entry is safely in the past.
//!
//! Format: index version 2, the oldest still-current format, chosen because
//! every Git and libgit2 in the support matrix reads it and nothing GFS needs —
//! no extensions, no split index, no untracked cache — requires anything newer.

use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::{HashAlgorithm, ObjectId, Timestamp};
use sha1::{Digest, Sha1};

/// One index entry, in the order the walk yields them.
#[derive(Debug, Clone)]
pub struct IndexEntry {
  /// Repository-relative path, no leading slash.
  pub path: Vec<u8>,
  /// The Git tree mode: `0o100644`, `0o100755`, `0o120000`, or `0o160000`.
  pub mode: u32,
  pub oid: ObjectId,
  pub size: u64,
}

/// Serialize entries into a version-2 index file.
///
/// Entries must arrive in Git's index order: byte-wise by path. The tree walk
/// that produces them yields exactly that order, and this asserts it rather than
/// sorting, because a mis-ordered index is silently misread by Git — entries
/// after the first inversion are simply not found.
pub fn write_index_v2(
  entries: &[IndexEntry],
  snapshot_time: Timestamp,
) -> Result<Vec<u8>, GfsError> {
  let mut out = Vec::with_capacity(entries.len() * 80 + 32);
  out.extend_from_slice(b"DIRC");
  out.extend_from_slice(&2u32.to_be_bytes());
  let count = u32::try_from(entries.len())
    .map_err(|_| GfsError::new(ErrorCode::ResourceLimit, "too many entries for an index"))?;
  out.extend_from_slice(&count.to_be_bytes());

  // Truncation to u32 is the index format's own: seconds wrap in 2106, and
  // nanoseconds always fit.
  let secs = snapshot_time.secs as u32;
  let nanos = snapshot_time.nanos;

  let mut previous: Option<&[u8]> = None;
  for entry in entries {
    if let Some(prev) = previous {
      if prev >= entry.path.as_slice() {
        return Err(GfsError::internal(format!(
          "index entries out of order: {:?} then {:?}",
          String::from_utf8_lossy(prev),
          String::from_utf8_lossy(&entry.path)
        )));
      }
    }
    previous = Some(entry.path.as_slice());

    if entry.oid.algorithm() != HashAlgorithm::Sha1 {
      return Err(GfsError::new(
        ErrorCode::UnsupportedRepositoryFormat,
        "index v2 records SHA-1 object IDs (ADR 0001)",
      ));
    }

    let start = out.len();
    out.extend_from_slice(&secs.to_be_bytes()); // ctime seconds
    out.extend_from_slice(&nanos.to_be_bytes()); // ctime nanoseconds
    out.extend_from_slice(&secs.to_be_bytes()); // mtime seconds
    out.extend_from_slice(&nanos.to_be_bytes()); // mtime nanoseconds
    out.extend_from_slice(&0u32.to_be_bytes()); // dev  -- excluded by checkStat=minimal
    out.extend_from_slice(&0u32.to_be_bytes()); // ino  -- excluded by checkStat=minimal
    out.extend_from_slice(&entry.mode.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // uid  -- excluded by checkStat=minimal
    out.extend_from_slice(&0u32.to_be_bytes()); // gid  -- excluded by checkStat=minimal
                                                // A >4 GiB file's size wraps; Git handles that by re-hashing on match
                                                // failure, which is the correct degradation for a case the corpus does not
                                                // contain.
    out.extend_from_slice(&(entry.size as u32).to_be_bytes());
    out.extend_from_slice(entry.oid.as_bytes());
    let name_len = entry.path.len().min(0xFFF) as u16;
    out.extend_from_slice(&name_len.to_be_bytes()); // flags: no assume-valid, stage 0
    out.extend_from_slice(&entry.path);
    // Pad with NULs until the entry length is a multiple of 8, always at least
    // one NUL so the path is terminated.
    let entry_len = out.len() - start;
    let padded = (entry_len / 8 + 1) * 8;
    out.resize(start + padded, 0);
  }

  let digest = Sha1::digest(&out);
  out.extend_from_slice(&digest);
  Ok(out)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn ts(secs: i64) -> Timestamp {
    Timestamp { secs, nanos: 0 }
  }

  fn entry(path: &str, mode: u32, size: u64) -> IndexEntry {
    IndexEntry {
      path: path.as_bytes().to_vec(),
      mode,
      oid: ObjectId::from_raw(HashAlgorithm::Sha1, &[7u8; 20]).unwrap(),
      size,
    }
  }

  #[test]
  fn the_header_counts_and_the_trailer_hashes() {
    let bytes = write_index_v2(&[entry("a.txt", 0o100644, 3)], ts(1_600_000_000)).unwrap();
    assert_eq!(&bytes[0..4], b"DIRC");
    assert_eq!(u32::from_be_bytes(bytes[4..8].try_into().unwrap()), 2);
    assert_eq!(u32::from_be_bytes(bytes[8..12].try_into().unwrap()), 1);
    let body = &bytes[..bytes.len() - 20];
    let trailer = &bytes[bytes.len() - 20..];
    assert_eq!(trailer, Sha1::digest(body).as_slice());
  }

  #[test]
  fn out_of_order_entries_are_refused_not_silently_misread() {
    // Git binary-searches the index, so entries after an inversion are simply
    // not found -- a quiet lie. Refusing loudly is the only safe behaviour.
    let err = write_index_v2(
      &[entry("b.txt", 0o100644, 1), entry("a.txt", 0o100644, 1)],
      ts(1_600_000_000),
    )
    .unwrap_err();
    assert!(format!("{err}").contains("out of order"), "{err}");
  }
}
