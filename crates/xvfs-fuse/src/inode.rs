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
//! Git refuses to record a tree entry named `.git` at any level, so a synthesized
//! `.git` path can never collide with a base path. One table therefore needs no
//! namespace tag, and `lookup` does not have to ask which world a path belongs to
//! before it can answer.

use std::collections::HashMap;

use xvfs_types::{BytePath, TreeEntryInfo};

use crate::gitdir::SynthNode;

/// The FUSE root inode. Fixed by the protocol, not by us.
pub const ROOT_INO: u64 = 1;

/// What an inode is backed by.
#[derive(Clone, Debug)]
pub enum Node {
  /// An entry of the pinned commit's tree.
  Base(TreeEntryInfo),
  /// Part of the synthesized read-only `.git` surface (ADR 0005).
  Synth(SynthNode),
}

impl Node {
  pub fn is_synth(&self) -> bool {
    matches!(self, Node::Synth(_))
  }
}

#[derive(Clone, Debug)]
pub struct Record {
  pub ino: u64,
  pub path: BytePath,
  pub node: Node,
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

  /// Record a successful lookup, refreshing the node and incrementing the
  /// kernel's reference count.
  pub fn insert_lookup(&mut self, path: BytePath, node: Node) -> Record {
    let ino = self.number_for(&path);
    let record = self.records.entry(ino).or_insert_with(|| Record {
      ino,
      path,
      node: node.clone(),
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

  /// Live records. Reported by `xvfs inspect`, and the number that would grow
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
  use xvfs_types::{EntryKind, HashAlgorithm, ObjectId};

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
