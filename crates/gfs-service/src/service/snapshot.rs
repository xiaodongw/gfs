//! The gRPC `SnapshotService` implementation.
//!
//! Every handler has the same shape, and the order of the steps is the security
//! property:
//!
//! 1. authenticate the caller;
//! 2. validate every field into a domain type -- so nothing unvalidated reaches
//!    libgit2 or the catalog;
//! 3. authorize (repository, then the specific commit);
//! 4. do the work;
//! 5. audit, and record the metric.
//!
//! Step 3 produces a [`CommitAccess`], and the read helpers take one rather than a
//! bare commit OID. A handler that skipped authorization therefore does not
//! type-check, which is a stronger guarantee than a review checklist.

use std::sync::Arc;
use std::time::Instant;

use gfs_proto::convert;
use gfs_proto::v1::{self, snapshot_service_server::SnapshotService};
use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::{
  limits, BytePath, EntryKind, HashAlgorithm, MountId, ObjectId, RepositoryId, RevisionSelector,
};
use tonic::{Request, Response, Status};

use crate::audit::{self, Action, AuditRecord};
use crate::auth::{Authorizer, CommitAccess, Identity, SnapshotAuthorization};
use crate::catalog::Catalog;
use crate::mounts::MountManager;
use crate::observability::{self, RequestId};
use crate::registry::Registry;

/// How long a blob ticket is valid.
///
/// Long enough for a FUSE client to finish an open-and-read after a metadata
/// lookup, short enough that a leaked ticket is not a lasting grant. DESIGN.md
/// section 7.3 calls it "short-lived" without fixing a number.
const BLOB_TICKET_TTL: std::time::Duration = std::time::Duration::from_secs(300);

pub struct SnapshotApi {
  pub registry: Arc<Registry>,
  pub catalog: Arc<Catalog>,
  pub mounts: Arc<MountManager>,
  pub authz: Arc<Authorizer>,
  pub search: Arc<crate::search::IndexManager>,
}

impl std::fmt::Debug for SnapshotApi {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("SnapshotApi").finish_non_exhaustive()
  }
}

/// Per-request context: who is calling, and under what identifier.
struct Ctx {
  identity: Identity,
  request_id: RequestId,
  started: Instant,
}

impl SnapshotApi {
  /// Authenticate and establish the request context.
  ///
  /// The bearer token is read from gRPC metadata and never logged; only its
  /// fingerprint would be, and only on an authentication failure where correlating
  /// repeated attempts is useful.
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

  /// Finish a request: audit, record the metric, and map the error.
  fn finish<T>(
    &self,
    action: Action,
    ctx: &Ctx,
    record: AuditRecord<'_>,
    result: Result<T, GfsError>,
  ) -> Result<Response<T>, Status> {
    let elapsed = ctx.started.elapsed();
    match result {
      Ok(value) => {
        audit::success(action, &record);
        observability::record_request(action.as_str(), None, elapsed);
        Ok(Response::new(value))
      }
      Err(e) => {
        audit::failure(action, &record, e.code);
        observability::record_request(action.as_str(), Some(e.code), elapsed);
        Err(convert::to_status(&e))
      }
    }
  }

  fn algorithm(&self, repository_id: &RepositoryId) -> Result<HashAlgorithm, GfsError> {
    // Read through the registry so the repository's servability is checked here
    // too; a caller must not learn the hash algorithm of a repository it cannot
    // read.
    Ok(self.registry.require_servable(repository_id)?.algorithm)
  }

  /// Validate a repository ID and authorize the subject against it.
  ///
  /// Authorization first, then the algorithm lookup, so an unauthorized caller
  /// never causes a catalog read (see `Authorizer::authorize_repository`).
  async fn repo_context(
    &self,
    ctx: &Ctx,
    repository_id: &str,
  ) -> Result<(RepositoryId, HashAlgorithm), GfsError> {
    let id = RepositoryId::parse(repository_id)?;
    self
      .authz
      .authorize_repository(&ctx.identity.subject, &id)?;
    let algorithm = self.algorithm(&id)?;
    Ok((id, algorithm))
  }

