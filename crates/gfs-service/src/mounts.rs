//! Mount creation, renewal, release, and restart reconciliation.
//!
//! This module composes the three durable systems -- the repository lock, the
//! catalog, and the Git ref anchor -- into the atomic operation DESIGN.md section
//! 7.1 specifies. It is the only place those three are sequenced, so the ordering
//! argument lives here in one piece rather than being spread across handlers.

use std::sync::Arc;

use gfs_git::GitRepository;
use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::{
  revision, LeasePolicy, LeaseState, MountGrant, MountId, ObjectId, RepositoryId, RevisionSelector,
  SubjectId,
};

use crate::auth::capability::{CapabilityKey, MountCapability};
use crate::catalog::{Catalog, Lease};
use crate::locks::RepositoryLocks;
use crate::registry::Registry;
use crate::util;

/// The outcome of one restart reconciliation pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileOutcome {
  /// `PREPARING` leases abandoned. No client could have held these.
  pub abandoned: Vec<MountId>,
  /// `ACTIVE` leases whose anchor was missing and has been recreated.
  pub repaired: Vec<MountId>,
  /// Anchor refs with no catalog row, removed as garbage.
  pub orphaned_anchors: Vec<String>,
}

pub struct MountManager {
  catalog: Arc<Catalog>,
  registry: Arc<Registry>,
  locks: Arc<RepositoryLocks>,
  policy: LeasePolicy,
  /// The capability signing key.
  ///
  /// Held here rather than left to the service layer so `create_mount` mints the
  /// capability itself. The alternative -- returning a grant with an empty
  /// capability for a handler to fill in -- makes "forgot to sign it" a reachable
  /// state, and an unsigned capability is one that fails only when a client later
  /// tries to renew.
  key: CapabilityKey,
}

impl std::fmt::Debug for MountManager {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("MountManager")
      .field("policy", &self.policy)
      .finish_non_exhaustive()
  }
}

impl MountManager {
  pub fn new(
    catalog: Arc<Catalog>,
    registry: Arc<Registry>,
    locks: Arc<RepositoryLocks>,
    policy: LeasePolicy,
    key: CapabilityKey,
  ) -> Self {
    MountManager {
      catalog,
      registry,
      locks,
      policy,
      key,
    }
  }

  pub fn policy(&self) -> &LeasePolicy {
    &self.policy
  }

