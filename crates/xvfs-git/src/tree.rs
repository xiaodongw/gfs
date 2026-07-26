//! Decoded tree representation, Git's entry ordering, and a bounded cache.
//!
//! The ordering rule here is the single most error-prone detail in the snapshot
//! API, and M0 measured the cost of getting it wrong.
//!
//! Git sorts a tree entry by its name **with `/` appended when the entry is a
//! directory**. So `byteorder.h` sorts before `byteorder/`, because `.` (0x2e)
//! precedes `/` (0x2f) -- even though the raw names compare the other way.
//! Paginating a directory on raw names therefore skips entries at page
//! boundaries: the M0 spike returned 1597 of the Linux kernel's 1598
//! `include/linux` entries, silently. A directory listing that drops one entry per
//! page boundary is exactly the kind of failure an agent cannot detect, so the
//! sort key is a named function with its own tests rather than an inline
//! expression.

use std::collections::BTreeMap;
use std::sync::Mutex;

use xvfs_types::mode;
use xvfs_types::ObjectId;

/// The key Git sorts a tree entry by: the name, with `/` appended for a
/// directory.
///
/// This is the order `git ls-tree` emits and the order a directory page token
/// must be expressed in. Note that a gitlink is *not* a directory for ordering
/// purposes: Git stores mode 0o160000 and sorts it by its bare name, even though
/// XVFS presents it as an empty directory.
pub fn sort_key(name: &[u8], entry_mode: u32) -> Vec<u8> {
  let mut k = Vec::with_capacity(name.len() + 1);
  k.extend_from_slice(name);
  if entry_mode == mode::DIRECTORY {
    k.push(b'/');
  }
  k
}

/// One decoded tree entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedEntry {
  pub name: Vec<u8>,
  pub mode: u32,
  pub oid: ObjectId,
}

impl DecodedEntry {
  pub fn sort_key(&self) -> Vec<u8> {
    sort_key(&self.name, self.mode)
  }
}

/// A decoded tree: entries in Git's order, plus a name-ordered index.
///
/// Two orders are stored because the two access patterns genuinely need
/// different ones and neither can be derived cheaply from the other:
///
/// * **pagination** walks entries in Git's sort-key order, so a page token stays
///   valid and no entry is skipped;
/// * **lookup by name** binary-searches `by_name`, because sort-key order is not
///   name order (see the module note) and a binary search over `entries` by raw
///   name would miss entries.
///
/// The alternative -- a linear scan for lookup -- was rejected because path
/// traversal does one lookup per path component, and a monorepo has directories
/// with thousands of entries.
#[derive(Clone, Debug)]
pub struct DecodedTree {
  entries: Vec<DecodedEntry>,
  /// Indices into `entries`, ordered by raw name.
  by_name: Vec<u32>,
}

impl DecodedTree {
  pub fn new(mut entries: Vec<DecodedEntry>) -> Self {
    entries.sort_by_key(DecodedEntry::sort_key);
    let mut by_name: Vec<u32> = (0..entries.len() as u32).collect();
    by_name.sort_by(|a, b| entries[*a as usize].name.cmp(&entries[*b as usize].name));
    DecodedTree { entries, by_name }
  }

  pub fn len(&self) -> usize {
    self.entries.len()
  }

  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  /// Entries in Git's sort order.
  pub fn entries(&self) -> &[DecodedEntry] {
    &self.entries
  }

  /// Find one entry by exact name.
  ///
  /// A name is unique within a Git tree -- Git cannot store both a file and a
  /// directory called `foo` -- so this is unambiguous.
  pub fn get(&self, name: &[u8]) -> Option<&DecodedEntry> {
    let idx = self
      .by_name
      .binary_search_by(|i| self.entries[*i as usize].name.as_slice().cmp(name))
      .ok()?;
    Some(&self.entries[self.by_name[idx] as usize])
  }

