//! Memoized ref-reachability verdicts.
//!
//! Object authorization asks one question — *is this commit reachable from a ref
//! this repository currently shows?* — and answering it costs a scan of the
//! whole ref namespace: `Libgit2Repository::is_visible` enumerates every ref and
//! peels every annotated tag, then, if no tip matches, runs a descendant check
//! per tip. Measured on the benchmark corpus that is **5.7–8.8 ms** for django's
//! 29 311 refs and **24–28 ms** for vscode's 73 989 (740 ms on the first call,
//! before the pack's ref data is in the page cache).
//!
//! Every snapshot RPC paid it. A directory listing's own work — decoding the
//! tree and reading each blob's header — is ~2.5 µs, so the check was ~100 % of
//! a listing, and a first `git status` over a vscode mount spent 555 s asking
//! the same reachability question 5 328 times about one commit.
//!
//! The answer only changes when a ref changes, so it is cached per
//! `(repository, commit)` and stamped with the repository's ref generation
//! ([`crate::catalog::Catalog::ref_generation`]), which every ref observation
//! advances. A verdict computed under a different generation is not reused.
//!
//! # Why a TTL as well as a generation
//!
//! The generation is bumped by the catalog, so it covers every ref change this
//! process records — a webhook, a poll, startup reconciliation. It cannot cover
//! a ref changed *underneath* the server, by an operator pushing into the bare
//! repository directly. The TTL bounds how long such a change stays unnoticed
//! by authorization, and is short: DESIGN.md section 7.1 already makes ref state
//! eventually consistent through webhooks and polling, and these windows are far
//! inside that envelope.
//!
//! The two TTLs differ because the two mistakes differ. Serving a commit that
//! just stopped being reachable is a few extra seconds of access to data the
//! caller could read a moment ago; refusing a commit that just *became*
//! reachable is a visible failure on a fresh push, so a negative verdict is held
//! for a fraction of the time.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use gfs_types::{ObjectId, RepositoryId};

/// How long a "reachable" verdict may be reused.
const VISIBLE_TTL: Duration = Duration::from_secs(10);

/// How long an "unreachable" verdict may be reused.
const HIDDEN_TTL: Duration = Duration::from_secs(2);

/// How many verdicts to hold before dropping the lot.
///
/// A flat clear rather than an eviction policy: the entries are tiny, the
/// working set is one commit per mount, and a cache that occasionally starts
/// over costs one ref scan per live mount. Anything cleverer would be machinery
/// with no measured job.
const CAPACITY: usize = 4096;

#[derive(Debug)]
struct Verdict {
  visible: bool,
  generation: u64,
  decided: Instant,
}

impl Verdict {
  fn usable(&self, generation: u64, now: Instant) -> bool {
    if self.generation != generation {
      return false;
    }
    let ttl = if self.visible {
      VISIBLE_TTL
    } else {
      HIDDEN_TTL
    };
    now.duration_since(self.decided) < ttl
  }
}

/// Reachability verdicts, keyed by repository and commit.
#[derive(Debug)]
pub struct VisibilityCache {
  entries: Mutex<HashMap<(RepositoryId, ObjectId), Verdict>>,
  hits: AtomicU64,
  misses: AtomicU64,
}

/// How often the cache answered, and how often a ref scan had to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VisibilityStats {
  pub hits: u64,
  pub misses: u64,
}

impl Default for VisibilityCache {
  fn default() -> Self {
    VisibilityCache::new()
  }
}

impl VisibilityCache {
  pub fn new() -> Self {
    VisibilityCache {
      entries: Mutex::new(HashMap::new()),
      hits: AtomicU64::new(0),
      misses: AtomicU64::new(0),
    }
  }

  /// Counted so the claim this module exists to make -- that a walk decides
  /// reachability once, not once per directory -- is observable rather than
  /// asserted.
  pub fn stats(&self) -> VisibilityStats {
    VisibilityStats {
      hits: self.hits.load(Ordering::Relaxed),
      misses: self.misses.load(Ordering::Relaxed),
    }
  }

  /// The remembered verdict, when one was decided under this generation and is
  /// still inside its TTL.
  pub fn get(
    &self,
    repository_id: &RepositoryId,
    commit: &ObjectId,
    generation: u64,
  ) -> Option<bool> {
    let key = (repository_id.clone(), commit.clone());
    let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
    let hit = entries
      .get(&key)
      .filter(|verdict| verdict.usable(generation, Instant::now()))
      .map(|verdict| verdict.visible);
    match hit {
      Some(_) => self.hits.fetch_add(1, Ordering::Relaxed),
      None => self.misses.fetch_add(1, Ordering::Relaxed),
    };
    hit
  }

  /// Remember a verdict against the generation it was computed under.
  ///
  /// The generation is the caller's, read *before* the verdict was computed. A
  /// ref change that lands during the computation therefore stamps the result
  /// with the older generation and the next reader recomputes, rather than the
  /// result being stamped with a generation it does not describe.
  pub fn insert(
    &self,
    repository_id: &RepositoryId,
    commit: &ObjectId,
    generation: u64,
    visible: bool,
  ) {
    let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
    if entries.len() >= CAPACITY {
      entries.clear();
    }
    entries.insert(
      (repository_id.clone(), commit.clone()),
      Verdict {
        visible,
        generation,
        decided: Instant::now(),
      },
    );
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use gfs_types::HashAlgorithm;

  fn commit(byte: u8) -> ObjectId {
    ObjectId::from_raw(HashAlgorithm::Sha1, &[byte; 20]).unwrap()
  }

  #[test]
  fn a_verdict_is_not_reused_across_a_ref_generation() {
    let cache = VisibilityCache::new();
    let repo = RepositoryId::parse("r").unwrap();
    cache.insert(&repo, &commit(1), 7, true);
    assert_eq!(cache.get(&repo, &commit(1), 7), Some(true));
    assert_eq!(cache.get(&repo, &commit(1), 8), None);
  }

  #[test]
  fn an_unknown_commit_has_no_verdict() {
    let cache = VisibilityCache::new();
    let repo = RepositoryId::parse("r").unwrap();
    cache.insert(&repo, &commit(1), 1, true);
    assert_eq!(cache.get(&repo, &commit(2), 1), None);
  }
}