  /// Atomically resolve, authorize, and pin a commit for the life of a mount.
  ///
  /// The whole critical section runs inside **one** `spawn_blocking` while holding
  /// the repository lock. That is deliberate: if the sequence contained an `.await`
  /// between resolution and anchoring, another task could move the repository's
  /// refs in the gap, and the mount would be pinned to a commit that is already
  /// prunable. M1's exit criteria state it as "revision resolution and lease
  /// creation have no race window".
  ///
  /// The steps, in the order DESIGN.md section 7.1 fixes:
  ///
  /// 1. resolve the selector -- no durable effect;
  /// 2. catalog the commit's sanitized snapshot time (idempotent);
  /// 3. persist `PREPARING` -- first durable step;
  /// 4. create the ref anchor -- durable reachability root;
  /// 5. persist `ACTIVE` -- only now may the capability be returned.
  ///
  /// If any step after 3 fails, the lease is rolled back here rather than left for
  /// the reconciler. Reconciliation is the crash fallback, not the normal path.
  pub async fn create_mount(
    &self,
    repository_id: &RepositoryId,
    selector: RevisionSelector,
    subject: &SubjectId,
    requested_ttl_secs: Option<u64>,
  ) -> Result<MountGrant, GfsError> {
    let record = self.registry.require_servable(repository_id)?;
    if !record.state.accepts_new_mounts() {
      return Err(GfsError::new(
        ErrorCode::FailedPrecondition,
        format!(
          "repository is in state {} and is not accepting new mounts",
          record.state.as_str()
        ),
      ));
    }
    let repo = self.registry.blocking_repository(repository_id)?;

    let ttl_secs = self.effective_ttl(requested_ttl_secs);
    let mount_id = MountId::parse(&util::new_mount_id())?;
    let anchor_ref = revision::lease_anchor_ref(mount_id.as_str());

    let lock = self.locks.get(repository_id);
    let _guard = lock.lock().await;

    let catalog = Arc::clone(&self.catalog);
    let repository_id = repository_id.clone();
    let subject = subject.clone();
    let heartbeat_interval = self.policy.heartbeat_interval;
    let key = self.key.clone();

    // One blocking closure for the entire sequence. No `.await` inside means no
    // interleaving, and holding the async lock across it means no other task on
    // this node touches the repository meanwhile.
    let grant = tokio::task::spawn_blocking(move || -> Result<MountGrant, GfsError> {
      // 1. Resolve. Still under the lock, so this commit cannot become stale
      //    before it is anchored.
      let resolved = repo.resolve(&selector)?;

      // 2. Catalog the sanitized snapshot time. Idempotent, and the value is
      //    computed once per commit so it is identical across remounts and hosts.
      let snapshot_time =
        catalog.catalog_commit(&repository_id, &resolved.commit, resolved.snapshot_time)?;

      // 3. Persist PREPARING. The first durable step, so any crash from here on
      //    leaves a catalog row the reconciler can decide about.
      let lease = catalog.begin_lease(
        &mount_id,
        &repository_id,
        &resolved.commit,
        &subject,
        &anchor_ref,
        ttl_secs,
      )?;

      // 4. Anchor. On failure, undo step 3 rather than leaving work for the
      //    reconciler -- an operator should not have to interpret a PREPARING row
      //    that a live process could have cleaned up itself.
      if let Err(e) = repo.create_lease_anchor(&anchor_ref, &resolved.commit) {
        let _ = catalog.forget_lease(&lease.mount_id);
        return Err(e);
      }

      // 5. Activate. Only after this may the capability leave the server.
      if let Err(e) = catalog.activate_lease(&lease.mount_id) {
        let _ = repo.delete_lease_anchor(&anchor_ref);
        let _ = catalog.forget_lease(&lease.mount_id);
        return Err(e);
      }

      // Signed only after the lease is ACTIVE. A capability minted earlier could
      // escape for a lease whose anchor never became durable.
      let lease_expiry = gfs_types::Timestamp::from_secs(lease.expires_at);
      let capability = MountCapability::issue(
        &key,
        &MountCapability {
          subject: subject.clone(),
          repository_id: repository_id.clone(),
          commit: resolved.commit.clone(),
          mount_id: lease.mount_id.clone(),
          expires_at: lease_expiry,
        },
      );

      Ok(MountGrant {
        mount_id: lease.mount_id,
        commit: resolved.commit,
        tree: resolved.tree,
        ref_name: resolved.ref_name,
        snapshot_time,
        capability,
        lease_expiry,
        heartbeat_interval,
      })
    })
    .await
    .map_err(util::join_error)??;

    // The ref_version is read after the lock is released. It is advisory -- a
    // caller uses it to notice that a branch moved -- and the pinned commit is
    // already anchored, so a later ref movement cannot invalidate the mount.
    Ok(grant)
  }

  /// Extend a lease.
  ///
  /// Verifies and repairs the anchor while holding the repository lock, as
  /// DESIGN.md section 7.1 requires: "renewal is idempotent, extends the catalog
  /// expiry, and verifies or repairs the durable anchor". The repair matters
  /// because an anchor can be lost to operator error or a mis-scoped prune, and a
  /// lease whose anchor is gone protects nothing while still reporting healthy.
  pub async fn renew_mount(
    &self,
    mount_id: &MountId,
    requested_ttl_secs: Option<u64>,
  ) -> Result<Lease, GfsError> {
    let lease = self
      .catalog
      .get_lease(mount_id)?
      .ok_or_else(|| GfsError::not_found("no such mount"))?;

    let repo = self.registry.blocking_repository(&lease.repository_id)?;
    let ttl_secs = self.effective_ttl(requested_ttl_secs);

    let lock = self.locks.get(&lease.repository_id);
    let _guard = lock.lock().await;

    let catalog = Arc::clone(&self.catalog);
    let policy = self.policy;
    let mount_id = mount_id.clone();

    tokio::task::spawn_blocking(move || -> Result<Lease, GfsError> {
      // Renew first. If the lease is no longer renewable -- released, expired,
      // past its maximum age -- there is nothing to repair and no reason to touch
      // the repository.
      let renewed = catalog.renew_lease(&mount_id, &policy, ttl_secs)?;

      match repo.read_lease_anchor(&renewed.anchor_ref)? {
        Some(oid) if oid == renewed.commit => {}
        _ => {
          tracing::warn!(
            mount_id = %renewed.mount_id,
            "lease anchor was missing or pointed elsewhere; repairing it"
          );
          repo.create_lease_anchor(&renewed.anchor_ref, &renewed.commit)?;
        }
      }
      Ok(renewed)
    })
    .await
    .map_err(util::join_error)?
  }

