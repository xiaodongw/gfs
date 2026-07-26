//! Per-repository serialization.
//!
//! PLAN.md M1.2 requires per-repository locking, and DESIGN.md section 7.1
//! requires mount creation to run "under the repository's ref/maintenance lock".
//! The property that matters is narrow but absolute: between resolving a selector
//! and durably anchoring the resulting commit, nothing else may move that
//! repository's refs or run maintenance on it. Otherwise a force push lands in the
//! gap and the mount is pinned to a commit that is already prunable -- which is the
//! "no client-visible gap between revision resolution and lease activation"
//! requirement in M1's exit criteria.
//!
//! In-process locks are sufficient for the single-node prototype (DESIGN.md
//! section 7.6) and are *not* sufficient for M7.1's multi-node data plane, which
//! partitions repositories to owner nodes precisely so this lock stays local. That
//! is a design decision rather than a limitation to fix later: a distributed lock
//! on this path would put a network round trip inside every mount creation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use xvfs_types::RepositoryId;

/// A registry of per-repository locks, created on first use.
#[derive(Default)]
pub struct RepositoryLocks {
  locks: Mutex<HashMap<RepositoryId, Arc<tokio::sync::Mutex<()>>>>,
}

impl std::fmt::Debug for RepositoryLocks {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
    f.debug_struct("RepositoryLocks")
      .field("repositories", &locks.len())
      .finish()
  }
}

impl RepositoryLocks {
  pub fn new() -> Self {
    Self::default()
  }

  /// The lock for one repository.
  ///
  /// Returned as an `Arc` rather than a guard so the caller decides the critical
  /// section's extent. Entries are never removed: a repository's lock is one
  /// pointer, repositories are created rarely, and removing an entry would need
  /// proof that no waiter holds it -- which is more machinery than the memory is
  /// worth.
  pub fn get(&self, id: &RepositoryId) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
    Arc::clone(locks.entry(id.clone()).or_default())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn the_same_repository_returns_the_same_lock() {
    let locks = RepositoryLocks::new();
    let a = RepositoryId::parse("r1").unwrap();
    assert!(Arc::ptr_eq(&locks.get(&a), &locks.get(&a)));
  }

  #[tokio::test]
  async fn different_repositories_do_not_serialize_against_each_other() {
    // A mount on one repository must not wait behind maintenance on another. With
    // one global lock, a slow `gc` on a monorepo would stall every unrelated job.
    let locks = RepositoryLocks::new();
    let a = locks.get(&RepositoryId::parse("r1").unwrap());
    let b = locks.get(&RepositoryId::parse("r2").unwrap());
    let held = a.lock().await;
    // Would deadlock if the two shared a lock.
    let _other = b.lock().await;
    drop(held);
  }

  #[tokio::test]
  async fn the_same_repository_serializes() {
    let locks = std::sync::Arc::new(RepositoryLocks::new());
    let id = RepositoryId::parse("r1").unwrap();
    let order = Arc::new(Mutex::new(Vec::new()));

    let guard = locks.get(&id).lock_owned().await;
    order.lock().unwrap().push("first-acquired");

    let l2 = locks.get(&id);
    let order2 = Arc::clone(&order);
    let waiter = tokio::spawn(async move {
      let _g = l2.lock().await;
      order2.lock().unwrap().push("second-acquired");
    });

    // Give the waiter a chance to run; it must not acquire.
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    order.lock().unwrap().push("first-releasing");
    drop(guard);
    waiter.await.unwrap();

    assert_eq!(
      *order.lock().unwrap(),
      ["first-acquired", "first-releasing", "second-acquired"]
    );
  }
}
