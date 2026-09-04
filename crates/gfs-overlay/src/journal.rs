//! `overlay.sqlite`: the durable half of the overlay.
//!
//! # What the journal is authoritative for
//!
//! Everything except file bytes. A row says a path exists, what kind it is, which
//! content it points at, and what the base had there. The bytes themselves live
//! in `files/`, and the ordering rule in [`crate::store`] guarantees that a
//! committed row's content is already on disk.
//!
//! # One transaction per acknowledged mutation
//!
//! A mutation is a [`Change`] list applied in a single SQLite transaction. That
//! matters most for `rename`, which is a delete and one or more inserts that must
//! not be separable: a crash between them would leave a path that exists twice or
//! not at all. PLAN.md M3.1 asks for exactly this transaction ordering, and it is
//! why `rename` is not implemented as "unlink then create".
//!
//! # `synchronous = NORMAL`, and what that buys
//!
//! Two failure models, and they get different guarantees, stated here so the
//! crash tests can assert the right thing:
//!
//! * **the daemon dies** (`kill -9`, panic, OOM) — every committed transaction
//!   survives, because the write-ahead log is already in the kernel's hands.
//!   Every mutation the filesystem acknowledged is therefore recoverable.
//! * **the host dies** (power loss) — recent commits may be lost unless something
//!   forced them out, which is what `fsync(2)` on the mount does
//!   ([`crate::Overlay::sync`]).
//!
//! `FULL` would fsync on every metadata operation. The catalog uses it because a
//! lease that survives the process but not the machine lets `gc` prune a live
//! mount's objects; the overlay's exposure is one job's uncommitted edits, and
//! POSIX already says those need an `fsync` to be durable. The content store
//! makes the same promise the same way ([`crate::store`]): no fsync per file,
//! everything forced out together by `sync`.
//!
//! # What a commit costs, and what is not committed
//!
//! One transaction is about 30 µs of SQLite on a warm machine, so the journal
//! keeps the count down rather than the engine fast: statements are prepared
//! once, the allocator counters are rewritten only when they moved, the parent
//! directory's timestamp travels in the child's transaction, and a `write`
//! commits nothing at all — the row's size and mtime catch up when the
//! descriptor is released ([`crate::Overlay::settle_content`]), and recovery
//! reads them off the content file for a row that never caught up.

use std::path::Path;

use gfs_types::Timestamp;
use rusqlite::{Connection, OptionalExtension};

use crate::error::{OverlayError, Result};
use crate::state::{OverlayEntry, Row};

/// The current schema version. Refused, never guessed at, when it is newer.
pub const SCHEMA_VERSION: i64 = 2;

/// How many vanished paths the journal will remember before it gives up and
/// tells the fsmonitor hook to ask for a full rescan.
///
/// A path is *vanished* when its row was deleted outright — created and then
/// removed, or renamed away from — leaving nothing for [`crate::Status`] to
/// report. The fsmonitor hook still has to name it, or Git trusts its cached
/// answer forever ([`crate::Overlay::vanished`] has the whole argument), so the
/// journal keeps the names.
///
/// The cap is what keeps that bounded. A build that writes and deletes a
/// million temporaries would otherwise grow this set without limit; past the
/// cap the set is dropped and the hook answers "rescan everything", which is
/// slow and correct rather than fast and wrong.
pub const VANISHED_LIMIT: usize = 4096;

/// The state-file format the overlay directory as a whole speaks.
pub const OVERLAY_FORMAT_VERSION: u32 = 1;

pub const OVERLAY_DB_FILE: &str = "overlay.sqlite";

/// One atomic edit to the journal.
#[derive(Clone, Debug)]
pub enum Change {
  Put(OverlayEntry),
  Delete(gfs_types::BytePath),
}

/// What one transaction did to the vanished set, computed from its changes.
///
/// Travels with the changes into the same SQLite transaction: a crash that
/// dropped the row but kept the path out of this set would leave a deletion the
/// fsmonitor hook can never mention again.
#[derive(Clone, Debug, Default)]
pub struct VanishedDelta {
  /// Paths whose row this transaction removes without leaving a whiteout.
  pub gone: Vec<Vec<u8>>,
  /// Paths this transaction gives a row back to. A row is what [`Status`]
  /// reports from, so the separate record is no longer needed.
  ///
  /// [`Status`]: crate::Status
  pub returned: Vec<Vec<u8>>,
}

