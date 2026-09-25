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

/// What seeding a workspace's caches needs to know about that workspace.
///
/// A fresh workspace's first `git status` pays for two things Git builds on
/// first run and persists afterwards: an `lstat` of every entry (no fsmonitor
/// state yet) and a `readdir` of every directory (no untracked cache yet). On
/// universe that is 14 s and 61 s through FUSE. Both describe the pristine
/// pin, which this index already describes, so the mount writes them itself.
#[derive(Debug)]
pub struct WorkspaceCaches<'a> {
  /// Git's untracked-cache ident: `Location <worktree>, system <sysname>`,
  /// with the worktree as Git resolves it (its realpath).
  pub ident: &'a str,
  /// The `dir_flags` Git will ask for: `status.showUntrackedFiles=all` asks
  /// for none, anything else for `SHOW_OTHER_DIRECTORIES|HIDE_EMPTY_DIRECTORIES`.
  pub dir_flags: u32,
  /// Blob IDs of `$GIT_DIR/info/exclude` and `core.excludesFile`, `None` when
  /// the file does not exist.
  pub info_exclude: Option<[u8; 20]>,
  pub excludes_file: Option<[u8; 20]>,
  /// The token the fsmonitor hook will next be asked about.
  pub fsmonitor_token: &'a str,
}

/// The index with the untracked cache (`UNTR`) and fsmonitor (`FSMN`)
/// extensions appended, as Git would have written them after one status of
/// the pristine pin. `None` for an index this did not write — anything but
/// SHA-1 version 2 without extended flags — which the caller seeds unchanged.
///
/// Why each piece is safe, read against Git's `dir.c` and `fsmonitor.c`:
///
/// * `FSMN` marks every entry valid under the given token. Git asks the hook
///   what changed since it; the daemon answers with *every* overlay change
///   relative to the pin (not a delta), or `/` for a token from another
///   generation, which makes Git re-check everything.
/// * `UNTR` records every directory of the tree as valid with no untracked
///   entries. Git trusts a valid directory without `lstat` only after a
///   non-trivial fsmonitor answer, which also invalidates the directories of
///   every path it names; after a `/` answer it compares each directory's
///   recorded stat data, which is zero here, so every directory is re-read.
/// * Each directory's `.gitignore` blob ID, and the two global exclude files',
///   are compared with what is there now; a mismatch invalidates rather than
///   trusts. So a wrong guess anywhere below costs a walk, never an answer.
pub fn with_workspace_caches(index: &[u8], caches: &WorkspaceCaches<'_>) -> Option<Vec<u8>> {
  const HASH: usize = 20;
  if index.len() < 12 + HASH || &index[..4] != b"DIRC" || index[4..8] != 2u32.to_be_bytes() {
    return None;
  }
  let count = u32::from_be_bytes(index[8..12].try_into().ok()?) as usize;

  // The directory tree, from the entries' paths. Index order is byte order of
  // whole paths, so everything under a directory is contiguous and one pass
  // with the chain of open directories builds the tree without a lookup.
  struct Dir {
    name: Vec<u8>,
    children: Vec<usize>,
    gitignore: Option<[u8; HASH]>,
  }
  let mut dirs = vec![Dir {
    name: Vec::new(),
    children: Vec::new(),
    gitignore: None,
  }];
  let mut open: Vec<usize> = Vec::new();
  let mut pos = 12;
  for _ in 0..count {
    let fixed = index.get(pos..pos + 62)?;
    let mode = u32::from_be_bytes(fixed[24..28].try_into().ok()?);
    let oid: [u8; HASH] = fixed[40..60].try_into().ok()?;
    let flags = u16::from_be_bytes(fixed[60..62].try_into().ok()?);
    if flags & 0x4000 != 0 {
      return None; // extended flags: not an index this wrote
    }
    let rest = index.get(pos + 62..)?;
    let len = match (flags & 0xFFF) as usize {
      0xFFF => rest.iter().position(|b| *b == 0)?,
      len => len,
    };
    let path = rest.get(..len)?;
    pos += (62 + len) / 8 * 8 + 8;

    let mut components: Vec<&[u8]> = path.split(|b| *b == b'/').collect();
    let file = components.pop()?;
    let kept = open
      .iter()
      .zip(&components)
      .take_while(|(dir, name)| dirs[**dir].name == **name)
      .count();
    open.truncate(kept);
    for name in &components[kept..] {
      let parent = open.last().copied().unwrap_or(0);
      let child = dirs.len();
      dirs.push(Dir {
        name: name.to_vec(),
        children: Vec::new(),
        gitignore: None,
      });
      dirs[parent].children.push(child);
      open.push(child);
    }
    if file == b".gitignore" && matches!(mode, 0o100644 | 0o100755) {
      dirs[open.last().copied().unwrap_or(0)].gitignore = Some(oid);
    }
  }
  // Siblings in plain byte order of their names: Git's `lookup_untracked`
  // binary-searches them with `strncmp`, and index order is not that order
  // (`a-b/` sorts before `a/` there).
  for dir in 0..dirs.len() {
    let mut children = std::mem::take(&mut dirs[dir].children);
    children.sort_by(|a, b| dirs[*a].name.cmp(&dirs[*b].name));
    dirs[dir].children = children;
  }
  if index.len() < pos + HASH {
    return None;
  }

  // Depth-first, parents before children: the order Git writes and reads.
  let mut order = Vec::with_capacity(dirs.len());
  let mut stack = vec![0usize];
  while let Some(dir) = stack.pop() {
    order.push(dir);
    stack.extend(dirs[dir].children.iter().rev());
  }

  let mut untr = Vec::with_capacity(dirs.len() * 64);
  let mut ident = caches.ident.as_bytes().to_vec();
  ident.push(0); // Git stores the ident NUL-terminated and counts the NUL
  encode_varint(ident.len() as u64, &mut untr);
  untr.extend_from_slice(&ident);
  untr.extend_from_slice(&[0u8; 36 * 2]); // stat data of the two exclude files: unchecked
  untr.extend_from_slice(&caches.dir_flags.to_be_bytes());
  untr.extend_from_slice(&caches.info_exclude.unwrap_or([0; HASH]));
  untr.extend_from_slice(&caches.excludes_file.unwrap_or([0; HASH]));
  untr.extend_from_slice(b".gitignore\0");
  encode_varint(order.len() as u64, &mut untr);
  for &dir in &order {
    encode_varint(0, &mut untr); // untracked entries
    encode_varint(dirs[dir].children.len() as u64, &mut untr);
    untr.extend_from_slice(&dirs[dir].name);
    untr.push(0);
  }
  untr.extend_from_slice(&ewah(order.len(), |_| true)); // valid
  untr.extend_from_slice(&ewah(order.len(), |_| false)); // check-only
  untr.extend_from_slice(&ewah(order.len(), |i| dirs[order[i]].gitignore.is_some()));
  // Stat data per valid directory. Zero never matches a real directory, so a
  // run without a usable fsmonitor answer re-reads every one.
  untr.resize(untr.len() + 36 * order.len(), 0);
  for &dir in &order {
    if let Some(oid) = dirs[dir].gitignore {
      untr.extend_from_slice(&oid);
    }
  }
  untr.push(0);

  let mut fsmn = Vec::with_capacity(64);
  fsmn.extend_from_slice(&2u32.to_be_bytes());
  fsmn.extend_from_slice(caches.fsmonitor_token.as_bytes());
  fsmn.push(0);
  let dirty = ewah(count, |_| false);
  fsmn.extend_from_slice(&(dirty.len() as u32).to_be_bytes());
  fsmn.extend_from_slice(&dirty);

  let mut out = Vec::with_capacity(index.len() + untr.len() + fsmn.len() + 16);
  out.extend_from_slice(&index[..index.len() - HASH]);
  for (signature, body) in [(b"UNTR", &untr), (b"FSMN", &fsmn)] {
    out.extend_from_slice(signature);
    out.extend_from_slice(&u32::try_from(body.len()).ok()?.to_be_bytes());
    out.extend_from_slice(body);
  }
  let digest = Sha1::digest(&out);
  out.extend_from_slice(&digest);
  Some(out)
}

