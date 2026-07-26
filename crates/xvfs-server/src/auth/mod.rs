//! Authentication and authorization.
//!
//! PLAN.md M1.5 asks for four things, and they are separated here because they
//! have different lifetimes and different owners:
//!
//! * **authentication** ([`Authenticator`]) turns a credential into a subject;
//! * **repository authorization** ([`RepositoryPolicy`]) decides whether a subject
//!   may read a repository at all, uniformly across gRPC, HTTP, Git, and search;
//! * **object authorization** ([`Authorizer::authorize_commit`]) decides whether a
//!   subject may reach one *specific* commit, which is where the mount capability
//!   matters;
//! * **audit** ([`crate::audit`]) records the decision.
//!
//! # Scope of the object-authorization rule
//!
//! ADR 0002 and PLAN.md M1.5 are explicit and this is the single most
//! misunderstandable part of the design, so it is restated at the code:
//!
//! The mount-capability requirement is **scoped to the XVFS APIs** -- snapshot,
//! blob, and search -- and **does not extend to the Git gateway**. M0.3 measured
//! that stock `upload-pack` serves any object in the repository's object database
//! by object ID over protocol v2, regardless of `uploadpack.allowAnySHA1InWant`, so
//! a repository reader can always reach a lease-retained commit through Git. One
//! bare repository is one authorization domain.
//!
//! PLAN.md says it plainly: "Do not write an acceptance test that expects the Git
//! path to deny it." The test in this module's suite asserts the opposite -- that
//! the Git path *does* reach such an object -- so that the boundary is recorded as
//! measured behaviour rather than rediscovered as a surprise.
//!
//! # OIDC is a seam, not an implementation
//!
//! M1.5 says "integrate the chosen OIDC/workload identity". No provider has been
//! chosen -- ADR 0006 leaves it open -- and none is reachable from this
//! environment. [`Authenticator`] is therefore a trait with a development
//! static-token verifier, and a JWT/OIDC verifier is the declared seam. Everything
//! M1.5 actually gates on is provider-independent and implemented: uniform
//! repository authorization, the mount capability, object authorization for
//! unreachable commits, audit records, and the confused-deputy and traversal
//! cases. This is a recorded scope reduction, not an oversight.

pub mod capability;
pub mod policy;

use std::sync::Arc;

use xvfs_types::error::{ErrorCode, XvfsError};
use xvfs_types::{ObjectId, RepositoryId, SubjectId, Timestamp};

pub use capability::{BlobTicket, CapabilityKey, MountCapability};
pub use policy::{AllowList, RepositoryPolicy};

use crate::registry::Registry;

/// An authenticated caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
  pub subject: SubjectId,
  /// How the caller authenticated, for audit. Never the credential itself.
  pub method: &'static str,
}

/// Turns a presented credential into an [`Identity`].
pub trait Authenticator: Send + Sync + std::fmt::Debug {
  /// `bearer` is the raw token from the `Authorization` header or gRPC metadata.
  fn authenticate(&self, bearer: &str) -> Result<Identity, XvfsError>;
}

/// A development authenticator backed by a static token table.
///
/// For the local development stack and tests. It is deliberately obvious what this
/// is, rather than looking like a production identity provider: the name says
/// `Static`, and [`StaticTokens::is_production_safe`] returns false so a startup
/// check can refuse it.
#[derive(Debug, Default)]
pub struct StaticTokens {
  tokens: std::collections::HashMap<String, SubjectId>,
}

impl StaticTokens {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn with_token(mut self, token: &str, subject: SubjectId) -> Self {
    self.tokens.insert(token.to_owned(), subject);
    self
  }

  pub fn is_production_safe(&self) -> bool {
    false
  }
}

impl Authenticator for StaticTokens {
  fn authenticate(&self, bearer: &str) -> Result<Identity, XvfsError> {
    // Compared by lookup rather than by iterating and testing each token, so the
    // failure path does not depend on how many tokens are configured.
    self
      .tokens
      .get(bearer)
      .map(|subject| Identity {
        subject: subject.clone(),
        method: "static-token",
      })
      .ok_or_else(|| {
        // No detail about *why*. "Unknown token" and "malformed token" are the
        // same answer to the caller; the difference only helps an attacker.
        XvfsError::new(ErrorCode::Unauthenticated, "invalid credential")
      })
  }
}

