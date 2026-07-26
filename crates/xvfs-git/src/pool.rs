//! A bounded pool of libgit2 repository handles.
//!
//! ADR 0001 fixed this model against two measured constraints:
//!
//! * `git2::Repository` is `Send` but not `Sync`, and libgit2 documents that one
//!   handle must not be used concurrently from multiple threads. So a handle is
//!   checked out for the life of one request and never shared.
//! * every libgit2 call is synchronous and can block on disk for an unbounded
//!   time -- a cold pack read, a long delta chain. That is the "libgit2 blocks an
//!   async worker" risk in DESIGN.md section 13.
//!
//! The bound is **admission control**, not just handle reuse. When every handle
//! is busy, a caller waits rather than starting another concurrent pack read; an
//! unbounded pool would turn a burst of requests into a thundering herd against
//! the object database, which is slower for everyone than queueing.
//!
//! Waiting happens in the *async* layer (see [`crate::repository::AsyncRepository`]),
//! which acquires a semaphore permit before entering `spawn_blocking`. By the time
//! [`RepoPool::checkout`] runs there is a free handle, so it does not park a
//! blocking thread on a condvar. The condvar remains as the correctness backstop
//! for any synchronous caller, such as a test or a maintenance task.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use xvfs_types::error::{ErrorCode, XvfsError};

pub struct RepoPool {
  path: PathBuf,
  inner: Mutex<PoolInner>,
  available: Condvar,
}

impl std::fmt::Debug for RepoPool {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let inner = self.inner.lock().unwrap();
    f.debug_struct("RepoPool")
      .field("path", &self.path)
      .field("idle", &inner.idle.len())
      .field("live", &inner.live)
      .field("max", &inner.max)
      .finish()
  }
}

struct PoolInner {
  idle: VecDeque<git2::Repository>,
  live: usize,
  max: usize,
}

impl RepoPool {
  /// Open a pool, failing immediately if the repository is unusable.
  ///
  /// The probe open is deliberate: an unopenable repository must fail here, at
  /// catalog registration, rather than becoming a per-request error later. That
  /// is the same "fail at creation, not at serve time" rule as
  /// [`crate::format::check`], applied to the handle rather than the format.
  pub fn open(path: impl AsRef<Path>, max: usize) -> Result<Arc<Self>, XvfsError> {
    let path = path.as_ref().to_path_buf();
    let probe = open_repo(&path)?;
    drop(probe);
    Ok(Arc::new(RepoPool {
      path,
      inner: Mutex::new(PoolInner {
        idle: VecDeque::new(),
        live: 0,
        max: max.max(1),
      }),
      available: Condvar::new(),
    }))
  }

  pub fn path(&self) -> &Path {
    &self.path
  }

  pub fn max_handles(&self) -> usize {
    self.inner.lock().unwrap().max
  }

  /// Check out a handle for the life of one operation.
  pub fn checkout(self: &Arc<Self>) -> Result<PooledRepo, XvfsError> {
    let mut inner = self.inner.lock().unwrap();
    loop {
      if let Some(repo) = inner.idle.pop_front() {
        return Ok(PooledRepo {
          pool: Arc::clone(self),
          repo: Some(repo),
        });
      }
      if inner.live < inner.max {
        inner.live += 1;
        drop(inner);
        // Opening outside the lock: `Repository::open` reads config and scans the
        // object database, and holding the pool lock across it would serialize
        // every other caller behind one cold open.
        let repo = match open_repo(&self.path) {
          Ok(r) => r,
          Err(e) => {
            self.inner.lock().unwrap().live -= 1;
            // Wake a waiter: the reservation just released may be the only one
            // they can get, and without this a failed open can hang the pool.
            self.available.notify_one();
            return Err(e);
          }
        };
        return Ok(PooledRepo {
          pool: Arc::clone(self),
          repo: Some(repo),
        });
      }
      inner = self.available.wait(inner).unwrap();
    }
  }
}

/// A checked-out handle. Returns to the pool on drop.
pub struct PooledRepo {
  pool: Arc<RepoPool>,
  repo: Option<git2::Repository>,
}

impl std::fmt::Debug for PooledRepo {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("PooledRepo")
      .field("path", &self.pool.path)
      .finish_non_exhaustive()
  }
}

impl std::ops::Deref for PooledRepo {
  type Target = git2::Repository;

  fn deref(&self) -> &git2::Repository {
    // Only `None` after `Drop::drop` has taken it, and `Drop` does not call
    // `deref`.
    self
      .repo
      .as_ref()
      .expect("handle taken while still borrowed")
  }
}

impl Drop for PooledRepo {
  fn drop(&mut self) {
    let mut inner = self.pool.inner.lock().unwrap();
    if let Some(repo) = self.repo.take() {
      inner.idle.push_back(repo);
    } else {
      inner.live -= 1;
    }
    drop(inner);
    self.pool.available.notify_one();
  }
}

fn open_repo(path: &Path) -> Result<git2::Repository, XvfsError> {
  git2::Repository::open_bare(path)
    .or_else(|_| git2::Repository::open(path))
    .map_err(|e| {
      // The repository path is a server-side filesystem path and is not echoed
      // to a client. Only libgit2's message travels, which names the extension
      // or condition rather than the location.
      XvfsError::new(
        ErrorCode::UnsupportedRepositoryFormat,
        format!("libgit2 could not open the repository: {}", e.message()),
      )
    })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn empty_repo() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bare.git");
    git2::Repository::init_bare(&path).unwrap();
    (dir, path)
  }

  #[test]
  fn a_handle_returns_to_the_pool_and_is_reused() {
    let (_dir, path) = empty_repo();
    let pool = RepoPool::open(&path, 2).unwrap();
    {
      let _a = pool.checkout().unwrap();
      let _b = pool.checkout().unwrap();
    }
    // Both are idle now, so two more checkouts must not open new handles.
    let _c = pool.checkout().unwrap();
    let _d = pool.checkout().unwrap();
    assert_eq!(pool.max_handles(), 2);
  }

  #[test]
  fn the_pool_bounds_concurrency_rather_than_growing() {
    // The admission-control property. With one handle, a second caller waits
    // until the first releases -- it does not open a second handle and add
    // another concurrent pack read.
    let (_dir, path) = empty_repo();
    let pool = RepoPool::open(&path, 1).unwrap();
    let held = pool.checkout().unwrap();

    let p2 = Arc::clone(&pool);
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let started2 = Arc::clone(&started);
    let waiter = std::thread::spawn(move || {
      let _h = p2.checkout().unwrap();
      started2.store(true, std::sync::atomic::Ordering::SeqCst);
    });

    // The waiter cannot have progressed while the only handle is held.
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(
      !started.load(std::sync::atomic::Ordering::SeqCst),
      "second checkout must block while the only handle is held"
    );

    drop(held);
    waiter.join().unwrap();
    assert!(started.load(std::sync::atomic::Ordering::SeqCst));
  }

  #[test]
  fn opening_a_nonexistent_repository_fails_at_pool_creation() {
    let dir = tempfile::tempdir().unwrap();
    let err = RepoPool::open(dir.path().join("absent"), 1).unwrap_err();
    assert_eq!(err.code, ErrorCode::UnsupportedRepositoryFormat);
  }

  #[test]
  fn a_zero_max_is_raised_to_one_rather_than_deadlocking() {
    let (_dir, path) = empty_repo();
    let pool = RepoPool::open(&path, 0).unwrap();
    assert_eq!(pool.max_handles(), 1);
    let _h = pool.checkout().unwrap();
  }
}
