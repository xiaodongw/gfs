//! The inode table.
//!
//! DESIGN.md section 8.2 states the contract: base inodes are stable for the life
//! of the mount and derived from the mount identity plus entry identity, and are
//! deliberately *not* stable across mounts, so build tooling that caches state
//! keyed on `(device, inode)` between jobs misses rather than producing a stale
//! hit.
//!
//! # Two maps, with different lifetimes, and that is the point
//!
//! * `by_path` maps a path to its inode number and is **never** pruned. This is
//!   what makes an inode number stable: a path that is looked up, forgotten, and
//!   looked up again gets the same number both times.
//! * `records` holds the metadata behind a live inode and **is** pruned when the
//!   kernel's lookup count reaches zero and no descriptor is open.
//!
//! Keeping only one map would force a choice between the two properties. Pruning
//! both would let a re-looked-up path acquire a different inode number mid-job,
//! which is exactly the stale-hit hazard the design calls out. Pruning neither
//! would make a full tree walk retain every entry's metadata for the life of the
//! mount, and a monorepo has millions of them.
//!
//! Because a number is never reused for a different path, the FUSE generation is
//! always zero. Generations exist to disambiguate a reused inode number; there is
//! nothing to disambiguate here.
//!
//! # Why `.git` shares the table
//!
//! Git refuses to record a tree entry named `.git` at any level, so a `.git`
//! path can never collide with a base path. One table therefore needs no
//! namespace tag, and `lookup` does not have to ask which world a path belongs
//! to before it can answer.

use std::collections::HashMap;

use gfs_types::{BytePath, EntryKind, TreeEntryInfo};

use gfs_overlay::OverlayEntry;

use crate::passthrough::{GitMeta, OdbNode};

/// The FUSE root inode. Fixed by the protocol, not by us.
pub const ROOT_INO: u64 = 1;

/// What an inode is backed by.
#[derive(Clone, Debug)]
pub enum Node {
  /// An entry of the pinned commit's tree.
  Base(TreeEntryInfo),
  /// An entry the overlay supplies: created, copied up, renamed, or with a mode
  /// the base does not have.
  Overlay(Box<OverlayEntry>),
  /// A real entry of the shadowed `.git`, passed through (ADR 0011). The meta
  /// is a snapshot; the passthrough re-stats on lookup and getattr.
  Git(GitMeta),
  /// An entry of the object-store projection at `.git/gfs/objects` (ADR 0009,
  /// presented inside the one mount by ADR 0011).
  Odb(OdbNode),
}

impl Node {
  /// Whether this inode belongs to the `.git` passthrough subtree.
  pub fn is_git(&self) -> bool {
    matches!(self, Node::Git(_))
  }

  /// Whether this inode belongs to the read-only object projection.
  pub fn is_odb(&self) -> bool {
    matches!(self, Node::Odb(_))
  }

  /// The inode number the node insists on, if it has one of its own.
  ///
  /// An overlay row carries its number in the journal, so it survives a daemon
  /// restart and a rename. Base entries have no opinion and take whatever the
  /// table's counter hands them.
  /// A regular file, which is the one kind whose size the kernel owns under
  /// writeback caching (see `Record::generation`).
  pub fn is_regular_file(&self) -> bool {
    match self {
      Node::Base(entry) => matches!(entry.kind, EntryKind::Regular | EntryKind::Executable),
      Node::Overlay(entry) => matches!(
        entry.kind,
        gfs_overlay::OverlayKind::Regular | gfs_overlay::OverlayKind::Executable
      ),
      Node::Git(meta) => meta.kind == fuser::FileType::RegularFile,
      Node::Odb(node) => !node.is_dir(),
    }
  }

  pub fn preferred_ino(&self) -> Option<u64> {
    match self {
      Node::Overlay(entry) => Some(entry.ino),
      _ => None,
    }
  }
}

