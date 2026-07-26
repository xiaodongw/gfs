//! The HTTP surface: immutable blobs, file-by-revision, health, metrics, and
//! webhook ingestion.
//!
//! DESIGN.md section 6.3 keeps this separate from the gRPC API because the two have
//! genuinely different needs: a blob wants range requests, `ETag` revalidation, and
//! CDN cacheability, none of which gRPC expresses well.
//!
//! Two routes with very different caching, and the difference is the point:
//!
//! * `/blobs/{algorithm}:{oid}` is **immutable**. Its content is a hash of itself,
//!   so it can be cached forever and revalidated with an `ETag` that is the object
//!   ID;
//! * `/file?rev=...` is a **convenience** that resolves a selector, so it must not
//!   be cached: the same URL means a different file after the branch moves. It
//!   returns the resolved commit in a header precisely so a caller can convert to
//!   the immutable form.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use xvfs_types::error::{ErrorCode, XvfsError};
use xvfs_types::{limits, BytePath, ObjectId, RepositoryId, RevisionSelector};

use crate::audit::{self, Action, AuditRecord};
use crate::auth::Authorizer;
use crate::catalog::Catalog;
use crate::observability::{self, RequestId};
use crate::registry::Registry;

#[derive(Clone)]
pub struct HttpState {
  pub registry: Arc<Registry>,
  pub catalog: Arc<Catalog>,
  pub authz: Arc<Authorizer>,
}

impl std::fmt::Debug for HttpState {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("HttpState").finish_non_exhaustive()
  }
}

