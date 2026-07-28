//! The snapshot preparation lifecycle: READY, BUILDING, FAILED.
//!
//! DESIGN.md section 7.3 is explicit that these three states are the whole
//! lifecycle and that `NOT_INDEXABLE` and `RESOURCE_LIMIT` are *request errors*,
//! not states. `gfs_types::SnapshotState` holds the vocabulary; this module
//! holds the machine.
//!
//! # Claiming is how simultaneous preparation is deduplicated
//!
//! [`SnapshotStore::claim`] is a compare-and-set inside one SQLite transaction.
//! The first caller to reach a commit gets [`Claim::Claimed`] and owns the build;
//! everyone else gets [`Claim::Building`] with the owner's operation ID and waits
//! or polls. That is the durable half of the dedup, and it is the half that
//! survives a restart — an in-process map cannot, and two server processes
//! sharing one index would otherwise both walk the same million-entry tree.
//!
//! # A crashed build must not wedge a commit forever
//!
//! A `BUILDING` row whose owner died is indistinguishable from one whose owner is
//! working. The store resolves it with time: a claim older than `stale_after` is
//! reclaimed, and the reclaiming caller gets `Claimed`. That is the same shape as
//! the catalog's lease reconciliation, and for the same reason — the alternative
//! is a snapshot nobody will ever prepare and every query on it failing.
//!
//! # Failure is retried, but not forever
//!
//! A repository whose object database is damaged will fail every attempt. Retry
//! without a bound turns that into a loop that re-walks the tree until someone
//! notices. `attempts` is recorded and a claim beyond `max_attempts` returns the
//! recorded failure instead, so the caller receives an honest `FAILED` rather
//! than an eternally optimistic `BUILDING`.

use std::collections::HashSet;
use std::sync::Arc;

use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::{ObjectId, RepositoryId, SnapshotState};
use rusqlite::OptionalExtension;

use crate::manifest::Manifest;
use crate::store::{db_error, SearchStore};

/// Unix seconds. The store's time unit, matching the catalog's.
pub fn now_secs() -> i64 {
  std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_secs() as i64)
    .unwrap_or(0)
}

/// A cooperative cancellation token.
///
/// A plain flag rather than `tokio_util::sync::CancellationToken`, because this
/// crate is deliberately runtime-free: the same builder runs inside the server's
/// Tokio blocking pool and inside a synchronous test.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<std::sync::atomic::AtomicBool>);

impl Cancel {
  pub fn new() -> Cancel {
    Cancel::default()
  }

  pub fn cancel(&self) {
    self.0.store(true, std::sync::atomic::Ordering::Relaxed);
  }

  pub fn is_cancelled(&self) -> bool {
    self.0.load(std::sync::atomic::Ordering::Relaxed)
  }

  /// `Err(Cancelled)` when cancelled, so a builder can `?` at each step.
  pub fn check(&self) -> Result<(), GfsError> {
    if self.is_cancelled() {
      return Err(GfsError::new(
        ErrorCode::Cancelled,
        "snapshot preparation was cancelled",
      ));
    }
    Ok(())
  }
}

/// What the store knows about one snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRecord {
  pub commit: ObjectId,
  pub state: SnapshotState,
  /// The index generation the manifest was built against. A query reports it, so
  /// two results from different generations are distinguishable.
  pub index_generation: u64,
  pub path_count: u64,
  pub checksum: Option<String>,
  pub updated_at: i64,
  /// `None` for a snapshot retained by policy — a configured branch tip.
  pub expires_at: Option<i64>,
  pub failure_reason: Option<String>,
  /// Present while `BUILDING`, so a waiter can correlate its poll with the build
  /// it is waiting on and notice when a *different* build replaced it.
  pub operation_id: Option<String>,
  pub attempts: u32,
  pub progress: Progress,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
  pub paths_walked: u64,
  pub blobs_classified: u64,
}

