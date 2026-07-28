//! The catalog schema and its migrations.
//!
//! SQLite for the single-node prototype (DESIGN.md section 7.6); M7.1 moves this
//! to PostgreSQL. The schema is written so that move is mechanical: no SQLite
//! specific types, integer Unix times rather than SQLite date functions, and
//! every state stored as its `SCREAMING_SNAKE_CASE` wire name so a dump is
//! readable and a migration does not have to decode an enum ordinal.

use gfs_types::error::{ErrorCode, GfsError};
use rusqlite::Connection;

/// The current schema version.
///
/// Checked on open. A database from a newer version is refused rather than read,
/// because reading a schema you do not understand is how a control plane
/// corrupts a lease -- and a corrupted lease is a mount that dies mid-job.
pub const SCHEMA_VERSION: i64 = 1;

pub fn open(path: &std::path::Path) -> Result<Connection, GfsError> {
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent).map_err(|e| {
      GfsError::new(
        ErrorCode::Internal,
        format!("cannot create catalog directory: {e}"),
      )
    })?;
  }
  let conn = Connection::open(path).map_err(db_error)?;
  configure(&conn)?;
  migrate(&conn)?;
  Ok(conn)
}

/// An in-memory catalog, for tests.
pub fn open_in_memory() -> Result<Connection, GfsError> {
  let conn = Connection::open_in_memory().map_err(db_error)?;
  configure(&conn)?;
  migrate(&conn)?;
  Ok(conn)
}

fn configure(conn: &Connection) -> Result<(), GfsError> {
  // WAL for concurrent readers alongside a writer, and FULL synchronous because
  // this database is the durable record behind a retention lease. A lease record
  // that survives the application but not the machine would let `gc` prune a
  // commit out from under a live mount after a power loss, which is the exact
  // failure retention leases exist to prevent.
  conn
    .execute_batch(
      "PRAGMA journal_mode = WAL;
       PRAGMA synchronous = FULL;
       PRAGMA foreign_keys = ON;
       PRAGMA busy_timeout = 5000;",
    )
    .map_err(db_error)?;
  Ok(())
}

