//! The base tree's directory-listing cache.
//!
//! The pinned commit is immutable, so one *complete* listing of a directory
//! answers every question about that directory for the life of the pin: its
//! children, each child's metadata, and — the part no TTL can give — the
//! definitive absence of any other name. The daemon caches listings here so a
//! warm metadata walk (`git status` every prompt redraw) costs zero server
//! round trips, where before it paid one `list_directory` per readdir and one
//! `get_entry` per expired negative dentry, forever.
//!
//! # Lifetime: the cache lives inside [`crate::fs::Pinned`]
//!
//! A repin swaps `Pinned` wholesale, so the cache is born empty alongside the
//! new commit's client. That is not convenience but correctness: a fetch
//! started against the old client cannot insert into the new generation's
//! cache, because it holds the old `Pinned`'s. A cache beside `Pinned` with an
//! explicit `clear()` would have exactly that race.
//!
//! # What is cached, and what deliberately is not
//!
//! Raw [`TreeEntryInfo`]s, never synthesized attributes. Inode numbers are
//! assigned by the inode table's never-pruned `by_path` map and must stay
//! stable per path for the life of the mount — git's index records them, and a
//! changed number is a re-hash and a hydration. Serving a cached listing
//! through the same assignment path makes cache and server indistinguishable.
//!
//! Bounded ([`crate::fs::FsConfig::listing_cache_dirs`]) because a monorepo
//! walk would otherwise pin every directory's metadata in daemon memory — the
//! same concern that rejected pinning the manifest at mount time (ADR 0009).
//! Eviction is least-recently-used by an O(cap) scan: at a few thousand slots
//! the scan is noise and buys freedom from a dependency.

use std::collections::HashMap;
use std::sync::Mutex;

use gfs_types::{BytePath, TreeEntryInfo};

/// A directory's complete base listing, with a by-name index.
#[derive(Debug)]
pub struct Listing {
  entries: Vec<TreeEntryInfo>,
  by_name: HashMap<Vec<u8>, usize>,
}

impl Listing {
  /// Build from the concatenation of every page of a directory.
  pub fn new(entries: Vec<TreeEntryInfo>) -> Listing {
    let by_name = entries
      .iter()
      .enumerate()
      .map(|(i, entry)| (entry.path.file_name().unwrap_or_default().to_vec(), i))
      .collect();
    Listing { entries, by_name }
  }

  /// The entry for a child name. `None` is definitive: the pin has no such
  /// child, now and for the life of the pin.
  pub fn get(&self, name: &[u8]) -> Option<&TreeEntryInfo> {
    self.by_name.get(name).map(|i| &self.entries[*i])
  }

  /// Every entry, in the server's listing order.
  pub fn entries(&self) -> &[TreeEntryInfo] {
    &self.entries
  }
}

#[derive(Debug)]
struct Slot {
  listing: std::sync::Arc<Listing>,
  /// The clock value of the last hit, for eviction.
  used: u64,
}

/// Complete listings of base directories, keyed by directory path.
#[derive(Debug)]
pub struct ListingCache {
  slots: Mutex<Slots>,
  capacity: usize,
}

#[derive(Debug, Default)]
struct Slots {
  dirs: HashMap<Vec<u8>, Slot>,
  clock: u64,
}

impl ListingCache {
  pub fn new(capacity: usize) -> ListingCache {
    ListingCache {
      slots: Mutex::new(Slots::default()),
      capacity: capacity.max(1),
    }
  }

  pub fn get(&self, dir: &BytePath) -> Option<std::sync::Arc<Listing>> {
    let mut slots = self.slots.lock().expect("listing cache");
    slots.clock += 1;
    let clock = slots.clock;
    let slot = slots.dirs.get_mut(dir.as_bytes())?;
    slot.used = clock;
    Some(std::sync::Arc::clone(&slot.listing))
  }

  pub fn insert(&self, dir: &BytePath, listing: std::sync::Arc<Listing>) {
    let mut slots = self.slots.lock().expect("listing cache");
    slots.clock += 1;
    let clock = slots.clock;
    if !slots.dirs.contains_key(dir.as_bytes()) && slots.dirs.len() >= self.capacity {
      if let Some(coldest) = slots
        .dirs
        .iter()
        .min_by_key(|(_, slot)| slot.used)
        .map(|(path, _)| path.clone())
      {
        slots.dirs.remove(&coldest);
      }
    }
    slots.dirs.insert(
      dir.as_bytes().to_vec(),
      Slot {
        listing,
        used: clock,
      },
    );
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use gfs_types::{EntryKind, HashAlgorithm, ObjectId};

  fn entry(path: &str) -> TreeEntryInfo {
    TreeEntryInfo {
      path: BytePath::new(path.as_bytes().to_vec()),
      kind: EntryKind::Regular,
      mode: 0o100_644,
      oid: ObjectId::from_raw(HashAlgorithm::Sha1, &[0; 20]).unwrap(),
      size: 0,
      symlink_target: None,
      blob_ticket: None,
    }
  }

  #[test]
  fn a_listing_answers_by_name_and_definitively_in_the_negative() {
    let listing = Listing::new(vec![entry("src/a.rs"), entry("src/b.rs")]);
    assert!(listing.get(b"a.rs").is_some());
    assert!(listing.get(b"nope.rs").is_none());
    assert_eq!(listing.entries().len(), 2);
  }

  #[test]
  fn the_coldest_directory_is_evicted_at_capacity() {
    let cache = ListingCache::new(2);
    let a = BytePath::new(b"a".to_vec());
    let b = BytePath::new(b"b".to_vec());
    let c = BytePath::new(b"c".to_vec());
    cache.insert(&a, std::sync::Arc::new(Listing::new(Vec::new())));
    cache.insert(&b, std::sync::Arc::new(Listing::new(Vec::new())));
    // Touch `a` so `b` is the coldest when `c` arrives.
    assert!(cache.get(&a).is_some());
    cache.insert(&c, std::sync::Arc::new(Listing::new(Vec::new())));
    assert!(cache.get(&a).is_some());
    assert!(cache.get(&b).is_none());
    assert!(cache.get(&c).is_some());
  }
}
