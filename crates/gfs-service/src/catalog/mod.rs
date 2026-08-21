//! The repository catalog: repositories, refs, cataloged commits, and leases.
//!
//! Every method is synchronous. Callers reach it from async code through
//! `spawn_blocking`, and the lease path deliberately runs its *whole* critical
//! section -- resolve, catalog write, ref anchor, catalog write -- inside one
//! blocking closure under the repository lock, so no `await` point can interleave
//! another mount into the middle of it.

pub mod leases;
pub mod refs;
pub mod repositories;
pub mod schema;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use gfs_types::error::GfsError;
use gfs_types::RepositoryId;
use rusqlite::Connection;

pub use leases::{Lease, LeaseSweepOutcome};
pub use repositories::{RepositoryRecord, RepositoryState};

/// The catalog.
///
/// One connection behind a mutex rather than a pool. SQLite serializes writers
/// anyway, the operations here are small local transactions, and a single
/// connection removes the class of bug where two connections in one logical
/// transaction deadlock against each other. M7.1 replaces this with PostgreSQL
/// and a real pool.
pub struct Catalog {
  conn: Mutex<Connection>,
  /// An in-process counter per repository, bumped by every ref observation that
  /// changed something. Read by caches that must not outlive the ref state they
  /// were computed against -- see [`crate::auth::VisibilityCache`].
  ///
  /// Deliberately *not* `MAX(ref_version)` from the table. That value is not
  /// monotonic: deleting the highest-versioned ref removes its row, so the
  /// maximum drops back and a later observation reuses the number. A counter
  /// that can return to a value it already had is not a generation. It is also
  /// free to read, where the query is one statement per authorization.
  ref_generations: Mutex<HashMap<RepositoryId, u64>>,
}

impl std::fmt::Debug for Catalog {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Catalog").finish_non_exhaustive()
  }
}

impl Catalog {
  pub fn open(path: &Path) -> Result<Self, GfsError> {
    Ok(Catalog {
      conn: Mutex::new(schema::open(path)?),
      ref_generations: Mutex::new(HashMap::new()),
    })
  }

  pub fn open_in_memory() -> Result<Self, GfsError> {
    Ok(Catalog {
      conn: Mutex::new(schema::open_in_memory()?),
      ref_generations: Mutex::new(HashMap::new()),
    })
  }

  /// Run a closure with the connection.
  ///
  /// Lock poisoning is recovered from rather than propagated: a panic while
  /// holding the lock leaves SQLite itself consistent, because any open
  /// transaction is rolled back when its `Transaction` guard drops. Refusing to
  /// serve afterwards would turn one request's bug into an outage, and would
  /// specifically stop lease *renewals* -- so a panic in an unrelated handler
  /// would expire every live mount.
  pub(crate) fn with_conn<T>(
    &self,
    f: impl FnOnce(&Connection) -> Result<T, GfsError>,
  ) -> Result<T, GfsError> {
    let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
    f(&conn)
  }

  pub(crate) fn with_tx<T>(
    &self,
    f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T, GfsError>,
  ) -> Result<T, GfsError> {
    let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
    let tx = conn.transaction().map_err(schema::db_error)?;
    let out = f(&tx)?;
    tx.commit().map_err(schema::db_error)?;
    Ok(out)
  }

  /// The repository's current ref generation.
  ///
  /// In-memory and meaningful only within this process, which is all a
  /// process-local cache needs: it dies with the value it keys on.
  pub fn ref_generation(&self, repository_id: &RepositoryId) -> u64 {
    self
      .ref_generations
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .get(repository_id)
      .copied()
      .unwrap_or(0)
  }

  /// Advance the repository's ref generation. Called by every path that records
  /// a real ref change, so a memoized reachability verdict is invalidated by the
  /// same write that made it wrong.
  pub(crate) fn bump_ref_generation(&self, repository_id: &RepositoryId) {
    let mut generations = self
      .ref_generations
      .lock()
      .unwrap_or_else(|e| e.into_inner());
    *generations.entry(repository_id.clone()).or_insert(0) += 1;
  }
}

/// Unix seconds, the catalog's time unit.
pub fn now_secs() -> i64 {
  std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_secs() as i64)
    .unwrap_or(0)
}
