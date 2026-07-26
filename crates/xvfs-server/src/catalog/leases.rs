//! The mount retention-lease state machine.
//!
//! Retention leases are in M1 rather than M7 on purpose. PLAN.md states the
//! reason plainly: without them, a routine upstream force push during a pilot job
//! prunes objects out from under a live mount and every uncached read fails
//! permanently mid-task.
//!
//! # Why the state machine has a `PREPARING` state
//!
//! The catalog and the Git ref anchor are two different storage systems and cannot
//! share one transaction. So a crash can land between them, and the ordering has
//! to make every landing site recoverable. DESIGN.md section 7.1 fixes it:
//!
//! ```text
//! under the repository lock:
//!   resolve the selector          -- no durable effect yet
//!   authorize the resolved commit
//!   persist PREPARING             <-- durable catalog record
//!   create the ref anchor         <-- durable reachability root
//!   persist ACTIVE                <-- durable; only now may the capability be returned
//! ```
//!
//! The invariant that makes recovery decidable: **an `ACTIVE` lease is never
//! returned to a client before its anchor is durable.** So a `PREPARING` lease
//! found at startup cannot be held by anyone, and abandoning it is always safe.
//! An `ACTIVE` lease may be held by a live daemon, so it is repaired rather than
//! abandoned. Neither decision requires knowing what the crashed process intended.
//!
//! Note that `PREPARING` still counts as a reachability root
//! ([`LeaseState::is_reachability_root`]): its anchor may already exist, and a
//! maintenance pass that ignored it could prune the very commit a `CreateMount`
//! call is in the middle of pinning.
//!
//! # Expiry has three stages, not one
//!
//! ADR 0006's table gives each a separate reason, and collapsing them would
//! destroy a live workspace over a transient network failure:
//!
//! | Stage | Elapsed | Meaning |
//! | --- | --- | --- |
//! | expired | `expires_at` | renewal is overdue; still protected |
//! | grace over | `+ 15 min` | leave `ACTIVE`; the mount is no longer trusted to be live |
//! | prunable | `+ 24 h` | drop the anchor; objects become collectable |

use rusqlite::OptionalExtension;
use xvfs_types::error::{ErrorCode, XvfsError};
use xvfs_types::{LeasePolicy, LeaseState, MountId, ObjectId, RepositoryId, SubjectId};

use super::schema::db_error;
use super::{now_secs, Catalog};

#[derive(Clone, Debug)]
pub struct Lease {
  pub mount_id: MountId,
  pub repository_id: RepositoryId,
  pub commit: ObjectId,
  pub subject: SubjectId,
  pub state: LeaseState,
  pub anchor_ref: String,
  pub created_at: i64,
  pub expires_at: i64,
  pub terminal_at: Option<i64>,
  pub renewal_failures: u32,
}

impl Lease {
  /// Whether the lease still keeps its commit reachable right now.
  ///
  /// Takes the policy because a terminal lease keeps protecting for the whole
  /// prune delay. That is not a detail: ADR 0006 keeps objects recoverable for a
  /// working day after a *mistaken* expiry, and an expiry that stopped protecting
  /// the instant it happened would make the mistake unrecoverable by definition --
  /// the next `gc` would take the objects.
  pub fn is_protecting(&self, now: i64, policy: &LeasePolicy) -> bool {
    if self.state.is_reachability_root() {
      return true;
    }
    let prune_delay = policy.prune_delay.as_secs() as i64;
    self
      .terminal_at
      .is_some_and(|t| now < t.saturating_add(prune_delay))
  }
}

/// What one sweep changed. Reported so the operator surface can alert on it
/// rather than infer it from logs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LeaseSweepOutcome {
  /// Leases that left `ACTIVE` because their grace elapsed.
  pub expired: Vec<MountId>,
  /// Leases whose anchors are now collectable.
  pub prunable: Vec<(MountId, String)>,
  /// Leases past `expires_at` but still inside grace. Not yet a failure, and the
  /// thing to warn about.
  pub in_grace: Vec<MountId>,
}