  /// One page of entries, resuming strictly after `after`.
  ///
  /// `after` is a sort key from a previous page, not a name. Returns the page and
  /// the token to resume from, which is `None` when the page reached the end.
  pub fn page(&self, after: Option<&[u8]>, limit: usize) -> (Vec<&DecodedEntry>, Option<Vec<u8>>) {
    // Entries are in sort-key order, so the resume point is a binary search
    // rather than a scan. This matters for the million-entry exit criterion:
    // scanning from the start on every page makes paging quadratic.
    let start = match after {
      None => 0,
      Some(after) => self
        .entries
        .partition_point(|e| e.sort_key().as_slice() <= after),
    };
    let end = (start + limit).min(self.entries.len());
    let page: Vec<&DecodedEntry> = self.entries[start..end].iter().collect();
    let next = if end < self.entries.len() {
      page.last().map(|e| e.sort_key())
    } else {
      None
    };
    (page, next)
  }

  /// Approximate heap cost, used to bound the cache by bytes rather than by
  /// entry count.
  ///
  /// Counting trees rather than bytes would let a cache of a few thousand
  /// thousand-entry directories consume far more memory than the same count of
  /// small trees -- and M1.3 asks for bounded *memory*.
  pub fn heap_bytes(&self) -> usize {
    let per_entry: usize = self
      .entries
      .iter()
      .map(|e| e.name.len() + e.oid.as_bytes().len() + std::mem::size_of::<DecodedEntry>())
      .sum();
    per_entry + self.by_name.len() * std::mem::size_of::<u32>()
  }
}

/// A bounded cache of decoded trees, keyed by tree object ID.
///
/// Safe to cache indefinitely because a Git tree object is immutable: its OID is
/// a hash of its contents, so an entry can never become stale. Only memory
/// pressure evicts.
///
/// Eviction is least-recently-used, implemented with a monotonic tick and a
/// `BTreeMap` ordered by it. That gives O(log n) get and insert without an
/// external LRU crate and without the O(n) removal a `VecDeque` of keys would
/// cost on every access.
pub struct TreeCache {
  inner: Mutex<CacheInner>,
  max_bytes: usize,
}

impl std::fmt::Debug for TreeCache {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let inner = self.inner.lock().unwrap();
    f.debug_struct("TreeCache")
      .field("trees", &inner.by_oid.len())
      .field("bytes", &inner.bytes)
      .field("max_bytes", &self.max_bytes)
      .field("hits", &inner.hits)
      .field("misses", &inner.misses)
      .finish()
  }
}

struct CacheInner {
  by_oid: std::collections::HashMap<ObjectId, (std::sync::Arc<DecodedTree>, u64)>,
  /// Access tick to OID, so the least-recently-used entry is `first_key_value`.
  by_tick: BTreeMap<u64, ObjectId>,
  tick: u64,
  bytes: usize,
  hits: u64,
  misses: u64,
}

/// Cache counters, exposed for the metrics surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeCacheStats {
  pub trees: usize,
  pub bytes: usize,
  pub hits: u64,
  pub misses: u64,
}

impl TreeCache {
  pub fn new(max_bytes: usize) -> Self {
    TreeCache {
      inner: Mutex::new(CacheInner {
        by_oid: std::collections::HashMap::new(),
        by_tick: BTreeMap::new(),
        tick: 0,
        bytes: 0,
        hits: 0,
        misses: 0,
      }),
      max_bytes,
    }
  }

  pub fn get(&self, oid: &ObjectId) -> Option<std::sync::Arc<DecodedTree>> {
    let mut inner = self.inner.lock().unwrap();
    let Some((tree, old_tick)) = inner.by_oid.get(oid).cloned() else {
      inner.misses += 1;
      return None;
    };
    inner.hits += 1;
    inner.tick += 1;
    let new_tick = inner.tick;
    inner.by_tick.remove(&old_tick);
    inner.by_tick.insert(new_tick, oid.clone());
    inner.by_oid.insert(oid.clone(), (tree.clone(), new_tick));
    Some(tree)
  }

