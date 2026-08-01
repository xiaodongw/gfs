//! The gRPC `RepositoryService` implementation: the write half.
//!
//! Same handler shape as [`crate::service::snapshot`] -- authenticate, validate
//! into domain types, authorize, work, audit -- with one addition that only
//! applies here: every method is refused outright when the deployment has not
//! configured [`crate::ingest::IngestConfig`]. A server that was never told
//! where to put a mirror has no safe default, and inventing one would let a
//! caller write a bare repository wherever the process can reach.
//!
//! # Why the work runs on a blocking thread
//!
//! Ingest and push shell out to stock Git and wait. Running that on a Tokio
//! worker would park the whole runtime's thread for the duration of a network
//! fetch, which on a large repository is minutes. `spawn_blocking` is not a
//! detail here; without it one clone stalls every mount's blob reads.

use std::sync::Arc;
use std::time::Instant;

use gfs_git::{CommitSignature, TreeChange, TreeChangeKind};
use gfs_proto::convert;
use gfs_proto::v1::{self, repository_service_server::RepositoryService};
use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::revision::{self, RevisionSelector};
use gfs_types::{BytePath, ObjectId, RepositoryId};
use tonic::{Request, Response, Status};

use crate::auth::{Authorizer, Identity};
use crate::catalog::Catalog;
use crate::ingest::{self, IngestConfig};
use crate::observability::{self, RequestId};
use crate::registry::Registry;

pub struct RepositoryApi {
  pub catalog: Arc<Catalog>,
  pub registry: Arc<Registry>,
  pub authz: Arc<Authorizer>,
  pub ingest: Option<Arc<IngestConfig>>,
}

impl std::fmt::Debug for RepositoryApi {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("RepositoryApi")
      .field("ingest_enabled", &self.ingest.is_some())
      .finish_non_exhaustive()
  }
}

struct Ctx {
  #[allow(dead_code)]
  identity: Identity,
  request_id: RequestId,
  #[allow(dead_code)]
  started: Instant,
}

impl RepositoryApi {
  fn begin<T>(&self, request: &Request<T>) -> Result<Ctx, Status> {
    let md = request.metadata();
    let request_id = RequestId::from_client(
      md.get(observability::REQUEST_ID_KEY)
        .and_then(|v| v.to_str().ok()),
    );
    let bearer = md
      .get("authorization")
      .and_then(|v| v.to_str().ok())
      .and_then(|v| v.strip_prefix("Bearer "))
      .unwrap_or("");

    match self.authz.authenticate(bearer) {
      Ok(identity) => Ok(Ctx {
        identity,
        request_id,
        started: Instant::now(),
      }),
      Err(e) => {
        tracing::warn!(
          request_id = %request_id,
          credential = %gfs_types::redact::token_fingerprint(bearer),
          "authentication failed"
        );
        Err(convert::to_status(&e))
      }
    }
  }

  /// The ingest configuration, or a stated refusal.
  fn config(&self) -> Result<Arc<IngestConfig>, GfsError> {
    self.ingest.clone().ok_or_else(|| {
      GfsError::new(
        ErrorCode::FailedPrecondition,
        "this server does not host repositories on request; it serves only the \
         repositories it was started with. Start it with --repos-root to allow \
         `gfs clone`.",
      )
    })
  }
}

/// The wire's changes as tree changes, refusing what a tree cannot express.
fn to_tree_changes(changes: &[v1::FileChange]) -> Result<Vec<TreeChange>, GfsError> {
  let mut out = Vec::with_capacity(changes.len());
  for change in changes {
    let path = BytePath::new(change.path.clone());
    // The same rule the overlay enforces on the way in. Checked again here
    // because this is a *service* boundary and the client is not the only
    // possible caller.
    gfs_overlay_path_condition(&path)?;
    let kind = v1::ChangeKind::try_from(change.kind)
      .map_err(|_| GfsError::invalid(format!("unknown change kind {}", change.kind)))?;
    match kind {
      v1::ChangeKind::Deleted => out.push(TreeChange {
        path,
        kind: TreeChangeKind::Delete,
      }),
      v1::ChangeKind::Added | v1::ChangeKind::Modified => out.push(TreeChange {
        path,
        kind: TreeChangeKind::Upsert {
          mode: change.mode,
          content: change.content.clone(),
        },
      }),
      v1::ChangeKind::Unspecified => {
        return Err(GfsError::invalid(format!(
          "{} has no change kind",
          BytePath::new(change.path.clone()).escaped()
        )))
      }
    }
  }
  Ok(out)
}

