//! The gRPC `SearchService` implementation.
//!
//! Same shape as `snapshot.rs` — authenticate, validate, authorize, work, audit —
//! with one addition that is the whole point of the service.
//!
//! # The terminal message is emitted on every path that reaches the stream
//!
//! A successful stream is zero or more matches followed by **exactly one**
//! completion. Every way of leaving the search — finishing, hitting a budget,
//! losing a backend partway — produces that message, because the client's
//! contract is that a stream without one is a failed search. The only case that
//! does not is a failure *before* the stream exists (authentication, a bad
//! pattern, an unprepared snapshot), which is a gRPC status: there is no stream
//! for the client to misread.
//!
//! That asymmetry is deliberate. A status is unambiguous; a truncated stream of
//! results is not, and the client treats it as failure.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tonic::{Request, Response, Status};
use xvfs_proto::convert;
use xvfs_proto::v1::{self, search_service_server::SearchService};
use xvfs_search::query::{Budget, Query};
use xvfs_search::Glob;
use xvfs_types::error::XvfsError;
use xvfs_types::{limits, RepositoryId};

use crate::audit::{self, Action, AuditRecord};
use crate::auth::{Authorizer, SnapshotAuthorization};
use crate::observability::{self, RequestId};
use crate::registry::Registry;
use crate::search::IndexManager;

pub struct SearchApi {
  pub registry: Arc<Registry>,
  pub authz: Arc<Authorizer>,
  pub index: Arc<IndexManager>,
}

impl std::fmt::Debug for SearchApi {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("SearchApi").finish_non_exhaustive()
  }
}

type ResponseStream =
  std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<v1::SearchResponse, Status>> + Send>>;

#[tonic::async_trait]
impl SearchService for SearchApi {
  type SearchStream = ResponseStream;

  async fn search(
    &self,
    request: Request<v1::SearchRequest>,
  ) -> Result<Response<Self::SearchStream>, Status> {
    let started = Instant::now();
    let md = request.metadata();
    let request_id = RequestId::from_client(
      md.get(observability::REQUEST_ID_KEY)
        .and_then(|v| v.to_str().ok()),
    );
    let bearer = md
      .get("authorization")
      .and_then(|v| v.to_str().ok())
      .and_then(|v| v.strip_prefix("Bearer "))
      .unwrap_or("")
      .to_owned();

    let identity = self.authz.authenticate(&bearer).map_err(|e| {
      tracing::warn!(
        request_id = %request_id,
        credential = %xvfs_types::redact::token_fingerprint(&bearer),
        "authentication failed"
      );
      convert::to_status(&e)
    })?;

    let req = request.into_inner();
    let result: Result<(crate::auth::CommitAccess, xvfs_search::SearchResult), XvfsError> = async {
      let repository_id = RepositoryId::parse(&req.repository_id)?;
      self
        .authz
        .authorize_repository(&identity.subject, &repository_id)?;
      let algorithm = self.registry.require_servable(&repository_id)?.algorithm;
      let commit = convert::try_oid(&req.commit_oid, algorithm, "commit_oid")?;
      let access = self
        .authz
        .authorize_commit(
          &identity.subject,
          &repository_id,
          &commit,
          &SnapshotAuthorization {
            mount_capability: req
              .authorization
              .as_ref()
              .map(|a| a.mount_capability.clone())
              .filter(|s| !s.is_empty()),
          },
        )
        .await?;

      let query = build_query(&req)?;
      let result = self.index.search(&repository_id, &commit, query).await?;
      Ok((access, result))
    }
    .await;

    let elapsed = started.elapsed();
    match result {
      Ok((access, result)) => {
        audit::success(
          Action::Search,
          &AuditRecord {
            subject: Some(&access.subject),
            repository_id: Some(&access.repository_id),
            commit: Some(&access.commit),
            via_capability: access.via_capability,
            request_id: Some(request_id.as_str()),
            ..Default::default()
          },
        );
        observability::record_request(Action::Search.as_str(), None, elapsed);

        // Materialized rather than lazily streamed. The search already ran to
        // completion, so streaming it lazily would buy nothing and would create
        // the one failure mode the contract forbids: a generator that can be
        // dropped between the last match and the completion message.
        let mut messages: Vec<Result<v1::SearchResponse, Status>> =
          Vec::with_capacity(result.matches.len() + 1);
        for m in &result.matches {
          messages.push(Ok(v1::SearchResponse {
            message: Some(v1::search_response::Message::Match(to_proto_match(m))),
          }));
        }
        messages.push(Ok(v1::SearchResponse {
          message: Some(v1::search_response::Message::Completion(
            to_proto_completion(&result.completion),
          )),
        }));

        let stream = tokio_stream::iter(messages);
        Ok(Response::new(Box::pin(stream) as Self::SearchStream))
      }
      Err(e) => {
        audit::failure(
          Action::Search,
          &AuditRecord {
            subject: Some(&identity.subject),
            request_id: Some(request_id.as_str()),
            ..Default::default()
          },
          e.code,
        );
        observability::record_request(Action::Search.as_str(), Some(e.code), elapsed);
        Err(convert::to_status(&e))
      }
    }
  }
}

