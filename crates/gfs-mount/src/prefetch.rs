//! Reading the access pattern, and fetching ahead of it.
//!
//! Two patterns are worth recognizing, because both turn one logical operation
//! into thousands of serialized round trips:
//!
//! * **a tree walk.** `git status` in a fresh workspace reads every directory
//!   once to populate its untracked cache — 5 328 directories on vscode, one
//!   `ListDirectory` each, strictly in order because the walk is single
//!   threaded. [`WalkDetector`] notices the descent and the daemon answers the
//!   rest of it with one `ListTree` for the subtree.
//! * **reading a directory's files.** An agent that opens `src/a.rs` and
//!   `src/b.rs` is usually about to open the rest of `src`. [`ReadDetector`]
//!   notices, and the daemon fetches the remaining blobs into the cache while
//!   the job is still reading the ones it asked for.
//!
//! # What a detector must not do
//!
//! Guess eagerly. A job that opens one file must pay for one file: prefetching
//! a monorepo's metadata for it would trade the cost this system exists to
//! avoid for a smaller version of the same cost. Both detectors therefore need
//! *evidence* — several misses, or several reads, inside a window — before they
//! fire, and both are bounded in what one firing may fetch.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gfs_types::{BytePath, EntryKind, TreeEntryInfo};

use crate::cache::BlobCache;
use crate::fs::{FsStats, Pinned};
use crate::listing::Listing;

/// How long a listing miss counts as evidence of a walk in progress.
const WALK_WINDOW: Duration = Duration::from_secs(2);

/// How long a file read counts as evidence of a directory being read through.
const READ_WINDOW: Duration = Duration::from_secs(30);

/// The directory a path sits in.
pub fn parent_of(path: &BytePath) -> BytePath {
  let bytes = path.as_bytes();
  match bytes.iter().rposition(|b| *b == b'/') {
    Some(i) => BytePath::new(bytes[..i].to_vec()),
    None => BytePath::root(),
  }
}

/// The longest path both arguments start with, on a component boundary.
fn common_ancestor(a: &BytePath, b: &BytePath) -> BytePath {
  let (a, b) = (a.as_bytes(), b.as_bytes());
  let mut end = 0;
  let mut i = 0;
  while i < a.len() && i < b.len() && a[i] == b[i] {
    if a[i] == b'/' {
      end = i;
    }
    i += 1;
  }
  // One is a prefix of the other, ending exactly on a boundary: `src` and
  // `src/lib` share `src`, not `sr`.
  if i == a.len() && (i == b.len() || b.get(i) == Some(&b'/')) {
    return BytePath::new(a.to_vec());
  }
  if i == b.len() && a.get(i) == Some(&b'/') {
    return BytePath::new(b.to_vec());
  }
  BytePath::new(a[..end].to_vec())
}

/// Recognizes a directory tree being walked.
#[derive(Debug)]
pub struct WalkDetector {
  /// Recent listing misses, newest last.
  recent: Vec<(BytePath, Instant)>,
  threshold: usize,
}

impl WalkDetector {
  pub fn new(threshold: usize) -> WalkDetector {
    WalkDetector {
      recent: Vec::new(),
      threshold: threshold.max(2),
    }
  }

  /// Record a listing miss and, when the recent ones look like a walk, return
  /// the subtree to fetch: their common ancestor.
  ///
  /// The common ancestor is what makes this useful rather than merely eager. A
  /// `git status` misses the root and then its children, so the ancestor is the
  /// root and one call covers the whole walk; a job reading `packages/web`
  /// misses only inside it, so the same rule fetches that subtree and nothing
  /// else.
  ///
  /// Firing clears the evidence: the next fetch decision needs its own.
  pub fn observe(&mut self, dir: &BytePath, now: Instant) -> Option<BytePath> {
    self
      .recent
      .retain(|(_, at)| now.duration_since(*at) < WALK_WINDOW);
    self.recent.push((dir.clone(), now));
    if self.recent.len() < self.threshold {
      return None;
    }
    let mut root = self.recent[0].0.clone();
    for (path, _) in &self.recent[1..] {
      root = common_ancestor(&root, path);
    }
    self.recent.clear();
    Some(root)
  }
}