/// Reject a path a commit must not carry.
///
/// `.git` anywhere in a path would produce a tree that stock Git refuses to
/// check out, and an absolute or `..`-bearing path would escape the repository.
fn gfs_overlay_path_condition(path: &BytePath) -> Result<(), GfsError> {
  if path.is_empty() {
    return Err(GfsError::invalid("a change with no path"));
  }
  for component in path.components() {
    if component == b".." || component == b"." || component.eq_ignore_ascii_case(b".git") {
      return Err(GfsError::invalid(format!(
        "{} is not a path a commit may carry",
        path.escaped()
      )));
    }
  }
  Ok(())
}

/// A non-empty string, or a stated fallback.
fn nonempty(value: &str, fallback: &str) -> String {
  let trimmed = value.trim();
  if trimmed.is_empty() {
    fallback.to_owned()
  } else {
    trimmed.to_owned()
  }
}

#[tonic::async_trait]
impl RepositoryService for RepositoryApi {
  async fn clone_repository(
    &self,
    request: Request<v1::CloneRepositoryRequest>,
  ) -> Result<Response<v1::CloneRepositoryResponse>, Status> {
    let ctx = self.begin(&request)?;
    let req = request.into_inner();

    let result = async {
      let config = self.config()?;
      let url = req.upstream_url.trim().to_owned();
      // Validated before anything runs, so a malformed URL is a clean
      // `InvalidArgument` rather than a subprocess failure several layers down.
      ingest::repository_id_for(&url)?;
      let directory = ingest::directory_for(&url)?;

      let credential = if req.credential.is_empty() {
        None
      } else {
        Some(req.credential.clone())
      };

      let catalog = Arc::clone(&self.catalog);
      let registry = Arc::clone(&self.registry);
      let outcome = tokio::task::spawn_blocking(move || {
        ingest::ingest(&catalog, &registry, &config, &url, credential.as_deref())
      })
      .await
      .map_err(|e| GfsError::internal(format!("the clone task did not finish: {e}")))??;

      Ok::<_, GfsError>(v1::CloneRepositoryResponse {
        repository_id: outcome.repository_id.as_str().to_owned(),
        created: outcome.created,
        default_branch: outcome.default_branch,
        directory,
        summary: outcome.summary,
      })
    }
    .await;

    match result {
      Ok(response) => {
        tracing::info!(
          request_id = %ctx.request_id,
          repository_id = %response.repository_id,
          created = response.created,
          "clone completed"
        );
        Ok(Response::new(response))
      }
      Err(e) => Err(convert::to_status(&e)),
    }
  }