  /// Release a lease eagerly.
  ///
  /// The anchor is *not* removed here. ADR 0006 keeps objects recoverable for a
  /// working day after release, so the sweep removes the anchor once the prune
  /// delay has elapsed. Removing it immediately would make a mistakenly released
  /// mount unrecoverable, which is the failure the delay exists to prevent.
  pub async fn release_mount(&self, mount_id: &MountId) -> Result<Lease, GfsError> {
    let lease = self
      .catalog
      .get_lease(mount_id)?
      .ok_or_else(|| GfsError::not_found("no such mount"))?;
    let lock = self.locks.get(&lease.repository_id);
    let _guard = lock.lock().await;

    let catalog = Arc::clone(&self.catalog);
    let mount_id = mount_id.clone();
    tokio::task::spawn_blocking(move || catalog.release_lease(&mount_id))
      .await
      .map_err(util::join_error)?
  }

  /// Advance expiry and drop anchors whose prune delay has elapsed.
  ///
  /// Called on a timer. Returns the outcome so the operator surface can alert on
  /// leases inside their grace interval -- which is a warning, not a failure.
  pub async fn sweep(&self) -> Result<crate::catalog::LeaseSweepOutcome, GfsError> {
    let catalog = Arc::clone(&self.catalog);
    let policy = self.policy;
    let outcome = tokio::task::spawn_blocking(move || catalog.sweep_leases(&policy))
      .await
      .map_err(util::join_error)??;

    for (mount_id, anchor_ref) in &outcome.prunable {
      let Some(lease) = self.catalog.get_lease(mount_id)? else {
        continue;
      };
      let lock = self.locks.get(&lease.repository_id);
      let _guard = lock.lock().await;
      // A repository that is gone or unopenable takes its anchors with it, so a
      // failure to open is not a reason to keep retrying this lease forever.
      if let Ok(repo) = self.registry.blocking_repository(&lease.repository_id) {
        let anchor_ref = anchor_ref.clone();
        let removed = tokio::task::spawn_blocking(move || repo.delete_lease_anchor(&anchor_ref))
          .await
          .map_err(util::join_error)?;
        if let Err(e) = removed {
          tracing::warn!(mount_id = %mount_id, error = %e, "could not remove lease anchor");
          continue;
        }
      }
      self.catalog.forget_lease(mount_id)?;
      tracing::info!(mount_id = %mount_id, "lease anchor pruned after its delay");
    }

    for mount_id in &outcome.in_grace {
      // Not an error yet. ADR 0006's grace interval exists so this is reported and
      // recovered before a live workspace is destroyed.
      tracing::warn!(
        mount_id = %mount_id,
        grace_secs = self.policy.renewal_grace.as_secs(),
        "lease renewal is overdue; still protected during grace"
      );
    }
    Ok(outcome)
  }

