//! Repository records, their lifecycle state machine, and cataloged commits.

use rusqlite::OptionalExtension;
use xvfs_types::error::{ErrorCode, XvfsError};
use xvfs_types::{time, DisplayName, HashAlgorithm, ObjectId, RepositoryId, Timestamp};

use super::schema::db_error;
use super::{now_secs, Catalog};

/// The repository lifecycle (PLAN.md M1.2).
///
/// `Quarantined` is a state rather than a deletion because a repository that
/// fails verification may hold the only copy of something, and a mirror that
/// looks broken is sometimes a disk that needs replacing rather than data that
/// needs discarding. Quarantine stops serving without destroying.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RepositoryState {
  /// The record exists but the mirror is not yet usable. Not served.
  Creating,
  Active,
  /// Failed verification or was administratively stopped. Not served, not
  /// deleted.
  Quarantined,
  /// Deletion in progress. Not served, and no new lease may be created.
  Deleting,
}

impl RepositoryState {
  pub fn as_str(self) -> &'static str {
    match self {
      RepositoryState::Creating => "CREATING",
      RepositoryState::Active => "ACTIVE",
      RepositoryState::Quarantined => "QUARANTINED",
      RepositoryState::Deleting => "DELETING",
    }
  }

  pub fn parse(s: &str) -> Result<Self, XvfsError> {
    match s {
      "CREATING" => Ok(RepositoryState::Creating),
      "ACTIVE" => Ok(RepositoryState::Active),
      "QUARANTINED" => Ok(RepositoryState::Quarantined),
      "DELETING" => Ok(RepositoryState::Deleting),
      other => Err(XvfsError::internal(format!(
        "unknown repository state {other:?} in the catalog"
      ))),
    }
  }

  /// Whether reads may be served.
  pub fn is_servable(self) -> bool {
    self == RepositoryState::Active
  }

  /// Whether a *new* mount may be created.
  ///
  /// The same as `is_servable` today, and kept separate because they diverge:
  /// draining a repository before deletion must stop new mounts while existing
  /// leases keep reading until their jobs finish.
  pub fn accepts_new_mounts(self) -> bool {
    self == RepositoryState::Active
  }
}

#[derive(Clone, Debug)]
pub struct RepositoryRecord {
  pub repository_id: RepositoryId,
  pub display_name: DisplayName,
  pub repo_path: std::path::PathBuf,
  pub state: RepositoryState,
  pub algorithm: HashAlgorithm,
  pub upstream_url: Option<String>,
  /// A reference into the deployment's secret store, never a secret.
  pub credential_ref: Option<String>,
  pub quarantine_reason: Option<String>,
}

/// What to create a repository from.
#[derive(Clone, Debug)]
pub struct NewRepository {
  pub repository_id: RepositoryId,
  pub display_name: DisplayName,
  pub repo_path: std::path::PathBuf,
  pub algorithm: HashAlgorithm,
  pub upstream_url: Option<String>,
  pub credential_ref: Option<String>,
}

impl Catalog {
  /// Insert a repository in `CREATING`.
  ///
  /// Two-phase on purpose: the record exists before the mirror is usable, so a
  /// crash during mirroring leaves a visible `CREATING` row to reconcile rather
  /// than an orphaned directory with nothing pointing at it.
  pub fn create_repository(&self, new: &NewRepository) -> Result<RepositoryRecord, XvfsError> {
    let now = now_secs();
    self.with_tx(|tx| {
      let existing: Option<String> = tx
        .query_row(
          "SELECT state FROM repositories WHERE repository_id = ?1",
          [new.repository_id.as_str()],
          |r| r.get(0),
        )
        .optional()
        .map_err(db_error)?;
      if let Some(state) = existing {
        return Err(XvfsError::new(
          ErrorCode::Conflict,
          format!("repository already exists in state {state}"),
        ));
      }
      tx.execute(
        "INSERT INTO repositories (repository_id, display_name, repo_path, state,
           algorithm, upstream_url, credential_ref, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
        rusqlite::params![
          new.repository_id.as_str(),
          new.display_name.as_str(),
          new.repo_path.to_string_lossy(),
          RepositoryState::Creating.as_str(),
          new.algorithm.name(),
          new.upstream_url,
          new.credential_ref,
          now,
        ],
      )
      .map_err(db_error)?;
      Ok(())
    })?;
    Ok(RepositoryRecord {
      repository_id: new.repository_id.clone(),
      display_name: new.display_name.clone(),
      repo_path: new.repo_path.clone(),
      state: RepositoryState::Creating,
      algorithm: new.algorithm,
      upstream_url: new.upstream_url.clone(),
      credential_ref: new.credential_ref.clone(),
      quarantine_reason: None,
    })
  }