/// Recognizes a directory's files being read one after another.
#[derive(Debug, Default)]
pub struct ReadDetector {
  /// Distinct base files read per directory, and when the directory was last
  /// touched.
  dirs: HashMap<Vec<u8>, DirReads>,
}

#[derive(Debug)]
struct DirReads {
  names: Vec<Vec<u8>>,
  last: Instant,
  /// Whether this directory has already triggered a fetch. One per directory
  /// per pin: the fetch covers everything in it, so a second firing would have
  /// nothing left to do.
  fired: bool,
}

impl ReadDetector {
  /// Record that a base file was read, and report whether its directory now
  /// looks like it is being read through.
  pub fn observe(&mut self, path: &BytePath, threshold: usize, now: Instant) -> Option<BytePath> {
    let dir = parent_of(path);
    let name = path.file_name().unwrap_or_default().to_vec();

    // Directories nothing has touched for a while are not evidence of anything.
    self
      .dirs
      .retain(|_, reads| now.duration_since(reads.last) < READ_WINDOW);

    let reads = self
      .dirs
      .entry(dir.as_bytes().to_vec())
      .or_insert_with(|| DirReads {
        names: Vec::new(),
        last: now,
        fired: false,
      });
    reads.last = now;
    if !reads.names.contains(&name) {
      reads.names.push(name);
    }
    if reads.fired || reads.names.len() < threshold.max(2) {
      return None;
    }
    reads.fired = true;
    Some(dir)
  }
}

/// The bounds a firing detector fetches within, taken from [`crate::fs::FsConfig`].
#[derive(Clone, Copy, Debug)]
pub struct PrefetchLimits {
  /// Entries per `ListTree` page.
  pub tree_page_entries: u32,
  /// Entries one recognized walk may fetch before it stops. A subtree larger
  /// than this degrades to per-directory listings for the rest, which is the
  /// behaviour that existed before prefetching.
  pub tree_max_entries: usize,
  /// Bytes one directory's content prefetch may fetch.
  pub content_max_bytes: u64,
  /// The largest single file a content prefetch will speculate on. A big file
  /// is exactly the one a wrong guess pays most for.
  pub content_max_file_bytes: u64,
  /// Blob fetches in flight during a content prefetch.
  pub content_concurrency: usize,
  /// The percentage of the hydration budget a prefetch will not touch, so a
  /// speculative read can never be the reason a real one is refused.
  pub budget_reserve_percent: u64,
}

/// Detectors and in-flight prefetches for one pinned commit.
///
/// Lives inside [`Pinned`] for the same reason the listing cache does: a repin
/// swaps the whole struct, so a prefetch started against the old commit inserts
/// into the old generation's cache and a new pin starts with no evidence and no
/// tasks it did not start.
#[derive(Debug)]
pub struct Prefetcher {
  walk: Mutex<WalkDetector>,
  reads: Mutex<ReadDetector>,
  /// Subtree fetches in flight, by root. The fetching task ticks its sender
  /// once per page, so a waiter re-checks the listing cache as each page lands
  /// rather than when the whole subtree does.
  ///
  /// The map holds *receivers*, and the one sender lives in the task. That is
  /// what makes a waiter safe against a task that dies: a dropped sender closes
  /// the channel, every waiter wakes with an error and takes the direct path.
  /// A sender clone parked in this map would leave them waiting forever on a
  /// fetch that is never coming.
  inflight: Mutex<HashMap<Vec<u8>, tokio::sync::watch::Receiver<u64>>>,
}

impl Prefetcher {
  pub fn new(walk_threshold: usize) -> Prefetcher {
    Prefetcher {
      walk: Mutex::new(WalkDetector::new(walk_threshold)),
      reads: Mutex::new(ReadDetector::default()),
      inflight: Mutex::new(HashMap::new()),
    }
  }