/// What a caller presented as proof for a specific commit.
#[derive(Clone, Debug, Default)]
pub struct SnapshotAuthorization {
  /// The mount capability token, when one was presented.
  pub mount_capability: Option<String>,
}

/// The authorized right to read a specific commit.
///
/// Produced only by [`Authorizer::authorize_commit`]. Handlers take this rather
/// than a bare commit OID, so a code path that skipped authorization cannot type-check.
#[derive(Clone, Debug)]
pub struct CommitAccess {
  pub repository_id: RepositoryId,
  pub commit: ObjectId,
  pub subject: SubjectId,
  /// True when access rests on a mount capability rather than on the commit being
  /// reachable from a visible ref. Recorded for audit.
  pub via_capability: bool,
}

pub struct Authorizer {
  authenticator: Arc<dyn Authenticator>,
  policy: Arc<dyn RepositoryPolicy>,
  registry: Arc<Registry>,
  key: CapabilityKey,
  lease_policy: xvfs_types::LeasePolicy,
}

impl std::fmt::Debug for Authorizer {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Authorizer")
      .field("authenticator", &self.authenticator)
      .finish_non_exhaustive()
  }
}

impl Authorizer {
  pub fn new(
    authenticator: Arc<dyn Authenticator>,
    policy: Arc<dyn RepositoryPolicy>,
    registry: Arc<Registry>,
    key: CapabilityKey,
    lease_policy: xvfs_types::LeasePolicy,
  ) -> Self {
    Authorizer {
      authenticator,
      policy,
      registry,
      key,
      lease_policy,
    }
  }

  pub fn key(&self) -> &CapabilityKey {
    &self.key
  }

  pub fn authenticate(&self, bearer: &str) -> Result<Identity, XvfsError> {
    self.authenticator.authenticate(bearer)
  }

  /// Repository-level authorization, applied identically on every surface.
  ///
  /// Returns `NOT_FOUND` for both "no such repository" and "you may not read it".
  /// M1's exit criteria require that an unauthorized caller cannot infer existence
  /// "through status, timing within a defined tolerance, cache, or error
  /// differences", and a distinct `PERMISSION_DENIED` would answer the existence
  /// question directly.
  ///
  /// The policy is consulted **before** the catalog is read, which is the timing
  /// half of the same requirement: an unauthorized caller's request costs the same
  /// whether or not the repository exists, because nothing about the repository is
  /// looked up.
  pub fn authorize_repository(
    &self,
    subject: &SubjectId,
    repository_id: &RepositoryId,
  ) -> Result<(), XvfsError> {
    if !self.policy.may_read(subject, repository_id) {
      return Err(XvfsError::masked_denial("no such repository"));
    }
    // Only now does existence matter. `require_servable` also masks its denial, so
    // a quarantined repository is indistinguishable from an absent one.
    self.registry.require_servable(repository_id)?;
    Ok(())
  }

  /// Object-level authorization for one commit.
  ///
  /// Two ways to be allowed, in this order:
  ///
  /// 1. the commit is reachable from a currently visible ref -- ordinary access;
  /// 2. the caller presents a mount capability binding *this* subject, repository,
  ///    and commit.
  ///
  /// The subject binding is the point. Without it, any repository reader could
  /// present any leaked capability, and DESIGN.md section 7.1's guarantee --
  /// "repository access alone does not grant access to every commit retained for
  /// another mount" -- would be false.
  pub async fn authorize_commit(
    &self,
    subject: &SubjectId,
    repository_id: &RepositoryId,
    commit: &ObjectId,
    auth: &SnapshotAuthorization,
  ) -> Result<CommitAccess, XvfsError> {
    self.authorize_repository(subject, repository_id)?;

    let repo = self.registry.repository(repository_id)?;
    if repo.is_visible(commit.clone()).await? {
      return Ok(CommitAccess {
        repository_id: repository_id.clone(),
        commit: commit.clone(),
        subject: subject.clone(),
        via_capability: false,
      });
    }

    // Not reachable from any visible ref. Only a matching capability gets in.
    let Some(token) = auth.mount_capability.as_deref() else {
      // Masked: the alternative tells an unauthorized caller that a commit exists
      // and is lease-retained, which is a fact about another subject's job.
      return Err(XvfsError::masked_denial("no such commit"));
    };
    let cap = MountCapability::verify(&self.key, token, Timestamp::now())?;

    // Every binding is checked. A capability for another subject, another
    // repository, or another commit is not a capability for this request.
    if &cap.subject != subject || &cap.repository_id != repository_id || &cap.commit != commit {
      return Err(XvfsError::masked_denial("no such commit"));
    }

    Ok(CommitAccess {
      repository_id: repository_id.clone(),
      commit: commit.clone(),
      subject: subject.clone(),
      via_capability: true,
    })
  }