/// The outcome of asking to build a snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Claim {
  /// Already prepared. Nothing to do.
  Ready(Box<SnapshotRecord>),
  /// Someone else owns the build.
  Building { operation_id: String },
  /// The caller owns the build and must call `complete` or `fail`.
  Claimed { operation_id: String },
  /// Repeatedly failed; not retried again until the record is cleared.
  Failed { reason: String, attempts: u32 },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
  pub expired: usize,
  pub retained: usize,
  pub bytes_reclaimed: u64,
}

/// Preparation policy.
#[derive(Clone, Copy, Debug)]
pub struct PreparePolicy {
  /// How long an on-demand snapshot is kept after its last use.
  pub ttl_seconds: i64,
  /// A `BUILDING` claim older than this is assumed dead and reclaimed.
  pub stale_after_seconds: i64,
  pub max_attempts: u32,
}

impl Default for PreparePolicy {
  fn default() -> Self {
    PreparePolicy {
      // An hour. An agent job holds a mount for far less (ADR 0006's lease TTL is
      // 30 minutes), so a snapshot outlives the job that asked for it and a
      // second job on the same commit finds it warm.
      ttl_seconds: 3600,
      // Ten minutes. ADR 0006 targets under 5 seconds to READY, so anything an
      // order of magnitude past that is a dead owner rather than a slow one.
      stale_after_seconds: 600,
      max_attempts: 3,
    }
  }
}

/// The snapshot half of the search store.
#[derive(Debug)]
pub struct SnapshotStore {
  store: Arc<SearchStore>,
  repository: RepositoryId,
  policy: PreparePolicy,
}

impl SnapshotStore {
  pub fn new(
    store: Arc<SearchStore>,
    repository: RepositoryId,
    policy: PreparePolicy,
  ) -> SnapshotStore {
    SnapshotStore {
      store,
      repository,
      policy,
    }
  }

  pub fn policy(&self) -> &PreparePolicy {
    &self.policy
  }

  /// The current index generation, creating it at 1 on first use.
  pub fn generation(&self) -> Result<u64, GfsError> {
    let repository = self.repository.as_str().to_owned();
    self.store.with_conn(|conn| {
      let value: Option<i64> = conn
        .query_row(
          "SELECT generation FROM index_generations WHERE repository_id = ?1",
          [&repository],
          |r| r.get(0),
        )
        .optional()
        .map_err(db_error)?;
      Ok(value.unwrap_or(1) as u64)
    })
  }

  /// Advance the generation.
  ///
  /// Called when posting lists change, not when a manifest is written: a
  /// manifest is per-commit and does not invalidate anyone else's result, while
  /// a change to the shared index means two answers computed either side of it
  /// were computed against different data.
  pub fn bump_generation(&self) -> Result<u64, GfsError> {
    let repository = self.repository.as_str().to_owned();
    self.store.with_tx(|tx| {
      let current: i64 = tx
        .query_row(
          "SELECT generation FROM index_generations WHERE repository_id = ?1",
          [&repository],
          |r| r.get(0),
        )
        .optional()
        .map_err(db_error)?
        .unwrap_or(1);
      let next = current + 1;
      tx.execute(
        "INSERT INTO index_generations (repository_id, generation) VALUES (?1, ?2)
         ON CONFLICT (repository_id) DO UPDATE SET generation = ?2",
        rusqlite::params![&repository, next],
      )
      .map_err(db_error)?;
      Ok(next as u64)
    })
  }

  pub fn get(&self, commit: &ObjectId) -> Result<Option<SnapshotRecord>, GfsError> {
    let repository = self.repository.as_str().to_owned();
    let commit_text = commit.to_qualified();
    self.store.with_conn(|conn| {
      conn
        .query_row(
          "SELECT commit_oid, state, index_generation, path_count, checksum, updated_at,
                  expires_at, failure_reason, operation_id, attempts,
                  progress_paths, progress_blobs
           FROM snapshots WHERE repository_id = ?1 AND commit_oid = ?2",
          rusqlite::params![&repository, &commit_text],
          row_to_record,
        )
        .optional()
        .map_err(db_error)?
        .transpose()
    })
  }