  /// The in-flight fetch covering this directory, if one is running.
  fn covering(&self, dir: &BytePath) -> Option<tokio::sync::watch::Receiver<u64>> {
    let mut inflight = self.inflight.lock().expect("prefetch");
    // A closed channel is a fetch that ended without releasing — the task died.
    // Dropping the entry here is what keeps one dead task from suppressing
    // every later prefetch of the same subtree.
    inflight.retain(|_, progress| progress.has_changed().is_ok());
    inflight
      .iter()
      .find(|(root, _)| covers(root, dir.as_bytes()))
      .map(|(_, progress)| progress.clone())
  }

  /// Record a listing miss, and report the subtree worth fetching when the
  /// recent misses look like a walk.
  pub fn note_listing_miss(&self, dir: &BytePath) -> Option<BytePath> {
    self
      .walk
      .lock()
      .expect("prefetch")
      .observe(dir, Instant::now())
  }

  /// Record that a base file was read, and report its directory when the
  /// directory looks like it is being read through.
  pub fn note_read(&self, path: &BytePath, threshold: usize) -> Option<BytePath> {
    self
      .reads
      .lock()
      .expect("prefetch")
      .observe(path, threshold, Instant::now())
  }

  /// Claim a subtree, or decline because one covering it is already running.
  fn claim(&self, root: &BytePath) -> Option<tokio::sync::watch::Sender<u64>> {
    let mut inflight = self.inflight.lock().expect("prefetch");
    inflight.retain(|_, progress| progress.has_changed().is_ok());
    if inflight
      .iter()
      .any(|(existing, _)| covers(existing, root.as_bytes()))
    {
      return None;
    }
    let (sender, receiver) = tokio::sync::watch::channel(0);
    inflight.insert(root.as_bytes().to_vec(), receiver);
    Some(sender)
  }

  fn release(&self, root: &BytePath) {
    self
      .inflight
      .lock()
      .expect("prefetch")
      .remove(root.as_bytes());
  }
}

/// Whether `root` contains `path` — the same path, or anything below it.
fn covers(root: &[u8], path: &[u8]) -> bool {
  if root.is_empty() {
    return true;
  }
  path.starts_with(root) && (path.len() == root.len() || path[root.len()] == b'/')
}

/// Wait for an in-flight subtree fetch to produce this directory's listing.
///
/// Returns `None` when no fetch covers it, or when the fetch ended without it —
/// in both cases the caller does its own single-directory fetch, which is the
/// behaviour prefetching accelerates rather than replaces.
///
/// Waiting rather than fetching alongside is the point of the whole mechanism:
/// the walk that produced the miss will ask for thousands more directories, and
/// answering them from one traversal is the difference between one round trip
/// and thousands. The wait is re-checked per page, so a directory that arrives
/// early does not wait for the rest of the tree.
pub async fn await_subtree(pinned: &Pinned, dir: &BytePath) -> Option<Arc<Listing>> {
  let mut progress = pinned.prefetch.covering(dir)?;
  loop {
    if let Some(listing) = pinned.listings.get(dir) {
      return Some(listing);
    }
    // `Err` means every sender is gone: the fetch finished and this directory
    // was not in it.
    if progress.changed().await.is_err() {
      return pinned.listings.get(dir);
    }
  }
}

/// Fetch a subtree's listings in the background.
///
/// Silent on failure by design. A prefetch is an optimisation: if the server
/// refuses it or the connection drops, every directory is still answerable one
/// at a time, and turning a speculative failure into a visible one would make
/// the feature less reliable than not having it.
pub fn spawn_subtree(
  pinned: Arc<Pinned>,
  root: BytePath,
  limits: PrefetchLimits,
  stats: Arc<Mutex<FsStats>>,
) {
  let Some(sender) = pinned.prefetch.claim(&root) else {
    return;
  };
  tokio::spawn(async move {
    {
      let mut stats = stats.lock().expect("fs stats");
      stats.tree_prefetches += 1;
    }
    let mut token = Vec::new();
    let mut fetched = 0usize;
    loop {
      let page = match pinned
        .client
        .list_tree(&root, token, limits.tree_page_entries)
        .await
      {
        Ok(page) => page,
        Err(e) => {
          tracing::debug!(root = ?root, "subtree prefetch stopped: {e}");
          break;
        }
      };
      let more = !page.next_page_token.is_empty();
      token = page.next_page_token;
      fetched += page.entries.len();
      let directories = page.directories.len();
      install(&pinned, page.entries, page.directories);
      {
        let mut stats = stats.lock().expect("fs stats");
        stats.tree_pages += 1;
        stats.prefetched_listings += directories as u64;
      }
      // Waiters re-check the cache on every tick.
      sender.send_modify(|pages| *pages += 1);
      if !more || fetched >= limits.tree_max_entries {
        if more {
          tracing::debug!(
            root = ?root,
            fetched,
            "subtree prefetch reached its entry bound; the rest lists per directory"
          );
        }
        break;
      }
    }
    // Released before the sender drops, so a waiter that wakes on the drop sees
    // no in-flight fetch and takes the direct path.
    pinned.prefetch.release(&root);
  });
}