impl VanishedDelta {
  /// Derive the delta from a change list.
  ///
  /// A path that is deleted *and* put in the same transaction — which is what a
  /// rename onto an existing path looks like — counts as returned: the row
  /// exists when the transaction ends, so `Status` sees it.
  pub fn of(changes: &[Change]) -> Self {
    let mut delta = VanishedDelta::default();
    for change in changes {
      match change {
        Change::Put(entry) => {
          let path = entry.path.as_bytes().to_vec();
          delta.gone.retain(|p| p != &path);
          delta.returned.push(path);
        }
        Change::Delete(path) => {
          let path = path.as_bytes().to_vec();
          delta.returned.retain(|p| p != &path);
          delta.gone.push(path);
        }
      }
    }
    delta
  }

  pub fn is_empty(&self) -> bool {
    self.gone.is_empty() && self.returned.is_empty()
  }
}

/// Identity the overlay is bound to, checked on every open.
#[derive(Clone, Debug)]
pub struct Binding {
  pub repository_id: String,
  /// The pinned commit, qualified. An overlay is only meaningful against the
  /// base it diverged from, so opening it against another commit is refused
  /// rather than merged — the paths would still resolve and the answers would
  /// silently be about a different tree.
  pub base_commit: String,
}

pub struct Journal {
  conn: Connection,
  /// The allocator values the meta table holds, so [`Journal::apply`] can skip
  /// rewriting them when nothing was allocated.
  persisted_next_ino: u64,
  persisted_next_content_id: u64,
}

impl std::fmt::Debug for Journal {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Journal").finish_non_exhaustive()
  }
}

impl Journal {
  pub fn open(path: &Path, binding: &Binding) -> Result<Self> {
    let conn = Connection::open(path).map_err(db)?;
    configure(&conn)?;
    migrate(&conn)?;
    let mut journal = Journal {
      conn,
      persisted_next_ino: 0,
      persisted_next_content_id: 0,
    };
    journal.bind(binding)?;
    journal.persisted_next_ino = journal.next_ino()?;
    journal.persisted_next_content_id = journal.next_content_id()?;
    Ok(journal)
  }

  /// Refuse an overlay that belongs to a different mount.
  fn bind(&self, binding: &Binding) -> Result<()> {
    let stored_repo: Option<String> = self.meta("repository_id")?;
    let stored_commit: Option<String> = self.meta("base_commit")?;
    match (stored_repo, stored_commit) {
      (None, None) => {
        self.set_meta("repository_id", &binding.repository_id)?;
        self.set_meta("base_commit", &binding.base_commit)?;
        self.set_meta("format_version", &OVERLAY_FORMAT_VERSION.to_string())?;
        self.set_meta("next_ino", &crate::OVERLAY_INO_BASE.to_string())?;
        self.set_meta("next_content_id", "1")?;
        Ok(())
      }
      (Some(repo), Some(commit)) => {
        if repo != binding.repository_id || commit != binding.base_commit {
          // The refusal protects edits. An overlay with no rows holds none —
          // the common leftover of an unmounted workspace whose branch moved
          // before the next mount — and refusing it would block every re-clone
          // for the sake of nothing.
          let edits: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
            .map_err(db)?;
          if edits == 0 {
            self.set_meta("repository_id", &binding.repository_id)?;
            self.set_meta("base_commit", &binding.base_commit)?;
            return Ok(());
          }
          return Err(OverlayError::new(
            crate::error::Condition::Invalid,
            format!(
              "this overlay holds edits against {repo}@{commit} and cannot be reopened \
               against {}@{}; export or discard them first",
              binding.repository_id, binding.base_commit
            ),
          ));
        }
        Ok(())
      }
      _ => Err(OverlayError::io(
        "the overlay journal is missing its binding; it is damaged",
      )),
    }
  }