impl Catalog {
  /// Persist a `PREPARING` lease.
  ///
  /// The first durable step. Returns an error if the mount ID is already in use,
  /// so a caller cannot collide with a live lease.
  pub fn begin_lease(
    &self,
    mount_id: &MountId,
    repository_id: &RepositoryId,
    commit: &ObjectId,
    subject: &SubjectId,
    anchor_ref: &str,
    ttl_secs: i64,
  ) -> Result<Lease, XvfsError> {
    let now = now_secs();
    let expires_at = now.saturating_add(ttl_secs);
    self.with_tx(|tx| {
      let existing: Option<String> = tx
        .query_row(
          "SELECT state FROM leases WHERE mount_id = ?1",
          [mount_id.as_str()],
          |r| r.get(0),
        )
        .optional()
        .map_err(db_error)?;
      if let Some(state) = existing {
        return Err(XvfsError::new(
          ErrorCode::Conflict,
          format!("mount id is already in use by a lease in state {state}"),
        ));
      }
      tx.execute(
        "INSERT INTO leases (mount_id, repository_id, commit_oid, subject, state,
           anchor_ref, created_at, expires_at, renewal_failures, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'PREPARING', ?5, ?6, ?7, 0, ?6)",
        rusqlite::params![
          mount_id.as_str(),
          repository_id.as_str(),
          commit.to_qualified(),
          subject.as_str(),
          anchor_ref,
          now,
          expires_at,
        ],
      )
      .map_err(db_error)?;
      Ok(())
    })?;
    Ok(Lease {
      mount_id: mount_id.clone(),
      repository_id: repository_id.clone(),
      commit: commit.clone(),
      subject: subject.clone(),
      state: LeaseState::Preparing,
      anchor_ref: anchor_ref.to_owned(),
      created_at: now,
      expires_at,
      terminal_at: None,
      renewal_failures: 0,
    })
  }

  /// Promote a `PREPARING` lease to `ACTIVE`, after its anchor is durable.
  ///
  /// Refuses any other source state. Promoting from `RELEASED` would resurrect a
  /// lease whose anchor has already been removed, producing an `ACTIVE` lease that
  /// protects nothing -- the worst failure mode available, because it looks fine.
  pub fn activate_lease(&self, mount_id: &MountId) -> Result<(), XvfsError> {
    let now = now_secs();
    self.with_tx(|tx| {
      let n = tx
        .execute(
          "UPDATE leases SET state = 'ACTIVE', updated_at = ?2
           WHERE mount_id = ?1 AND state = 'PREPARING'",
          rusqlite::params![mount_id.as_str(), now],
        )
        .map_err(db_error)?;
      if n == 0 {
        let state: Option<String> = tx
          .query_row(
            "SELECT state FROM leases WHERE mount_id = ?1",
            [mount_id.as_str()],
            |r| r.get(0),
          )
          .optional()
          .map_err(db_error)?;
        return Err(match state {
          None => XvfsError::not_found("no such lease"),
          Some(s) => XvfsError::new(
            ErrorCode::FailedPrecondition,
            format!("cannot activate a lease in state {s}"),
          ),
        });
      }
      Ok(())
    })
  }

  pub fn get_lease(&self, mount_id: &MountId) -> Result<Option<Lease>, XvfsError> {
    self.with_conn(|conn| {
      conn
        .query_row(LEASE_SELECT, [mount_id.as_str()], row_to_lease)
        .optional()
        .map_err(db_error)?
        .transpose()
    })
  }

  /// Extend a lease's expiry.
  ///
  /// Idempotent, which is what makes a heartbeat that cannot tell whether its last
  /// attempt landed recoverable: replaying a renewal is harmless.
  ///
  /// Enforced here rather than by the caller:
  ///
  /// * only an `ACTIVE` lease renews. A released or expired one must not come
  ///   back to life -- its anchor may already be gone;
  /// * `max_total_age` from ADR 0006 bounds the *total* life of a lease, so an
  ///   abandoned daemon that renews forever is still collected. Renewal past it
  ///   fails and alerts rather than silently continuing.
  pub fn renew_lease(
    &self,
    mount_id: &MountId,
    policy: &LeasePolicy,
    ttl_secs: i64,
  ) -> Result<Lease, XvfsError> {
    let now = now_secs();
    self.with_tx(|tx| {
      let lease = tx
        .query_row(LEASE_SELECT, [mount_id.as_str()], row_to_lease)
        .optional()
        .map_err(db_error)?
        .transpose()?
        .ok_or_else(|| XvfsError::not_found("no such lease"))?;

      if lease.state != LeaseState::Active {
        return Err(XvfsError::new(
          ErrorCode::FailedPrecondition,
          format!(
            "cannot renew a lease in state {}; it no longer protects its commit",
            lease.state.as_str()
          ),
        ));
      }

      let age = now.saturating_sub(lease.created_at);
      if age > policy.max_total_age.as_secs() as i64 {
        return Err(XvfsError::new(
          ErrorCode::FailedPrecondition,
          format!(
            "lease has reached the maximum total age of {} hours",
            policy.max_total_age.as_secs() / 3600
          ),
        ));
      }

      // Never shorten. A renewal that moved expiry backwards -- because a request
      // arrived out of order, or asked for a smaller TTL -- would bring a healthy
      // lease closer to expiry, which is the opposite of what a heartbeat is for.
      let proposed = now.saturating_add(ttl_secs);
      let expires_at = proposed.max(lease.expires_at);

      tx.execute(
        "UPDATE leases SET expires_at = ?2, renewal_failures = 0, updated_at = ?3
         WHERE mount_id = ?1 AND state = 'ACTIVE'",
        rusqlite::params![mount_id.as_str(), expires_at, now],
      )
      .map_err(db_error)?;

      Ok(Lease {
        expires_at,
        renewal_failures: 0,
        ..lease
      })
    })
  }

  /// Release a lease eagerly, on unmount or job cleanup.
  ///
  /// Idempotent: releasing an already-released lease succeeds. Teardown runs on
  /// several paths -- unmount, job cleanup, orphan reaping -- and making them
  /// coordinate would be a source of stuck leases rather than of safety.
  pub fn release_lease(&self, mount_id: &MountId) -> Result<Lease, XvfsError> {
    let now = now_secs();
    self.with_tx(|tx| {
      let lease = tx
        .query_row(LEASE_SELECT, [mount_id.as_str()], row_to_lease)
        .optional()
        .map_err(db_error)?
        .transpose()?
        .ok_or_else(|| XvfsError::not_found("no such lease"))?;

      if !lease.state.is_reachability_root() {
        return Ok(lease);
      }
      tx.execute(
        "UPDATE leases SET state = 'RELEASED', terminal_at = ?2, updated_at = ?2
         WHERE mount_id = ?1",
        rusqlite::params![mount_id.as_str(), now],
      )
      .map_err(db_error)?;
      Ok(Lease {
        state: LeaseState::Released,
        terminal_at: Some(now),
        ..lease
      })
    })
  }

  /// Record that a renewal attempt failed, for alerting.
  ///
  /// ADR 0006 alerts after two consecutive failures, roughly ten minutes before
  /// grace begins. The counter is reset by a successful renewal, so it measures
  /// *consecutive* failures rather than lifetime ones.
  pub fn record_renewal_failure(&self, mount_id: &MountId) -> Result<u32, XvfsError> {
    let now = now_secs();
    self.with_tx(|tx| {
      tx.execute(
        "UPDATE leases SET renewal_failures = renewal_failures + 1, updated_at = ?2
         WHERE mount_id = ?1",
        rusqlite::params![mount_id.as_str(), now],
      )
      .map_err(db_error)?;
      let n: i64 = tx
        .query_row(
          "SELECT renewal_failures FROM leases WHERE mount_id = ?1",
          [mount_id.as_str()],
          |r| r.get(0),
        )
        .optional()
        .map_err(db_error)?
        .unwrap_or(0);
      Ok(n as u32)
    })
  }

  /// Every lease that currently keeps a commit reachable.
  ///
  /// This is the reachability-root set Git maintenance must respect. It includes
  /// leases inside their prune delay, because ADR 0006 keeps objects recoverable
  /// for a working day after a *mistaken* expiry -- and a mistaken expiry that
  /// immediately destroyed the objects would be unrecoverable by definition.
  pub fn protecting_leases(
    &self,
    repository_id: &RepositoryId,
    policy: &LeasePolicy,
  ) -> Result<Vec<Lease>, XvfsError> {
    let now = now_secs();
    self.with_conn(|conn| {
      let mut stmt = conn
        .prepare(&format!(
          "{LEASE_SELECT_BASE} WHERE repository_id = ?1 ORDER BY mount_id"
        ))
        .map_err(db_error)?;
      let rows = stmt
        .query_map([repository_id.as_str()], row_to_lease)
        .map_err(db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db_error)?;
      let mut out = Vec::new();
      for r in rows {
        let lease = r?;
        if lease.is_protecting(now, policy) {
          out.push(lease);
        }
      }
      Ok(out)
    })
  }

  /// Leases still in `PREPARING` or `ACTIVE`, for restart reconciliation.
  pub fn unreconciled_leases(&self) -> Result<Vec<Lease>, XvfsError> {
    self.with_conn(|conn| {
      let mut stmt = conn
        .prepare(&format!(
          "{LEASE_SELECT_BASE} WHERE state IN ('PREPARING', 'ACTIVE') ORDER BY mount_id"
        ))
        .map_err(db_error)?;
      let rows = stmt
        .query_map([], row_to_lease)
        .map_err(db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db_error)?;
      rows.into_iter().collect()
    })
  }

  /// Advance expiry for every lease whose deadline has passed.
  ///
  /// Returns what changed and what is merely at risk, so the caller can alert on
  /// the second without treating it as the first.
  pub fn sweep_leases(&self, policy: &LeasePolicy) -> Result<LeaseSweepOutcome, XvfsError> {
    let now = now_secs();
    let grace = policy.renewal_grace.as_secs() as i64;
    let prune_delay = policy.prune_delay.as_secs() as i64;

    self.with_tx(|tx| {
      let mut outcome = LeaseSweepOutcome::default();

      let leases: Vec<Lease> = {
        let mut stmt = tx
          .prepare(&format!(
            "{LEASE_SELECT_BASE} WHERE state IN ('PREPARING', 'ACTIVE', 'EXPIRED', 'RELEASED')"
          ))
          .map_err(db_error)?;
        let rows = stmt
          .query_map([], row_to_lease)
          .map_err(db_error)?
          .collect::<Result<Vec<_>, _>>()
          .map_err(db_error)?;
        rows.into_iter().collect::<Result<Vec<_>, _>>()?
      };

      for lease in leases {
        match lease.state {
          LeaseState::Preparing | LeaseState::Active => {
            if now <= lease.expires_at {
              continue;
            }
            if now <= lease.expires_at.saturating_add(grace) {
              // Overdue but still protected. This is the state ADR 0006's grace
              // interval exists for: a transient renewal failure gets reported
              // and recovered before a live workspace is destroyed.
              outcome.in_grace.push(lease.mount_id.clone());
              continue;
            }
            tx.execute(
              "UPDATE leases SET state = 'EXPIRED', terminal_at = ?2, updated_at = ?2
               WHERE mount_id = ?1",
              rusqlite::params![lease.mount_id.as_str(), now],
            )
            .map_err(db_error)?;
            outcome.expired.push(lease.mount_id);
          }
          LeaseState::Expired | LeaseState::Released => {
            // The anchor stays for the prune delay so objects remain recoverable
            // for a working day after a mistaken expiry.
            let Some(terminal_at) = lease.terminal_at else {
              continue;
            };
            if now >= terminal_at.saturating_add(prune_delay) {
              outcome.prunable.push((lease.mount_id, lease.anchor_ref));
            }
          }
        }
      }

      Ok(outcome)
    })
  }

  /// Forget a lease whose anchor has been removed.
  pub fn forget_lease(&self, mount_id: &MountId) -> Result<(), XvfsError> {
    self.with_conn(|conn| {
      conn
        .execute(
          "DELETE FROM leases WHERE mount_id = ?1",
          [mount_id.as_str()],
        )
        .map_err(db_error)?;
      Ok(())
    })
  }

  /// Mark a lease terminal without waiting for expiry, used by reconciliation
  /// when abandoning a `PREPARING` lease.
  pub fn abandon_lease(&self, mount_id: &MountId, reason: &str) -> Result<(), XvfsError> {
    let now = now_secs();
    tracing::warn!(
      mount_id = %mount_id,
      reason,
      "abandoning an unreconciled lease"
    );
    self.with_conn(|conn| {
      conn
        .execute(
          "UPDATE leases SET state = 'EXPIRED', terminal_at = ?2, updated_at = ?2
           WHERE mount_id = ?1",
          rusqlite::params![mount_id.as_str(), now],
        )
        .map_err(db_error)?;
      Ok(())
    })
  }
}