  /// Validate and authorize a commit-scoped request.
  async fn commit_context(
    &self,
    ctx: &Ctx,
    repository_id: &str,
    commit_oid: &str,
    auth: Option<v1::SnapshotAuthorization>,
  ) -> Result<(CommitAccess, HashAlgorithm), GfsError> {
    let (id, algorithm) = self.repo_context(ctx, repository_id).await?;
    let commit = convert::try_oid(commit_oid, algorithm, "commit_oid")?;
    let auth = SnapshotAuthorization {
      mount_capability: auth.map(|a| a.mount_capability).filter(|s| !s.is_empty()),
    };
    let access = self
      .authz
      .authorize_commit(&ctx.identity.subject, &id, &commit, &auth)
      .await?;
    Ok((access, algorithm))
  }

  /// The snapshot time for a commit, cataloging it if this is the first sight.
  ///
  /// `ResolveRevision` and `GetCommit` can both be the first API call to mention a
  /// commit, so both must be able to establish the sanitized time rather than
  /// depending on `CreateMount` having run.
  async fn snapshot_time(
    &self,
    repository_id: &RepositoryId,
    commit: &ObjectId,
    committer_time: gfs_types::Timestamp,
  ) -> Result<gfs_types::Timestamp, GfsError> {
    let catalog = Arc::clone(&self.catalog);
    let repository_id = repository_id.clone();
    let commit = commit.clone();
    tokio::task::spawn_blocking(move || {
      catalog.catalog_commit(&repository_id, &commit, committer_time)
    })
    .await
    .map_err(crate::util::join_error)?
  }

  /// Mint a blob ticket for an entry, when the caller asked for one.
  fn attach_ticket(
    &self,
    access: &CommitAccess,
    mut entry: gfs_types::TreeEntryInfo,
    wanted: bool,
  ) -> gfs_types::TreeEntryInfo {
    if wanted && entry.kind.has_blob_content() {
      entry.blob_ticket = Some(
        self
          .authz
          .issue_blob_ticket(access, &entry.oid, BLOB_TICKET_TTL),
      );
    }
    entry
  }
}

/// Resolve a client's requested page size against a default and a ceiling.
///
/// Clamped rather than rejected, for the reason `list_directory` gives: a client
/// that asks for too much should get a smaller page and a way to continue, not an
/// error it has to learn to handle. Zero means "server default".
fn page_limit(requested: u32, default: usize, max: usize) -> usize {
  if requested == 0 {
    default
  } else {
    (requested as usize).min(max)
  }
}

#[tonic::async_trait]
impl SnapshotService for SnapshotApi {
  async fn resolve_revision(
    &self,
    request: Request<v1::ResolveRevisionRequest>,
  ) -> Result<Response<v1::ResolveRevisionResponse>, Status> {
    let ctx = self.begin(&request)?;
    let req = request.into_inner();

    let result = async {
      let (id, algorithm) = self.repo_context(&ctx, &req.repository_id).await?;
      // The closed selector grammar. A `revparse` expression never reaches libgit2.
      let selector = RevisionSelector::parse(&req.revision_selector, algorithm)?;
      let repo = self.registry.repository(&id)?;
      let mut resolved = repo.resolve(selector).await?;

      // The raw committer time from the repository layer is replaced by the
      // cataloged sanitized one. The API never returns an unsanitized timestamp:
      // ADR 0006 makes that the base filesystem clock, and a future-dated commit
      // would otherwise become a future-dated file.
      resolved.snapshot_time = self
        .snapshot_time(&id, &resolved.commit, resolved.snapshot_time)
        .await?;
      resolved.ref_version = {
        let catalog = Arc::clone(&self.catalog);
        let id = id.clone();
        let ref_name = resolved.ref_name.clone();
        tokio::task::spawn_blocking(move || catalog.ref_version(&id, ref_name.as_deref()))
          .await
          .map_err(crate::util::join_error)??
      };
      Ok((id, resolved))
    }
    .await;

    match result {
      Ok((id, resolved)) => {
        // The response is built before the audit record borrows, because
        // `From<ResolvedRevision>` consumes the value.
        let commit = resolved.commit.clone();
        let response = v1::ResolveRevisionResponse::from(resolved);
        self.finish(
          Action::ResolveRevision,
          &ctx,
          AuditRecord {
            subject: Some(&ctx.identity.subject),
            repository_id: Some(&id),
            commit: Some(&commit),
            request_id: Some(ctx.request_id.as_str()),
            ..Default::default()
          },
          Ok(response),
        )
      }
      Err(e) => self.finish(
        Action::ResolveRevision,
        &ctx,
        AuditRecord {
          subject: Some(&ctx.identity.subject),
          request_id: Some(ctx.request_id.as_str()),
          ..Default::default()
        },
        Err(e),
      ),
    }
  }