/// The blob ID Git gives `content`, as `git hash-object` would.
pub fn blob_id(content: &[u8]) -> [u8; 20] {
  let mut hasher = Sha1::new();
  hasher.update(format!("blob {}\0", content.len()).as_bytes());
  hasher.update(content);
  hasher.finalize().into()
}

/// Git's `encode_varint` (varint.c): big-endian base-128 where each
/// continuation also subtracts one, so every value has one encoding.
fn encode_varint(mut value: u64, out: &mut Vec<u8>) {
  let mut buf = [0u8; 16];
  let mut pos = buf.len() - 1;
  buf[pos] = (value & 127) as u8;
  loop {
    value >>= 7;
    if value == 0 {
      break;
    }
    value -= 1;
    pos -= 1;
    buf[pos] = 128 | (value & 127) as u8;
  }
  out.extend_from_slice(&buf[pos..]);
}

/// A serialized EWAH bitmap of `bits` bits (ewah/ewah_io.c): bit count, word
/// count, the words, then the index of the last run-length word. Runs of
/// all-zero or all-one words collapse into the run-length word before a
/// stretch of literal words; bit 0 of a run-length word is the run's bit,
/// bits 1-32 its length, bits 33-63 how many literal words follow.
fn ewah(bits: usize, set: impl Fn(usize) -> bool) -> Vec<u8> {
  const MAX_RUN: usize = (1 << 32) - 1;
  const MAX_LITERALS: usize = (1 << 31) - 1;
  let mut plain = vec![0u64; bits.div_ceil(64)];
  for bit in (0..bits).filter(|&b| set(b)) {
    plain[bit / 64] |= 1 << (bit % 64);
  }
  let mut words: Vec<u64> = Vec::new();
  let mut last_rlw = 0usize;
  let mut i = 0;
  while i < plain.len() || words.is_empty() {
    let (run_bit, mut run) = match plain.get(i) {
      Some(&0) => (0u64, 0usize),
      Some(&u64::MAX) => (1, 0),
      _ => (0, 0),
    };
    while run < MAX_RUN && plain.get(i) == Some(&if run_bit == 1 { u64::MAX } else { 0 }) {
      run += 1;
      i += 1;
    }
    let start = i;
    while i < plain.len() && i - start < MAX_LITERALS && plain[i] != 0 && plain[i] != u64::MAX {
      i += 1;
    }
    last_rlw = words.len();
    words.push(run_bit | (run as u64) << 1 | ((i - start) as u64) << 33);
    words.extend_from_slice(&plain[start..i]);
  }
  let mut out = Vec::with_capacity(12 + words.len() * 8);
  out.extend_from_slice(&(bits as u32).to_be_bytes());
  out.extend_from_slice(&(words.len() as u32).to_be_bytes());
  for word in &words {
    out.extend_from_slice(&word.to_be_bytes());
  }
  out.extend_from_slice(&(last_rlw as u32).to_be_bytes());
  out
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