  pub fn insert(&self, oid: ObjectId, tree: DecodedTree) -> std::sync::Arc<DecodedTree> {
    let tree = std::sync::Arc::new(tree);
    let cost = tree.heap_bytes();
    let mut inner = self.inner.lock().unwrap();

    // A single tree larger than the whole budget is returned but not stored.
    // Caching it would evict everything else to hold one item, which is worse
    // than not caching it at all.
    if cost > self.max_bytes {
      return tree;
    }

    if let Some((old, old_tick)) = inner.by_oid.remove(&oid) {
      inner.bytes -= old.heap_bytes();
      inner.by_tick.remove(&old_tick);
    }

    inner.tick += 1;
    let tick = inner.tick;
    inner.by_oid.insert(oid.clone(), (tree.clone(), tick));
    inner.by_tick.insert(tick, oid);
    inner.bytes += cost;

    while inner.bytes > self.max_bytes {
      let Some((&oldest_tick, oldest_oid)) = inner.by_tick.iter().next() else {
        break;
      };
      let oldest_oid = oldest_oid.clone();
      inner.by_tick.remove(&oldest_tick);
      if let Some((evicted, _)) = inner.by_oid.remove(&oldest_oid) {
        inner.bytes -= evicted.heap_bytes();
      }
    }

    tree
  }