/// Turn a page of a recursive listing into cached per-directory listings.
///
/// Every directory named in the page is complete by the protocol's contract, so
/// each becomes an authoritative listing — including one with no entries, which
/// is why the response carries the directory names separately.
fn install(pinned: &Pinned, entries: Vec<TreeEntryInfo>, directories: Vec<BytePath>) {
  let mut by_dir: HashMap<Vec<u8>, Vec<TreeEntryInfo>> = directories
    .iter()
    .map(|dir| (dir.as_bytes().to_vec(), Vec::new()))
    .collect();
  for entry in entries {
    let dir = parent_of(&entry.path);
    if let Some(bucket) = by_dir.get_mut(dir.as_bytes()) {
      bucket.push(entry);
    }
  }
  for dir in directories {
    if let Some(entries) = by_dir.remove(dir.as_bytes()) {
      pinned
        .listings
        .insert(&dir, Arc::new(Listing::new(entries)));
    }
  }
}

/// Fetch the rest of a directory's file content in the background.
///
/// Bounded three ways — per file, per directory, and by what is left of the
/// job's hydration budget — because this is the speculative half of prefetching
/// and the one that moves real bytes.
pub fn spawn_content(
  pinned: Arc<Pinned>,
  dir: BytePath,
  limits: PrefetchLimits,
  cache: Arc<BlobCache>,
  budget: Arc<crate::budget::HydrationBudget>,
  stats: Arc<Mutex<FsStats>>,
) {
  tokio::spawn(async move {
    // Only from a listing already in hand. Fetching one in order to speculate
    // would make a wrong guess cost a round trip on top of the bytes.
    let Some(listing) = pinned.listings.get(&dir) else {
      return;
    };
    let mut wanted: Vec<TreeEntryInfo> = Vec::new();
    let mut bytes = 0u64;
    for entry in listing.entries() {
      if !matches!(entry.kind, EntryKind::Regular | EntryKind::Executable) {
        continue;
      }
      if entry.size == 0 || entry.size > limits.content_max_file_bytes {
        continue;
      }
      if cache.contains(&entry.oid) {
        continue;
      }
      if bytes + entry.size > limits.content_max_bytes {
        break;
      }
      bytes += entry.size;
      wanted.push(entry.clone());
    }
    if wanted.is_empty() {
      return;
    }
    {
      let mut stats = stats.lock().expect("fs stats");
      stats.content_prefetches += 1;
    }

    // Tickets in one round trip. A ticket is short-lived authorization, so it is
    // minted for the blobs this fetch is about to read and no others.
    let paths: Vec<BytePath> = wanted.iter().map(|e| e.path.clone()).collect();
    let ticketed = match pinned.client.batch_get_entry(&paths, true).await {
      Ok(entries) => entries,
      Err(e) => {
        tracing::debug!(dir = ?dir, "content prefetch stopped: {e}");
        return;
      }
    };

    let mut queue: Vec<TreeEntryInfo> = ticketed.into_iter().flatten().collect();
    let mut tasks = tokio::task::JoinSet::new();
    while !queue.is_empty() || !tasks.is_empty() {
      while tasks.len() < limits.content_concurrency.max(1) {
        let Some(entry) = queue.pop() else { break };
        if !admit(&budget, &entry, limits) {
          queue.clear();
          break;
        }
        let pinned = Arc::clone(&pinned);
        let cache = Arc::clone(&cache);
        let stats = Arc::clone(&stats);
        tasks.spawn(async move {
          let ticket = entry.blob_ticket.clone().unwrap_or_default();
          match cache
            .ensure_cached(&pinned.client, &entry.oid, &ticket)
            .await
          {
            Ok(()) => {
              let mut stats = stats.lock().expect("fs stats");
              stats.prefetched_blobs += 1;
              stats.prefetched_bytes += entry.size;
            }
            Err(e) => tracing::debug!(path = ?entry.path, "prefetching a blob failed: {e}"),
          }
        });
      }
      if tasks.join_next().await.is_none() {
        break;
      }
    }
  });
}

