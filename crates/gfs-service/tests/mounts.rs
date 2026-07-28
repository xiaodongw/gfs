//! End-to-end tests for mount creation, renewal, release, sweep, and restart
//! reconciliation against real Git repositories.
//!
//! These cover the M1 exit criteria that need all three durable systems at once --
//! the catalog, the ref anchor, and the repository lock -- which the unit tests in
//! `catalog::leases` cannot, because they have no repository.

use std::sync::Arc;

use gfs_git::GitRepository;
use gfs_service::auth::CapabilityKey;
use gfs_service::catalog::repositories::{NewRepository, RepositoryState};
use gfs_service::{Catalog, MountManager, Registry, RepositoryLocks};
use gfs_types::error::ErrorCode;
use gfs_types::{
  DisplayName, HashAlgorithm, LeasePolicy, MountId, RepositoryId, RevisionSelector, SubjectId,
};

struct Harness {
  catalog: Arc<Catalog>,
  registry: Arc<Registry>,
  mounts: MountManager,
  repo_id: RepositoryId,
  repo_path: std::path::PathBuf,
  subject: SubjectId,
  _tmp: tempfile::TempDir,
}

impl Harness {
  fn new() -> Self {
    Self::with_policy(LeasePolicy::adr_0006())
  }

  fn with_policy(policy: LeasePolicy) -> Self {
    let (tmp, repo_path) = gfs_test::scratch_clone("basic").unwrap();
    let catalog = Arc::new(Catalog::open_in_memory().unwrap());
    let repo_id = RepositoryId::parse("r-test").unwrap();
    catalog
      .create_repository(&NewRepository {
        repository_id: repo_id.clone(),
        display_name: DisplayName::parse("acme/monorepo").unwrap(),
        repo_path: repo_path.clone(),
        algorithm: HashAlgorithm::Sha1,
        upstream_url: None,
        credential_ref: None,
      })
      .unwrap();
    let registry = Arc::new(Registry::new(Arc::clone(&catalog)));
    registry.activate(&repo_id).unwrap();

    let locks = Arc::new(RepositoryLocks::new());
    let mounts = MountManager::new(
      Arc::clone(&catalog),
      Arc::clone(&registry),
      locks,
      policy,
      CapabilityKey::generate().unwrap(),
    );
    Harness {
      catalog,
      registry,
      mounts,
      repo_id,
      repo_path,
      subject: SubjectId::parse("job-123").unwrap(),
      _tmp: tmp,
    }
  }

  fn selector(&self, s: &str) -> RevisionSelector {
    RevisionSelector::parse(s, HashAlgorithm::Sha1).unwrap()
  }

  fn repo(&self) -> Arc<dyn GitRepository> {
    self.registry.blocking_repository(&self.repo_id).unwrap()
  }

  async fn create(&self) -> gfs_types::MountGrant {
    self
      .mounts
      .create_mount(&self.repo_id, self.selector("main"), &self.subject, None)
      .await
      .unwrap()
  }
}

#[tokio::test]
async fn create_mount_pins_a_commit_and_anchors_it_before_returning() {
  // The atomic sequence from DESIGN.md section 7.1. The observable guarantee: by
  // the time the caller has a grant, the anchor is already durable -- there is no
  // window in which the client believes it has a pinned commit that gc could take.
  let h = Harness::new();
  let grant = h.create().await;

  assert_eq!(grant.ref_name.as_deref(), Some("refs/heads/main"));
  let expected = gfs_test::git(&h.repo_path, &["rev-parse", "main"])
    .unwrap()
    .trim()
    .to_owned();
  assert_eq!(grant.commit.to_hex(), expected);

  let anchor = gfs_types::revision::lease_anchor_ref(grant.mount_id.as_str());
  assert_eq!(
    h.repo().read_lease_anchor(&anchor).unwrap(),
    Some(grant.commit.clone()),
    "the anchor must exist by the time the grant is returned"
  );

  let lease = h.catalog.get_lease(&grant.mount_id).unwrap().unwrap();
  assert_eq!(lease.state, gfs_types::LeaseState::Active);
  assert_eq!(lease.subject, h.subject);

  // The snapshot time is the sanitized one, not the raw committer time, and it is
  // in the past.
  assert!(grant.snapshot_time <= gfs_types::Timestamp::now());
  assert!(grant.snapshot_time >= gfs_types::Timestamp::MIN_SUPPORTED);
  // And the server supplies the heartbeat cadence rather than the client assuming
  // one.
  assert_eq!(
    grant.heartbeat_interval,
    LeasePolicy::adr_0006().heartbeat_interval
  );
}