#[derive(Clone, Debug)]
pub struct Record {
  pub ino: u64,
  pub path: BytePath,
  pub node: Node,
  /// The inode generation the kernel is told at lookup. Bumped when a regular
  /// file's bytes change behind the kernel -- a re-pin -- because with the
  /// writeback cache on, the kernel trusts its own size and mtime for a
  /// regular file over anything `getattr` says, and only a new generation
  /// makes it drop the inode and start over.
  pub generation: u64,
  /// The kernel's outstanding lookup count. Decremented by `forget`.
  lookups: u64,
  /// Open file and directory handles. An inode is not dropped while any exist,
  /// even at zero lookups: the kernel is entitled to forget a name while a
  /// descriptor obtained through it is still open.
  opens: u32,
}

#[derive(Debug, Default)]
pub struct InodeTable {
  by_path: HashMap<Vec<u8>, u64>,
  records: HashMap<u64, Record>,
  next: u64,
}

/// A kernel-cached name a re-pin has moved out from under: the dentry to drop
/// (`parent` + `name`) and the inode whose cached pages -- file content, or a
/// directory listing -- may no longer describe what the path holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaleEntry {
  pub parent: u64,
  pub name: Vec<u8>,
  pub ino: u64,
}

impl InodeTable {
  /// Build a table whose root is the snapshot root.
  pub fn new(root: TreeEntryInfo) -> Self {
    let mut table = InodeTable {
      by_path: HashMap::new(),
      records: HashMap::new(),
      next: ROOT_INO + 1,
    };
    table.by_path.insert(Vec::new(), ROOT_INO);
    table.records.insert(
      ROOT_INO,
      Record {
        ino: ROOT_INO,
        path: BytePath::root(),
        node: Node::Base(root),
        generation: 0,
        // The root is never forgotten by the kernel, and a `forget` for it would
        // otherwise be able to drop the record every other operation depends on.
        lookups: 1,
        opens: 0,
      },
    );
    table
  }

  pub fn get(&self, ino: u64) -> Option<&Record> {
    self.records.get(&ino)
  }

  pub fn path(&self, ino: u64) -> Option<BytePath> {
    self.records.get(&ino).map(|r| r.path.clone())
  }

  /// The stable inode number for a path, assigning one on first sight.
  fn number_for(&mut self, path: &BytePath) -> u64 {
    if let Some(ino) = self.by_path.get(path.as_bytes()) {
      return *ino;
    }
    let ino = self.next;
    self.next += 1;
    self.by_path.insert(path.as_bytes().to_vec(), ino);
    ino
  }

  /// The stable inode number for a path, assigning one on first sight.
  ///
  /// Public because a mutation has to know the number *before* it writes the
  /// journal row: the row records it so a restarted daemon can hand out the same
  /// one, and a row that disagreed with the live table would make a path change
  /// identity across a restart for no reason.
  pub fn number_for_path(&mut self, path: &BytePath) -> u64 {
    self.number_for(path)
  }

  /// Adopt a number a node brought with it, unless the path already has one.
  ///
  /// Advisory rather than authoritative, and that direction matters: the kernel
  /// keeps a dentry's inode across a `rename(2)` without ever asking, so a live
  /// number always outranks a number recorded in the journal. The journal's copy
  /// is what a restart falls back to, when there are no dentries left to disagree
  /// with.
  fn bind_if_absent(&mut self, path: &BytePath, ino: u64) {
    if self.by_path.contains_key(path.as_bytes()) {
      return;
    }
    self.by_path.insert(path.as_bytes().to_vec(), ino);
    if ino >= self.next && ino < gfs_overlay::OVERLAY_INO_BASE {
      self.next = ino + 1;
    }
  }