  async fn get_commit(
    &self,
    request: Request<v1::GetCommitRequest>,
  ) -> Result<Response<v1::GetCommitResponse>, Status> {
    let ctx = self.begin(&request)?;
    let req = request.into_inner();

    let result = async {
      let (access, _) = self
        .commit_context(&ctx, &req.repository_id, &req.commit_oid, req.authorization)
        .await?;
      let repo = self.registry.repository(&access.repository_id)?;
      let mut meta = repo.read_commit(access.commit.clone()).await?;
      meta.snapshot_time = self
        .snapshot_time(&access.repository_id, &access.commit, meta.snapshot_time)
        .await?;
      Ok((access, meta))
    }
    .await;

    match result {
      Ok((access, meta)) => {
        let response = v1::GetCommitResponse::from(meta);
        self.finish(
          Action::GetCommit,
          &ctx,
          AuditRecord {
            subject: Some(&access.subject),
            repository_id: Some(&access.repository_id),
            commit: Some(&access.commit),
            via_capability: access.via_capability,
            request_id: Some(ctx.request_id.as_str()),
            ..Default::default()
          },
          Ok(response),
        )
      }
      Err(e) => self.finish(
        Action::GetCommit,
        &ctx,
        AuditRecord {
          subject: Some(&ctx.identity.subject),
          request_id: Some(ctx.request_id.as_str()),
          ..Default::default()
        },
        Err(e),
      ),
    }
  }

  async fn get_entry(
    &self,
    request: Request<v1::GetEntryRequest>,
  ) -> Result<Response<v1::GetEntryResponse>, Status> {
    let ctx = self.begin(&request)?;
    let req = request.into_inner();
    let path = BytePath::new(req.path.clone());

    let result = async {
      let (access, _) = self
        .commit_context(&ctx, &req.repository_id, &req.commit_oid, req.authorization)
        .await?;
      let path = convert::try_path(req.path)?;
      let repo = self.registry.repository(&access.repository_id)?;
      let entry = repo
        .entry(access.commit.clone(), path)
        .await?
        // A path absent from the commit is `NOT_FOUND`, not an empty success: a
        // negative lookup is a normal, frequent FUSE result and a distinct status
        // lets the client cache it as negative without inspecting the body.
        .ok_or_else(|| GfsError::not_found("no such path in this commit"))?;
      let entry = self.attach_ticket(&access, entry, req.want_blob_ticket);
      Ok((access, entry))
    }
    .await;

    match result {
      Ok((access, entry)) => {
        let response = v1::GetEntryResponse {
          entry: Some(v1::TreeEntry::from(entry)),
          commit_oid: access.commit.to_qualified(),
        };
        self.finish(
          Action::GetEntry,
          &ctx,
          AuditRecord {
            subject: Some(&access.subject),
            repository_id: Some(&access.repository_id),
            commit: Some(&access.commit),
            path: Some(&path),
            via_capability: access.via_capability,
            request_id: Some(ctx.request_id.as_str()),
            ..Default::default()
          },
          Ok(response),
        )
      }
      Err(e) => self.finish(
        Action::GetEntry,
        &ctx,
        AuditRecord {
          subject: Some(&ctx.identity.subject),
          path: Some(&path),
          request_id: Some(ctx.request_id.as_str()),
          ..Default::default()
        },
        Err(e),
      ),
    }
  }