  pub fn get_repository(&self, id: &RepositoryId) -> Result<Option<RepositoryRecord>, XvfsError> {
    self.with_conn(|conn| {
      conn
        .query_row(
          "SELECT repository_id, display_name, repo_path, state, algorithm,
                  upstream_url, credential_ref, quarantine_reason
           FROM repositories WHERE repository_id = ?1",
          [id.as_str()],
          row_to_repository,
        )
        .optional()
        .map_err(db_error)?
        .transpose()
    })
  }

  pub fn list_repositories(&self) -> Result<Vec<RepositoryRecord>, XvfsError> {
    self.with_conn(|conn| {
      let mut stmt = conn
        .prepare(
          "SELECT repository_id, display_name, repo_path, state, algorithm,
                  upstream_url, credential_ref, quarantine_reason
           FROM repositories ORDER BY repository_id",
        )
        .map_err(db_error)?;
      let rows = stmt
        .query_map([], row_to_repository)
        .map_err(db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db_error)?;
      rows.into_iter().collect()
    })
  }

  /// Move a repository to a new lifecycle state.
  ///
  /// Transitions are checked rather than assigned, so a bug cannot move a
  /// repository from `Deleting` back to `Active` and resume serving something
  /// whose objects are being removed.
  pub fn set_repository_state(
    &self,
    id: &RepositoryId,
    to: RepositoryState,
    reason: Option<&str>,
  ) -> Result<(), XvfsError> {
    let now = now_secs();
    self.with_tx(|tx| {
      let current: String = tx
        .query_row(
          "SELECT state FROM repositories WHERE repository_id = ?1",
          [id.as_str()],
          |r| r.get(0),
        )
        .optional()
        .map_err(db_error)?
        .ok_or_else(|| XvfsError::not_found("no such repository"))?;
      let from = RepositoryState::parse(&current)?;
      if !transition_allowed(from, to) {
        return Err(XvfsError::new(
          ErrorCode::FailedPrecondition,
          format!(
            "cannot move a repository from {} to {}",
            from.as_str(),
            to.as_str()
          ),
        ));
      }
      tx.execute(
        "UPDATE repositories
         SET state = ?2, quarantine_reason = ?3, updated_at = ?4
         WHERE repository_id = ?1",
        rusqlite::params![id.as_str(), to.as_str(), reason, now],
      )
      .map_err(db_error)?;
      Ok(())
    })
  }

  /// Remove a repository record.
  ///
  /// Refuses while any lease is still a reachability root. Deleting the row would
  /// cascade the lease rows away and leave the anchor refs behind with nothing
  /// recording why they exist -- and a live mount reading through them.
  pub fn delete_repository(&self, id: &RepositoryId) -> Result<(), XvfsError> {
    self.with_tx(|tx| {
      let live: i64 = tx
        .query_row(
          "SELECT count(*) FROM leases
           WHERE repository_id = ?1 AND state IN ('PREPARING', 'ACTIVE')",
          [id.as_str()],
          |r| r.get(0),
        )
        .map_err(db_error)?;
      if live > 0 {
        return Err(XvfsError::new(
          ErrorCode::FailedPrecondition,
          format!("{live} live mount lease(s) still reference this repository"),
        ));
      }
      let n = tx
        .execute(
          "DELETE FROM repositories WHERE repository_id = ?1",
          [id.as_str()],
        )
        .map_err(db_error)?;
      if n == 0 {
        return Err(XvfsError::not_found("no such repository"));
      }
      Ok(())
    })
  }

  // -------------------------------------------------------------------------
  // Cataloged commits and snapshot times
  // -------------------------------------------------------------------------