  /// Move a path and everything under it, keeping every inode number.
  ///
  /// The kernel does not re-look-up after a `rename(2)`: it relinks the dentry it
  /// already has, so the inode it will send in the *next* request for the new
  /// name is the one it learned under the old one. A table that did not follow
  /// would answer `readdir` for the destination out of the source's record, which
  /// is exactly what a moved directory listing empty looks like.
  /// Returns the moved `(inode, new path)` pairs, so the caller can refresh the
  /// node behind each one: the record still holds whatever the path *used* to be
  /// backed by, and a moved directory whose record still says "base entry" makes
  /// `opendir` page a base listing that no longer describes it.
  pub fn rename_subtree(&mut self, from: &BytePath, to: &BytePath) -> Vec<(u64, BytePath)> {
    let moving: Vec<(Vec<u8>, u64)> = self
      .by_path
      .iter()
      .filter(|(path, _)| {
        let path = BytePath::new((*path).clone());
        gfs_overlay::is_within(&path, from)
      })
      .map(|(path, ino)| (path.clone(), *ino))
      .collect();
    let mut moved = Vec::new();
    for (path, ino) in moving {
      let mut target = to.as_bytes().to_vec();
      target.extend_from_slice(&path[from.as_bytes().len()..]);
      self.by_path.remove(&path);
      self.by_path.insert(target.clone(), ino);
      let target = BytePath::new(target);
      if let Some(record) = self.records.get_mut(&ino) {
        record.path = target.clone();
      }
      moved.push((ino, target));
    }
    moved
  }

  /// Replace what a live inode is backed by, keeping its number and its counts.
  pub fn refresh(&mut self, ino: u64, node: Node) {
    if let Some(record) = self.records.get_mut(&ino) {
      record.node = node;
    }
  }

  /// Restore the numbers a previous process handed out.
  ///
  /// Without this a restarted daemon would re-issue a low number that a surviving
  /// journal row already claims, and two paths would share an inode.
  pub fn seed(&mut self, entries: &[OverlayEntry]) {
    for entry in entries {
      self.bind_if_absent(&entry.path, entry.ino);
    }
  }

  /// Record a successful lookup, refreshing the node and incrementing the
  /// kernel's reference count.
  pub fn insert_lookup(&mut self, path: BytePath, node: Node) -> Record {
    if let Some(ino) = node.preferred_ino() {
      self.bind_if_absent(&path, ino);
    }
    let ino = self.number_for(&path);
    let record = self.records.entry(ino).or_insert_with(|| Record {
      ino,
      path,
      node: node.clone(),
      generation: 0,
      lookups: 0,
      opens: 0,
    });
    record.node = node;
    record.lookups += 1;
    record.clone()
  }

  /// Record an entry returned by `readdirplus`, which counts as a lookup.
  ///
  /// Identical to [`InodeTable::insert_lookup`]; named separately because the
  /// reason is different, and because getting this wrong is a slow leak rather
  /// than a visible bug: the kernel sends one `forget` per `readdirplus` entry,
  /// and a table that did not count them would drop records out from under live
  /// inodes.
  pub fn insert_readdirplus(&mut self, path: BytePath, node: Node) -> Record {
    self.insert_lookup(path, node)
  }

  /// Drop `nlookup` of the kernel's references.
  pub fn forget(&mut self, ino: u64, nlookup: u64) {
    if ino == ROOT_INO {
      return;
    }
    let drop_it = match self.records.get_mut(&ino) {
      Some(record) => {
        record.lookups = record.lookups.saturating_sub(nlookup);
        record.lookups == 0 && record.opens == 0
      }
      None => false,
    };
    if drop_it {
      // Only the metadata. The path keeps its number in `by_path`, which is what
      // makes the number stable across a forget/lookup cycle.
      self.records.remove(&ino);
    }
  }

  pub fn open(&mut self, ino: u64) {
    if let Some(record) = self.records.get_mut(&ino) {
      record.opens += 1;
    }
  }

  pub fn close(&mut self, ino: u64) {
    let drop_it = match self.records.get_mut(&ino) {
      Some(record) => {
        record.opens = record.opens.saturating_sub(1);
        record.lookups == 0 && record.opens == 0 && record.ino != ROOT_INO
      }
      None => false,
    };
    if drop_it {
      self.records.remove(&ino);
    }
  }