#[tokio::test]
async fn two_mounts_of_the_same_branch_share_a_snapshot_time_and_have_distinct_anchors() {
  let h = Harness::new();
  let a = h.create().await;
  let b = h.create().await;

  assert_ne!(a.mount_id, b.mount_id);
  assert_eq!(a.commit, b.commit);
  // Cataloged once per commit, so every mount of the same commit reports the same
  // base timestamp -- the M2 criterion about remounts.
  assert_eq!(a.snapshot_time, b.snapshot_time);

  let repo = h.repo();
  for grant in [&a, &b] {
    let anchor = gfs_types::revision::lease_anchor_ref(grant.mount_id.as_str());
    assert_eq!(
      repo.read_lease_anchor(&anchor).unwrap(),
      Some(grant.commit.clone())
    );
  }
  // Both anchors exist and neither is visible to a client.
  assert_eq!(repo.reserved_refs().unwrap().len(), 2);
  assert!(repo
    .visible_refs()
    .unwrap()
    .iter()
    .all(|(n, _)| !n.starts_with("refs/gfs/")));
}

#[tokio::test]
async fn a_mounted_commit_survives_a_force_push_and_a_full_gc() {
  // M1's headline exit criterion, through the full server path rather than the
  // repository layer alone.
  let h = Harness::new();
  let grant = h.create().await;

  let older = gfs_test::git(&h.repo_path, &["rev-parse", "v1.0"])
    .unwrap()
    .trim()
    .to_owned();
  gfs_test::git(&h.repo_path, &["update-ref", "refs/heads/main", &older]).unwrap();
  gfs_test::git(&h.repo_path, &["update-ref", "-d", "refs/heads/feature"]).unwrap();
  gfs_test::git(&h.repo_path, &["tag", "-d", "v2.0"]).unwrap();
  gfs_test::git(&h.repo_path, &["tag", "-d", "tree-tag"]).unwrap();
  gfs_test::git(
    &h.repo_path,
    &["-c", "gc.reflogExpire=now", "gc", "-q", "--prune=now"],
  )
  .unwrap();

  // Reopen so nothing comes from an in-process cache.
  h.registry.evict(&h.repo_id);
  let repo = h.repo();
  assert!(
    repo.read_commit(&grant.commit).is_ok(),
    "the leased commit must survive gc"
  );
  // And it is no longer reachable from any visible ref, which is what makes the
  // mount capability necessary rather than decorative.
  assert!(!repo.is_visible(&grant.commit).unwrap());
}

#[tokio::test]
async fn renewal_extends_a_live_lease_and_repairs_a_missing_anchor() {
  // DESIGN.md section 7.1 requires renewal to "verify or repair the durable
  // anchor". An anchor lost to operator error or a mis-scoped prune leaves a lease
  // that reports healthy while protecting nothing, so the repair is the point.
  let h = Harness::new();
  let grant = h.create().await;
  let anchor = gfs_types::revision::lease_anchor_ref(grant.mount_id.as_str());

  // Someone removes the anchor out of band.
  h.repo().delete_lease_anchor(&anchor).unwrap();
  assert_eq!(h.repo().read_lease_anchor(&anchor).unwrap(), None);

  let renewed = h.mounts.renew_mount(&grant.mount_id, None).await.unwrap();
  assert_eq!(
    h.repo().read_lease_anchor(&anchor).unwrap(),
    Some(grant.commit.clone()),
    "renewal must recreate a missing anchor"
  );
  assert!(renewed.expires_at >= grant.lease_expiry.secs);
}

#[tokio::test]
async fn release_leaves_the_anchor_until_the_prune_delay_elapses() {
  // ADR 0006 keeps objects recoverable for a working day after release. Removing
  // the anchor on release would make a mistaken release unrecoverable.
  let h = Harness::new();
  let grant = h.create().await;
  let anchor = gfs_types::revision::lease_anchor_ref(grant.mount_id.as_str());

  h.mounts.release_mount(&grant.mount_id).await.unwrap();
  assert_eq!(
    h.repo().read_lease_anchor(&anchor).unwrap(),
    Some(grant.commit.clone()),
    "the anchor must outlive the release"
  );

  // A sweep now changes nothing.
  let outcome = h.mounts.sweep().await.unwrap();
  assert!(outcome.prunable.is_empty());
  assert_eq!(
    h.repo().read_lease_anchor(&anchor).unwrap(),
    Some(grant.commit)
  );
}