  async fn list_directory(
    &self,
    request: Request<v1::ListDirectoryRequest>,
  ) -> Result<Response<v1::ListDirectoryResponse>, Status> {
    let ctx = self.begin(&request)?;
    let req = request.into_inner();
    let path = BytePath::new(req.path.clone());

    let result = async {
      let (access, _) = self
        .commit_context(&ctx, &req.repository_id, &req.commit_oid, req.authorization)
        .await?;
      let path = convert::try_path(req.path)?;
      // Clamped, not rejected: a client asking for too much gets a smaller page and
      // a continuation token, which is a better outcome than an error it has to
      // learn to handle. Zero means "server default".
      let limit = if req.page_size == 0 {
        limits::DEFAULT_DIRECTORY_PAGE_SIZE
      } else {
        (req.page_size as usize).min(limits::MAX_DIRECTORY_PAGE_SIZE)
      };
      let after = (!req.page_token.is_empty()).then_some(req.page_token);

      let repo = self.registry.repository(&access.repository_id)?;
      let page = repo
        .list_directory(access.commit.clone(), path, after, limit)
        .await?;
      let entries = page
        .entries
        .into_iter()
        .map(|e| self.attach_ticket(&access, e, req.want_blob_tickets))
        .map(v1::TreeEntry::from)
        .collect();
      Ok((
        access,
        v1::ListDirectoryResponse {
          entries,
          next_page_token: page.next_page_token.unwrap_or_default(),
          commit_oid: String::new(),
        },
      ))
    }
    .await;

    match result {
      Ok((access, mut response)) => {
        response.commit_oid = access.commit.to_qualified();
        self.finish(
          Action::ListDirectory,
          &ctx,
          AuditRecord {
            subject: Some(&access.subject),
            repository_id: Some(&access.repository_id),
            commit: Some(&access.commit),
            path: Some(&path),
            via_capability: access.via_capability,
            request_id: Some(ctx.request_id.as_str()),
            ..Default::default()
          },
          Ok(response),
        )
      }
      Err(e) => self.finish(
        Action::ListDirectory,
        &ctx,
        AuditRecord {
          subject: Some(&ctx.identity.subject),
          path: Some(&path),
          request_id: Some(ctx.request_id.as_str()),
          ..Default::default()
        },
        Err(e),
      ),
    }
  }

  async fn batch_get_entry(
    &self,
    request: Request<v1::BatchGetEntryRequest>,
  ) -> Result<Response<v1::BatchGetEntryResponse>, Status> {
    let ctx = self.begin(&request)?;
    let req = request.into_inner();

    let result = async {
      let (access, _) = self
        .commit_context(&ctx, &req.repository_id, &req.commit_oid, req.authorization)
        .await?;
      if req.paths.len() > limits::MAX_BATCH_ENTRIES {
        return Err(GfsError::new(
          ErrorCode::ResourceLimit,
          format!(
            "a batch may contain at most {} paths",
            limits::MAX_BATCH_ENTRIES
          ),
        ));
      }

      // Paths are validated up front, and an invalid one becomes a per-item error
      // rather than failing the batch. The positional contract is what makes this
      // safe: the response has the same length and order as the request, so a
      // client can always match a result to its path.
      let mut validated: Vec<Result<BytePath, GfsError>> = Vec::with_capacity(req.paths.len());
      for p in req.paths {
        validated.push(convert::try_path(p));
      }
      let lookup: Vec<BytePath> = validated
        .iter()
        .filter_map(|v| v.as_ref().ok().cloned())
        .collect();

      let repo = self.registry.repository(&access.repository_id)?;
      let mut found = repo
        .batch_entries(access.commit.clone(), lookup)
        .await?
        .into_iter();

      let mut results = Vec::with_capacity(validated.len());
      for v in validated {
        let item = match v {
          Err(e) => v1::EntryResult {
            result: Some(v1::entry_result::Result::Error(v1::ErrorDetail::from(&e))),
          },
          Ok(_) => match found.next() {
            Some(Ok(Some(entry))) => v1::EntryResult {
              result: Some(v1::entry_result::Result::Entry(v1::TreeEntry::from(
                self.attach_ticket(&access, entry, req.want_blob_tickets),
              ))),
            },
            Some(Ok(None)) => v1::EntryResult {
              result: Some(v1::entry_result::Result::Error(v1::ErrorDetail::from(
                &GfsError::not_found("no such path in this commit"),
              ))),
            },
            Some(Err(e)) => v1::EntryResult {
              result: Some(v1::entry_result::Result::Error(v1::ErrorDetail::from(&e))),
            },
            None => v1::EntryResult {
              result: Some(v1::entry_result::Result::Error(v1::ErrorDetail::from(
                &GfsError::internal("missing batch result"),
              ))),
            },
          },
        };
        results.push(item);
      }

      Ok((
        access.clone(),
        v1::BatchGetEntryResponse {
          results,
          commit_oid: access.commit.to_qualified(),
        },
      ))
    }
    .await;

    match result {
      Ok((access, response)) => self.finish(
        Action::BatchGetEntry,
        &ctx,
        AuditRecord {
          subject: Some(&access.subject),
          repository_id: Some(&access.repository_id),
          commit: Some(&access.commit),
          via_capability: access.via_capability,
          request_id: Some(ctx.request_id.as_str()),
          ..Default::default()
        },
        Ok(response),
      ),
      Err(e) => self.finish(
        Action::BatchGetEntry,
        &ctx,
        AuditRecord {
          subject: Some(&ctx.identity.subject),
          request_id: Some(ctx.request_id.as_str()),
          ..Default::default()
        },
        Err(e),
      ),
    }
  }