  /// Re-point the table at another commit's root, and report what the kernel
  /// must be told to forget.
  ///
  /// `by_path` is left completely alone. DESIGN.md section 8.2 promises a path
  /// keeps its inode number for the life of the *mount*, and a re-pin does not
  /// end the mount — `gfs switch` is the same mount looking at a different
  /// commit, exactly as `git switch` is. Renumbering here would reintroduce the
  /// stale-`(device, inode)`-hit hazard that section is about.
  ///
  /// Records are also left alone, and that is deliberate rather than lazy. The
  /// kernel still holds a dentry for each one; dropping the record first would
  /// make the `getattr` it is entitled to send next answer `ESTALE` for a file
  /// that exists perfectly well on the new commit. Instead the caller
  /// invalidates the returned paths, the kernel drops the dentries and sends
  /// `forget`, and the records leave through the door they normally leave by.
  /// The next access is a fresh `lookup` resolved against the new commit.
  ///
  /// Only the root is rewritten in place, because nothing ever invalidates it:
  /// inode 1 is never forgotten, so it would otherwise keep describing the tree
  /// the mount was created with.
  ///
  /// The result is `(parent inode, name)` pairs rather than paths because that
  /// is what `FUSE_NOTIFY_INVAL_ENTRY` takes, and resolving a parent path to its
  /// number needs `by_path` — which lives here and nowhere else.
  pub fn repin(&mut self, root: TreeEntryInfo) -> Vec<StaleEntry> {
    if let Some(record) = self.records.get_mut(&ROOT_INO) {
      record.node = Node::Base(root);
    }
    let paths: Vec<(u64, BytePath)> = self
      .records
      .values()
      .filter(|record| record.ino != ROOT_INO)
      .map(|record| (record.ino, record.path.clone()))
      .collect();
    // Every regular file the kernel may hold comes back as a new inode: the
    // pin has changed what its bytes are, and under writeback caching the
    // kernel would otherwise keep the old size. Directories keep theirs, so
    // a shell's working directory survives the switch.
    for (ino, _) in &paths {
      if let Some(record) = self.records.get_mut(ino) {
        if record.node.is_regular_file() {
          record.generation += 1;
        }
      }
    }
    paths
      .into_iter()
      .filter_map(|(ino, path)| {
        let bytes = path.as_bytes();
        let (parent, name) = match bytes.iter().rposition(|b| *b == b'/') {
          Some(slash) => (&bytes[..slash], &bytes[slash + 1..]),
          None => (&bytes[..0], bytes),
        };
        if name.is_empty() {
          return None;
        }
        // A child is only ever numbered by a lookup through its parent, so the
        // parent is numbered too. `filter_map` rather than `expect` because a
        // missing parent is a reason to invalidate less, never to abort a switch.
        let parent_ino = *self.by_path.get(parent)?;
        Some(StaleEntry {
          parent: parent_ino,
          name: name.to_vec(),
          ino,
        })
      })
      .collect()
  }

  /// A regular file the daemon rewrote behind the kernel outside a re-pin:
  /// bump its generation and name the dentry to drop, if the kernel has it.
  pub fn rewritten_behind_kernel(&mut self, path: &BytePath) -> Option<StaleEntry> {
    let ino = *self.by_path.get(path.as_bytes())?;
    let record = self.records.get_mut(&ino)?;
    if !record.node.is_regular_file() {
      return None;
    }
    record.generation += 1;
    let bytes = path.as_bytes();
    let slash = bytes.iter().rposition(|b| *b == b'/');
    let (parent, name) = match slash {
      Some(slash) => (&bytes[..slash], &bytes[slash + 1..]),
      None => (&bytes[..0], bytes),
    };
    let parent = *self.by_path.get(parent)?;
    Some(StaleEntry {
      parent,
      name: name.to_vec(),
      ino,
    })
  }