  /// Point the journal at a new base, discarding every row.
  ///
  /// One transaction, so there is no window in which the overlay is
  /// half-cleared: a crash either left the old binding with the old rows or
  /// the new binding with none. The id counters are deliberately not reset —
  /// an inode number handed out under the old pin must never be reissued for
  /// a different path (DESIGN.md section 8.2's stable-identity rule).
  ///
  /// This is what a repin uses (ADR 0011): the overlay directory and its
  /// SQLite connection live for the whole mount — opened before the workspace
  /// is mounted over, never reopened through it — and only the binding moves.
  pub fn rebind(&self, binding: &Binding) -> Result<()> {
    self.conn.execute_batch("BEGIN IMMEDIATE;").map_err(db)?;
    let result = (|| {
      self.conn.execute("DELETE FROM entries", []).map_err(db)?;
      // A new generation is a full rescan for the fsmonitor hook whatever this
      // set held, so carrying it across would only make the first answer of the
      // new pin name paths that belong to the old one.
      self.conn.execute("DELETE FROM vanished", []).map_err(db)?;
      self.set_meta("vanished_overflow", "0")?;
      self.set_meta("root_mtime", "")?;
      self.set_meta("repository_id", &binding.repository_id)?;
      self.set_meta("base_commit", &binding.base_commit)?;
      Ok(())
    })();
    match result {
      Ok(()) => self.conn.execute_batch("COMMIT;").map_err(db),
      Err(e) => {
        let _ = self.conn.execute_batch("ROLLBACK;");
        Err(e)
      }
    }
  }

  pub fn meta(&self, key: &str) -> Result<Option<String>> {
    self
      .conn
      .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
      .optional()
      .map_err(db)
  }

  pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
    self
      .conn
      .prepare_cached(UPSERT_META)
      .map_err(db)?
      .execute((key, value))
      .map_err(db)?;
    Ok(())
  }

  fn counter(&self, key: &str, floor: u64) -> Result<u64> {
    Ok(
      self
        .meta(key)?
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(floor),
    )
  }

  pub fn next_ino(&self) -> Result<u64> {
    self.counter("next_ino", crate::OVERLAY_INO_BASE)
  }

  pub fn next_content_id(&self) -> Result<u64> {
    self.counter("next_content_id", 1)
  }

  /// The mount root's recorded modification and change times.
  ///
  /// Meta rather than an entry: the root cannot have a journal row. The empty
  /// path terminates every ancestor walk in this crate, and giving it a row
  /// would make the root resolvable two ways. `None` means nothing has changed
  /// in the root directory yet and the snapshot time still describes it.
  pub fn root_times(&self) -> Result<Option<(Timestamp, Timestamp)>> {
    let Some(raw) = self.meta("root_mtime")? else {
      return Ok(None);
    };
    let mtime = parse_time(&raw);
    let ctime = self.meta("root_ctime")?.as_deref().and_then(parse_time);
    match (mtime, ctime) {
      (Some(m), Some(c)) => Ok(Some((m, c))),
      (Some(m), None) => Ok(Some((m, m))),
      _ => Ok(None),
    }
  }

  /// The remembered vanished paths, and whether the set overflowed its cap.
  ///
  /// Overflow is sticky for the generation: once the journal has stopped
  /// recording names it cannot reconstruct the ones it dropped, so the only
  /// honest answer left is "rescan".
  pub fn vanished(&self) -> Result<(Vec<Vec<u8>>, bool)> {
    let overflow = self.meta("vanished_overflow")?.as_deref() == Some("1");
    let mut stmt = self.conn.prepare("SELECT path FROM vanished").map_err(db)?;
    let rows = stmt
      .query_map([], |r| r.get::<_, Vec<u8>>(0))
      .map_err(db)?
      .collect::<std::result::Result<Vec<_>, _>>()
      .map_err(db)?;
    Ok((rows, overflow))
  }

  /// Every row, for the in-memory index built at open.
  pub fn load(&self) -> Result<Vec<OverlayEntry>> {
    let mut stmt = self.conn.prepare(SELECT_ALL).map_err(db)?;
    let rows = stmt
      .query_map([], |r| {
        Ok(Row {
          path: r.get(0)?,
          parent: r.get(1)?,
          present: r.get(2)?,
          kind: r.get(3)?,
          opaque: r.get(4)?,
          ino: r.get(5)?,
          content_kind: r.get(6)?,
          content_id: r.get(7)?,
          content_oid: r.get(8)?,
          symlink_target: r.get(9)?,
          size: r.get(10)?,
          mtime_secs: r.get(11)?,
          mtime_nanos: r.get(12)?,
          ctime_secs: r.get(13)?,
          ctime_nanos: r.get(14)?,
          renamed_from: r.get(15)?,
          base_oid: r.get(16)?,
          base_mode: r.get(17)?,
          base_size: r.get(18)?,
        })
      })
      .map_err(db)?;

    let mut out = Vec::new();
    for row in rows {
      let row = row.map_err(db)?;
      out.push(
        row
          .decode()
          .map_err(|e| OverlayError::io(format!("an overlay row is undecodable: {e}")))?,
      );
    }
    Ok(out)
  }

  /// Apply a mutation atomically, advancing the persisted allocators with it.
  ///
  /// The allocators travel in the same transaction on purpose. A crash between
  /// "the row that uses content id 7 is committed" and "the counter says 8" would
  /// hand id 7 out twice, and the second use would rename a new file over the
  /// bytes the first one is still serving.
  ///
  /// `vanished` and `overflow` travel with them for the same reason: the set of
  /// deletions the fsmonitor hook still has to report is only useful if it
  /// cannot disagree with the rows. `root_times`, when given, are the mount
  /// root's new timestamps — a create or delete directly in the root advances
  /// them in the same transaction as the row it adds or removes.
  #[allow(clippy::too_many_arguments)]
  pub fn apply(
    &mut self,
    changes: &[Change],
    vanished: &VanishedDelta,
    overflow: bool,
    next_ino: u64,
    next_content_id: u64,
    root_times: Option<(Timestamp, Timestamp)>,
  ) -> Result<()> {
    let tx = self.conn.transaction().map_err(db)?;
    for change in changes {
      match change {
        Change::Put(entry) => {
          let row = Row::of(entry);
          tx.prepare_cached(INSERT)
            .map_err(db)?
            .execute(rusqlite::params![
              row.path,
              row.parent,
              row.present,
              row.kind,
              row.opaque,
              row.ino,
              row.content_kind,
              row.content_id,
              row.content_oid,
              row.symlink_target,
              row.size,
              row.mtime_secs,
              row.mtime_nanos,
              row.ctime_secs,
              row.ctime_nanos,
              row.renamed_from,
              row.base_oid,
              row.base_mode,
              row.base_size,
            ])
            .map_err(db)?;
        }
        Change::Delete(path) => {
          tx.prepare_cached("DELETE FROM entries WHERE path = ?1")
            .map_err(db)?
            .execute([path.as_bytes()])
            .map_err(db)?;
        }
      }
    }
    if overflow {
      // Nothing left to say path by path. The flag is the whole answer, and
      // keeping the names would only cost space for a set that is already
      // superseded.
      tx.execute("DELETE FROM vanished", []).map_err(db)?;
      tx.prepare_cached(UPSERT_META)
        .map_err(db)?
        .execute(("vanished_overflow", "1"))
        .map_err(db)?;
    } else {
      for path in &vanished.returned {
        tx.prepare_cached("DELETE FROM vanished WHERE path = ?1")
          .map_err(db)?
          .execute([path])
          .map_err(db)?;
      }
      for path in &vanished.gone {
        tx.prepare_cached("INSERT OR IGNORE INTO vanished (path) VALUES (?1)")
          .map_err(db)?
          .execute([path])
          .map_err(db)?;
      }
    }
    if next_ino != self.persisted_next_ino {
      tx.prepare_cached(UPSERT_META)
        .map_err(db)?
        .execute(("next_ino", next_ino.to_string()))
        .map_err(db)?;
    }
    if next_content_id != self.persisted_next_content_id {
      tx.prepare_cached(UPSERT_META)
        .map_err(db)?
        .execute(("next_content_id", next_content_id.to_string()))
        .map_err(db)?;
    }
    if let Some((mtime, ctime)) = root_times {
      tx.prepare_cached(UPSERT_META)
        .map_err(db)?
        .execute(("root_mtime", format_time(mtime)))
        .map_err(db)?;
      tx.prepare_cached(UPSERT_META)
        .map_err(db)?
        .execute(("root_ctime", format_time(ctime)))
        .map_err(db)?;
    }
    crate::fault::trip(crate::fault::point::JOURNAL_UNCOMMITTED);
    tx.commit().map_err(db)?;
    self.persisted_next_ino = next_ino;
    self.persisted_next_content_id = next_content_id;
    crate::fault::trip(crate::fault::point::JOURNAL_COMMITTED);
    Ok(())
  }

  /// Force everything committed so far onto stable storage.
  ///
  /// What `fsync(2)` on a mounted file ultimately reaches. A checkpoint rather
  /// than a pragma flip because `synchronous` is a connection setting and
  /// changing it per call would race any other statement on this connection.
  pub fn sync(&self) -> Result<()> {
    self
      .conn
      .execute_batch("PRAGMA wal_checkpoint(FULL);")
      .map_err(db)
  }
}