  /// Ask to build. See the module docs for the four outcomes.
  ///
  /// `operation_id` is supplied by the caller rather than generated here so that
  /// a caller which retries after an ambiguous failure can present the same ID
  /// and recognize its own claim instead of colliding with it.
  pub fn claim(&self, commit: &ObjectId, operation_id: &str) -> Result<Claim, GfsError> {
    let repository = self.repository.as_str().to_owned();
    let commit_text = commit.to_qualified();
    let policy = self.policy;
    let now = now_secs();

    self.store.with_tx(|tx| {
      let existing: Option<SnapshotRecord> = tx
        .query_row(
          "SELECT commit_oid, state, index_generation, path_count, checksum, updated_at,
                  expires_at, failure_reason, operation_id, attempts,
                  progress_paths, progress_blobs
           FROM snapshots WHERE repository_id = ?1 AND commit_oid = ?2",
          rusqlite::params![&repository, &commit_text],
          row_to_record,
        )
        .optional()
        .map_err(db_error)?
        .transpose()?;

      let attempts = match &existing {
        Some(record) => match record.state {
          SnapshotState::Ready => {
            // Touch the expiry: a snapshot still being asked for is a snapshot
            // still in use, and GC must not reclaim it under a live query.
            if record.expires_at.is_some() {
              tx.execute(
                "UPDATE snapshots SET expires_at = ?3
                 WHERE repository_id = ?1 AND commit_oid = ?2",
                rusqlite::params![&repository, &commit_text, now + policy.ttl_seconds],
              )
              .map_err(db_error)?;
            }
            return Ok(Claim::Ready(Box::new(record.clone())));
          }
          SnapshotState::Building => {
            let age = now - record.updated_at;
            if age < policy.stale_after_seconds {
              return Ok(Claim::Building {
                operation_id: record.operation_id.clone().unwrap_or_default(),
              });
            }
            // Reclaimed. The previous owner is presumed dead; see the module
            // docs on why time is the only signal available.
            record.attempts
          }
          SnapshotState::Failed => {
            if record.attempts >= policy.max_attempts {
              return Ok(Claim::Failed {
                reason: record
                  .failure_reason
                  .clone()
                  .unwrap_or_else(|| "snapshot preparation failed".to_owned()),
                attempts: record.attempts,
              });
            }
            record.attempts
          }
        },
        None => 0,
      };

      tx.execute(
        "INSERT INTO snapshots (repository_id, commit_oid, state, format_version,
                                index_generation, path_count, manifest, checksum,
                                updated_at, expires_at, failure_reason, operation_id,
                                attempts, progress_paths, progress_blobs)
         VALUES (?1, ?2, 'BUILDING', ?3, 0, 0, NULL, NULL, ?4, NULL, NULL, ?5, ?6, 0, 0)
         ON CONFLICT (repository_id, commit_oid) DO UPDATE SET
           state = 'BUILDING', updated_at = ?4, failure_reason = NULL,
           operation_id = ?5, attempts = ?6, progress_paths = 0, progress_blobs = 0",
        rusqlite::params![
          &repository,
          &commit_text,
          crate::manifest::MANIFEST_FORMAT_VERSION as i64,
          now,
          operation_id,
          attempts as i64,
        ],
      )
      .map_err(db_error)?;

      Ok(Claim::Claimed {
        operation_id: operation_id.to_owned(),
      })
    })
  }

  /// Record progress and refresh the claim's timestamp.
  ///
  /// The refresh is the load-bearing half: a build that takes longer than
  /// `stale_after` must not have its own claim reclaimed out from under it while
  /// it is plainly making progress.
  pub fn record_progress(&self, commit: &ObjectId, progress: Progress) -> Result<(), GfsError> {
    let repository = self.repository.as_str().to_owned();
    let commit_text = commit.to_qualified();
    self.store.with_tx(|tx| {
      tx.execute(
        "UPDATE snapshots SET progress_paths = ?3, progress_blobs = ?4, updated_at = ?5
         WHERE repository_id = ?1 AND commit_oid = ?2 AND state = 'BUILDING'",
        rusqlite::params![
          &repository,
          &commit_text,
          progress.paths_walked as i64,
          progress.blobs_classified as i64,
          now_secs(),
        ],
      )
      .map_err(db_error)?;
      Ok(())
    })
  }

  /// Publish a finished manifest.
  ///
  /// `retained` marks a snapshot kept by policy — a configured branch tip — which
  /// is stored as a NULL expiry rather than a far-future one, so "kept forever"
  /// and "kept until 2038" are different rows rather than the same row read
  /// differently.
  pub fn complete(
    &self,
    manifest: &Manifest,
    index_generation: u64,
    retained: bool,
  ) -> Result<SnapshotRecord, GfsError> {
    let repository = self.repository.as_str().to_owned();
    let commit_text = manifest.commit().to_qualified();
    let encoded = manifest.encode();
    let checksum = Manifest::checksum(&encoded);
    let now = now_secs();
    let expires_at = (!retained).then_some(now + self.policy.ttl_seconds);

    self.store.with_tx(|tx| {
      tx.execute(
        "INSERT INTO snapshots (repository_id, commit_oid, state, format_version,
                                index_generation, path_count, manifest, checksum,
                                updated_at, expires_at, failure_reason, operation_id,
                                attempts, progress_paths, progress_blobs)
         VALUES (?1, ?2, 'READY', ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, NULL, 0, ?5, 0)
         ON CONFLICT (repository_id, commit_oid) DO UPDATE SET
           state = 'READY', format_version = ?3, index_generation = ?4,
           path_count = ?5, manifest = ?6, checksum = ?7, updated_at = ?8,
           expires_at = ?9, failure_reason = NULL, operation_id = NULL, attempts = 0",
        rusqlite::params![
          &repository,
          &commit_text,
          crate::manifest::MANIFEST_FORMAT_VERSION as i64,
          index_generation as i64,
          manifest.len() as i64,
          &encoded,
          &checksum,
          now,
          expires_at,
        ],
      )
      .map_err(db_error)?;
      Ok(())
    })?;

    self
      .get(manifest.commit())?
      .ok_or_else(|| GfsError::internal("the manifest just written is not readable"))
  }

  /// Record a failure, incrementing the attempt count.
  pub fn fail(&self, commit: &ObjectId, reason: &str) -> Result<(), GfsError> {
    let repository = self.repository.as_str().to_owned();
    let commit_text = commit.to_qualified();
    self.store.with_tx(|tx| {
      tx.execute(
        "UPDATE snapshots
         SET state = 'FAILED', failure_reason = ?3, operation_id = NULL,
             attempts = attempts + 1, updated_at = ?4
         WHERE repository_id = ?1 AND commit_oid = ?2",
        rusqlite::params![&repository, &commit_text, reason, now_secs()],
      )
      .map_err(db_error)?;
      Ok(())
    })
  }

  /// Abandon a claim without recording a failure.
  ///
  /// Cancellation is not failure: the snapshot is simply not prepared, and the
  /// next caller should build it rather than inherit an attempt count from a
  /// client that went away.
  pub fn abandon(&self, commit: &ObjectId) -> Result<(), GfsError> {
    let repository = self.repository.as_str().to_owned();
    let commit_text = commit.to_qualified();
    self.store.with_tx(|tx| {
      tx.execute(
        "DELETE FROM snapshots
         WHERE repository_id = ?1 AND commit_oid = ?2 AND state = 'BUILDING'",
        rusqlite::params![&repository, &commit_text],
      )
      .map_err(db_error)?;
      Ok(())
    })
  }

  /// The decoded manifest, if the snapshot is READY.
  pub fn manifest(&self, commit: &ObjectId) -> Result<Option<Manifest>, GfsError> {
    let repository = self.repository.as_str().to_owned();
    let commit_text = commit.to_qualified();
    let encoded: Option<Option<Vec<u8>>> = self.store.with_conn(|conn| {
      conn
        .query_row(
          "SELECT manifest FROM snapshots
           WHERE repository_id = ?1 AND commit_oid = ?2 AND state = 'READY'",
          rusqlite::params![&repository, &commit_text],
          |r| r.get(0),
        )
        .optional()
        .map_err(db_error)
    })?;
    match encoded.and_then(|inner| inner) {
      Some(bytes) => Ok(Some(Manifest::decode(&bytes)?)),
      None => Ok(None),
    }
  }

  /// Drop snapshots nobody is retaining.
  ///
  /// `pinned` is the set of qualified commit IDs that a ref, a live mount lease,
  /// or a configured policy keeps. Everything else expires on its TTL. The
  /// caller assembles that set because only the server knows it; passing it in
  /// rather than reaching for the catalog is what keeps this crate free of one.
  pub fn gc(&self, pinned: &HashSet<String>) -> Result<GcReport, GfsError> {
    let repository = self.repository.as_str().to_owned();
    let now = now_secs();
    self.store.with_tx(|tx| {
      let mut report = GcReport::default();
      let candidates: Vec<(String, Option<i64>, i64)> = {
        let mut stmt = tx
          .prepare(
            "SELECT commit_oid, expires_at, LENGTH(COALESCE(manifest, X''))
             FROM snapshots WHERE repository_id = ?1",
          )
          .map_err(db_error)?;
        let rows = stmt
          .query_map([&repository], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
          .map_err(db_error)?;
        let mut out = Vec::new();
        for row in rows {
          out.push(row.map_err(db_error)?);
        }
        out
      };

      let mut doomed = Vec::new();
      for (commit, expires_at, bytes) in candidates {
        // A pinned commit is never collected, whatever its expiry says. A mount
        // holding a lease on it can search at any moment, and rebuilding under
        // that query would turn a warm search into a cold one at best.
        if pinned.contains(&commit) {
          report.retained += 1;
          continue;
        }
        match expires_at {
          Some(at) if at <= now => {
            report.expired += 1;
            report.bytes_reclaimed += bytes.max(0) as u64;
            doomed.push(commit);
          }
          _ => report.retained += 1,
        }
      }

      for commit in doomed {
        tx.execute(
          "DELETE FROM snapshots WHERE repository_id = ?1 AND commit_oid = ?2",
          rusqlite::params![&repository, &commit],
        )
        .map_err(db_error)?;
      }
      Ok(report)
    })
  }

  /// Every READY snapshot, newest first. For diagnostics and for choosing an
  /// incremental base.
  pub fn ready_commits(&self) -> Result<Vec<ObjectId>, GfsError> {
    let repository = self.repository.as_str().to_owned();
    self.store.with_conn(|conn| {
      let mut stmt = conn
        .prepare(
          "SELECT commit_oid FROM snapshots
           WHERE repository_id = ?1 AND state = 'READY'
           ORDER BY updated_at DESC",
        )
        .map_err(db_error)?;
      let rows = stmt
        .query_map([&repository], |r| r.get::<_, String>(0))
        .map_err(db_error)?;
      let mut out = Vec::new();
      for row in rows {
        let text = row.map_err(db_error)?;
        out.push(
          ObjectId::parse_qualified(&text)
            .map_err(|e| GfsError::internal(format!("stored commit id: {e}")))?,
        );
      }
      Ok(out)
    })
  }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<SnapshotRecord, GfsError>> {
  let commit: String = row.get(0)?;
  let state: String = row.get(1)?;
  let generation: i64 = row.get(2)?;
  let path_count: i64 = row.get(3)?;
  let checksum: Option<String> = row.get(4)?;
  let updated_at: i64 = row.get(5)?;
  let expires_at: Option<i64> = row.get(6)?;
  let failure_reason: Option<String> = row.get(7)?;
  let operation_id: Option<String> = row.get(8)?;
  let attempts: i64 = row.get(9)?;
  let paths_walked: i64 = row.get(10)?;
  let blobs_classified: i64 = row.get(11)?;
  Ok((|| {
    let state = match state.as_str() {
      "READY" => SnapshotState::Ready,
      "BUILDING" => SnapshotState::Building,
      "FAILED" => SnapshotState::Failed,
      other => {
        return Err(GfsError::internal(format!(
          "unknown stored snapshot state {other:?}"
        )))
      }
    };
    Ok(SnapshotRecord {
      commit: ObjectId::parse_qualified(&commit)
        .map_err(|e| GfsError::internal(format!("stored commit id: {e}")))?,
      state,
      index_generation: generation as u64,
      path_count: path_count as u64,
      checksum,
      updated_at,
      expires_at,
      failure_reason,
      operation_id,
      attempts: attempts as u32,
      progress: Progress {
        paths_walked: paths_walked as u64,
        blobs_classified: blobs_classified as u64,
      },
    })
  })())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::manifest::PathEntry;
  use gfs_types::{BytePath, HashAlgorithm};

  fn commit(n: u8) -> ObjectId {
    ObjectId::from_raw(HashAlgorithm::Sha1, &[n; 20]).unwrap()
  }

  fn snapshots(policy: PreparePolicy) -> SnapshotStore {
    SnapshotStore::new(
      Arc::new(SearchStore::open_in_memory().unwrap()),
      RepositoryId::parse("r-test").unwrap(),
      policy,
    )
  }

  fn manifest(commit: ObjectId) -> Manifest {
    Manifest::build(
      commit,
      vec![PathEntry {
        path: BytePath::new(b"a.rs".to_vec()),
        mode: gfs_types::mode::REGULAR,
        key: 0,
      }],
    )
  }

  #[test]
  fn the_second_caller_waits_instead_of_building_the_same_tree() {
    let s = snapshots(PreparePolicy::default());
    assert!(matches!(
      s.claim(&commit(1), "op-a").unwrap(),
      Claim::Claimed { .. }
    ));
    match s.claim(&commit(1), "op-b").unwrap() {
      Claim::Building { operation_id } => assert_eq!(operation_id, "op-a"),
      other => panic!("expected to wait on op-a, got {other:?}"),
    }
  }

  #[test]
  fn a_completed_snapshot_is_ready_and_its_manifest_round_trips() {
    let s = snapshots(PreparePolicy::default());
    s.claim(&commit(1), "op").unwrap();
    let record = s.complete(&manifest(commit(1)), 7, false).unwrap();
    assert_eq!(record.state, SnapshotState::Ready);
    assert_eq!(record.index_generation, 7);
    assert_eq!(record.path_count, 1);

    let back = s.manifest(&commit(1)).unwrap().unwrap();
    assert_eq!(back.paths()[0].path.as_bytes(), b"a.rs");
    assert!(matches!(
      s.claim(&commit(1), "op2").unwrap(),
      Claim::Ready(_)
    ));
  }

  #[test]
  fn a_dead_builders_claim_is_reclaimed_rather_than_wedging_the_commit() {
    let s = snapshots(PreparePolicy {
      stale_after_seconds: 0,
      ..PreparePolicy::default()
    });
    s.claim(&commit(1), "op-dead").unwrap();
    match s.claim(&commit(1), "op-live").unwrap() {
      Claim::Claimed { operation_id } => assert_eq!(operation_id, "op-live"),
      other => panic!("a stale claim must be reclaimable, got {other:?}"),
    }
  }

  #[test]
  fn repeated_failures_stop_being_retried_and_report_the_reason() {
    let s = snapshots(PreparePolicy {
      max_attempts: 2,
      ..PreparePolicy::default()
    });
    for _ in 0..2 {
      s.claim(&commit(1), "op").unwrap();
      s.fail(&commit(1), "the pack is corrupt").unwrap();
    }
    match s.claim(&commit(1), "op").unwrap() {
      Claim::Failed { reason, attempts } => {
        assert_eq!(reason, "the pack is corrupt");
        assert_eq!(attempts, 2);
      }
      other => panic!("a repeatedly failing snapshot must report FAILED, got {other:?}"),
    }
  }

  #[test]
  fn cancellation_leaves_no_attempt_behind() {
    // A client that gave up must not consume the retry budget of the next one.
    let s = snapshots(PreparePolicy::default());
    s.claim(&commit(1), "op").unwrap();
    s.abandon(&commit(1)).unwrap();
    assert_eq!(s.get(&commit(1)).unwrap(), None);
    assert!(matches!(
      s.claim(&commit(1), "op2").unwrap(),
      Claim::Claimed { .. }
    ));
  }

  #[test]
  fn a_pinned_snapshot_is_never_collected_even_when_expired() {
    let s = snapshots(PreparePolicy {
      ttl_seconds: -1,
      ..PreparePolicy::default()
    });
    s.claim(&commit(1), "op").unwrap();
    s.complete(&manifest(commit(1)), 1, false).unwrap();
    s.claim(&commit(2), "op").unwrap();
    s.complete(&manifest(commit(2)), 1, false).unwrap();

    let pinned: HashSet<String> = [commit(1).to_qualified()].into_iter().collect();
    let report = s.gc(&pinned).unwrap();
    assert_eq!(report.expired, 1);
    assert_eq!(report.retained, 1);
    assert!(s.manifest(&commit(1)).unwrap().is_some());
    assert!(s.manifest(&commit(2)).unwrap().is_none());
  }

  #[test]
  fn a_snapshot_retained_by_policy_has_no_expiry_at_all() {
    let s = snapshots(PreparePolicy::default());
    s.claim(&commit(1), "op").unwrap();
    let record = s.complete(&manifest(commit(1)), 1, true).unwrap();
    assert_eq!(record.expires_at, None);
    let report = s.gc(&HashSet::new()).unwrap();
    assert_eq!(report.expired, 0);
  }

  #[test]
  fn asking_again_extends_an_on_demand_snapshots_life() {
    let s = snapshots(PreparePolicy {
      ttl_seconds: 100,
      ..PreparePolicy::default()
    });
    s.claim(&commit(1), "op").unwrap();
    s.complete(&manifest(commit(1)), 1, false).unwrap();
    let before = s.get(&commit(1)).unwrap().unwrap().expires_at.unwrap();
    // Rewind the expiry to simulate the passage of time, then re-ask.
    s.store
      .with_tx(|tx| {
        tx.execute(
          "UPDATE snapshots SET expires_at = ?1 WHERE repository_id = 'r-test'",
          [before - 90],
        )
        .map_err(db_error)?;
        Ok(())
      })
      .unwrap();
    s.claim(&commit(1), "op").unwrap();
    let after = s.get(&commit(1)).unwrap().unwrap().expires_at.unwrap();
    assert!(
      after > before - 90,
      "a snapshot in use must not expire under it"
    );
  }

  #[test]
  fn the_generation_advances_monotonically() {
    let s = snapshots(PreparePolicy::default());
    assert_eq!(s.generation().unwrap(), 1);
    assert_eq!(s.bump_generation().unwrap(), 2);
    assert_eq!(s.generation().unwrap(), 2);
  }

  #[test]
  fn progress_refreshes_the_claim_so_a_slow_build_is_not_reclaimed() {
    let s = snapshots(PreparePolicy {
      stale_after_seconds: 600,
      ..PreparePolicy::default()
    });
    s.claim(&commit(1), "op").unwrap();
    s.record_progress(
      &commit(1),
      Progress {
        paths_walked: 500,
        blobs_classified: 400,
      },
    )
    .unwrap();
    let record = s.get(&commit(1)).unwrap().unwrap();
    assert_eq!(record.progress.paths_walked, 500);
    assert!(matches!(
      s.claim(&commit(1), "other").unwrap(),
      Claim::Building { .. }
    ));
  }
}