  async fn log(
    &self,
    request: Request<v1::LogRequest>,
  ) -> Result<Response<v1::LogResponse>, Status> {
    let ctx = self.begin(&request)?;
    let req = request.into_inner();

    let result = async {
      let (access, _) = self
        .commit_context(&ctx, &req.repository_id, &req.commit_oid, req.authorization)
        .await?;
      let repo = self.registry.repository(&access.repository_id)?;
      let limit = page_limit(req.limit, limits::DEFAULT_LOG_LIMIT, limits::MAX_LOG_LIMIT);
      let (commits, has_more) = repo
        .log(access.commit.clone(), req.skip as usize, limit)
        .await?;
      Ok((access, commits, has_more))
    }
    .await;

    match result {
      Ok((access, commits, has_more)) => {
        let response = v1::LogResponse {
          commits: commits.into_iter().map(v1::LogCommit::from).collect(),
          has_more,
        };
        self.finish(
          Action::Log,
          &ctx,
          AuditRecord {
            subject: Some(&access.subject),
            repository_id: Some(&access.repository_id),
            commit: Some(&access.commit),
            via_capability: access.via_capability,
            request_id: Some(ctx.request_id.as_str()),
            ..Default::default()
          },
          Ok(response),
        )
      }
      Err(e) => self.finish(
        Action::Log,
        &ctx,
        AuditRecord {
          subject: Some(&ctx.identity.subject),
          request_id: Some(ctx.request_id.as_str()),
          ..Default::default()
        },
        Err(e),
      ),
    }
  }

  async fn find_paths(
    &self,
    request: Request<v1::FindPathsRequest>,
  ) -> Result<Response<v1::FindPathsResponse>, Status> {
    let ctx = self.begin(&request)?;
    let req = request.into_inner();

    let result = async {
      let (access, _) = self
        .commit_context(&ctx, &req.repository_id, &req.commit_oid, req.authorization)
        .await?;
      let repo = self.registry.repository(&access.repository_id)?;
      let scope = convert::try_path(req.scope)?;
      let limit = page_limit(
        req.limit,
        limits::DEFAULT_FIND_PATHS_LIMIT,
        limits::MAX_FIND_PATHS_LIMIT,
      );
      let includes: Vec<gfs_search::Glob> = req
        .include_globs
        .iter()
        .map(|g| gfs_search::Glob::new(g))
        .collect();
      let excludes: Vec<gfs_search::Glob> = req
        .exclude_globs
        .iter()
        .map(|g| gfs_search::Glob::new(g))
        .collect();

      // The whole subtree, then filtered here. The walk collects rather than
      // streams by design (see `RepoPool::walk_paths`), so the cost is one pass
      // over the scope no matter how selective the glob is — which is the trade
      // that makes this one round trip instead of one per directory.
      //
      // `walk_paths`, not `walk_tree`: the latter's corpus is the searchable one
      // and drops symlinks and gitlinks, which a filename query must report.
      let entries = repo.walk_paths(access.commit.clone(), scope).await?;

      let mut paths = Vec::new();
      let mut truncated = false;
      for (entry_path, mode) in entries {
        // Paging over a deterministic walk: resume strictly after the last path
        // the previous page emitted. Git's tree order is stable for a fixed
        // commit, so this cannot skip or repeat an entry.
        if !req.start_after_path.is_empty()
          && entry_path.as_bytes() <= req.start_after_path.as_slice()
        {
          continue;
        }
        let path = entry_path.as_bytes();
        if !includes.is_empty() && !includes.iter().any(|g| g.matches(path)) {
          continue;
        }
        if excludes.iter().any(|g| g.matches(path)) {
          continue;
        }
        if paths.len() == limit {
          truncated = true;
          break;
        }
        paths.push((entry_path, mode));
      }
      Ok((access, paths, truncated))
    }
    .await;

    match result {
      Ok((access, paths, truncated)) => {
        let response = v1::FindPathsResponse {
          next_start_after_path: if truncated {
            paths
              .last()
              .map(|(path, _)| path.as_bytes().to_vec())
              .unwrap_or_default()
          } else {
            Vec::new()
          },
          paths: paths
            .into_iter()
            .map(|(path, mode)| v1::FoundPath {
              kind: v1::EntryKind::from(EntryKind::from_mode(mode)) as i32,
              path: path.as_bytes().to_vec(),
              mode,
            })
            .collect(),
          truncated,
        };
        self.finish(
          Action::FindPaths,
          &ctx,
          AuditRecord {
            subject: Some(&access.subject),
            repository_id: Some(&access.repository_id),
            commit: Some(&access.commit),
            via_capability: access.via_capability,
            request_id: Some(ctx.request_id.as_str()),
            ..Default::default()
          },
          Ok(response),
        )
      }
      Err(e) => self.finish(
        Action::FindPaths,
        &ctx,
        AuditRecord {
          subject: Some(&ctx.identity.subject),
          request_id: Some(ctx.request_id.as_str()),
          ..Default::default()
        },
        Err(e),
      ),
    }
  }