  /// Record a commit's sanitized snapshot time, or return the one already stored.
  ///
  /// This is the single place ADR 0006's clamp is applied, and it is applied
  /// **once per commit**. Every later read returns the stored value, because the
  /// clamp's upper bound is `first_seen - one_tick` and recomputing it on another
  /// host with a different clock would produce a different answer -- which is
  /// exactly the M2 exit criterion "base timestamps are identical across remounts
  /// and hosts" failing.
  ///
  /// Idempotent, so a concurrent second caller for the same commit gets the first
  /// caller's value rather than its own.
  pub fn catalog_commit(
    &self,
    repository_id: &RepositoryId,
    commit: &ObjectId,
    committer_time: Timestamp,
  ) -> Result<Timestamp, XvfsError> {
    let first_seen = Timestamp::now();
    self.with_tx(|tx| {
      let existing: Option<(i64, i64)> = tx
        .query_row(
          "SELECT snapshot_secs, snapshot_nanos FROM commits
           WHERE repository_id = ?1 AND commit_oid = ?2",
          rusqlite::params![repository_id.as_str(), commit.to_qualified()],
          |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(db_error)?;
      if let Some((secs, nanos)) = existing {
        return Ok(Timestamp::new(secs, nanos as u32));
      }

      let snapshot = time::snapshot_time(committer_time, first_seen);
      tx.execute(
        "INSERT INTO commits (repository_id, commit_oid, snapshot_secs,
           snapshot_nanos, first_seen_secs, first_seen_nanos, committer_secs)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
          repository_id.as_str(),
          commit.to_qualified(),
          snapshot.secs,
          snapshot.nanos as i64,
          first_seen.secs,
          first_seen.nanos as i64,
          committer_time.secs,
        ],
      )
      .map_err(db_error)?;
      Ok(snapshot)
    })
  }

  pub fn snapshot_time(
    &self,
    repository_id: &RepositoryId,
    commit: &ObjectId,
  ) -> Result<Option<Timestamp>, XvfsError> {
    self.with_conn(|conn| {
      conn
        .query_row(
          "SELECT snapshot_secs, snapshot_nanos FROM commits
           WHERE repository_id = ?1 AND commit_oid = ?2",
          rusqlite::params![repository_id.as_str(), commit.to_qualified()],
          |r| {
            let secs: i64 = r.get(0)?;
            let nanos: i64 = r.get(1)?;
            Ok(Timestamp::new(secs, nanos as u32))
          },
        )
        .optional()
        .map_err(db_error)
    })
  }
}

/// The allowed repository lifecycle transitions.
fn transition_allowed(from: RepositoryState, to: RepositoryState) -> bool {
  use RepositoryState::*;
  match (from, to) {
    // Idempotent re-assertion of the current state.
    (a, b) if a == b => true,
    (Creating, Active) | (Creating, Quarantined) | (Creating, Deleting) => true,
    (Active, Quarantined) | (Active, Deleting) => true,
    // Recoverable: quarantine is not a one-way door, which is the point of
    // having it rather than deleting.
    (Quarantined, Active) | (Quarantined, Deleting) => true,
    // `Deleting` is terminal. Resuming service for a repository whose objects
    // are being removed would serve a partial view.
    (Deleting, _) => false,
    _ => false,
  }
}

