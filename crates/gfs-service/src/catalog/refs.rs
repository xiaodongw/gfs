//! Ref state and the idempotent ref-event outbox.
//!
//! DESIGN.md section 7.1: ingestion uses webhooks plus polling for a proxy
//! deployment, and ref events are idempotent and keyed by
//! `(repository_id, ref_name, old_oid, new_oid)`. Both sources call
//! [`Catalog::observe_ref`], which is the only place ref state changes, so the
//! outbox cannot disagree with the ref table.

use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::{revision, ObjectId, RepositoryId};
use rusqlite::OptionalExtension;

use super::schema::db_error;
use super::{now_secs, Catalog};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefRecord {
  pub ref_name: String,
  pub oid: ObjectId,
  pub ref_version: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefEvent {
  pub event_id: i64,
  pub repository_id: RepositoryId,
  pub ref_name: String,
  /// `None` when the ref was created.
  pub old_oid: Option<ObjectId>,
  /// `None` when the ref was deleted.
  pub new_oid: Option<ObjectId>,
  pub ref_version: u64,
}

/// What an observation actually changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefObservation {
  Created,
  Updated,
  Deleted,
  /// The ref already had this value. No event was emitted.
  Unchanged,
}

impl Catalog {
  /// Record the observed state of one ref.
  ///
  /// `new_oid` of `None` means the ref is gone. Returns what changed, so a poller
  /// can log only real transitions rather than every poll.
  ///
  /// Rejects the reserved namespace. Lease anchors are internal reachability
  /// roots, not mirrored refs; recording one would put it in the ref table where
  /// `visible_refs`-style consumers and the prune logic could act on it.
  pub fn observe_ref(
    &self,
    repository_id: &RepositoryId,
    ref_name: &str,
    new_oid: Option<&ObjectId>,
  ) -> Result<RefObservation, GfsError> {
    if revision::is_reserved_ref(ref_name) {
      return Err(GfsError::new(
        ErrorCode::ReservedNamespace,
        "the reserved internal namespace is not a mirrored ref",
      ));
    }
    let now = now_secs();
    self.with_tx(|tx| {
      let existing: Option<(String, i64)> = tx
        .query_row(
          "SELECT oid, ref_version FROM repository_refs
           WHERE repository_id = ?1 AND ref_name = ?2",
          rusqlite::params![repository_id.as_str(), ref_name],
          |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(db_error)?;

      let old_oid = match &existing {
        Some((oid, _)) => Some(ObjectId::parse_qualified(oid)?),
        None => None,
      };

      if old_oid.as_ref() == new_oid {
        return Ok(RefObservation::Unchanged);
      }

      // The version counter is per repository, not per ref, so it also orders
      // updates *between* refs -- which is what lets a snapshot response say
      // "this is the repository state I saw" rather than only "this branch".
      let next_version: i64 = tx
        .query_row(
          "SELECT COALESCE(MAX(ref_version), 0) + 1 FROM repository_refs
           WHERE repository_id = ?1",
          [repository_id.as_str()],
          |r| r.get(0),
        )
        .map_err(db_error)?;

      let outcome = match (&old_oid, new_oid) {
        (None, Some(_)) => RefObservation::Created,
        (Some(_), Some(_)) => RefObservation::Updated,
        (Some(_), None) => RefObservation::Deleted,
        (None, None) => unreachable!("equal case returned above"),
      };

      match new_oid {
        Some(oid) => {
          tx.execute(
            "INSERT INTO repository_refs (repository_id, ref_name, oid, ref_version, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (repository_id, ref_name)
             DO UPDATE SET oid = ?3, ref_version = ?4, updated_at = ?5",
            rusqlite::params![
              repository_id.as_str(),
              ref_name,
              oid.to_qualified(),
              next_version,
              now
            ],
          )
          .map_err(db_error)?;
        }
        None => {
          tx.execute(
            "DELETE FROM repository_refs WHERE repository_id = ?1 AND ref_name = ?2",
            rusqlite::params![repository_id.as_str(), ref_name],
          )
          .map_err(db_error)?;
        }
      }

      // `INSERT OR IGNORE` is where idempotency lives. A webhook delivered twice,
      // or a webhook racing the poller, collapses to one event.
      //
      // The consequence worth stating: a ref that moves A -> B -> A -> B records
      // the A -> B transition once. That is acceptable because the outbox exists
      // to trigger work on `new_oid`, that work is itself idempotent, and the ref
      // table already holds the current value. It would *not* be acceptable if
      // the outbox were an audit log, which it is not.
      tx.execute(
        "INSERT OR IGNORE INTO ref_events
           (repository_id, ref_name, old_oid, new_oid, ref_version, observed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
          repository_id.as_str(),
          ref_name,
          old_oid.as_ref().map(ObjectId::to_qualified),
          new_oid.map(ObjectId::to_qualified),
          next_version,
          now,
        ],
      )
      .map_err(db_error)?;

      Ok(outcome)
    })
  }

  /// Reconcile the catalog's ref table against the repository's actual refs.
  ///
  /// Called after a fetch, and after a restart or a missed webhook. Emits events
  /// for every difference, including deletions the catalog did not see, so a
  /// missed webhook is recovered rather than silently leaving stale state.
  pub fn reconcile_refs(
    &self,
    repository_id: &RepositoryId,
    actual: &[(String, ObjectId)],
  ) -> Result<Vec<(String, RefObservation)>, GfsError> {
    let known = self.list_refs(repository_id)?;
    let actual_map: std::collections::BTreeMap<&str, &ObjectId> = actual
      .iter()
      .filter(|(name, _)| !revision::is_reserved_ref(name))
      .map(|(name, oid)| (name.as_str(), oid))
      .collect();

    let mut changes = Vec::new();

    for (name, oid) in &actual_map {
      let outcome = self.observe_ref(repository_id, name, Some(oid))?;
      if outcome != RefObservation::Unchanged {
        changes.push(((*name).to_owned(), outcome));
      }
    }
    for existing in &known {
      if !actual_map.contains_key(existing.ref_name.as_str()) {
        let outcome = self.observe_ref(repository_id, &existing.ref_name, None)?;
        if outcome != RefObservation::Unchanged {
          changes.push((existing.ref_name.clone(), outcome));
        }
      }
    }
    Ok(changes)
  }

  pub fn list_refs(&self, repository_id: &RepositoryId) -> Result<Vec<RefRecord>, GfsError> {
    self.with_conn(|conn| {
      let mut stmt = conn
        .prepare(
          "SELECT ref_name, oid, ref_version FROM repository_refs
           WHERE repository_id = ?1 ORDER BY ref_name",
        )
        .map_err(db_error)?;
      let rows = stmt
        .query_map([repository_id.as_str()], |row| {
          let name: String = row.get(0)?;
          let oid: String = row.get(1)?;
          let version: i64 = row.get(2)?;
          Ok((name, oid, version))
        })
        .map_err(db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db_error)?;
      rows
        .into_iter()
        .map(|(ref_name, oid, ref_version)| {
          Ok(RefRecord {
            ref_name,
            oid: ObjectId::parse_qualified(&oid)?,
            ref_version: ref_version as u64,
          })
        })
        .collect()
    })
  }

  /// The current version of one ref, or the repository's high-water mark when the
  /// ref is not named.
  ///
  /// Returned from `ResolveRevision` so a caller can detect that a branch moved
  /// between two calls without comparing OIDs, which cannot distinguish
  /// "unchanged" from "moved and moved back".
  pub fn ref_version(
    &self,
    repository_id: &RepositoryId,
    ref_name: Option<&str>,
  ) -> Result<u64, GfsError> {
    self.with_conn(|conn| {
      let version: i64 = match ref_name {
        Some(name) => conn
          .query_row(
            "SELECT ref_version FROM repository_refs
             WHERE repository_id = ?1 AND ref_name = ?2",
            rusqlite::params![repository_id.as_str(), name],
            |r| r.get(0),
          )
          .optional()
          .map_err(db_error)?
          .unwrap_or(0),
        None => conn
          .query_row(
            "SELECT COALESCE(MAX(ref_version), 0) FROM repository_refs
             WHERE repository_id = ?1",
            [repository_id.as_str()],
            |r| r.get(0),
          )
          .map_err(db_error)?,
      };
      Ok(version as u64)
    })
  }

  /// Unprocessed ref events, oldest first.
  pub fn pending_ref_events(
    &self,
    repository_id: &RepositoryId,
    limit: usize,
  ) -> Result<Vec<RefEvent>, GfsError> {
    self.with_conn(|conn| {
      let mut stmt = conn
        .prepare(
          "SELECT event_id, repository_id, ref_name, old_oid, new_oid, ref_version
           FROM ref_events
           WHERE repository_id = ?1 AND processed_at IS NULL
           ORDER BY event_id LIMIT ?2",
        )
        .map_err(db_error)?;
      let rows = stmt
        .query_map(
          rusqlite::params![repository_id.as_str(), limit as i64],
          |row| {
            let event_id: i64 = row.get(0)?;
            let repo: String = row.get(1)?;
            let name: String = row.get(2)?;
            let old: Option<String> = row.get(3)?;
            let new: Option<String> = row.get(4)?;
            let version: i64 = row.get(5)?;
            Ok((event_id, repo, name, old, new, version))
          },
        )
        .map_err(db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db_error)?;
      rows
        .into_iter()
        .map(|(event_id, repo, ref_name, old, new, version)| {
          Ok(RefEvent {
            event_id,
            repository_id: RepositoryId::parse(&repo)?,
            ref_name,
            old_oid: old.as_deref().map(ObjectId::parse_qualified).transpose()?,
            new_oid: new.as_deref().map(ObjectId::parse_qualified).transpose()?,
            ref_version: version as u64,
          })
        })
        .collect()
    })
  }

  pub fn mark_ref_event_processed(&self, event_id: i64) -> Result<(), GfsError> {
    let now = now_secs();
    self.with_conn(|conn| {
      conn
        .execute(
          "UPDATE ref_events SET processed_at = ?2 WHERE event_id = ?1",
          rusqlite::params![event_id, now],
        )
        .map_err(db_error)?;
      Ok(())
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::catalog::repositories::NewRepository;
  use gfs_types::{DisplayName, HashAlgorithm};

  fn setup() -> (Catalog, RepositoryId) {
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
    (cat, repo)
  }

  fn oid(b: &str) -> ObjectId {
    ObjectId::from_hex(HashAlgorithm::Sha1, &b.repeat(20)).unwrap()
  }

  #[test]
  fn a_duplicate_observation_produces_no_second_event() {
    // A webhook delivered twice, or a webhook racing the poller.
    let (cat, repo) = setup();
    assert_eq!(
      cat
        .observe_ref(&repo, "refs/heads/main", Some(&oid("aa")))
        .unwrap(),
      RefObservation::Created
    );
    assert_eq!(
      cat
        .observe_ref(&repo, "refs/heads/main", Some(&oid("aa")))
        .unwrap(),
      RefObservation::Unchanged
    );
    assert_eq!(cat.pending_ref_events(&repo, 100).unwrap().len(), 1);
  }

  #[test]
  fn ref_version_advances_on_every_real_change_and_not_otherwise() {
    let (cat, repo) = setup();
    cat
      .observe_ref(&repo, "refs/heads/main", Some(&oid("aa")))
      .unwrap();
    let v1 = cat.ref_version(&repo, Some("refs/heads/main")).unwrap();
    cat
      .observe_ref(&repo, "refs/heads/main", Some(&oid("aa")))
      .unwrap();
    assert_eq!(cat.ref_version(&repo, Some("refs/heads/main")).unwrap(), v1);
    cat
      .observe_ref(&repo, "refs/heads/main", Some(&oid("bb")))
      .unwrap();
    let v2 = cat.ref_version(&repo, Some("refs/heads/main")).unwrap();
    assert!(v2 > v1);
  }

  #[test]
  fn a_branch_that_moves_back_and_forth_is_detectable_by_version_not_by_oid() {
    // The reason `ref_version` exists at all: comparing commit OIDs across two
    // calls cannot distinguish "unchanged" from "moved and moved back".
    let (cat, repo) = setup();
    cat
      .observe_ref(&repo, "refs/heads/main", Some(&oid("aa")))
      .unwrap();
    let v_start = cat.ref_version(&repo, Some("refs/heads/main")).unwrap();
    cat
      .observe_ref(&repo, "refs/heads/main", Some(&oid("bb")))
      .unwrap();
    cat
      .observe_ref(&repo, "refs/heads/main", Some(&oid("aa")))
      .unwrap();

    let v_end = cat.ref_version(&repo, Some("refs/heads/main")).unwrap();
    let record = &cat.list_refs(&repo).unwrap()[0];
    assert_eq!(record.oid, oid("aa"), "the OID is back where it started");
    assert!(v_end > v_start, "but the version records that it moved");
  }

  #[test]
  fn deletion_emits_an_event_and_removes_the_ref() {
    let (cat, repo) = setup();
    cat
      .observe_ref(&repo, "refs/heads/gone", Some(&oid("aa")))
      .unwrap();
    assert_eq!(
      cat.observe_ref(&repo, "refs/heads/gone", None).unwrap(),
      RefObservation::Deleted
    );
    assert!(cat.list_refs(&repo).unwrap().is_empty());
    let events = cat.pending_ref_events(&repo, 100).unwrap();
    let deletion = events.last().unwrap();
    assert_eq!(deletion.old_oid, Some(oid("aa")));
    assert_eq!(deletion.new_oid, None);
  }

  #[test]
  fn the_reserved_namespace_is_not_a_mirrored_ref() {
    // A lease anchor recorded in the ref table would be visible to consumers of
    // ref state and reachable by prune logic.
    let (cat, repo) = setup();
    let anchor = gfs_types::revision::lease_anchor_ref("m-1");
    assert_eq!(
      cat
        .observe_ref(&repo, &anchor, Some(&oid("aa")))
        .unwrap_err()
        .code,
      ErrorCode::ReservedNamespace
    );
  }

  #[test]
  fn reconciliation_recovers_a_missed_webhook_in_both_directions() {
    // The restart / missed-delivery path. The catalog is stale in two ways at
    // once: a branch it never saw, and a branch that has since been deleted.
    let (cat, repo) = setup();
    cat
      .observe_ref(&repo, "refs/heads/main", Some(&oid("aa")))
      .unwrap();
    cat
      .observe_ref(&repo, "refs/heads/stale", Some(&oid("cc")))
      .unwrap();

    let actual = vec![
      ("refs/heads/main".to_owned(), oid("bb")),
      ("refs/heads/added".to_owned(), oid("dd")),
      // A lease anchor present in the repository must be ignored, not mirrored.
      (gfs_types::revision::lease_anchor_ref("m-1"), oid("ee")),
    ];
    let changes = cat.reconcile_refs(&repo, &actual).unwrap();

    let mut by_name: std::collections::BTreeMap<&str, RefObservation> =
      changes.iter().map(|(n, o)| (n.as_str(), *o)).collect();
    assert_eq!(
      by_name.remove("refs/heads/main"),
      Some(RefObservation::Updated)
    );
    assert_eq!(
      by_name.remove("refs/heads/added"),
      Some(RefObservation::Created)
    );
    assert_eq!(
      by_name.remove("refs/heads/stale"),
      Some(RefObservation::Deleted)
    );
    assert!(by_name.is_empty(), "unexpected changes: {by_name:?}");

    let names: Vec<String> = cat
      .list_refs(&repo)
      .unwrap()
      .into_iter()
      .map(|r| r.ref_name)
      .collect();
    assert_eq!(names, ["refs/heads/added", "refs/heads/main"]);
  }

  #[test]
  fn events_are_processed_once() {
    let (cat, repo) = setup();
    cat
      .observe_ref(&repo, "refs/heads/main", Some(&oid("aa")))
      .unwrap();
    let pending = cat.pending_ref_events(&repo, 100).unwrap();
    assert_eq!(pending.len(), 1);
    cat.mark_ref_event_processed(pending[0].event_id).unwrap();
    assert!(cat.pending_ref_events(&repo, 100).unwrap().is_empty());
  }
}
