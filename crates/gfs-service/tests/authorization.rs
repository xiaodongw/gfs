//! Authorization tests, including M1's exit criteria on existence inference and
//! the documented boundary that the Git path does *not* enforce object
//! authorization.

use std::sync::Arc;

use gfs_git::GitRepository;
use gfs_service::auth::{
  AllowList, Authorizer, CapabilityKey, MountCapability, SnapshotAuthorization, StaticTokens,
};
use gfs_service::catalog::repositories::{NewRepository, RepositoryState};
use gfs_service::{Catalog, MountManager, Registry, RepositoryLocks};
use gfs_types::error::ErrorCode;
use gfs_types::{
  DisplayName, HashAlgorithm, LeasePolicy, MountId, ObjectId, RepositoryId, RevisionSelector,
  SubjectId, Timestamp,
};

const OWNER_TOKEN: &str = "token-owner";
const STRANGER_TOKEN: &str = "token-stranger";

struct Harness {
  catalog: Arc<Catalog>,
  registry: Arc<Registry>,
  mounts: MountManager,
  authz: Authorizer,
  repo_id: RepositoryId,
  repo_path: std::path::PathBuf,
  owner: SubjectId,
  stranger: SubjectId,
  _tmp: tempfile::TempDir,
}

impl Harness {
  fn new() -> Self {
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

    let owner = SubjectId::parse("job-owner").unwrap();
    let stranger = SubjectId::parse("job-stranger").unwrap();

    let authenticator = Arc::new(
      StaticTokens::new()
        .with_token(OWNER_TOKEN, owner.clone())
        .with_token(STRANGER_TOKEN, stranger.clone()),
    );
    // Both subjects may read the repository. That is the interesting
    // configuration: object authorization has to hold *between* two subjects who
    // both have repository access.
    let policy = Arc::new(
      AllowList::new()
        .allow(&owner, &repo_id)
        .allow(&stranger, &repo_id),
    );
    let policy_lease = LeasePolicy::adr_0006();
    // The manager and the authorizer share one signing key: the manager mints
    // capabilities, the authorizer verifies them.
    let key = CapabilityKey::generate().unwrap();
    let authz = Authorizer::new(
      authenticator,
      policy,
      Arc::clone(&registry),
      key.clone(),
      policy_lease,
    );
    let mounts = MountManager::new(
      Arc::clone(&catalog),
      Arc::clone(&registry),
      Arc::new(RepositoryLocks::new()),
      policy_lease,
      key,
    );

    Harness {
      catalog,
      registry,
      mounts,
      authz,
      repo_id,
      repo_path,
      owner,
      stranger,
      _tmp: tmp,
    }
  }

  fn repo(&self) -> Arc<dyn GitRepository> {
    self.registry.blocking_repository(&self.repo_id).unwrap()
  }

  fn selector(&self, s: &str) -> RevisionSelector {
    RevisionSelector::parse(s, HashAlgorithm::Sha1).unwrap()
  }