  async fn prepare_snapshot(
    &self,
    request: Request<v1::PrepareSnapshotRequest>,
  ) -> Result<Response<v1::PrepareSnapshotResponse>, Status> {
    let ctx = self.begin(&request)?;
    let req = request.into_inner();

    let result = async {
      let (access, _) = self
        .commit_context(&ctx, &req.repository_id, &req.commit_oid, req.authorization)
        .await?;
      // Retained by policy only when the commit is still reachable from a visible
      // ref. An arbitrary commit a job asked about gets a TTL instead, which is
      // what stops one exploratory search pinning a manifest forever.
      let repo = self.registry.repository(&access.repository_id)?;
      let retained = repo
        .is_visible(access.commit.clone())
        .await
        .unwrap_or(false);
      let outcome = self
        .search
        .prepare(&access.repository_id, &access.commit, retained)
        .await?;
      Ok((access, outcome))
    }
    .await;

    match result {
      Ok((access, outcome)) => {
        let response = v1::PrepareSnapshotResponse {
          state: v1::SnapshotState::from(outcome.state()) as i32,
          operation_id: match &outcome {
            crate::search::PrepareOutcome::Building { operation_id } => Some(operation_id.clone()),
            _ => None,
          },
          failure_reason: match &outcome {
            crate::search::PrepareOutcome::Failed { reason } => Some(reason.clone()),
            _ => None,
          },
          commit_oid: access.commit.to_qualified(),
        };
        self.finish(
          Action::PrepareSnapshot,
          &ctx,
          AuditRecord {
            subject: Some(&access.subject),
            repository_id: Some(&access.repository_id),
            commit: Some(&access.commit),
            via_capability: access.via_capability,
            request_id: Some(ctx.request_id.as_str()),
            ..Default::default()
          },
          Ok(response),
        )
      }
      Err(e) => self.finish(
        Action::PrepareSnapshot,
        &ctx,
        AuditRecord {
          subject: Some(&ctx.identity.subject),
          request_id: Some(ctx.request_id.as_str()),
          ..Default::default()
        },
        Err(e),
      ),
    }
  }

  async fn create_mount(
    &self,
    request: Request<v1::CreateMountRequest>,
  ) -> Result<Response<v1::CreateMountResponse>, Status> {
    let ctx = self.begin(&request)?;
    let req = request.into_inner();

    let result = async {
      let (id, algorithm) = self.repo_context(&ctx, &req.repository_id).await?;
      let selector = RevisionSelector::parse(&req.revision_selector, algorithm)?;
      let ttl = (req.requested_ttl_seconds > 0).then_some(req.requested_ttl_seconds);
      let grant = self
        .mounts
        .create_mount(&id, selector, &ctx.identity.subject, ttl)
        .await?;
      Ok((id, grant))
    }
    .await;

    match result {
      Ok((id, grant)) => {
        let response = v1::CreateMountResponse {
          mount_id: grant.mount_id.to_string(),
          commit_oid: grant.commit.to_qualified(),
          tree_oid: grant.tree.to_qualified(),
          ref_name: grant.ref_name.clone(),
          snapshot_time: Some(grant.snapshot_time.into()),
          mount_capability: grant.capability.clone(),
          lease_expiry: Some(grant.lease_expiry.into()),
          heartbeat_interval_seconds: grant.heartbeat_interval.as_secs(),
        };
        self.finish(
          Action::CreateMount,
          &ctx,
          AuditRecord {
            subject: Some(&ctx.identity.subject),
            repository_id: Some(&id),
            commit: Some(&grant.commit),
            mount_id: Some(&grant.mount_id),
            request_id: Some(ctx.request_id.as_str()),
            ..Default::default()
          },
          Ok(response),
        )
      }
      Err(e) => self.finish(
        Action::CreateMount,
        &ctx,
        AuditRecord {
          subject: Some(&ctx.identity.subject),
          request_id: Some(ctx.request_id.as_str()),
          ..Default::default()
        },
        Err(e),
      ),
    }
  }