fn configure(conn: &Connection) -> Result<()> {
  conn
    .execute_batch(
      "PRAGMA journal_mode = WAL;
       PRAGMA synchronous = NORMAL;
       PRAGMA busy_timeout = 5000;",
    )
    .map_err(db)
}

fn migrate(conn: &Connection) -> Result<()> {
  let current: i64 = conn
    .query_row("PRAGMA user_version", [], |r| r.get(0))
    .map_err(db)?;
  if current > SCHEMA_VERSION {
    return Err(OverlayError::new(
      crate::error::Condition::Invalid,
      format!(
        "overlay schema version {current} was written by a newer GFS \
         (this build speaks {SCHEMA_VERSION}); refusing to read it"
      ),
    ));
  }
  if current == SCHEMA_VERSION {
    return Ok(());
  }
  if current < 1 {
    conn.execute_batch(V1).map_err(db)?;
  }
  if current < 2 {
    conn.execute_batch(V2).map_err(db)?;
  }
  conn
    .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
    .map_err(db)?;
  Ok(())
}

const V1: &str = r#"
CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) STRICT;

CREATE TABLE entries (
  -- The byte path, with no leading slash. Bytes rather than text because a Git
  -- path is not required to be UTF-8 and a lossy round trip renames a file.
  path           BLOB PRIMARY KEY,
  parent         BLOB NOT NULL,
  present        INTEGER NOT NULL,
  kind           INTEGER NOT NULL,
  opaque         INTEGER NOT NULL,
  ino            INTEGER NOT NULL,
  content_kind   INTEGER NOT NULL,
  content_id     INTEGER,
  content_oid    TEXT,
  symlink_target BLOB,
  size           INTEGER NOT NULL,
  mtime_secs     INTEGER NOT NULL,
  mtime_nanos    INTEGER NOT NULL,
  ctime_secs     INTEGER NOT NULL,
  ctime_nanos    INTEGER NOT NULL,
  renamed_from   BLOB,
  base_oid       TEXT,
  base_mode      INTEGER,
  base_size      INTEGER
) STRICT;

CREATE INDEX entries_parent ON entries(parent);
"#;

/// Schema 2: the paths whose rows are gone.
///
/// Separate from `entries` because these are exactly the paths that have no
/// entry. See [`VANISHED_LIMIT`] for why the table is capped and
/// [`crate::Overlay::vanished`] for what reads it.
const V2: &str = r#"
CREATE TABLE vanished (
  path BLOB PRIMARY KEY
) STRICT;
"#;

/// The column order every `Row` encode/decode pair depends on. Written once and
/// referenced by both statements, so an added column cannot be forgotten in one.
const SELECT_ALL: &str =
  "SELECT path, parent, present, kind, opaque, ino, content_kind, content_id, \
   content_oid, symlink_target, size, mtime_secs, mtime_nanos, ctime_secs, ctime_nanos, \
   renamed_from, base_oid, base_mode, base_size FROM entries";

const UPSERT_META: &str = "INSERT INTO meta (key, value) VALUES (?1, ?2) \
   ON CONFLICT(key) DO UPDATE SET value = excluded.value";

const INSERT: &str = "INSERT OR REPLACE INTO entries (path, parent, present, kind, opaque, ino, \
   content_kind, content_id, content_oid, symlink_target, size, mtime_secs, mtime_nanos, \
   ctime_secs, ctime_nanos, renamed_from, base_oid, base_mode, base_size) \
   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)";

/// `<secs>.<nanos>`, so the meta table stays human-readable text.
fn format_time(t: Timestamp) -> String {
  format!("{}.{}", t.secs, t.nanos)
}

fn parse_time(raw: &str) -> Option<Timestamp> {
  let (secs, nanos) = raw.split_once('.')?;
  Some(Timestamp::new(secs.parse().ok()?, nanos.parse().ok()?))
}

fn db(e: rusqlite::Error) -> OverlayError {
  OverlayError::io(format!("overlay journal: {e}"))
}