/// Charge a speculative read against the budget, refusing to spend the reserve.
///
/// The reserve is what keeps a wrong guess from turning into an `EDQUOT` on a
/// read the job actually made: prefetching stops well short of the limit. What
/// it does move is charged, because those bytes crossed the network exactly as
/// a real read's would have — a budget that ignored them would stop describing
/// what the job cost.
fn admit(
  budget: &crate::budget::HydrationBudget,
  entry: &TreeEntryInfo,
  limits: PrefetchLimits,
) -> bool {
  let Some(remaining) = budget.remaining() else {
    return true;
  };
  let reserve = budget.limit() / 100 * limits.budget_reserve_percent;
  if remaining <= reserve.saturating_add(entry.size) {
    return false;
  }
  budget.admit(&entry.oid, entry.size).is_ok()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn p(s: &str) -> BytePath {
    BytePath::new(s.as_bytes().to_vec())
  }

  #[test]
  fn a_common_ancestor_stops_on_a_component_boundary() {
    assert_eq!(common_ancestor(&p("src/a"), &p("src/b")).as_bytes(), b"src");
    assert_eq!(common_ancestor(&p("src"), &p("src/b")).as_bytes(), b"src");
    assert_eq!(common_ancestor(&p("srcs/a"), &p("src/b")).as_bytes(), b"");
    assert_eq!(
      common_ancestor(&p("a/b/c"), &p("a/b/d/e")).as_bytes(),
      b"a/b"
    );
    assert_eq!(common_ancestor(&BytePath::root(), &p("a")).as_bytes(), b"");
  }

  #[test]
  fn a_descent_fires_at_the_common_ancestor() {
    let mut detector = WalkDetector::new(3);
    let now = Instant::now();
    assert!(detector.observe(&p("src"), now).is_none());
    assert!(detector.observe(&p("src/net"), now).is_none());
    assert_eq!(
      detector
        .observe(&p("src/net/http"), now)
        .map(|r| r.as_bytes().to_vec()),
      Some(b"src".to_vec())
    );
    // The evidence is spent, so the next miss starts a new case.
    assert!(detector.observe(&p("src/net/tls"), now).is_none());
  }

  #[test]
  fn scattered_misses_over_time_are_not_a_walk() {
    let mut detector = WalkDetector::new(3);
    let start = Instant::now();
    assert!(detector.observe(&p("a"), start).is_none());
    assert!(detector
      .observe(&p("b"), start + WALK_WINDOW + Duration::from_millis(1))
      .is_none());
    assert!(detector
      .observe(&p("c"), start + WALK_WINDOW * 2 + Duration::from_millis(2))
      .is_none());
  }

  #[test]
  fn a_directory_read_through_fires_once() {
    let mut detector = ReadDetector::default();
    let now = Instant::now();
    assert!(detector.observe(&p("src/a.rs"), 3, now).is_none());
    // The same file twice is one file: a re-read is not a new one.
    assert!(detector.observe(&p("src/a.rs"), 3, now).is_none());
    assert!(detector.observe(&p("src/b.rs"), 3, now).is_none());
    assert_eq!(
      detector
        .observe(&p("src/c.rs"), 3, now)
        .map(|d| d.as_bytes().to_vec()),
      Some(b"src".to_vec())
    );
    assert!(detector.observe(&p("src/d.rs"), 3, now).is_none());
  }
}