#[tokio::test]
async fn the_sweep_removes_an_anchor_once_its_prune_delay_has_passed() {
  // A zero prune delay stands in for the passage of a day. Everything else is the
  // real path: sweep marks it prunable, removes the anchor under the repository
  // lock, then forgets the lease.
  let mut policy = LeasePolicy::adr_0006();
  policy.prune_delay = std::time::Duration::ZERO;
  let h = Harness::with_policy(policy);

  let grant = h.create().await;
  let anchor = gfs_types::revision::lease_anchor_ref(grant.mount_id.as_str());
  h.mounts.release_mount(&grant.mount_id).await.unwrap();

  let outcome = h.mounts.sweep().await.unwrap();
  assert_eq!(outcome.prunable.len(), 1);
  assert_eq!(
    h.repo().read_lease_anchor(&anchor).unwrap(),
    None,
    "the anchor must be gone once the delay has elapsed"
  );
  assert!(
    h.catalog.get_lease(&grant.mount_id).unwrap().is_none(),
    "and the lease is forgotten"
  );

  // Now gc really can reclaim the commit: an anchor that could never be released
  // would be a leak rather than a lease.
  let older = gfs_test::git(&h.repo_path, &["rev-parse", "v1.0"])
    .unwrap()
    .trim()
    .to_owned();
  gfs_test::git(&h.repo_path, &["update-ref", "refs/heads/main", &older]).unwrap();
  gfs_test::git(&h.repo_path, &["update-ref", "-d", "refs/heads/feature"]).unwrap();
  gfs_test::git(&h.repo_path, &["tag", "-d", "v2.0"]).unwrap();
  gfs_test::git(&h.repo_path, &["tag", "-d", "tree-tag"]).unwrap();
  gfs_test::git(
    &h.repo_path,
    &["-c", "gc.reflogExpire=now", "gc", "-q", "--prune=now"],
  )
  .unwrap();
  h.registry.evict(&h.repo_id);
  assert!(h.repo().read_commit(&grant.commit).is_err());
}

#[tokio::test]
async fn an_overdue_lease_is_reported_in_grace_rather_than_expired() {
  // The distinction ADR 0006's grace interval exists to make. Reported so an
  // operator can act before a live workspace is destroyed.
  let mut policy = LeasePolicy::adr_0006();
  policy.initial_ttl = std::time::Duration::ZERO;
  let h = Harness::with_policy(policy);
  let grant = h.create().await;

  // The TTL clamps to at least one second, and the catalog stores whole seconds,
  // so a 1.1s wait can land on exactly `expires_at` -- which is still unexpired.
  // Waiting past two whole seconds removes the granularity race rather than
  // making the comparison sloppy: `now <= expires_at` is the correct boundary and
  // should not be loosened to accommodate a test.
  tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
  let outcome = h.mounts.sweep().await.unwrap();
  assert_eq!(outcome.in_grace, vec![grant.mount_id.clone()]);
  assert!(outcome.expired.is_empty());
  // Still active, still protecting.
  assert_eq!(
    h.catalog.get_lease(&grant.mount_id).unwrap().unwrap().state,
    gfs_types::LeaseState::Active
  );
}

#[tokio::test]
async fn reconciliation_abandons_a_preparing_lease_and_removes_its_anchor() {
  // Simulates a crash between step 3 (persist PREPARING) and step 5 (persist
  // ACTIVE). No capability was ever issued for such a lease -- ACTIVE is a
  // precondition of returning one -- so abandoning it is always safe, and
  // completing it would create a mount nobody asked for that holds objects for
  // 30 minutes.
  let h = Harness::new();
  let mount_id = MountId::parse("m-crashed").unwrap();
  let anchor = gfs_types::revision::lease_anchor_ref(mount_id.as_str());
  let commit = {
    let sel = h.selector("main");
    h.repo().resolve(&sel).unwrap().commit
  };

  h.catalog
    .begin_lease(&mount_id, &h.repo_id, &commit, &h.subject, &anchor, 1800)
    .unwrap();
  // The crash landed *after* the anchor was written, the harder of the two cases.
  h.repo().create_lease_anchor(&anchor, &commit).unwrap();

  let outcome = h.mounts.reconcile().await.unwrap();
  assert_eq!(outcome.abandoned, vec![mount_id.clone()]);
  assert_eq!(h.repo().read_lease_anchor(&anchor).unwrap(), None);
  assert!(h.catalog.get_lease(&mount_id).unwrap().is_none());
}

#[tokio::test]
async fn reconciliation_repairs_an_active_lease_rather_than_abandoning_it() {
  // The asymmetry with PREPARING: an ACTIVE lease may be held by a live daemon
  // that is about to renew, so its anchor is recreated rather than removed.
  let h = Harness::new();
  let grant = h.create().await;
  let anchor = gfs_types::revision::lease_anchor_ref(grant.mount_id.as_str());

  h.repo().delete_lease_anchor(&anchor).unwrap();
  let outcome = h.mounts.reconcile().await.unwrap();

  assert_eq!(outcome.repaired, vec![grant.mount_id.clone()]);
  assert!(outcome.abandoned.is_empty());
  assert_eq!(
    h.repo().read_lease_anchor(&anchor).unwrap(),
    Some(grant.commit)
  );
  assert_eq!(
    h.catalog.get_lease(&grant.mount_id).unwrap().unwrap().state,
    gfs_types::LeaseState::Active
  );
}