  /// Mount `main`, then make its commit unreachable from every visible ref -- the
  /// force-push situation the mount capability exists for.
  async fn mount_then_force_push_away(&self) -> (gfs_types::MountGrant, String) {
    let grant = self
      .mounts
      .create_mount(&self.repo_id, self.selector("main"), &self.owner, None)
      .await
      .unwrap();
    // The capability the manager itself minted, not one re-issued by the test.
    // Signing inside `create_mount` is what makes "forgot to sign it"
    // unreachable, so the tests have to exercise that path rather than a parallel
    // one.
    let capability = grant.capability.clone();
    assert!(
      !capability.is_empty(),
      "create_mount must mint a capability"
    );

    let older = gfs_test::git(&self.repo_path, &["rev-parse", "v1.0"])
      .unwrap()
      .trim()
      .to_owned();
    gfs_test::git(&self.repo_path, &["update-ref", "refs/heads/main", &older]).unwrap();
    gfs_test::git(&self.repo_path, &["update-ref", "-d", "refs/heads/feature"]).unwrap();
    gfs_test::git(&self.repo_path, &["tag", "-d", "v2.0"]).unwrap();
    gfs_test::git(&self.repo_path, &["tag", "-d", "tree-tag"]).unwrap();
    self.registry.evict(&self.repo_id);

    assert!(
      !self.repo().is_visible(&grant.commit).unwrap(),
      "the setup must actually make the commit unreachable"
    );
    (grant, capability)
  }
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_or_malformed_credential_gets_one_answer() {
  // "Unknown token" and "malformed token" are the same answer to the caller. The
  // difference only helps an attacker enumerate valid shapes.
  let h = Harness::new();
  for bad in [
    "",
    "not-a-token",
    "token-owner ",
    "TOKEN-OWNER",
    "Bearer token-owner",
  ] {
    let err = h.authz.authenticate(bad).unwrap_err();
    assert_eq!(err.code, ErrorCode::Unauthenticated, "{bad:?}");
    assert_eq!(err.message, "invalid credential", "{bad:?}");
  }
  assert_eq!(h.authz.authenticate(OWNER_TOKEN).unwrap().subject, h.owner);
}

// ---------------------------------------------------------------------------
// Repository authorization and existence inference
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unauthorized_subject_cannot_distinguish_an_existing_repository_from_an_absent_one() {
  // M1's exit criterion on inference "through status, timing within a defined
  // tolerance, cache, or error differences". The status and error halves are
  // checked exactly here.
  let h = Harness::new();
  let outsider = SubjectId::parse("job-outsider").unwrap();

  let existing = h
    .authz
    .authorize_repository(&outsider, &h.repo_id)
    .unwrap_err();
  let absent = h
    .authz
    .authorize_repository(&outsider, &RepositoryId::parse("r-nonexistent").unwrap())
    .unwrap_err();

  assert_eq!(existing.code, ErrorCode::NotFound);
  assert_eq!(existing, absent, "the two answers must be byte-identical");
  assert_eq!(existing.code.http_status(), absent.code.http_status());
  assert_eq!(existing.code.grpc_code(), absent.code.grpc_code());
}

#[tokio::test]
async fn an_unauthorized_request_is_refused_before_the_repository_is_touched() {
  // The timing half, asserted structurally rather than by measuring a clock.
  //
  // The catalog is made to point at a path that does not exist, so *any* attempt
  // to open or stat the repository would fail with a different error. An
  // unauthorized caller still gets the ordinary masked NOT_FOUND, which proves the
  // policy ran before anything about the repository was looked up -- and therefore
  // that the cost of the rejection does not depend on the repository at all.
  let h = Harness::new();
  let broken = RepositoryId::parse("r-broken").unwrap();
  h.catalog
    .create_repository(&NewRepository {
      repository_id: broken.clone(),
      display_name: DisplayName::parse("acme/broken").unwrap(),
      repo_path: std::path::PathBuf::from("/nonexistent/gfs/definitely-not-here.git"),
      algorithm: HashAlgorithm::Sha1,
      upstream_url: None,
      credential_ref: None,
    })
    .unwrap();
  h.catalog
    .set_repository_state(&broken, RepositoryState::Active, None)
    .unwrap();

  let outsider = SubjectId::parse("job-outsider").unwrap();
  let err = h
    .authz
    .authorize_repository(&outsider, &broken)
    .unwrap_err();
  assert_eq!(err.code, ErrorCode::NotFound);
  assert_eq!(err.message, "no such repository");
}

#[tokio::test]
async fn a_quarantined_repository_is_indistinguishable_from_an_absent_one() {
  let h = Harness::new();
  h.catalog
    .set_repository_state(&h.repo_id, RepositoryState::Quarantined, Some("test"))
    .unwrap();

  let err = h
    .authz
    .authorize_repository(&h.owner, &h.repo_id)
    .unwrap_err();
  let absent = h
    .authz
    .authorize_repository(&h.owner, &RepositoryId::parse("r-absent").unwrap())
    .unwrap_err();
  assert_eq!(err, absent);
}

// ---------------------------------------------------------------------------
// Object authorization
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_visible_commit_needs_no_capability() {
  let h = Harness::new();
  let commit = h.repo().resolve(&h.selector("main")).unwrap().commit;
  let access = h
    .authz
    .authorize_commit(
      &h.owner,
      &h.repo_id,
      &commit,
      &SnapshotAuthorization::default(),
    )
    .await
    .unwrap();
  assert!(!access.via_capability);
}

#[tokio::test]
async fn an_unreachable_commit_needs_a_capability_and_the_owner_has_one() {
  let h = Harness::new();
  let (grant, capability) = h.mount_then_force_push_away().await;

  // Without a capability: masked, so a caller cannot learn that the commit exists
  // and is lease-retained -- which is a fact about another subject's job.
  let err = h
    .authz
    .authorize_commit(
      &h.owner,
      &h.repo_id,
      &grant.commit,
      &SnapshotAuthorization::default(),
    )
    .await
    .unwrap_err();
  assert_eq!(err.code, ErrorCode::NotFound);

  // With it: allowed, and marked as resting on the capability.
  let access = h
    .authz
    .authorize_commit(
      &h.owner,
      &h.repo_id,
      &grant.commit,
      &SnapshotAuthorization {
        mount_capability: Some(capability),
      },
    )
    .await
    .unwrap();
  assert!(access.via_capability);
}

#[tokio::test]
async fn repository_access_alone_does_not_reach_another_subjects_retained_commit() {
  // The exact guarantee in DESIGN.md section 7.1 and PLAN.md M1.5. The stranger
  // has full repository read access, so this can only be enforced by the
  // capability's subject binding.
  let h = Harness::new();
  let (grant, owner_capability) = h.mount_then_force_push_away().await;

  // The stranger can read the repository.
  h.authz
    .authorize_repository(&h.stranger, &h.repo_id)
    .unwrap();

  // But not the retained commit, with or without the owner's leaked capability.
  for auth in [
    SnapshotAuthorization::default(),
    SnapshotAuthorization {
      mount_capability: Some(owner_capability.clone()),
    },
  ] {
    let err = h
      .authz
      .authorize_commit(&h.stranger, &h.repo_id, &grant.commit, &auth)
      .await
      .unwrap_err();
    assert_eq!(
      err.code,
      ErrorCode::NotFound,
      "a stranger must not reach the retained commit"
    );
  }
}

#[tokio::test]
async fn a_capability_for_one_commit_does_not_authorize_another() {
  let h = Harness::new();
  let (grant, capability) = h.mount_then_force_push_away().await;

  // A different unreachable commit: the tree of the leased commit is not a commit,
  // so use a fabricated OID, which is equally unreachable.
  let other = ObjectId::from_hex(HashAlgorithm::Sha1, &"ab".repeat(20)).unwrap();
  let err = h
    .authz
    .authorize_commit(
      &h.owner,
      &h.repo_id,
      &other,
      &SnapshotAuthorization {
        mount_capability: Some(capability),
      },
    )
    .await
    .unwrap_err();
  assert_eq!(err.code, ErrorCode::NotFound);
  let _ = grant;
}

#[tokio::test]
async fn a_capability_for_one_repository_does_not_authorize_another() {
  // The confused-deputy case across repositories.
  let h = Harness::new();
  let (grant, _) = h.mount_then_force_push_away().await;

  let other_repo = RepositoryId::parse("r-other").unwrap();
  let forged = MountCapability::issue(
    h.authz.key(),
    &MountCapability {
      subject: h.owner.clone(),
      repository_id: other_repo,
      commit: grant.commit.clone(),
      mount_id: grant.mount_id.clone(),
      expires_at: Timestamp::from_secs(Timestamp::now().secs + 3600),
    },
  );

  let err = h
    .authz
    .authorize_commit(
      &h.owner,
      &h.repo_id,
      &grant.commit,
      &SnapshotAuthorization {
        mount_capability: Some(forged),
      },
    )
    .await
    .unwrap_err();
  assert_eq!(err.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn an_expired_capability_cannot_read() {
  // The read path uses no expiry tolerance: only renewal does.
  let h = Harness::new();
  let (grant, _) = h.mount_then_force_push_away().await;

  let stale = MountCapability::issue(
    h.authz.key(),
    &MountCapability {
      subject: h.owner.clone(),
      repository_id: h.repo_id.clone(),
      commit: grant.commit.clone(),
      mount_id: grant.mount_id.clone(),
      expires_at: Timestamp::from_secs(Timestamp::now().secs - 3600),
    },
  );

  let err = h
    .authz
    .authorize_commit(
      &h.owner,
      &h.repo_id,
      &grant.commit,
      &SnapshotAuthorization {
        mount_capability: Some(stale),
      },
    )
    .await
    .unwrap_err();
  // Reported as expired rather than masked: the caller is the legitimate owner and
  // needs to know to renew, and the token itself already proved they knew the
  // commit exists.
  assert_eq!(err.code, ErrorCode::Expired);
}

// ---------------------------------------------------------------------------
// Blob tickets
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_blob_ticket_is_bound_to_its_subject_repository_and_blob() {
  let h = Harness::new();
  let commit = h.repo().resolve(&h.selector("main")).unwrap().commit;
  let access = h
    .authz
    .authorize_commit(
      &h.owner,
      &h.repo_id,
      &commit,
      &SnapshotAuthorization::default(),
    )
    .await
    .unwrap();
  let blob = h
    .repo()
    .entry(&commit, &gfs_types::BytePath::new("README.md"))
    .unwrap()
    .unwrap()
    .oid;

  let ticket = h
    .authz
    .issue_blob_ticket(&access, &blob, std::time::Duration::from_secs(300));

  // The owner, for this blob, in this repository: allowed.
  h.authz
    .verify_blob_ticket(&h.owner, &h.repo_id, &blob, &ticket)
    .unwrap();

  // Another subject: refused, even though it holds a valid signature.
  assert_eq!(
    h.authz
      .verify_blob_ticket(&h.stranger, &h.repo_id, &blob, &ticket)
      .unwrap_err()
      .code,
    ErrorCode::NotFound
  );
  // Another blob: refused.
  let other = ObjectId::from_hex(HashAlgorithm::Sha1, &"cd".repeat(20)).unwrap();
  assert_eq!(
    h.authz
      .verify_blob_ticket(&h.owner, &h.repo_id, &other, &ticket)
      .unwrap_err()
      .code,
    ErrorCode::NotFound
  );
  // Another repository: refused.
  assert_eq!(
    h.authz
      .verify_blob_ticket(
        &h.owner,
        &RepositoryId::parse("r-other").unwrap(),
        &blob,
        &ticket
      )
      .unwrap_err()
      .code,
    ErrorCode::NotFound
  );
}

// ---------------------------------------------------------------------------
// Mount operations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_capability_cannot_renew_or_release_another_mount() {
  let h = Harness::new();
  let a = h
    .mounts
    .create_mount(&h.repo_id, h.selector("main"), &h.owner, None)
    .await
    .unwrap();
  let b = h
    .mounts
    .create_mount(&h.repo_id, h.selector("main"), &h.owner, None)
    .await
    .unwrap();
  let cap_a = h.authz.issue_mount_capability(&h.owner, &h.repo_id, &a);

  h.authz
    .authorize_mount_operation(&h.owner, &a.mount_id, &cap_a)
    .unwrap();
  assert_eq!(
    h.authz
      .authorize_mount_operation(&h.owner, &b.mount_id, &cap_a)
      .unwrap_err()
      .code,
    ErrorCode::NotFound,
    "a capability for one mount must not operate on another"
  );
  // And not by another subject either.
  assert_eq!(
    h.authz
      .authorize_mount_operation(&h.stranger, &a.mount_id, &cap_a)
      .unwrap_err()
      .code,
    ErrorCode::NotFound
  );
}

#[tokio::test]
async fn a_capability_that_lapsed_inside_the_grace_interval_can_still_renew() {
  // A capability expires *with* its lease, so a daemon renewing during ADR 0006's
  // grace necessarily presents a just-expired token. Refusing it would make the
  // grace interval unreachable, defeating the mechanism that exists so a transient
  // renewal failure does not destroy a live workspace.
  let h = Harness::new();
  let policy = LeasePolicy::adr_0006();
  let mount_id = MountId::parse("m-grace").unwrap();
  let commit = h.repo().resolve(&h.selector("main")).unwrap().commit;

  let lapsed_within_grace = MountCapability::issue(
    h.authz.key(),
    &MountCapability {
      subject: h.owner.clone(),
      repository_id: h.repo_id.clone(),
      commit: commit.clone(),
      mount_id: mount_id.clone(),
      expires_at: Timestamp::from_secs(Timestamp::now().secs - 60),
    },
  );
  h.authz
    .authorize_mount_operation(&h.owner, &mount_id, &lapsed_within_grace)
    .expect("a token lapsed inside grace must still renew");

  // Past the grace interval, it does not.
  let lapsed_past_grace = MountCapability::issue(
    h.authz.key(),
    &MountCapability {
      subject: h.owner.clone(),
      repository_id: h.repo_id.clone(),
      commit,
      mount_id: mount_id.clone(),
      expires_at: Timestamp::from_secs(
        Timestamp::now().secs - policy.renewal_grace.as_secs() as i64 - 60,
      ),
    },
  );
  assert_eq!(
    h.authz
      .authorize_mount_operation(&h.owner, &mount_id, &lapsed_past_grace)
      .unwrap_err()
      .code,
    ErrorCode::Expired
  );
}

// ---------------------------------------------------------------------------
// The documented Git-path boundary
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_git_path_does_reach_a_lease_retained_commit_as_adr_0002_records() {
  // **This test asserts a limitation, on purpose.**
  //
  // ADR 0002 measured that stock `upload-pack` serves any object in a repository's
  // object database by object ID over protocol v2, regardless of
  // `uploadpack.allowAnySHA1InWant`. PLAN.md M1.5 is explicit: the
  // mount-capability rule is scoped to the GFS APIs and "does not extend to the
  // Git gateway... Do not write an acceptance test that expects the Git path to
  // deny it."
  //
  // So this checks the opposite direction. It exists so the boundary is recorded as
  // measured behaviour that a future change would have to consciously alter, rather
  // than being rediscovered as a surprise -- and so nobody adds the inverted
  // assertion later and "fixes" a design decision.
  let h = Harness::new();
  let (grant, _) = h.mount_then_force_push_away().await;

  // The GFS API denies the stranger, as the tests above establish.
  assert_eq!(
    h.authz
      .authorize_commit(
        &h.stranger,
        &h.repo_id,
        &grant.commit,
        &SnapshotAuthorization::default()
      )
      .await
      .unwrap_err()
      .code,
    ErrorCode::NotFound
  );

  // Git does not, and cannot be made to by hiding the ref.
  let dir = tempfile::tempdir().unwrap();
  let dest = dir.path().join("fetched.git");
  gfs_test::git(
    dir.path(),
    &["init", "-q", "--bare", dest.to_str().unwrap()],
  )
  .unwrap();

  let fetched = gfs_test::git(
    &dest,
    &[
      "-c",
      "protocol.version=2",
      "fetch",
      "--no-tags",
      "--upload-pack",
      // Even with the reserved namespace hidden and arbitrary wants left disabled
      // -- the protected configuration M5.3 applies -- the object is served.
      "git -c uploadpack.hideRefs=refs/gfs/ -c uploadpack.allowAnySHA1InWant=false upload-pack",
      h.repo_path.to_str().unwrap(),
      &grant.commit.to_hex(),
    ],
  );

  assert!(
    fetched.is_ok(),
    "ADR 0002 measured that protocol v2 serves this object; if this now fails, the \
     measurement has changed and ADR 0002 plus PLAN.md M1.5 need revisiting rather \
     than this test being deleted: {:?}",
    fetched.err()
  );
  let present = gfs_test::git(&dest, &["cat-file", "-t", &grant.commit.to_hex()]).unwrap();
  assert_eq!(present.trim(), "commit");
}