fn build_query(req: &v1::SearchRequest) -> Result<Query, XvfsError> {
  if req.pattern.is_empty() {
    return Err(XvfsError::invalid("the search pattern is empty"));
  }
  if req.scope.len() > limits::MAX_PATH_BYTES {
    return Err(XvfsError::invalid("the search scope is too long"));
  }
  let defaults = Budget::default();
  Ok(Query {
    pattern: req.pattern.clone(),
    literal: req.literal,
    case_insensitive: req.case_insensitive,
    scope: req.scope.clone(),
    include_globs: req.include_globs.iter().map(|g| Glob::new(g)).collect(),
    exclude_globs: req.exclude_globs.iter().map(|g| Glob::new(g)).collect(),
    // Capped rather than rejected: a caller asking for 500 lines of context is
    // not making an error, it is making an expensive request, and the cap is the
    // honest answer.
    context_before: (req.context_before as usize).min(64),
    context_after: (req.context_after as usize).min(64),
    start_after_path: (!req.start_after_path.is_empty()).then(|| req.start_after_path.clone()),
    budget: Budget {
      // Zero means "the server default" for each, so a client that sets nothing
      // gets the policy rather than a query that returns nothing.
      max_results: if req.max_results == 0 {
        defaults.max_results
      } else {
        (req.max_results as usize).min(10_000)
      },
      max_time: if req.max_time_ms == 0 {
        defaults.max_time
      } else {
        Duration::from_millis(req.max_time_ms).min(Duration::from_secs(60))
      },
      max_bytes_read: if req.max_bytes_read == 0 {
        defaults.max_bytes_read
      } else {
        req.max_bytes_read.min(defaults.max_bytes_read)
      },
      max_candidates: if req.max_candidates == 0 {
        defaults.max_candidates
      } else {
        req.max_candidates.min(defaults.max_candidates)
      },
      max_regex_bytes: defaults.max_regex_bytes,
      // Clamped down only. A client may ask for narrower lines than the server
      // default; it may not ask the server to retain more than the default, or
      // the two caps above -- 10 000 results and 64 lines of context each way --
      // would multiply into gigabytes of `line_text` from one legal request.
      max_line_bytes: if req.max_line_bytes == 0 {
        defaults.max_line_bytes
      } else {
        (req.max_line_bytes as usize).min(defaults.max_line_bytes)
      },
      max_display_bytes: if req.max_display_bytes == 0 {
        defaults.max_display_bytes
      } else {
        req.max_display_bytes.min(defaults.max_display_bytes)
      },
    },
  })
}

fn to_proto_match(m: &xvfs_search::Match) -> v1::SearchMatch {
  v1::SearchMatch {
    path: m.path.clone(),
    line: m.line,
    column: m.column,
    matched: m.matched.clone(),
    line_text: m.line_text.clone(),
    before: m.before.clone(),
    after: m.after.clone(),
    blob_oid: m.blob_oid.clone(),
    line_truncated: m.line_truncated,
  }
}

fn to_proto_completion(c: &xvfs_search::Completion) -> v1::SearchCompletion {
  v1::SearchCompletion {
    execution_status: match c.execution_status {
      xvfs_search::ExecutionStatus::Complete => v1::ExecutionStatus::Complete as i32,
      xvfs_search::ExecutionStatus::Truncated => v1::ExecutionStatus::Truncated as i32,
    },
    truncation_reason: c.truncation.map(|t| truncation_name(t).to_owned()),
    stop_budget: c.stop_budget.clone(),
    coverage: Some(v1::Coverage {
      scope: c.coverage.scope.clone(),
      eligible_paths: c.coverage.eligible_paths,
      excluded: c.coverage.excluded.clone().into_iter().collect(),
      declared_exclusions: c.coverage.declared_exclusions.clone(),
    }),
    index_generation: c.index_generation,
    commit_oid: c.commit.clone(),
    candidates_considered: c.candidates_considered,
    bytes_read: c.bytes_read,
    elapsed_ms: c.elapsed_ms,
  }
}

/// The wire name of a truncation reason.
///
/// Written out rather than derived from `Debug`, because these strings are part
/// of the agent-facing contract: an agent branches on `no_required_literal`
/// differently from `time_budget`, and a rename during a refactor would silently
/// change behaviour on the other side.
fn truncation_name(reason: xvfs_search::TruncationReason) -> &'static str {
  use xvfs_search::TruncationReason as T;
  match reason {
    T::ResultLimit => "result_limit",
    T::TimeBudget => "time_budget",
    T::BytesBudget => "bytes_budget",
    T::CandidateBudget => "candidate_budget",
    T::DisplayBudget => "display_budget",
    T::NoRequiredLiteral => "no_required_literal",
    T::BackendFailure => "backend_failure",
  }
}