  async fn create_branch(
    &self,
    request: Request<v1::CreateBranchRequest>,
  ) -> Result<Response<v1::CreateBranchResponse>, Status> {
    let ctx = self.begin(&request)?;
    let req = request.into_inner();

    let result = async {
      let id = RepositoryId::parse(&req.repository_id)?;
      self
        .authz
        .authorize_repository(&ctx.identity.subject, &id)?;

      if !revision::is_valid_branch_name(&req.branch) {
        return Err(GfsError::invalid(format!(
          "not a usable branch name: {:?}",
          req.branch
        )));
      }
      let repo = self.registry.repository(&id)?;

      // The starting commit. An empty selector means the repository's default
      // view, spelled the way every other caller spells it.
      let algorithm = self.registry.require_servable(&id)?.algorithm;
      let start = if req.start_point.trim().is_empty() {
        "HEAD"
      } else {
        req.start_point.trim()
      };
      let selector = RevisionSelector::parse(start, algorithm)?;
      let resolved = repo.resolve(selector).await?;

      let ref_name = revision::work_ref(ctx.identity.subject.as_str(), &req.branch);
      // `None` for the expected value: this creates, and creating over an
      // existing branch is a conflict rather than a reset. `gfs switch` without
      // `-c` is the operation that moves to an existing one.
      repo
        .update_work_ref(ref_name.clone(), resolved.commit.clone(), None)
        .await?;

      Ok::<_, GfsError>(v1::CreateBranchResponse {
        ref_name,
        commit_oid: resolved.commit.to_qualified(),
      })
    }
    .await;

    match result {
      Ok(response) => {
        tracing::info!(
          request_id = %ctx.request_id,
          ref_name = %response.ref_name,
          "branch created"
        );
        Ok(Response::new(response))
      }
      Err(e) => Err(convert::to_status(&e)),
    }
  }

  async fn commit_changes(
    &self,
    request: Request<v1::CommitChangesRequest>,
  ) -> Result<Response<v1::CommitChangesResponse>, Status> {
    let ctx = self.begin(&request)?;
    let req = request.into_inner();

    let result = async {
      let id = RepositoryId::parse(&req.repository_id)?;
      self
        .authz
        .authorize_repository(&ctx.identity.subject, &id)?;
      if !revision::is_valid_branch_name(&req.branch) {
        return Err(GfsError::invalid(format!(
          "not a usable branch name: {:?}",
          req.branch
        )));
      }
      if req.message.trim().is_empty() {
        return Err(GfsError::invalid("a commit needs a message"));
      }

      let algorithm = self.registry.require_servable(&id)?.algorithm;
      let base = ObjectId::parse_qualified(&req.base_commit_oid)?;
      if base.algorithm() != algorithm {
        return Err(GfsError::invalid("the base commit is the wrong algorithm"));
      }
      let repo = self.registry.repository(&id)?;
      let ref_name = revision::work_ref(ctx.identity.subject.as_str(), &req.branch);

      // The branch must still be where the workspace thinks it is. Read first so
      // a moved branch is refused *before* any object is written -- writing the
      // tree and commit anyway would leave unreachable objects behind on every
      // conflict.
      let head = repo.read_ref(ref_name.clone()).await?;
      let expected = match &head {
        Some(current) if *current == base => Some(base.clone()),
        Some(current) => {
          return Err(GfsError::new(
            ErrorCode::Conflict,
            format!(
              "{} has moved to {} since this workspace was pinned to {}; \
               refresh and commit again",
              req.branch,
              current.to_qualified(),
              base.to_qualified()
            ),
          ))
        }
        // The branch does not exist yet. Committing creates it, which is what
        // happens when a view committed before `gfs switch -c` ever ran.
        None => None,
      };

      let mut changes = to_tree_changes(&req.changes)?;
      // Directory deletions arrive as prefixes because the workspace has no base
      // tree to expand them against. Expanded here, where the tree is.
      for dir in &req.deleted_directories {
        let root = BytePath::new(dir.clone());
        let mut removed = Vec::new();
        repo
          .walk_paths_collect(base.clone(), root, &mut removed)
          .await?;
        changes.extend(removed.into_iter().map(|path| TreeChange {
          path,
          kind: TreeChangeKind::Delete,
        }));
      }

      let signature = CommitSignature {
        name: nonempty(&req.author_name, ctx.identity.subject.as_str()),
        email: nonempty(&req.author_email, "gfs@localhost"),
        when_secs: gfs_types::Timestamp::now().secs,
        offset_minutes: 0,
      };

      let tree = repo.write_tree(Some(base.clone()), changes).await?;
      let commit = repo
        .create_commit(
          tree.clone(),
          vec![base.clone()],
          signature.clone(),
          signature,
          req.message.clone(),
        )
        .await?;
      // The compare-and-swap. A concurrent committer that won between the read
      // above and here loses at this point rather than clobbering.
      repo
        .update_work_ref(ref_name.clone(), commit.clone(), expected)
        .await?;

      Ok::<_, GfsError>(v1::CommitChangesResponse {
        commit_oid: commit.to_qualified(),
        tree_oid: tree.to_qualified(),
        ref_name,
      })
    }
    .await;

    match result {
      Ok(response) => {
        tracing::info!(
          request_id = %ctx.request_id,
          ref_name = %response.ref_name,
          commit = %response.commit_oid,
          "commit created"
        );
        Ok(Response::new(response))
      }
      Err(e) => Err(convert::to_status(&e)),
    }
  }