#[tokio::test]
async fn reconciliation_is_idempotent_and_leaves_a_healthy_lease_alone() {
  let h = Harness::new();
  let grant = h.create().await;

  for _ in 0..3 {
    let outcome = h.mounts.reconcile().await.unwrap();
    assert_eq!(outcome, gfs_service::ReconcileOutcome::default());
  }
  assert_eq!(
    h.catalog.get_lease(&grant.mount_id).unwrap().unwrap().state,
    gfs_types::LeaseState::Active
  );
}

#[tokio::test]
async fn reconciliation_removes_an_anchor_with_no_catalog_record() {
  // The catalog row is written *before* the anchor, so no crash ordering produces
  // this state -- only a partial delete or manual intervention. Leaving it would
  // pin objects permanently with nothing recording why.
  let h = Harness::new();
  let commit = {
    let sel = h.selector("main");
    h.repo().resolve(&sel).unwrap().commit
  };
  let orphan = gfs_types::revision::lease_anchor_ref("m-orphan");
  h.repo().create_lease_anchor(&orphan, &commit).unwrap();

  let outcome = h.mounts.reconcile().await.unwrap();
  assert_eq!(outcome.orphaned_anchors, vec![orphan.clone()]);
  assert_eq!(h.repo().read_lease_anchor(&orphan).unwrap(), None);
}

#[tokio::test]
async fn concurrent_mount_creation_produces_distinct_leases_and_anchors() {
  // The repository lock serializes the critical sections; the test asserts the
  // observable consequence, which is that no two mounts collide on an ID or an
  // anchor even under contention.
  let h = Arc::new(Harness::new());
  let mut set = tokio::task::JoinSet::new();
  for _ in 0..12 {
    let h = Arc::clone(&h);
    set.spawn(async move {
      h.mounts
        .create_mount(&h.repo_id, h.selector("main"), &h.subject, None)
        .await
        .unwrap()
    });
  }
  let mut ids = std::collections::BTreeSet::new();
  while let Some(res) = set.join_next().await {
    let grant = res.unwrap();
    assert!(ids.insert(grant.mount_id.to_string()), "duplicate mount id");
  }
  assert_eq!(ids.len(), 12);
  assert_eq!(h.repo().reserved_refs().unwrap().len(), 12);
}

#[tokio::test]
async fn a_mount_cannot_be_created_on_a_quarantined_repository() {
  let h = Harness::new();
  h.catalog
    .set_repository_state(&h.repo_id, RepositoryState::Quarantined, Some("test"))
    .unwrap();

  let err = h
    .mounts
    .create_mount(&h.repo_id, h.selector("main"), &h.subject, None)
    .await
    .unwrap_err();
  // NOT_FOUND rather than a distinct "quarantined" status: M1's exit criteria
  // require that a caller cannot infer a repository's existence through status
  // differences, and operators see the real state through the admin surface.
  assert_eq!(err.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn the_reserved_namespace_cannot_be_mounted() {
  let h = Harness::new();
  let grant = h.create().await;
  let anchor = gfs_types::revision::lease_anchor_ref(grant.mount_id.as_str());

  // Both spellings, at the selector layer that every request passes through.
  for name in [anchor.as_str(), "gfs/mounts/m-1"] {
    assert_eq!(
      RevisionSelector::parse(name, HashAlgorithm::Sha1)
        .unwrap_err()
        .code,
      ErrorCode::ReservedNamespace
    );
  }
}

#[tokio::test]
async fn a_requested_ttl_is_capped_by_policy() {
  // `max_total_age` bounds a lease's whole life, so a first TTL longer than the
  // policy would make that bound unreachable.
  let h = Harness::new();
  let grant = h
    .mounts
    .create_mount(
      &h.repo_id,
      h.selector("main"),
      &h.subject,
      Some(u64::MAX / 2),
    )
    .await
    .unwrap();
  let lease = h.catalog.get_lease(&grant.mount_id).unwrap().unwrap();
  let granted = lease.expires_at - lease.created_at;
  assert!(
    granted <= LeasePolicy::adr_0006().initial_ttl.as_secs() as i64,
    "granted {granted}s exceeds the policy TTL"
  );
}

#[tokio::test]
async fn resolving_a_missing_branch_creates_no_lease_and_leaves_no_anchor() {
  // The failure path of step 1. Nothing durable happened, so nothing needs
  // cleaning up -- and the test confirms that rather than assuming it.
  let h = Harness::new();
  let err = h
    .mounts
    .create_mount(&h.repo_id, h.selector("no-such-branch"), &h.subject, None)
    .await
    .unwrap_err();
  assert_eq!(err.code, ErrorCode::NotFound);
  assert!(h.repo().reserved_refs().unwrap().is_empty());
  assert!(h.catalog.unreconciled_leases().unwrap().is_empty());
}