  async fn renew_mount(
    &self,
    request: Request<v1::RenewMountRequest>,
  ) -> Result<Response<v1::RenewMountResponse>, Status> {
    let ctx = self.begin(&request)?;
    let req = request.into_inner();

    let result = async {
      let mount_id = MountId::parse(&req.mount_id)?;
      // The capability authorizes the operation, bound to this subject *and* this
      // mount, so a capability for one mount cannot renew another.
      let cap = self.authz.authorize_mount_operation(
        &ctx.identity.subject,
        &mount_id,
        &req.mount_capability,
      )?;
      let lease = self.mounts.renew_mount(&mount_id, None).await?;

      // A fresh capability with the extended expiry. The old one stays valid until
      // its own expiry, so a renewal whose response was lost does not strand a
      // daemon holding the previous token.
      let capability = crate::auth::MountCapability::issue(
        self.authz.key(),
        &crate::auth::MountCapability {
          subject: cap.subject,
          repository_id: lease.repository_id.clone(),
          commit: lease.commit.clone(),
          mount_id: lease.mount_id.clone(),
          expires_at: gfs_types::Timestamp::from_secs(lease.expires_at),
        },
      );
      Ok((lease, capability))
    }
    .await;

    match result {
      Ok((lease, capability)) => {
        let response = v1::RenewMountResponse {
          mount_capability: capability,
          lease_expiry: Some(gfs_types::Timestamp::from_secs(lease.expires_at).into()),
        };
        self.finish(
          Action::RenewMount,
          &ctx,
          AuditRecord {
            subject: Some(&ctx.identity.subject),
            repository_id: Some(&lease.repository_id),
            commit: Some(&lease.commit),
            mount_id: Some(&lease.mount_id),
            request_id: Some(ctx.request_id.as_str()),
            ..Default::default()
          },
          Ok(response),
        )
      }
      Err(e) => {
        // A failed renewal is the signal ADR 0006 alerts on, so it is counted
        // rather than only logged.
        metrics::counter!(observability::metric::LEASE_RENEWAL_FAILURES).increment(1);
        self.finish(
          Action::RenewMount,
          &ctx,
          AuditRecord {
            subject: Some(&ctx.identity.subject),
            request_id: Some(ctx.request_id.as_str()),
            ..Default::default()
          },
          Err(e),
        )
      }
    }
  }

  async fn release_mount(
    &self,
    request: Request<v1::ReleaseMountRequest>,
  ) -> Result<Response<v1::ReleaseMountResponse>, Status> {
    let ctx = self.begin(&request)?;
    let req = request.into_inner();

    let result = async {
      let mount_id = MountId::parse(&req.mount_id)?;
      self.authz.authorize_mount_operation(
        &ctx.identity.subject,
        &mount_id,
        &req.mount_capability,
      )?;
      let lease = self.mounts.release_mount(&mount_id).await?;
      Ok(lease)
    }
    .await;

    match result {
      Ok(lease) => self.finish(
        Action::ReleaseMount,
        &ctx,
        AuditRecord {
          subject: Some(&ctx.identity.subject),
          repository_id: Some(&lease.repository_id),
          commit: Some(&lease.commit),
          mount_id: Some(&lease.mount_id),
          request_id: Some(ctx.request_id.as_str()),
          ..Default::default()
        },
        Ok(v1::ReleaseMountResponse {}),
      ),
      Err(e) => self.finish(
        Action::ReleaseMount,
        &ctx,
        AuditRecord {
          subject: Some(&ctx.identity.subject),
          request_id: Some(ctx.request_id.as_str()),
          ..Default::default()
        },
        Err(e),
      ),
    }
  }
}