fn row_to_repository(
  row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<RepositoryRecord, XvfsError>> {
  let id: String = row.get(0)?;
  let name: String = row.get(1)?;
  let path: String = row.get(2)?;
  let state: String = row.get(3)?;
  let algorithm: String = row.get(4)?;
  let upstream_url: Option<String> = row.get(5)?;
  let credential_ref: Option<String> = row.get(6)?;
  let quarantine_reason: Option<String> = row.get(7)?;
  Ok((|| {
    Ok(RepositoryRecord {
      repository_id: RepositoryId::parse(&id)?,
      display_name: DisplayName::parse(&name)?,
      repo_path: std::path::PathBuf::from(path),
      state: RepositoryState::parse(&state)?,
      algorithm: HashAlgorithm::from_name(&algorithm).ok_or_else(|| {
        XvfsError::internal(format!(
          "unknown hash algorithm {algorithm:?} in the catalog"
        ))
      })?,
      upstream_url,
      credential_ref,
      quarantine_reason,
    })
  })())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn new_repo(id: &str) -> NewRepository {
    NewRepository {
      repository_id: RepositoryId::parse(id).unwrap(),
      display_name: DisplayName::parse("acme/monorepo").unwrap(),
      repo_path: std::path::PathBuf::from("/srv/git/r.git"),
      algorithm: HashAlgorithm::Sha1,
      upstream_url: Some("https://example.invalid/r.git".to_owned()),
      credential_ref: Some("secret-store://xvfs/upstream/acme".to_owned()),
    }
  }

  #[test]
  fn a_repository_starts_in_creating_and_is_not_servable() {
    let cat = Catalog::open_in_memory().unwrap();
    let rec = cat.create_repository(&new_repo("r1")).unwrap();
    assert_eq!(rec.state, RepositoryState::Creating);
    assert!(!rec.state.is_servable());
    assert!(!rec.state.accepts_new_mounts());
  }

  #[test]
  fn the_catalog_stores_a_credential_reference_and_not_a_secret() {
    // PLAN.md M1.2. A catalog dump must not be a credential leak, so the column
    // holds a pointer into the secret store.
    let cat = Catalog::open_in_memory().unwrap();
    cat.create_repository(&new_repo("r1")).unwrap();
    let rec = cat
      .get_repository(&RepositoryId::parse("r1").unwrap())
      .unwrap()
      .unwrap();
    let stored = rec.credential_ref.unwrap();
    assert!(stored.starts_with("secret-store://"));
    assert!(!stored.contains("password"));
  }

  #[test]
  fn creating_the_same_repository_twice_is_a_conflict() {
    let cat = Catalog::open_in_memory().unwrap();
    cat.create_repository(&new_repo("r1")).unwrap();
    assert_eq!(
      cat.create_repository(&new_repo("r1")).unwrap_err().code,
      ErrorCode::Conflict
    );
  }

  #[test]
  fn deleting_is_terminal_and_cannot_resume_service() {
    // A repository whose objects are being removed must not start serving again;
    // it would serve a partial view.
    let cat = Catalog::open_in_memory().unwrap();
    let id = RepositoryId::parse("r1").unwrap();
    cat.create_repository(&new_repo("r1")).unwrap();
    cat
      .set_repository_state(&id, RepositoryState::Active, None)
      .unwrap();
    cat
      .set_repository_state(&id, RepositoryState::Deleting, None)
      .unwrap();
    assert_eq!(
      cat
        .set_repository_state(&id, RepositoryState::Active, None)
        .unwrap_err()
        .code,
      ErrorCode::FailedPrecondition
    );
  }

  #[test]
  fn quarantine_is_recoverable_and_records_its_reason() {
    let cat = Catalog::open_in_memory().unwrap();
    let id = RepositoryId::parse("r1").unwrap();
    cat.create_repository(&new_repo("r1")).unwrap();
    cat
      .set_repository_state(&id, RepositoryState::Active, None)
      .unwrap();
    cat
      .set_repository_state(
        &id,
        RepositoryState::Quarantined,
        Some("fsck reported a bad object"),
      )
      .unwrap();
    let rec = cat.get_repository(&id).unwrap().unwrap();
    assert!(!rec.state.is_servable());
    assert_eq!(
      rec.quarantine_reason.as_deref(),
      Some("fsck reported a bad object")
    );
    // Recoverable: that is the whole point of quarantine over deletion.
    cat
      .set_repository_state(&id, RepositoryState::Active, None)
      .unwrap();
    assert!(cat
      .get_repository(&id)
      .unwrap()
      .unwrap()
      .state
      .is_servable());
  }

  #[test]
  fn a_snapshot_time_is_computed_once_and_then_reused() {
    // The property M2's "identical across remounts and hosts" criterion depends
    // on. The clamp's upper bound is `first_seen - one_tick`, so recomputing on
    // another host with another clock would give a different answer.
    let cat = Catalog::open_in_memory().unwrap();
    let id = RepositoryId::parse("r1").unwrap();
    cat.create_repository(&new_repo("r1")).unwrap();
    let commit = ObjectId::from_hex(HashAlgorithm::Sha1, &"ab".repeat(20)).unwrap();

    let first = cat
      .catalog_commit(&id, &commit, Timestamp::from_secs(1_600_000_000))
      .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    // A second call, even with a *different* committer time, must return the
    // stored value rather than recompute.
    let second = cat
      .catalog_commit(&id, &commit, Timestamp::from_secs(1_700_000_000))
      .unwrap();
    assert_eq!(first, second);
    assert_eq!(cat.snapshot_time(&id, &commit).unwrap(), Some(first));
  }

  #[test]
  fn a_future_dated_commit_is_cataloged_with_a_sanitized_time() {
    let cat = Catalog::open_in_memory().unwrap();
    let id = RepositoryId::parse("r1").unwrap();
    cat.create_repository(&new_repo("r1")).unwrap();
    let commit = ObjectId::from_hex(HashAlgorithm::Sha1, &"cd".repeat(20)).unwrap();
    // Claims to be from 2049.
    let stored = cat
      .catalog_commit(&id, &commit, Timestamp::from_secs(2_500_000_000))
      .unwrap();
    assert!(
      stored < Timestamp::now(),
      "a future-dated commit must not become a future base timestamp: {stored:?}"
    );
    assert!(stored >= Timestamp::MIN_SUPPORTED);
  }
}