const LEASE_SELECT_BASE: &str = "SELECT mount_id, repository_id, commit_oid, subject, state,
          anchor_ref, created_at, expires_at, terminal_at, renewal_failures
   FROM leases";

const LEASE_SELECT: &str = "SELECT mount_id, repository_id, commit_oid, subject, state,
          anchor_ref, created_at, expires_at, terminal_at, renewal_failures
   FROM leases WHERE mount_id = ?1";

fn parse_lease_state(s: &str) -> Result<LeaseState, XvfsError> {
  match s {
    "PREPARING" => Ok(LeaseState::Preparing),
    "ACTIVE" => Ok(LeaseState::Active),
    "RELEASED" => Ok(LeaseState::Released),
    "EXPIRED" => Ok(LeaseState::Expired),
    other => Err(XvfsError::internal(format!(
      "unknown lease state {other:?} in the catalog"
    ))),
  }
}

fn row_to_lease(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Lease, XvfsError>> {
  let mount_id: String = row.get(0)?;
  let repository_id: String = row.get(1)?;
  let commit: String = row.get(2)?;
  let subject: String = row.get(3)?;
  let state: String = row.get(4)?;
  let anchor_ref: String = row.get(5)?;
  let created_at: i64 = row.get(6)?;
  let expires_at: i64 = row.get(7)?;
  let terminal_at: Option<i64> = row.get(8)?;
  let renewal_failures: i64 = row.get(9)?;
  Ok((|| {
    Ok(Lease {
      mount_id: MountId::parse(&mount_id)?,
      repository_id: RepositoryId::parse(&repository_id)?,
      commit: ObjectId::parse_qualified(&commit)?,
      subject: SubjectId::parse(&subject)?,
      state: parse_lease_state(&state)?,
      anchor_ref,
      created_at,
      expires_at,
      terminal_at,
      renewal_failures: renewal_failures as u32,
    })
  })())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::catalog::repositories::{NewRepository, RepositoryState};
  use xvfs_types::{DisplayName, HashAlgorithm};

  struct Fixture {
    cat: Catalog,
    repo: RepositoryId,
    commit: ObjectId,
    subject: SubjectId,
  }

  fn fixture() -> Fixture {
    let cat = Catalog::open_in_memory().unwrap();
    let repo = RepositoryId::parse("r1").unwrap();
    cat
      .create_repository(&NewRepository {
        repository_id: repo.clone(),
        display_name: DisplayName::parse("acme/monorepo").unwrap(),
        repo_path: std::path::PathBuf::from("/srv/git/r.git"),
        algorithm: HashAlgorithm::Sha1,
        upstream_url: None,
        credential_ref: None,
      })
      .unwrap();
    cat
      .set_repository_state(&repo, RepositoryState::Active, None)
      .unwrap();
    Fixture {
      cat,
      repo,
      commit: ObjectId::from_hex(HashAlgorithm::Sha1, &"ab".repeat(20)).unwrap(),
      subject: SubjectId::parse("job-123").unwrap(),
    }
  }

  fn begin(f: &Fixture, id: &str, ttl: i64) -> Lease {
    let mount = MountId::parse(id).unwrap();
    let anchor = xvfs_types::revision::lease_anchor_ref(id);
    f.cat
      .begin_lease(&mount, &f.repo, &f.commit, &f.subject, &anchor, ttl)
      .unwrap()
  }

  #[test]
  fn a_lease_becomes_active_only_from_preparing() {
    let f = fixture();
    let lease = begin(&f, "m-1", 1800);
    assert_eq!(lease.state, LeaseState::Preparing);
    // Preparing already protects: its anchor may exist, and maintenance must not
    // prune the commit a CreateMount call is pinning.
    assert!(lease.state.is_reachability_root());

    f.cat.activate_lease(&lease.mount_id).unwrap();
    assert_eq!(
      f.cat.get_lease(&lease.mount_id).unwrap().unwrap().state,
      LeaseState::Active
    );

    // Activating twice is refused rather than silently accepted: the second call
    // means the state machine was driven twice, which is a bug worth surfacing.
    assert_eq!(
      f.cat.activate_lease(&lease.mount_id).unwrap_err().code,
      ErrorCode::FailedPrecondition
    );
  }

  #[test]
  fn a_released_lease_cannot_be_resurrected_into_active() {
    // The worst available failure mode: an ACTIVE lease whose anchor is already
    // gone protects nothing while looking healthy.
    let f = fixture();
    let lease = begin(&f, "m-1", 1800);
    f.cat.activate_lease(&lease.mount_id).unwrap();
    f.cat.release_lease(&lease.mount_id).unwrap();
    assert_eq!(
      f.cat.activate_lease(&lease.mount_id).unwrap_err().code,
      ErrorCode::FailedPrecondition
    );
  }

  #[test]
  fn a_duplicate_mount_id_is_a_conflict() {
    let f = fixture();
    begin(&f, "m-1", 1800);
    let mount = MountId::parse("m-1").unwrap();
    let anchor = xvfs_types::revision::lease_anchor_ref("m-1");
    assert_eq!(
      f.cat
        .begin_lease(&mount, &f.repo, &f.commit, &f.subject, &anchor, 1800)
        .unwrap_err()
        .code,
      ErrorCode::Conflict
    );
  }

  #[test]
  fn renewal_is_idempotent_and_extends_past_the_original_ttl() {
    // M1's exit criterion: "a renewed live lease survives past its original TTL".
    let f = fixture();
    let lease = begin(&f, "m-1", 60);
    f.cat.activate_lease(&lease.mount_id).unwrap();
    let original_expiry = lease.expires_at;

    let policy = LeasePolicy::adr_0006();
    let renewed = f.cat.renew_lease(&lease.mount_id, &policy, 1800).unwrap();
    assert!(renewed.expires_at > original_expiry);

    // Replaying the renewal is safe, which is what makes a heartbeat that cannot
    // tell whether its last attempt landed recoverable.
    let again = f.cat.renew_lease(&lease.mount_id, &policy, 1800).unwrap();
    assert!(again.expires_at >= renewed.expires_at);
  }

  #[test]
  fn renewal_never_shortens_an_existing_expiry() {
    // An out-of-order renewal, or one asking for a smaller TTL, must not bring a
    // healthy lease closer to expiry.
    let f = fixture();
    let lease = begin(&f, "m-1", 3600);
    f.cat.activate_lease(&lease.mount_id).unwrap();
    let policy = LeasePolicy::adr_0006();
    let renewed = f.cat.renew_lease(&lease.mount_id, &policy, 1).unwrap();
    assert_eq!(renewed.expires_at, lease.expires_at);
  }

  #[test]
  fn a_released_or_expired_lease_cannot_be_renewed() {
    let f = fixture();
    let lease = begin(&f, "m-1", 1800);
    f.cat.activate_lease(&lease.mount_id).unwrap();
    f.cat.release_lease(&lease.mount_id).unwrap();
    let policy = LeasePolicy::adr_0006();
    assert_eq!(
      f.cat
        .renew_lease(&lease.mount_id, &policy, 1800)
        .unwrap_err()
        .code,
      ErrorCode::FailedPrecondition
    );
  }

  #[test]
  fn release_is_idempotent_across_teardown_paths() {
    // Unmount, job cleanup, and orphan reaping all release. Making them
    // coordinate would produce stuck leases, not safety.
    let f = fixture();
    let lease = begin(&f, "m-1", 1800);
    f.cat.activate_lease(&lease.mount_id).unwrap();
    let first = f.cat.release_lease(&lease.mount_id).unwrap();
    let second = f.cat.release_lease(&lease.mount_id).unwrap();
    assert_eq!(first.state, LeaseState::Released);
    assert_eq!(second.state, LeaseState::Released);
    assert_eq!(first.terminal_at, second.terminal_at, "must not re-stamp");
  }

  #[test]
  fn an_overdue_lease_stays_protected_through_its_grace_interval() {
    // ADR 0006's grace exists so a transient renewal failure is reported and
    // recovered before a live workspace is destroyed.
    let f = fixture();
    // A negative TTL puts expiry in the past without sleeping.
    let lease = begin(&f, "m-1", -60);
    f.cat.activate_lease(&lease.mount_id).unwrap();

    let policy = LeasePolicy::adr_0006();
    let outcome = f.cat.sweep_leases(&policy).unwrap();
    assert_eq!(outcome.in_grace, vec![lease.mount_id.clone()]);
    assert!(outcome.expired.is_empty(), "grace must not expire it yet");

    // And it is still a reachability root, so maintenance will not prune it.
    let protecting = f.cat.protecting_leases(&f.repo, &policy).unwrap();
    assert_eq!(protecting.len(), 1);
    assert!(protecting[0].is_protecting(now_secs(), &policy));
  }

  #[test]
  fn a_lease_past_its_grace_expires_but_its_anchor_survives_the_prune_delay() {
    // The three stages ADR 0006 separates. Collapsing them would make a mistaken
    // expiry unrecoverable.
    let f = fixture();
    let policy = LeasePolicy::adr_0006();
    let past_grace = -(policy.renewal_grace.as_secs() as i64) - 60;
    let lease = begin(&f, "m-1", past_grace);
    f.cat.activate_lease(&lease.mount_id).unwrap();

    let outcome = f.cat.sweep_leases(&policy).unwrap();
    assert_eq!(outcome.expired, vec![lease.mount_id.clone()]);
    assert!(
      outcome.prunable.is_empty(),
      "the anchor must survive the prune delay so objects stay recoverable"
    );

    let stored = f.cat.get_lease(&lease.mount_id).unwrap().unwrap();
    assert_eq!(stored.state, LeaseState::Expired);
    // Still protecting, because the prune delay has not elapsed.
    assert!(stored.is_protecting(now_secs(), &policy));
    assert_eq!(f.cat.protecting_leases(&f.repo, &policy).unwrap().len(), 1);
  }

  #[test]
  fn an_anchor_becomes_prunable_only_after_the_prune_delay() {
    let f = fixture();
    let policy = LeasePolicy::adr_0006();
    let lease = begin(&f, "m-1", 1800);
    f.cat.activate_lease(&lease.mount_id).unwrap();
    f.cat.release_lease(&lease.mount_id).unwrap();

    // Just released: not prunable.
    assert!(f.cat.sweep_leases(&policy).unwrap().prunable.is_empty());

    // Backdate the terminal timestamp past the prune delay.
    let long_ago = now_secs() - policy.prune_delay.as_secs() as i64 - 60;
    f.cat
      .with_conn(|conn| {
        conn
          .execute(
            "UPDATE leases SET terminal_at = ?2 WHERE mount_id = ?1",
            rusqlite::params![lease.mount_id.as_str(), long_ago],
          )
          .map_err(db_error)?;
        Ok(())
      })
      .unwrap();

    let outcome = f.cat.sweep_leases(&policy).unwrap();
    assert_eq!(outcome.prunable.len(), 1);
    assert_eq!(outcome.prunable[0].0, lease.mount_id);
    assert_eq!(outcome.prunable[0].1, lease.anchor_ref);
    // And it no longer protects.
    assert!(f
      .cat
      .protecting_leases(&f.repo, &policy)
      .unwrap()
      .is_empty());
  }

  #[test]
  fn a_lease_past_the_maximum_total_age_cannot_renew() {
    // ADR 0006 bounds the total life of a lease so an abandoned daemon that
    // renews forever is still collected.
    let f = fixture();
    let policy = LeasePolicy::adr_0006();
    let lease = begin(&f, "m-1", 1800);
    f.cat.activate_lease(&lease.mount_id).unwrap();

    let long_ago = now_secs() - policy.max_total_age.as_secs() as i64 - 60;
    f.cat
      .with_conn(|conn| {
        conn
          .execute(
            "UPDATE leases SET created_at = ?2 WHERE mount_id = ?1",
            rusqlite::params![lease.mount_id.as_str(), long_ago],
          )
          .map_err(db_error)?;
        Ok(())
      })
      .unwrap();

    let err = f
      .cat
      .renew_lease(&lease.mount_id, &policy, 1800)
      .unwrap_err();
    assert_eq!(err.code, ErrorCode::FailedPrecondition);
    assert!(err.message.contains("maximum total age"), "{err}");
  }

  #[test]
  fn consecutive_renewal_failures_reach_the_alert_threshold_and_reset_on_success() {
    let f = fixture();
    let policy = LeasePolicy::adr_0006();
    let lease = begin(&f, "m-1", 1800);
    f.cat.activate_lease(&lease.mount_id).unwrap();

    assert_eq!(f.cat.record_renewal_failure(&lease.mount_id).unwrap(), 1);
    let second = f.cat.record_renewal_failure(&lease.mount_id).unwrap();
    assert_eq!(second, 2);
    assert!(
      second >= policy.alert_after_failures,
      "two failures must reach ADR 0006's alert threshold"
    );

    // A successful renewal resets it, so the counter measures consecutive
    // failures rather than lifetime ones.
    let renewed = f.cat.renew_lease(&lease.mount_id, &policy, 1800).unwrap();
    assert_eq!(renewed.renewal_failures, 0);
    assert_eq!(
      f.cat
        .get_lease(&lease.mount_id)
        .unwrap()
        .unwrap()
        .renewal_failures,
      0
    );
  }

  #[test]
  fn a_repository_with_a_live_lease_cannot_be_deleted() {
    // Deleting the row would cascade the lease away and leave the anchor behind
    // with a live mount reading through it.
    let f = fixture();
    let lease = begin(&f, "m-1", 1800);
    f.cat.activate_lease(&lease.mount_id).unwrap();
    assert_eq!(
      f.cat.delete_repository(&f.repo).unwrap_err().code,
      ErrorCode::FailedPrecondition
    );
    f.cat.release_lease(&lease.mount_id).unwrap();
    f.cat.delete_repository(&f.repo).unwrap();
  }

  #[test]
  fn unreconciled_leases_report_exactly_the_states_a_restart_must_resolve() {
    let f = fixture();
    let preparing = begin(&f, "m-prep", 1800);
    let active = begin(&f, "m-act", 1800);
    f.cat.activate_lease(&active.mount_id).unwrap();
    let released = begin(&f, "m-rel", 1800);
    f.cat.activate_lease(&released.mount_id).unwrap();
    f.cat.release_lease(&released.mount_id).unwrap();

    let ids: Vec<String> = f
      .cat
      .unreconciled_leases()
      .unwrap()
      .into_iter()
      .map(|l| l.mount_id.to_string())
      .collect();
    assert_eq!(ids, vec!["m-act", "m-prep"]);
    let _ = preparing;
  }
}