pub fn router(state: HttpState) -> Router {
  Router::new()
    .route("/healthz", get(healthz))
    .route("/readyz", get(readyz))
    .route("/metrics", get(metrics_endpoint))
    .route("/v1/repos/{repository_id}/file", get(file_by_revision))
    .route("/v1/repos/{repository_id}/blobs/{oid}", get(immutable_blob))
    .route("/v1/repos/{repository_id}/ref-events", post(ref_webhook))
    .layer(tower_http::limit::RequestBodyLimitLayer::new(
      // Webhook payloads are small; anything larger is a mistake or an attack.
      64 * 1024,
    ))
    // `with_status_code` rather than the deprecated `new`: a timed-out request must
    // report 504, which is what a client's retry policy reads.
    .layer(tower_http::timeout::TimeoutLayer::with_status_code(
      StatusCode::GATEWAY_TIMEOUT,
      limits::DEFAULT_REQUEST_TIMEOUT,
    ))
    .with_state(state)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A JSON error body, matching `XvfsError`'s serialization so the HTTP and gRPC
/// surfaces report the same code under the same name.
fn error_response(e: &XvfsError, request_id: &RequestId) -> Response {
  let status =
    StatusCode::from_u16(e.code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
  let body = serde_json::json!({
    "code": e.code.as_str(),
    "message": e.message,
    "request_id": request_id.as_str(),
  });
  let mut response = (status, axum::Json(body)).into_response();
  if let Ok(value) = request_id.as_str().parse() {
    response
      .headers_mut()
      .insert(observability::REQUEST_ID_KEY, value);
  }
  response
}

fn request_id(headers: &HeaderMap) -> RequestId {
  RequestId::from_client(
    headers
      .get(observability::REQUEST_ID_KEY)
      .and_then(|v| v.to_str().ok()),
  )
}

fn bearer(headers: &HeaderMap) -> &str {
  headers
    .get(header::AUTHORIZATION)
    .and_then(|v| v.to_str().ok())
    .and_then(|v| v.strip_prefix("Bearer "))
    .unwrap_or("")
}

// ---------------------------------------------------------------------------
// Health and metrics
// ---------------------------------------------------------------------------

/// Liveness: the process is running.
async fn healthz() -> impl IntoResponse {
  (StatusCode::OK, "ok\n")
}

/// Readiness: the catalog answers.
///
/// Distinct from liveness because they call for different operator actions. A
/// process that is alive but not ready should be taken out of rotation; one that is
/// not alive should be restarted, and restarting a process whose *catalog* is
/// unreachable just moves the outage.
async fn readyz(State(state): State<HttpState>) -> Response {
  let catalog = Arc::clone(&state.catalog);
  let ok = tokio::task::spawn_blocking(move || catalog.list_repositories())
    .await
    .is_ok_and(|r| r.is_ok());
  if ok {
    (StatusCode::OK, "ready\n").into_response()
  } else {
    (StatusCode::SERVICE_UNAVAILABLE, "catalog unavailable\n").into_response()
  }
}

async fn metrics_endpoint(State(_state): State<HttpState>) -> Response {
  match crate::service::prometheus_handle() {
    Some(handle) => (
      StatusCode::OK,
      [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
      handle.render(),
    )
      .into_response(),
    None => (StatusCode::SERVICE_UNAVAILABLE, "metrics not installed\n").into_response(),
  }
}

// ---------------------------------------------------------------------------
// File by revision
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
pub struct FileQuery {
  /// A branch, tag, object ID, or allowed abbreviation.
  rev: String,
  /// The path, base64url-encoded.
  ///
  /// Base64url rather than a percent-encoded path segment: DESIGN.md section 7.3
  /// chose it to avoid ambiguous URL normalization of encoded slashes and non-UTF-8
  /// names, both of which are real Git path shapes and both of which a proxy in the
  /// middle is entitled to rewrite.
  path_b64url: String,
}

async fn file_by_revision(
  State(state): State<HttpState>,
  Path(repository_id): Path<String>,
  Query(query): Query<FileQuery>,
  headers: HeaderMap,
) -> Response {
  let rid = request_id(&headers);
  let result = async {
    let identity = state.authz.authenticate(bearer(&headers))?;
    let repo_id = RepositoryId::parse(&repository_id)?;
    state
      .authz
      .authorize_repository(&identity.subject, &repo_id)?;
    let algorithm = state.registry.require_servable(&repo_id)?.algorithm;

    let selector = RevisionSelector::parse(&query.rev, algorithm)?;
    let path = BytePath::from_b64url(&query.path_b64url)?;
    path.validate()?;

    let repo = state.registry.repository(&repo_id)?;
    // Resolved once, atomically, and the resolved commit is what everything else
    // uses -- including the response header. DESIGN.md section 6.2: a branch name is
    // only a selector.
    let resolved = repo.resolve(selector).await?;

    let entry = repo
      .entry(resolved.commit.clone(), path.clone())
      .await?
      .ok_or_else(|| XvfsError::not_found("no such path in this commit"))?;
    if !entry.kind.has_blob_content() {
      return Err(XvfsError::new(
        ErrorCode::InvalidArgument,
        "path is not a file",
      ));
    }
    let bytes = repo.read_blob(entry.oid.clone()).await?;
    Ok((identity, repo_id, resolved, entry, bytes))
  }
  .await;

  match result {
    Ok((identity, repo_id, resolved, entry, bytes)) => {
      audit::success(
        Action::ReadBlob,
        &AuditRecord {
          subject: Some(&identity.subject),
          repository_id: Some(&repo_id),
          commit: Some(&resolved.commit),
          path: Some(&path_of(&entry)),
          request_id: Some(rid.as_str()),
          ..Default::default()
        },
      );
      metrics::counter!(observability::metric::BLOB_BYTES_SERVED).increment(bytes.len() as u64);

      let mut response = (StatusCode::OK, bytes).into_response();
      let h = response.headers_mut();
      h.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/octet-stream"),
      );
      // The resolved commit, the mode, and the blob OID, so a caller can convert
      // this convenience call into an immutable, cacheable blob request.
      set(h, "x-xvfs-commit", &resolved.commit.to_qualified());
      set(h, "x-xvfs-blob-oid", &entry.oid.to_qualified());
      set(h, "x-xvfs-mode", &format!("{:06o}", entry.mode));
      set(h, "x-xvfs-size", &entry.size.to_string());
      // Never cached: the same URL names a different file once the branch moves.
      h.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
      );
      set(h, observability::REQUEST_ID_KEY, rid.as_str());
      response
    }
    Err(e) => error_response(&e, &rid),
  }
}

fn path_of(entry: &xvfs_types::TreeEntryInfo) -> BytePath {
  entry.path.clone()
}

fn set(headers: &mut HeaderMap, name: &str, value: &str) {
  if let (Ok(name), Ok(value)) = (
    header::HeaderName::from_bytes(name.as_bytes()),
    header::HeaderValue::from_str(value),
  ) {
    headers.insert(name, value);
  }
}

// ---------------------------------------------------------------------------
// Immutable blob
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
pub struct BlobQuery {
  /// The short-lived ticket from `GetEntry`.
  ticket: String,
}

async fn immutable_blob(
  State(state): State<HttpState>,
  Path((repository_id, oid)): Path<(String, String)>,
  Query(query): Query<BlobQuery>,
  headers: HeaderMap,
) -> Response {
  let rid = request_id(&headers);
  let result = async {
    let identity = state.authz.authenticate(bearer(&headers))?;
    let repo_id = RepositoryId::parse(&repository_id)?;
    state
      .authz
      .authorize_repository(&identity.subject, &repo_id)?;
    let blob = ObjectId::parse_qualified(&oid)?;

    // Repository access is not enough. DESIGN.md section 7.3 requires proof that
    // the blob is reachable from an allowed revision, and the ticket -- issued only
    // from an authorized `CommitAccess` -- is that proof. Without this, a caller
    // with repository access could read any blob OID it could guess or observe,
    // including one reachable only from another subject's retained commit.
    state
      .authz
      .verify_blob_ticket(&identity.subject, &repo_id, &blob, &query.ticket)?;

    let repo = state.registry.repository(&repo_id)?;
    let bytes = repo.read_blob(blob.clone()).await?;
    Ok((identity, repo_id, blob, bytes))
  }
  .await;

  let (identity, repo_id, blob, bytes) = match result {
    Ok(v) => v,
    Err(e) => return error_response(&e, &rid),
  };

  let etag = format!("\"{}\"", blob.to_qualified());

  // Revalidation. The ETag *is* the object ID, so a match is proof the client
  // already holds these exact bytes -- there is no staleness window to reason
  // about, which is the whole advantage of content addressing.
  if let Some(inm) = headers
    .get(header::IF_NONE_MATCH)
    .and_then(|v| v.to_str().ok())
  {
    if inm.split(',').any(|candidate| candidate.trim() == etag) {
      let mut response = StatusCode::NOT_MODIFIED.into_response();
      set(
        response.headers_mut(),
        observability::REQUEST_ID_KEY,
        rid.as_str(),
      );
      return response;
    }
  }

  audit::success(
    Action::ReadBlob,
    &AuditRecord {
      subject: Some(&identity.subject),
      repository_id: Some(&repo_id),
      request_id: Some(rid.as_str()),
      ..Default::default()
    },
  );

  let total = bytes.len() as u64;
  let range = headers
    .get(header::RANGE)
    .and_then(|v| v.to_str().ok())
    .map(|v| parse_range(v, total));

  match range {
    Some(Err(e)) => {
      let mut response = error_response(&e, &rid);
      // RFC 9110: an unsatisfiable range must report the actual length so a client
      // can correct its request rather than retrying the same one.
      set(
        response.headers_mut(),
        header::CONTENT_RANGE.as_str(),
        &format!("bytes */{total}"),
      );
      *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
      response
    }
    Some(Ok((start, end))) => {
      let slice = bytes[start as usize..=end as usize].to_vec();
      metrics::counter!(observability::metric::BLOB_BYTES_SERVED).increment(slice.len() as u64);
      let mut response = (StatusCode::PARTIAL_CONTENT, Body::from(slice)).into_response();
      let h = response.headers_mut();
      set(
        h,
        header::CONTENT_RANGE.as_str(),
        &format!("bytes {start}-{end}/{total}"),
      );
      blob_headers(h, &etag, &rid);
      response
    }
    None => {
      metrics::counter!(observability::metric::BLOB_BYTES_SERVED).increment(total);
      let mut response = (StatusCode::OK, bytes).into_response();
      blob_headers(response.headers_mut(), &etag, &rid);
      response
    }
  }
}

fn blob_headers(h: &mut HeaderMap, etag: &str, rid: &RequestId) {
  h.insert(
    header::CONTENT_TYPE,
    header::HeaderValue::from_static("application/octet-stream"),
  );
  set(h, header::ETAG.as_str(), etag);
  h.insert(
    header::ACCEPT_RANGES,
    header::HeaderValue::from_static("bytes"),
  );
  // Immutable: the content is a hash of itself, so it can never change under this
  // URL. `immutable` tells a conforming cache not to revalidate at all.
  h.insert(
    header::CACHE_CONTROL,
    header::HeaderValue::from_static("public, max-age=31536000, immutable"),
  );
  set(h, observability::REQUEST_ID_KEY, rid.as_str());
}

/// Parse a single-range `Range` header, returning an inclusive byte range.
///
/// Only one range is supported. Multipart ranges need a multipart body, and no
/// XVFS client wants them; a request for several is refused rather than silently
/// answered with the first, which would return the wrong bytes without saying so.
fn parse_range(value: &str, total: u64) -> Result<(u64, u64), XvfsError> {
  let bad = || XvfsError::new(ErrorCode::InvalidArgument, "unsatisfiable range");
  let spec = value.strip_prefix("bytes=").ok_or_else(bad)?;
  if spec.contains(',') {
    return Err(XvfsError::new(
      ErrorCode::InvalidArgument,
      "only a single byte range is supported",
    ));
  }
  let (start, end) = spec.split_once('-').ok_or_else(bad)?;
  if total == 0 {
    return Err(bad());
  }

  let (start, end) = match (start.trim(), end.trim()) {
    // `-N`: the last N bytes.
    ("", n) => {
      let n: u64 = n.parse().map_err(|_| bad())?;
      if n == 0 {
        return Err(bad());
      }
      (total.saturating_sub(n), total - 1)
    }
    // `N-`: from N to the end.
    (s, "") => (s.parse().map_err(|_| bad())?, total - 1),
    (s, e) => (s.parse().map_err(|_| bad())?, e.parse().map_err(|_| bad())?),
  };

  if start > end || start >= total {
    return Err(bad());
  }
  // Clamped rather than rejected: RFC 9110 says an end past the last byte is
  // satisfiable and answered with what exists.
  Ok((start, end.min(total - 1)))
}

// ---------------------------------------------------------------------------
// Webhook ingestion
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
pub struct RefEventPayload {
  pub ref_name: String,
  /// Absent or null means the ref was deleted.
  #[serde(default)]
  pub new_oid: Option<String>,
}

/// Accept a ref-change notification.
///
/// Idempotent, because the catalog's event key is
/// `(repository_id, ref_name, old_oid, new_oid)`: a webhook delivered twice, or one
/// racing the poller, collapses to a single event.
async fn ref_webhook(
  State(state): State<HttpState>,
  Path(repository_id): Path<String>,
  headers: HeaderMap,
  axum::Json(payload): axum::Json<RefEventPayload>,
) -> Response {
  let rid = request_id(&headers);
  let result = async {
    let identity = state.authz.authenticate(bearer(&headers))?;
    let repo_id = RepositoryId::parse(&repository_id)?;
    state
      .authz
      .authorize_repository(&identity.subject, &repo_id)?;
    let algorithm = state.registry.require_servable(&repo_id)?.algorithm;

    let new_oid = match payload.new_oid.as_deref().filter(|s| !s.is_empty()) {
      Some(s) => Some(xvfs_proto::convert::try_oid(s, algorithm, "new_oid")?),
      None => None,
    };

    let catalog = Arc::clone(&state.catalog);
    let repo_for_task = repo_id.clone();
    let ref_name = payload.ref_name.clone();
    let observed = tokio::task::spawn_blocking(move || {
      catalog.observe_ref(&repo_for_task, &ref_name, new_oid.as_ref())
    })
    .await
    .map_err(crate::util::join_error)??;
    Ok((identity, repo_id, observed))
  }
  .await;

  match result {
    Ok((_identity, _repo_id, observed)) => {
      let mut response = (
        StatusCode::ACCEPTED,
        axum::Json(serde_json::json!({ "outcome": format!("{observed:?}") })),
      )
        .into_response();
      set(
        response.headers_mut(),
        observability::REQUEST_ID_KEY,
        rid.as_str(),
      );
      response
    }
    Err(e) => error_response(&e, &rid),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn range_parsing_covers_the_rfc_9110_forms() {
    assert_eq!(parse_range("bytes=0-9", 100).unwrap(), (0, 9));
    assert_eq!(parse_range("bytes=10-", 100).unwrap(), (10, 99));
    assert_eq!(parse_range("bytes=-10", 100).unwrap(), (90, 99));
    // An end past the last byte is satisfiable and clamped, not rejected.
    assert_eq!(parse_range("bytes=90-1000", 100).unwrap(), (90, 99));
    // A suffix longer than the content yields the whole content.
    assert_eq!(parse_range("bytes=-500", 100).unwrap(), (0, 99));
    assert_eq!(parse_range("bytes=0-0", 100).unwrap(), (0, 0));
  }

  #[test]
  fn unsatisfiable_and_malformed_ranges_are_rejected() {
    for bad in [
      "bytes=100-200", // start past the end
      "bytes=50-10",   // inverted
      "bytes=-0",      // zero-length suffix
      "items=0-9",     // wrong unit
      "bytes=abc-def",
      "bytes=",
      "0-9",
    ] {
      assert!(parse_range(bad, 100).is_err(), "{bad} must be rejected");
    }
    // Any range against empty content is unsatisfiable.
    assert!(parse_range("bytes=0-0", 0).is_err());
  }

  #[test]
  fn multiple_ranges_are_refused_rather_than_partly_answered() {
    // Answering only the first range would return the wrong bytes without saying
    // so, which is worse than refusing.
    let err = parse_range("bytes=0-9,20-29", 100).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert!(err.message.contains("single"));
  }
}
