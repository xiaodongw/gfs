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

use gfs_proto::convert;
use gfs_proto::v1::{self, repository_service_server::RepositoryService};
use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::revision::{self, RevisionSelector};
use gfs_types::RepositoryId;
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
    _request: Request<v1::CommitChangesRequest>,
  ) -> Result<Response<v1::CommitChangesResponse>, Status> {
    Err(Status::unimplemented(
      "CommitChanges is not implemented yet",
    ))
  }

  async fn push_branch(
    &self,
    _request: Request<v1::PushBranchRequest>,
  ) -> Result<Response<v1::PushBranchResponse>, Status> {
    Err(Status::unimplemented("PushBranch is not implemented yet"))
  }
}