  /// Mint a blob ticket for an authorized commit.
  ///
  /// DESIGN.md section 7.3: blob authorization requires both repository access and
  /// proof that the blob is reachable from an allowed revision. The commit in the
  /// ticket is that proof, and it is only ever taken from a [`CommitAccess`] the
  /// authorizer produced -- so a ticket cannot be minted for a commit the caller
  /// was never allowed to read.
  pub fn issue_blob_ticket(
    &self,
    access: &CommitAccess,
    blob: &ObjectId,
    ttl: std::time::Duration,
  ) -> String {
    BlobTicket::issue(
      &self.key,
      &BlobTicket {
        subject: access.subject.clone(),
        repository_id: access.repository_id.clone(),
        commit: access.commit.clone(),
        blob: blob.clone(),
        expires_at: Timestamp::from_secs(Timestamp::now().secs + ttl.as_secs() as i64),
      },
    )
  }

  /// Verify a blob ticket presented on the immutable blob endpoint.
  pub fn verify_blob_ticket(
    &self,
    subject: &SubjectId,
    repository_id: &RepositoryId,
    blob: &ObjectId,
    token: &str,
  ) -> Result<BlobTicket, XvfsError> {
    let ticket = BlobTicket::verify(&self.key, token, Timestamp::now())?;
    if &ticket.subject != subject || &ticket.repository_id != repository_id || &ticket.blob != blob
    {
      // The confused-deputy case: a ticket minted for one subject or one blob must
      // not authorize another.
      return Err(XvfsError::masked_denial("no such blob"));
    }
    Ok(ticket)
  }

  /// Mint the mount capability for a freshly created mount.
  pub fn issue_mount_capability(
    &self,
    subject: &SubjectId,
    repository_id: &RepositoryId,
    grant: &xvfs_types::MountGrant,
  ) -> String {
    MountCapability::issue(
      &self.key,
      &MountCapability {
        subject: subject.clone(),
        repository_id: repository_id.clone(),
        commit: grant.commit.clone(),
        mount_id: grant.mount_id.clone(),
        expires_at: grant.lease_expiry,
      },
    )
  }

  /// Verify a capability presented for a lease operation such as renewal.
  ///
  /// Bound to the mount ID as well as the subject, so a capability for one mount
  /// cannot renew or release another -- including another job's.
  pub fn authorize_mount_operation(
    &self,
    subject: &SubjectId,
    mount_id: &xvfs_types::MountId,
    token: &str,
  ) -> Result<MountCapability, XvfsError> {
    // A capability expires *with* its lease, so a daemon renewing inside ADR
    // 0006's grace interval necessarily presents a just-expired token. The
    // tolerance is exactly that grace: shorter would make the grace unreachable,
    // longer would let an abandoned daemon keep a lease alive past the point the
    // policy says it should be reclaimed.
    let cap = MountCapability::verify_with_tolerance(
      &self.key,
      token,
      Timestamp::now(),
      self.lease_policy.renewal_grace,
    )?;
    if &cap.subject != subject || &cap.mount_id != mount_id {
      return Err(XvfsError::masked_denial("no such mount"));
    }
    Ok(cap)
  }
}