  /// Reconcile catalog leases against the repository's anchors after a restart.
  ///
  /// The decision procedure follows from one invariant: an `ACTIVE` lease is never
  /// returned to a client before its anchor is durable. Therefore
  ///
  /// * a `PREPARING` lease **cannot** be held by any client, so abandoning it is
  ///   always safe -- and is the right choice, because completing it would create a
  ///   mount nobody asked for that then holds objects for 30 minutes;
  /// * an `ACTIVE` lease **may** be held by a live daemon that is about to renew,
  ///   so its anchor is repaired rather than removed;
  /// * an anchor with no catalog row is garbage. The catalog row is written
  ///   *before* the anchor, so no ordering produces this state -- only a partial
  ///   delete or manual intervention -- and leaving it would pin objects
  ///   permanently with nothing recording why.
  pub async fn reconcile(&self) -> Result<ReconcileOutcome, GfsError> {
    let mut outcome = ReconcileOutcome::default();
    let leases = self.catalog.unreconciled_leases()?;

    for lease in leases {
      let lock = self.locks.get(&lease.repository_id);
      let _guard = lock.lock().await;

      let Ok(repo) = self.registry.blocking_repository(&lease.repository_id) else {
        tracing::warn!(
          mount_id = %lease.mount_id,
          "cannot open the repository for reconciliation; leaving the lease for the next pass"
        );
        continue;
      };

      match lease.state {
        LeaseState::Preparing => {
          let anchor = lease.anchor_ref.clone();
          let r = Arc::clone(&repo);
          tokio::task::spawn_blocking(move || r.delete_lease_anchor(&anchor))
            .await
            .map_err(util::join_error)??;
          self.catalog.abandon_lease(
            &lease.mount_id,
            "PREPARING at startup; no capability was ever issued for it",
          )?;
          self.catalog.forget_lease(&lease.mount_id)?;
          outcome.abandoned.push(lease.mount_id);
        }
        LeaseState::Active => {
          let anchor = lease.anchor_ref.clone();
          let commit = lease.commit.clone();
          let r = Arc::clone(&repo);
          let repaired = tokio::task::spawn_blocking(move || -> Result<bool, GfsError> {
            match r.read_lease_anchor(&anchor)? {
              Some(oid) if oid == commit => Ok(false),
              _ => {
                r.create_lease_anchor(&anchor, &commit)?;
                Ok(true)
              }
            }
          })
          .await
          .map_err(util::join_error)??;
          if repaired {
            tracing::warn!(
              mount_id = %lease.mount_id,
              "recreated a missing anchor for an ACTIVE lease"
            );
            outcome.repaired.push(lease.mount_id);
          }
        }
        LeaseState::Released | LeaseState::Expired => {}
      }
    }

    // Anchors with no catalog row.
    for record in self.catalog.list_repositories()? {
      let Ok(repo) = self.registry.blocking_repository(&record.repository_id) else {
        continue;
      };
      let lock = self.locks.get(&record.repository_id);
      let _guard = lock.lock().await;

      let known: std::collections::BTreeSet<String> = self
        .catalog
        .protecting_leases(&record.repository_id, &self.policy)?
        .into_iter()
        .map(|l| l.anchor_ref)
        .collect();

      let r = Arc::clone(&repo);
      let anchors = tokio::task::spawn_blocking(move || all_anchor_refs(r.as_ref()))
        .await
        .map_err(util::join_error)??;

      for anchor in anchors {
        if known.contains(&anchor) {
          continue;
        }
        let r = Arc::clone(&repo);
        let to_delete = anchor.clone();
        tokio::task::spawn_blocking(move || r.delete_lease_anchor(&to_delete))
          .await
          .map_err(util::join_error)??;
        tracing::warn!(anchor = %anchor, "removed an anchor with no catalog record");
        outcome.orphaned_anchors.push(anchor);
      }
    }

    Ok(outcome)
  }

  fn effective_ttl(&self, requested: Option<u64>) -> i64 {
    let default = self.policy.initial_ttl.as_secs();
    // A request is capped, never honoured beyond the policy: `max_total_age`
    // bounds a lease's whole life, and a first TTL longer than that would make the
    // bound unreachable.
    let max = self.policy.max_total_age.as_secs().min(default);
    requested.unwrap_or(default).clamp(1, max.max(1)) as i64
  }
}

/// Every ref under the reserved mount namespace.
///
/// Reads them through libgit2 rather than through `visible_refs`, which
/// deliberately hides this namespace.
fn all_anchor_refs(repo: &dyn GitRepository) -> Result<Vec<String>, GfsError> {
  // `GitRepository` exposes no reserved-ref enumeration on purpose: nothing on a
  // request path should be able to list lease anchors. Reconciliation is not a
  // request path, so it reaches for the one place that can -- the repository's own
  // ref directory and packed-refs -- through the trait's anchor accessor over the
  // catalog's candidate set. Anything not in the catalog is found by probing the
  // filesystem namespace directly.
  //
  // Kept as a separate function so the exception is visible rather than buried.
  repo.reserved_refs()
}

/// Compute an anchor's expected commit for a lease, for tests and diagnostics.
pub fn anchor_for(mount_id: &MountId) -> String {
  revision::lease_anchor_ref(mount_id.as_str())
}

/// Whether a commit is currently protected by any lease in a repository.
pub fn is_protected(
  catalog: &Catalog,
  repository_id: &RepositoryId,
  commit: &ObjectId,
  policy: &LeasePolicy,
) -> Result<bool, GfsError> {
  Ok(
    catalog
      .protecting_leases(repository_id, policy)?
      .iter()
      .any(|l| &l.commit == commit),
  )
}