  async fn push_branch(
    &self,
    request: Request<v1::PushBranchRequest>,
  ) -> Result<Response<v1::PushBranchResponse>, Status> {
    let ctx = self.begin(&request)?;
    let req = request.into_inner();

    let result = async {
      let config = self.config()?;
      let id = RepositoryId::parse(&req.repository_id)?;
      self
        .authz
        .authorize_repository(&ctx.identity.subject, &id)?;
      if !revision::is_valid_branch_name(&req.branch) {
        return Err(GfsError::invalid(format!(
          "not a usable branch name: {:?}",
          req.branch
        )));
      }
      let remote_branch = if req.remote_branch.trim().is_empty() {
        req.branch.clone()
      } else {
        req.remote_branch.trim().to_owned()
      };
      if !revision::is_valid_branch_name(&remote_branch) {
        return Err(GfsError::invalid(format!(
          "not a usable upstream branch name: {remote_branch:?}"
        )));
      }

      let record = self.registry.require_servable(&id)?;
      let Some(upstream_url) = record.upstream_url.clone() else {
        return Err(GfsError::new(
          ErrorCode::FailedPrecondition,
          "this repository has no upstream to push to; it was imported from a \
           local path rather than cloned",
        ));
      };

      // The caller's own work branch when they have one (the RPC commit flow
      // writes there), otherwise the real branch a git-native push landed on.
      // Checked before the subprocess runs, so "there is no such branch" is a
      // clean `NOT_FOUND` and not a `git push` error mentioning a ref name the
      // caller never typed.
      let repo = self.registry.repository(&id)?;
      let work_ref = revision::work_ref(ctx.identity.subject.as_str(), &req.branch);
      let branch_ref = format!("refs/heads/{}", req.branch);
      let local_ref = if repo.read_ref(work_ref.clone()).await?.is_some() {
        work_ref
      } else if repo.read_ref(branch_ref.clone()).await?.is_some() {
        branch_ref
      } else {
        return Err(GfsError::not_found(format!(
          "no branch {:?} on the gateway, and no work branch of that name for \
           this caller",
          req.branch
        )));
      };

      let credential = if req.credential.is_empty() {
        None
      } else {
        Some(req.credential.clone())
      };
      let repo_path = record.repo_path.clone();
      let force = req.force;
      let remote = remote_branch.clone();
      let local = local_ref.clone();
      let outcome = tokio::task::spawn_blocking(move || {
        crate::mirror::push(
          &repo_path,
          &upstream_url,
          &local,
          &remote,
          force,
          credential.as_deref(),
          &config.git_binary,
        )
      })
      .await
      .map_err(|e| GfsError::internal(format!("the push task did not finish: {e}")))??;

      Ok::<_, GfsError>(v1::PushBranchResponse {
        summary: outcome.summary,
        remote_ref: format!("refs/heads/{remote_branch}"),
      })
    }
    .await;

    match result {
      Ok(response) => {
        tracing::info!(
          request_id = %ctx.request_id,
          remote_ref = %response.remote_ref,
          "push completed"
        );
        Ok(Response::new(response))
      }
      Err(e) => Err(convert::to_status(&e)),
    }
  }
}