  /// Live records. Reported by `gfs inspect`, and the number that would grow
  /// without bound if `forget` were ignored.
  pub fn live(&self) -> usize {
    self.records.len()
  }

  /// Distinct paths that have ever been assigned a number.
  pub fn assigned(&self) -> usize {
    self.by_path.len()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use gfs_types::{EntryKind, HashAlgorithm, ObjectId};

  fn oid(byte: u8) -> ObjectId {
    ObjectId::from_raw(HashAlgorithm::Sha1, &[byte; 20]).unwrap()
  }

  fn entry(path: &str, kind: EntryKind) -> TreeEntryInfo {
    TreeEntryInfo {
      path: BytePath::new(path.as_bytes().to_vec()),
      kind,
      mode: 0o100_644,
      oid: oid(1),
      size: 0,
      symlink_target: None,
      blob_ticket: None,
    }
  }

  fn table() -> InodeTable {
    InodeTable::new(entry("", EntryKind::Directory))
  }

  #[test]
  fn the_root_is_present_before_any_lookup() {
    let table = table();
    assert!(table.get(ROOT_INO).is_some());
    assert_eq!(table.path(ROOT_INO).unwrap(), BytePath::root());
  }

  #[test]
  fn an_inode_number_survives_a_forget_and_lookup_cycle() {
    // The property the two-map split exists for. A build tool that cached
    // (device, inode) inside one job must not see the same path change identity.
    let mut table = table();
    let path = BytePath::new(b"src/main.rs".to_vec());
    let first = table
      .insert_lookup(
        path.clone(),
        Node::Base(entry("src/main.rs", EntryKind::Regular)),
      )
      .ino;

    table.forget(first, 1);
    assert!(table.get(first).is_none(), "the record is dropped");

    let second = table
      .insert_lookup(path, Node::Base(entry("src/main.rs", EntryKind::Regular)))
      .ino;
    assert_eq!(first, second, "the number must be stable");
  }

  #[test]
  fn distinct_paths_never_share_a_number() {
    let mut table = table();
    let a = table
      .insert_lookup(
        BytePath::new(b"a".to_vec()),
        Node::Base(entry("a", EntryKind::Regular)),
      )
      .ino;
    let b = table
      .insert_lookup(
        BytePath::new(b"b".to_vec()),
        Node::Base(entry("b", EntryKind::Regular)),
      )
      .ino;
    assert_ne!(a, b);
  }

  #[test]
  fn a_record_survives_forget_while_a_descriptor_is_open() {
    // The kernel may forget a name while a descriptor obtained through it is
    // still open. Dropping the record there would fail an in-progress read.
    let mut table = table();
    let ino = table
      .insert_lookup(
        BytePath::new(b"f".to_vec()),
        Node::Base(entry("f", EntryKind::Regular)),
      )
      .ino;
    table.open(ino);
    table.forget(ino, 1);
    assert!(table.get(ino).is_some(), "an open handle pins the record");
    table.close(ino);
    assert!(table.get(ino).is_none());
  }

  #[test]
  fn repeated_lookups_need_matching_forgets() {
    let mut table = table();
    let path = BytePath::new(b"x".to_vec());
    let node = Node::Base(entry("x", EntryKind::Regular));
    let ino = table.insert_lookup(path.clone(), node.clone()).ino;
    table.insert_lookup(path.clone(), node.clone());
    table.insert_lookup(path, node);

    table.forget(ino, 2);
    assert!(table.get(ino).is_some(), "one reference remains");
    table.forget(ino, 1);
    assert!(table.get(ino).is_none());
  }

  #[test]
  fn the_root_cannot_be_forgotten() {
    let mut table = table();
    table.forget(ROOT_INO, u64::MAX);
    assert!(table.get(ROOT_INO).is_some());
  }
}