  pub fn stats(&self) -> TreeCacheStats {
    let inner = self.inner.lock().unwrap();
    TreeCacheStats {
      trees: inner.by_oid.len(),
      bytes: inner.bytes,
      hits: inner.hits,
      misses: inner.misses,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use xvfs_types::HashAlgorithm;

  fn oid(n: u8) -> ObjectId {
    ObjectId::from_raw(HashAlgorithm::Sha1, &[n; 20]).unwrap()
  }

  fn entry(name: &str, m: u32) -> DecodedEntry {
    DecodedEntry {
      name: name.as_bytes().to_vec(),
      mode: m,
      oid: oid(1),
    }
  }

  #[test]
  fn a_directory_sorts_as_if_its_name_ended_in_a_slash() {
    // The exact case ADR 0005 measured. `.` is 0x2e and `/` is 0x2f, so the file
    // sorts first -- the opposite of raw-name order.
    assert!(sort_key(b"byteorder", mode::DIRECTORY) > sort_key(b"byteorder.h", mode::REGULAR));
    assert_eq!(sort_key(b"byteorder", mode::DIRECTORY), b"byteorder/");
    assert_eq!(sort_key(b"byteorder.h", mode::REGULAR), b"byteorder.h");
    // A gitlink sorts by its bare name: Git stores mode 0o160000, and XVFS
    // presenting it as a directory does not change the stored ordering.
    assert_eq!(sort_key(b"sub", mode::GITLINK), b"sub");
  }

  #[test]
  fn paging_returns_every_entry_exactly_once_across_the_boundary() {
    // The regression the M0 spike hit: 1597 of 1598 entries, because the page
    // boundary fell between `byteorder.h` and `byteorder/`.
    let tree = DecodedTree::new(vec![
      entry("byteorder.h", mode::REGULAR),
      entry("byteorder", mode::DIRECTORY),
      entry("atomic.h", mode::REGULAR),
      entry("atomic", mode::DIRECTORY),
    ]);

    let mut seen: Vec<Vec<u8>> = Vec::new();
    let mut token: Option<Vec<u8>> = None;
    loop {
      let (page, next) = tree.page(token.as_deref(), 1);
      assert!(page.len() <= 1);
      for e in page {
        seen.push(e.name.clone());
      }
      match next {
        None => break,
        Some(t) => token = Some(t),
      }
    }

    assert_eq!(seen.len(), 4, "every entry must appear exactly once");
    let names: Vec<String> = seen
      .iter()
      .map(|n| String::from_utf8(n.clone()).unwrap())
      .collect();
    // Git's order, not name order.
    assert_eq!(names, ["atomic.h", "atomic", "byteorder.h", "byteorder"]);
  }

  #[test]
  fn paging_a_large_tree_is_complete_and_ordered() {
    // A scale check on the paging arithmetic, since the exit criterion pages a
    // million-entry snapshot one directory at a time.
    let entries: Vec<DecodedEntry> = (0..5000)
      .map(|i| {
        entry(
          &format!("e{i:05}"),
          if i % 3 == 0 {
            mode::DIRECTORY
          } else {
            mode::REGULAR
          },
        )
      })
      .collect();
    let tree = DecodedTree::new(entries);

    let mut count = 0;
    let mut last_key: Option<Vec<u8>> = None;
    let mut token: Option<Vec<u8>> = None;
    loop {
      let (page, next) = tree.page(token.as_deref(), 97);
      for e in &page {
        let k = e.sort_key();
        if let Some(prev) = &last_key {
          assert!(prev < &k, "pages must be strictly increasing in sort order");
        }
        last_key = Some(k);
        count += 1;
      }
      match next {
        None => break,
        Some(t) => token = Some(t),
      }
    }
    assert_eq!(count, 5000);
  }

  #[test]
  fn lookup_by_name_finds_entries_that_sort_key_order_would_hide() {
    let tree = DecodedTree::new(vec![
      entry("byteorder.h", mode::REGULAR),
      entry("byteorder", mode::DIRECTORY),
    ]);
    // A binary search over sort-key order by raw name would miss `byteorder`,
    // because its key is `byteorder/`.
    assert_eq!(tree.get(b"byteorder").unwrap().mode, mode::DIRECTORY);
    assert_eq!(tree.get(b"byteorder.h").unwrap().mode, mode::REGULAR);
    assert!(tree.get(b"absent").is_none());
  }

  #[test]
  fn lookup_handles_non_utf8_names() {
    let tree = DecodedTree::new(vec![DecodedEntry {
      name: b"bad\xff.c".to_vec(),
      mode: mode::REGULAR,
      oid: oid(2),
    }]);
    assert!(tree.get(b"bad\xff.c").is_some());
    assert!(tree.get(b"bad.c").is_none());
  }

  #[test]
  fn the_cache_evicts_least_recently_used_and_stays_within_its_budget() {
    let one = DecodedTree::new(vec![entry("a", mode::REGULAR)]);
    let budget = one.heap_bytes() * 2 + 1;
    let cache = TreeCache::new(budget);

    cache.insert(oid(1), DecodedTree::new(vec![entry("a", mode::REGULAR)]));
    cache.insert(oid(2), DecodedTree::new(vec![entry("b", mode::REGULAR)]));
    // Touch 1 so 2 becomes the least recently used.
    assert!(cache.get(&oid(1)).is_some());
    cache.insert(oid(3), DecodedTree::new(vec![entry("c", mode::REGULAR)]));

    assert!(cache.get(&oid(1)).is_some(), "recently used must survive");
    assert!(
      cache.get(&oid(2)).is_none(),
      "least recently used is evicted"
    );
    assert!(cache.get(&oid(3)).is_some());
    assert!(cache.stats().bytes <= budget);
  }

  #[test]
  fn a_tree_bigger_than_the_whole_budget_is_returned_but_not_stored() {
    // Caching it would evict everything to hold one item.
    let cache = TreeCache::new(64);
    let big: Vec<DecodedEntry> = (0..500)
      .map(|i| entry(&format!("e{i}"), mode::REGULAR))
      .collect();
    let returned = cache.insert(oid(9), DecodedTree::new(big));
    assert_eq!(returned.len(), 500, "the caller still gets the tree");
    assert!(cache.get(&oid(9)).is_none(), "but it was not stored");
    assert_eq!(cache.stats().bytes, 0);
  }

  #[test]
  fn reinserting_the_same_oid_does_not_double_count_its_bytes() {
    let cache = TreeCache::new(1 << 20);
    let t = || DecodedTree::new(vec![entry("a", mode::REGULAR)]);
    cache.insert(oid(1), t());
    let after_first = cache.stats().bytes;
    cache.insert(oid(1), t());
    assert_eq!(cache.stats().bytes, after_first);
    assert_eq!(cache.stats().trees, 1);
  }
}