fn migrate(conn: &Connection) -> Result<(), GfsError> {
  let current: i64 = conn
    .query_row("PRAGMA user_version", [], |r| r.get(0))
    .map_err(db_error)?;

  if current > SCHEMA_VERSION {
    return Err(GfsError::new(
      ErrorCode::FailedPrecondition,
      format!(
        "catalog schema version {current} is newer than this build understands \
         ({SCHEMA_VERSION}); refusing to read it"
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
    .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
    .map_err(db_error)?;
  Ok(())
}

const V1: &str = r#"
CREATE TABLE repositories (
  repository_id   TEXT PRIMARY KEY,
  display_name    TEXT NOT NULL,
  -- Server-side filesystem path to the bare repository. Never derived from the
  -- display name, and never sent to a client.
  repo_path       TEXT NOT NULL,
  state           TEXT NOT NULL,
  algorithm       TEXT NOT NULL,
  upstream_url    TEXT,
  -- A *reference* to a credential in the deployment's secret store, never the
  -- secret itself (PLAN.md M1.2). A catalog dump must not be a credential leak.
  credential_ref  TEXT,
  -- Why the repository was quarantined, when it was.
  quarantine_reason TEXT,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL
) STRICT;

-- The current tip of every ref GFS mirrors.
CREATE TABLE repository_refs (
  repository_id   TEXT NOT NULL REFERENCES repositories(repository_id) ON DELETE CASCADE,
  ref_name        TEXT NOT NULL,
  oid             TEXT NOT NULL,
  -- Monotonic within a repository, incremented on every observed update. Lets a
  -- caller detect that a ref moved without comparing OIDs, which cannot tell
  -- "unchanged" from "moved and moved back".
  ref_version     INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL,
  PRIMARY KEY (repository_id, ref_name)
) STRICT;

-- The durable ref-event outbox.
CREATE TABLE ref_events (
  event_id        INTEGER PRIMARY KEY AUTOINCREMENT,
  repository_id   TEXT NOT NULL REFERENCES repositories(repository_id) ON DELETE CASCADE,
  ref_name        TEXT NOT NULL,
  -- NULL means the ref was created / deleted respectively.
  old_oid         TEXT,
  new_oid         TEXT,
  ref_version     INTEGER NOT NULL,
  observed_at     INTEGER NOT NULL,
  processed_at    INTEGER,
  -- DESIGN.md section 7.1: "Ref events are idempotent and keyed by
  -- (repository_id, ref_name, old_oid, new_oid)". A webhook delivered twice, or
  -- a webhook racing the poller, must produce one event.
  UNIQUE (repository_id, ref_name, old_oid, new_oid)
) STRICT;

CREATE INDEX ref_events_unprocessed
  ON ref_events (repository_id, processed_at)
  WHERE processed_at IS NULL;

-- Cataloged commits and their sanitized snapshot times.
CREATE TABLE commits (
  repository_id   TEXT NOT NULL REFERENCES repositories(repository_id) ON DELETE CASCADE,
  commit_oid      TEXT NOT NULL,
  -- ADR 0006's clamp, computed once and then durable. Replicas reuse the stored
  -- value and never recompute it, or base timestamps would differ between hosts
  -- and M2's "identical across remounts and hosts" criterion would fail.
  snapshot_secs   INTEGER NOT NULL,
  snapshot_nanos  INTEGER NOT NULL,
  -- The authoritative first-seen time the clamp used. Kept so the derivation can
  -- be audited, and so a future migration can recompute deliberately rather than
  -- by guessing.
  first_seen_secs INTEGER NOT NULL,
  first_seen_nanos INTEGER NOT NULL,
  committer_secs  INTEGER NOT NULL,
  PRIMARY KEY (repository_id, commit_oid)
) STRICT;

-- Mount retention leases.
CREATE TABLE leases (
  mount_id        TEXT PRIMARY KEY,
  repository_id   TEXT NOT NULL REFERENCES repositories(repository_id) ON DELETE CASCADE,
  commit_oid      TEXT NOT NULL,
  -- The subject the capability is bound to. An authorization decision, so it is
  -- recorded rather than re-derived.
  subject         TEXT NOT NULL,
  state           TEXT NOT NULL,
  anchor_ref      TEXT NOT NULL,
  created_at      INTEGER NOT NULL,
  expires_at      INTEGER NOT NULL,
  -- When the lease left ACTIVE, so the prune delay is measured from a fact
  -- rather than from when a sweep happened to notice.
  terminal_at     INTEGER,
  renewal_failures INTEGER NOT NULL DEFAULT 0,
  updated_at      INTEGER NOT NULL
) STRICT;

CREATE INDEX leases_by_repo_state ON leases (repository_id, state);
CREATE INDEX leases_by_expiry ON leases (state, expires_at);
-- Reachability roots are looked up by commit during maintenance.
CREATE INDEX leases_by_commit ON leases (repository_id, commit_oid, state);
"#;

pub fn db_error(e: rusqlite::Error) -> GfsError {
  // The message may contain SQL but never repository content, so it is safe to
  // surface and to log. A busy database is reported as retryable rather than
  // internal, because a caller retrying is exactly the right response.
  match e {
    rusqlite::Error::SqliteFailure(err, _)
      if err.code == rusqlite::ErrorCode::DatabaseBusy
        || err.code == rusqlite::ErrorCode::DatabaseLocked =>
    {
      GfsError::new(ErrorCode::Unavailable, "catalog is busy")
    }
    other => GfsError::new(ErrorCode::Internal, format!("catalog error: {other}")),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_fresh_catalog_migrates_to_the_current_version() {
    let conn = open_in_memory().unwrap();
    let v: i64 = conn
      .query_row("PRAGMA user_version", [], |r| r.get(0))
      .unwrap();
    assert_eq!(v, SCHEMA_VERSION);
  }

  #[test]
  fn migration_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.sqlite");
    open(&path).unwrap();
    // Reopening must not fail or re-run the migration.
    open(&path).unwrap();
    let conn = open(&path).unwrap();
    let repos: i64 = conn
      .query_row("SELECT count(*) FROM repositories", [], |r| r.get(0))
      .unwrap();
    assert_eq!(repos, 0);
  }

  #[test]
  fn a_newer_schema_is_refused_rather_than_read() {
    // Reading a schema this build does not understand could mis-parse a lease,
    // and a mis-parsed lease is a mount that dies mid-job.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.sqlite");
    {
      let conn = open(&path).unwrap();
      conn
        .execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
        .unwrap();
    }
    let err = open(&path).unwrap_err();
    assert_eq!(err.code, ErrorCode::FailedPrecondition);
  }

  #[test]
  fn the_ref_event_idempotency_key_collapses_a_duplicate_delivery() {
    // A webhook delivered twice, or a webhook racing the poller, must produce one
    // event. DESIGN.md section 7.1 fixes the key.
    let conn = open_in_memory().unwrap();
    conn
      .execute(
        "INSERT INTO repositories (repository_id, display_name, repo_path, state,
           algorithm, created_at, updated_at)
         VALUES ('r1', 'r', '/tmp/r', 'ACTIVE', 'sha1', 0, 0)",
        [],
      )
      .unwrap();
    let insert = "INSERT OR IGNORE INTO ref_events
      (repository_id, ref_name, old_oid, new_oid, ref_version, observed_at)
      VALUES ('r1', 'refs/heads/main', 'sha1:aa', 'sha1:bb', 1, 0)";
    assert_eq!(conn.execute(insert, []).unwrap(), 1);
    assert_eq!(
      conn.execute(insert, []).unwrap(),
      0,
      "duplicate must collapse"
    );
    let n: i64 = conn
      .query_row("SELECT count(*) FROM ref_events", [], |r| r.get(0))
      .unwrap();
    assert_eq!(n, 1);
  }
}
