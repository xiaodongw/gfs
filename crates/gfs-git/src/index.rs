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
//! no split index and no untracked cache — requires anything newer.
//!
//! One extension is written: `TREE`, the cache tree. Without it Git cannot know
//! that any directory is unchanged, so the *first* `git commit` in a workspace
//! re-derives every tree in the repository and writes each one out — measured at
//! 8.06 s and 3 254 loose objects on django, 12.34 s and 4 299 on vscode, for a
//! five-file change. With it, the same commits cost 0.10 s / 15 objects and
//! 0.206 s / 25. Git only paid that once per workspace, because it persists the
//! cache tree it was forced to build; a workspace's first commit is exactly the
//! one that matters here.
//!
//! The cache tree is safe to ship precisely because the index is generated from
//! a commit: every directory in it is unmodified by construction, so no node is
//! ever invalid. Git *trusts* a well-formed cache tree — a wrong OID or entry
//! count produces a wrong commit tree with no error — which is why both come
//! from the same walk that produces the entries, and why `write_index_v2` checks
//! their arithmetic against the entries it was handed.

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

/// One directory of the `TREE` extension: the tree object it hashes to, how many
/// index entries it covers, and its subdirectories.
///
/// A gitlink is *not* a node here. It is one index entry in its parent, and the
/// tree it names lives in another repository — which is also why the walk that
/// builds this never recurses into one.
#[derive(Debug, Clone)]
pub struct CacheTree {
  /// The path component, empty for the root.
  pub name: Vec<u8>,
  /// The tree object this directory hashes to.
  pub oid: ObjectId,
  /// Index entries covered, counted recursively.
  pub entries: u32,
  /// Direct subdirectories. `write_index_v2` sorts these.
  pub children: Vec<CacheTree>,
}

/// Git's `subtree_name_cmp`: shorter names first, then bytes.
///
/// This is *not* the order the enclosing tree lists them in — a Git tree sorts a
/// directory as though its name ended in `/`, so `a-b` precedes `a` there and
/// follows it here. The corpus hits the difference (vscode's `extensions/`
/// among others), so the walk's order cannot be reused.
fn subtree_name_cmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
  a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn write_cache_tree(out: &mut Vec<u8>, node: &CacheTree) -> Result<(), GfsError> {
  out.extend_from_slice(&node.name);
  out.push(0);
  out.extend_from_slice(node.entries.to_string().as_bytes());
  out.push(b' ');
  out.extend_from_slice(node.children.len().to_string().as_bytes());
  out.push(b'\n');
  if node.oid.algorithm() != HashAlgorithm::Sha1 {
    return Err(GfsError::new(
      ErrorCode::UnsupportedRepositoryFormat,
      "index v2 records SHA-1 object IDs (ADR 0001)",
    ));
  }
  out.extend_from_slice(node.oid.as_bytes());
  for child in &node.children {
    write_cache_tree(out, child)?;
  }
  Ok(())
}

/// Put every level in Git's order, and refuse a level that names a directory
/// twice.
fn sort_cache_tree(node: &mut CacheTree) -> Result<(), GfsError> {
  node
    .children
    .sort_by(|a, b| subtree_name_cmp(&a.name, &b.name));
  for pair in node.children.windows(2) {
    if subtree_name_cmp(&pair[0].name, &pair[1].name) != std::cmp::Ordering::Less {
      return Err(GfsError::internal(format!(
        "cache tree names the directory {:?} twice",
        String::from_utf8_lossy(&pair[0].name)
      )));
    }
  }
  for child in &mut node.children {
    sort_cache_tree(child)?;
  }
  Ok(())
}

/// Check every recorded entry count against the entries themselves.
///
/// Git trusts these numbers: a directory whose count is right is reused whole at
/// the next commit, and one whose count is wrong yields a wrong tree with no
/// diagnostic. Entries are sorted by path, so the ones under a directory are a
/// contiguous run and the true count is a range lookup — cheap enough to pay on
/// every index rather than trusting the walk.
fn verify_cache_tree(
  node: &CacheTree,
  prefix: &mut Vec<u8>,
  entries: &[IndexEntry],
) -> Result<(), GfsError> {
  let start = entries.partition_point(|e| e.path.as_slice() < prefix.as_slice());
  let covered = entries[start..]
    .iter()
    .take_while(|e| e.path.starts_with(prefix))
    .count();
  if covered != node.entries as usize {
    return Err(GfsError::internal(format!(
      "cache tree for {:?} claims {} entries but the index holds {}",
      String::from_utf8_lossy(prefix),
      node.entries,
      covered
    )));
  }
  for child in &node.children {
    let mark = prefix.len();
    prefix.extend_from_slice(&child.name);
    prefix.push(b'/');
    verify_cache_tree(child, prefix, entries)?;
    prefix.truncate(mark);
  }
  Ok(())
}

/// Serialize entries into a version-2 index file, with an optional cache tree.
///
/// Entries must arrive in Git's index order: byte-wise by path. The tree walk
/// that produces them yields exactly that order, and this asserts it rather than
/// sorting, because a mis-ordered index is silently misread by Git — entries
/// after the first inversion are simply not found.
pub fn write_index_v2(
  entries: &[IndexEntry],
  snapshot_time: Timestamp,
  cache_tree: Option<CacheTree>,
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

  if let Some(mut root) = cache_tree {
    sort_cache_tree(&mut root)?;
    verify_cache_tree(&root, &mut Vec::new(), entries)?;
    let mut body = Vec::with_capacity(root.entries as usize * 8);
    write_cache_tree(&mut body, &root)?;
    let size = u32::try_from(body.len()).map_err(|_| {
      GfsError::new(
        ErrorCode::ResourceLimit,
        "cache tree is too large to record",
      )
    })?;
    out.extend_from_slice(b"TREE");
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(&body);
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
    let bytes = write_index_v2(&[entry("a.txt", 0o100644, 3)], ts(1_600_000_000), None).unwrap();
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
      None,
    )
    .unwrap_err();
    assert!(format!("{err}").contains("out of order"), "{err}");
  }
}
