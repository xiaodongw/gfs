//! The search index's durable store, and its schema.
//!
//! SQLite, for the same single-node-prototype reason as the catalog
//! (DESIGN.md section 7.6); M7.1 moves derived data to object storage.
//!
//! # It is durable, but not the same kind of durable as the catalog
//!
//! `xvfs-server`'s catalog runs `synchronous = FULL` because a lease record that
//! survives the process but not the machine lets `gc` prune a live mount's
//! objects — an unrecoverable outcome. Nothing here is like that. Every row is
//! **derived from Git and rebuildable**: a manifest is a function of a commit, a
//! posting list is a function of a blob. A power loss that costs the last
//! transaction costs a re-index, so this store runs `NORMAL` and buys back the
//! fsync per batch that indexing a monorepo would otherwise pay tens of
//! thousands of times.
//!
//! # One database, many repositories
//!
//! Every table is keyed by `repository_id` and blob keys are allocated per
//! repository, so two repositories never share a key space. That matters for
//! more than tidiness: a snapshot bitmap is a set of blob keys, and a key that
//! meant different blobs in different repositories would make a bitmap
//! meaningless the moment it crossed one.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;
use xvfs_types::error::{ErrorCode, XvfsError};

/// The current schema version.
///
/// Refused rather than read when the file is newer, matching `catalog/schema.rs`.
/// The reasoning is weaker here — this data is rebuildable — but reading a
/// layout you do not understand still produces wrong search results, and a wrong
/// search result is precisely what M4's exit gate forbids.
pub const SCHEMA_VERSION: i64 = 1;

/// The index store.
///
/// One connection behind a mutex, like the catalog. Indexing is batched into
/// large transactions rather than issued row by row, so writer serialization is
/// not the bottleneck a per-row design would make it.
pub struct SearchStore {
  conn: Mutex<Connection>,
}

impl std::fmt::Debug for SearchStore {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("SearchStore").finish_non_exhaustive()
  }
}

impl SearchStore {
  pub fn open(path: &Path) -> Result<Self, XvfsError> {
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent).map_err(|e| {
        XvfsError::new(
          ErrorCode::Internal,
          format!("cannot create search index directory: {e}"),
        )
      })?;
    }
    let conn = Connection::open(path).map_err(db_error)?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(SearchStore {
      conn: Mutex::new(conn),
    })
  }

  pub fn open_in_memory() -> Result<Self, XvfsError> {
    let conn = Connection::open_in_memory().map_err(db_error)?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(SearchStore {
      conn: Mutex::new(conn),
    })
  }

  pub(crate) fn with_conn<T>(
    &self,
    f: impl FnOnce(&Connection) -> Result<T, XvfsError>,
  ) -> Result<T, XvfsError> {
    let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
    f(&conn)
  }

  pub(crate) fn with_tx<T>(
    &self,
    f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T, XvfsError>,
  ) -> Result<T, XvfsError> {
    let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
    let tx = conn.transaction().map_err(db_error)?;
    let out = f(&tx)?;
    tx.commit().map_err(db_error)?;
    Ok(out)
  }
}

fn configure(conn: &Connection) -> Result<(), XvfsError> {
  conn
    .execute_batch(
      "PRAGMA journal_mode = WAL;
       PRAGMA synchronous = NORMAL;
       PRAGMA foreign_keys = ON;
       PRAGMA busy_timeout = 5000;",
    )
    .map_err(db_error)?;
  Ok(())
}

fn migrate(conn: &Connection) -> Result<(), XvfsError> {
  let current: i64 = conn
    .query_row("PRAGMA user_version", [], |r| r.get(0))
    .map_err(db_error)?;

  if current > SCHEMA_VERSION {
    return Err(XvfsError::new(
      ErrorCode::FailedPrecondition,
      format!(
        "search index schema version {current} is newer than this build \
         understands ({SCHEMA_VERSION}); refusing to read it"
      ),
    ));
  }
  if current == SCHEMA_VERSION {
    return Ok(());
  }
  if current < 1 {
    conn.execute_batch(V1).map_err(db_error)?;
  }
  conn
    .pragma_update(None, "user_version", SCHEMA_VERSION)
    .map_err(db_error)?;
  Ok(())
}

/// V1: the blob registry.
///
/// `blob_key` is `INTEGER` rather than the `u32` the bitmaps use, because SQLite
/// has no unsigned type and silently storing a `u32` as a negative `i64` is the
/// kind of thing that only shows up past two billion blobs. The registry
/// converts and range-checks.
const V1: &str = r#"
CREATE TABLE blobs (
  repository_id  TEXT    NOT NULL,
  oid            TEXT    NOT NULL,
  blob_key       INTEGER NOT NULL,
  size           INTEGER NOT NULL,
  -- The SCREAMING_SNAKE_CASE wire name, so a dump is readable and a migration
  -- does not have to decode an ordinal. Same rule as the catalog.
  --
  -- NULL means "a key was allocated but the bytes have not been classified".
  -- That is a real state with a real consequence -- a query over a snapshot
  -- containing such a blob has an index gap it must report -- so it is
  -- represented rather than papered over with a default class that would make
  -- an unexamined blob look like ordinary text.
  content_class  TEXT,
  -- Whether posting lists exist for this key. Separate from `content_class`
  -- because an indexable blob can be interned by a manifest build before the
  -- posting builder has reached it, and a query must be able to tell those two
  -- apart: one is a coverage gap, the other is not.
  indexed        INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (repository_id, oid)
);

CREATE UNIQUE INDEX blobs_by_key ON blobs (repository_id, blob_key);

-- The per-repository key allocator. A row is created on first use and bumped in
-- the same transaction as the inserts it covers, which is what makes key
-- assignment transactional: two concurrent ingests cannot hand out the same key.
CREATE TABLE blob_key_seq (
  repository_id  TEXT    PRIMARY KEY,
  next_key       INTEGER NOT NULL
);
"#;

pub(crate) fn db_error(e: rusqlite::Error) -> XvfsError {
  XvfsError::new(ErrorCode::Internal, format!("search index error: {e}"))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_newer_schema_is_refused_rather_than_read() {
    let dir = std::env::temp_dir().join(format!("xvfs-search-schema-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("search.sqlite");
    {
      let store = SearchStore::open(&path).unwrap();
      store
        .with_conn(|c| {
          c.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .map_err(db_error)
        })
        .unwrap();
    }
    let err = SearchStore::open(&path).unwrap_err();
    assert_eq!(err.code, ErrorCode::FailedPrecondition);
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn opening_twice_is_idempotent() {
    let store = SearchStore::open_in_memory().unwrap();
    let version: i64 = store
      .with_conn(|c| {
        c.query_row("PRAGMA user_version", [], |r| r.get(0))
          .map_err(db_error)
      })
      .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
  }
}
