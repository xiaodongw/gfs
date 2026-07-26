//! The snapshot API client the filesystem reads through.
//!
//! Two transports, because DESIGN.md section 6.3 kept them separate on purpose:
//! gRPC for metadata, where a typed request/response and per-item error detail
//! matter, and HTTP for blob bytes, where ranges, `ETag` revalidation, and
//! cacheability matter and gRPC expresses none of them well.
//!
//! # Every call names the commit
//!
//! The client is constructed *around* one pinned commit and there is no method
//! that takes a revision selector. DESIGN.md section 6.2 makes a branch name only
//! a selector; resolving it belongs to `CreateMount`, once, before the mount
//! exists. A filesystem that could re-resolve would be able to serve two
//! generations of a tree through one mount, which is the failure the pinning rule
//! exists to prevent.
//!
//! # The capability is mutable, the commit is not
//!
//! The mount capability is replaced on every heartbeat renewal (`RenewMount`
//! returns a fresh one), so it lives behind a lock while the repository, commit,
//! and algorithm are fixed at construction.

use std::sync::{Arc, RwLock};

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use xvfs_proto::convert;
use xvfs_proto::v1;
use xvfs_types::error::{ErrorCode, XvfsError};
use xvfs_types::{
  BytePath, CommitMeta, HashAlgorithm, MountId, ObjectId, RepositoryId, Timestamp, TreeEntryInfo,
};

type Grpc = v1::snapshot_service_client::SnapshotServiceClient<tonic::transport::Channel>;
type Http = hyper_util::client::legacy::Client<HttpConnector, Empty<Bytes>>;

/// Everything the client needs that does not change for the life of a mount.
#[derive(Clone, Debug)]
pub struct MountBinding {
  pub repository_id: RepositoryId,
  pub commit: ObjectId,
  pub algorithm: HashAlgorithm,
  pub snapshot_time: Timestamp,
}

/// One page of a directory listing.
#[derive(Clone, Debug)]
pub struct DirectoryPage {
  pub entries: Vec<TreeEntryInfo>,
  /// Empty when this was the last page.
  pub next_page_token: Vec<u8>,
}

pub struct SnapshotClient {
  grpc: Grpc,
  http: Http,
  http_endpoint: String,
  token: String,
  binding: MountBinding,
  /// Replaced by every successful heartbeat renewal.
  capability: RwLock<String>,
}

impl std::fmt::Debug for SnapshotClient {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    // Neither the bearer token nor the capability appears: both are credentials,
    // and a struct dump into a log is how credentials leak.
    f.debug_struct("SnapshotClient")
      .field("http_endpoint", &self.http_endpoint)
      .field("binding", &self.binding)
      .finish_non_exhaustive()
  }
}

impl SnapshotClient {
  /// Connect to the gRPC endpoint and build the blob HTTP client.
  pub async fn connect(
    grpc_endpoint: &str,
    http_endpoint: &str,
    token: &str,
    binding: MountBinding,
    capability: String,
  ) -> Result<Arc<Self>, XvfsError> {
    let channel = tonic::transport::Endpoint::from_shared(grpc_endpoint.to_owned())
      .map_err(|e| XvfsError::invalid(format!("invalid gRPC endpoint: {e}")))?
      .connect()
      .await
      .map_err(|e| {
        XvfsError::new(
          ErrorCode::Unavailable,
          format!("connecting to the XVFS server: {e}"),
        )
      })?;

    Ok(Arc::new(SnapshotClient {
      grpc: Grpc::new(channel),
      http: hyper_util::client::legacy::Client::builder(TokioExecutor::new()).build_http(),
      http_endpoint: http_endpoint.trim_end_matches('/').to_owned(),
      token: token.to_owned(),
      binding,
      capability: RwLock::new(capability),
    }))
  }

  pub fn binding(&self) -> &MountBinding {
    &self.binding
  }

  /// Install the capability a renewal returned.
  pub fn set_capability(&self, capability: String) {
    *self.capability.write().expect("capability lock") = capability;
  }

  fn capability(&self) -> String {
    self.capability.read().expect("capability lock").clone()
  }

  /// The current capability, for writing into `mount.json`.
  ///
  /// Deliberately verbose. This is the one call that takes a credential out of
  /// the client, and a reviewer reading `state.rs` should be able to see that it
  /// was an explicit decision rather than an accessor someone reached for.
  pub fn capability_for_persistence(&self) -> String {
    self.capability()
  }

  /// The authorization proof for a commit-scoped call.
  ///
  /// Always sent, even while the commit is still reachable from a visible ref.
  /// The alternative -- sending it only after a reachability check fails -- would
  /// make the *first* read after a force push fail before the client learned it
  /// needed the capability, which is exactly the moment a mount must not break.
  fn authorization(&self) -> Option<v1::SnapshotAuthorization> {
    let capability = self.capability();
    if capability.is_empty() {
      None
    } else {
      Some(v1::SnapshotAuthorization {
        mount_capability: capability,
      })
    }
  }

  fn authed<T>(&self, message: T) -> Result<tonic::Request<T>, XvfsError> {
    let mut request = tonic::Request::new(message);
    if !self.token.is_empty() {
      let value = format!("Bearer {}", self.token)
        .parse()
        .map_err(|_| XvfsError::invalid("token is not a valid header value"))?;
      request.metadata_mut().insert("authorization", value);
    }
    Ok(request)
  }

  /// One path's metadata, or `None` when the commit has no such path.
  ///
  /// The `None` is deliberate: a negative lookup is the most common FUSE result
  /// during a build, and the gRPC `NOT_FOUND` status lets the caller cache it as
  /// negative without inspecting a body.
  pub async fn get_entry(
    &self,
    path: &BytePath,
    want_blob_ticket: bool,
  ) -> Result<Option<TreeEntryInfo>, XvfsError> {
    let request = self.authed(v1::GetEntryRequest {
      repository_id: self.binding.repository_id.as_str().to_owned(),
      commit_oid: self.binding.commit.to_qualified(),
      path: path.as_bytes().to_vec(),
      authorization: self.authorization(),
      want_blob_ticket,
    })?;
    match self.grpc.clone().get_entry(request).await {
      Ok(response) => {
        let entry = response
          .into_inner()
          .entry
          .ok_or_else(|| XvfsError::internal("server returned no entry"))?;
        Ok(Some(entry.try_into_domain(self.binding.algorithm)?))
      }
      Err(status) => {
        let error = convert::from_status(&status);
        if error.code == ErrorCode::NotFound {
          Ok(None)
        } else {
          Err(error)
        }
      }
    }
  }

  pub async fn list_directory(
    &self,
    path: &BytePath,
    page_token: Vec<u8>,
    page_size: u32,
    want_blob_tickets: bool,
  ) -> Result<DirectoryPage, XvfsError> {
    let request = self.authed(v1::ListDirectoryRequest {
      repository_id: self.binding.repository_id.as_str().to_owned(),
      commit_oid: self.binding.commit.to_qualified(),
      path: path.as_bytes().to_vec(),
      page_token,
      page_size,
      authorization: self.authorization(),
      want_blob_tickets,
    })?;
    let page = self
      .grpc
      .clone()
      .list_directory(request)
      .await
      .map_err(|s| convert::from_status(&s))?
      .into_inner();
    let mut entries = Vec::with_capacity(page.entries.len());
    for entry in page.entries {
      entries.push(entry.try_into_domain(self.binding.algorithm)?);
    }
    Ok(DirectoryPage {
      entries,
      next_page_token: page.next_page_token,
    })
  }

  /// Many paths in one round trip.
  ///
  /// Per-path failures are folded into `None` rather than surfaced: the caller is
  /// a prefetch path, and a batch is an optimisation whose partial failure must
  /// degrade to an individual `GetEntry` rather than to an error.
  pub async fn batch_get_entry(
    &self,
    paths: &[BytePath],
    want_blob_tickets: bool,
  ) -> Result<Vec<Option<TreeEntryInfo>>, XvfsError> {
    let request = self.authed(v1::BatchGetEntryRequest {
      repository_id: self.binding.repository_id.as_str().to_owned(),
      commit_oid: self.binding.commit.to_qualified(),
      paths: paths.iter().map(|p| p.as_bytes().to_vec()).collect(),
      authorization: self.authorization(),
      want_blob_tickets,
    })?;
    let response = self
      .grpc
      .clone()
      .batch_get_entry(request)
      .await
      .map_err(|s| convert::from_status(&s))?
      .into_inner();
    Ok(
      response
        .results
        .into_iter()
        .map(|result| match result.result {
          Some(v1::entry_result::Result::Entry(e)) => {
            e.try_into_domain(self.binding.algorithm).ok()
          }
          _ => None,
        })
        .collect(),
    )
  }

  /// Extend the lease, installing the fresh capability the server returns.
  ///
  /// Idempotent by contract, which is what makes a heartbeat that cannot tell
  /// whether its last attempt landed recoverable: the previous capability stays
  /// valid until its own expiry, so a renewal whose *response* was lost does not
  /// strand the daemon holding a token the server has forgotten.
  pub async fn renew_mount(&self, mount_id: &MountId) -> Result<Timestamp, XvfsError> {
    let request = self.authed(v1::RenewMountRequest {
      mount_id: mount_id.as_str().to_owned(),
      mount_capability: self.capability(),
    })?;
    let response = self
      .grpc
      .clone()
      .renew_mount(request)
      .await
      .map_err(|s| convert::from_status(&s))?
      .into_inner();
    self.set_capability(response.mount_capability);
    Ok(
      response
        .lease_expiry
        .map(|t| Timestamp::new(t.secs, t.nanos))
        .unwrap_or_else(Timestamp::now),
    )
  }

  /// Release the lease eagerly. Expiry is the crash fallback, not the normal path.
  pub async fn release_mount(&self, mount_id: &MountId) -> Result<(), XvfsError> {
    let request = self.authed(v1::ReleaseMountRequest {
      mount_id: mount_id.as_str().to_owned(),
      mount_capability: self.capability(),
    })?;
    self
      .grpc
      .clone()
      .release_mount(request)
      .await
      .map_err(|s| convert::from_status(&s))?;
    Ok(())
  }

  pub async fn get_commit(&self) -> Result<CommitMeta, XvfsError> {
    let request = self.authed(v1::GetCommitRequest {
      repository_id: self.binding.repository_id.as_str().to_owned(),
      commit_oid: self.binding.commit.to_qualified(),
      authorization: self.authorization(),
    })?;
    self
      .grpc
      .clone()
      .get_commit(request)
      .await
      .map_err(|s| convert::from_status(&s))?
      .into_inner()
      .try_into_domain(self.binding.algorithm)
  }

  /// Fetch a whole blob over the immutable HTTP endpoint.
  ///
  /// Whole-blob, not ranged: DESIGN.md section 12 fixes whole-blob fetch as the
  /// MVP boundary, and the cache verifies the canonical object hash, which a
  /// partial body cannot satisfy.
  pub async fn read_blob(&self, oid: &ObjectId, ticket: &str) -> Result<Vec<u8>, XvfsError> {
    let url = format!(
      "{}/v1/repos/{}/blobs/{}?ticket={}",
      self.http_endpoint,
      self.binding.repository_id.as_str(),
      oid.to_qualified(),
      ticket,
    );
    let uri: http::Uri = url
      .parse()
      .map_err(|_| XvfsError::invalid("invalid blob URL"))?;

    let mut builder = http::Request::builder().uri(uri);
    if !self.token.is_empty() {
      builder = builder.header(
        http::header::AUTHORIZATION,
        format!("Bearer {}", self.token),
      );
    }
    let request = builder
      .body(Empty::<Bytes>::new())
      .map_err(|e| XvfsError::internal(format!("building blob request: {e}")))?;

    let response = self.http.request(request).await.map_err(|e| {
      XvfsError::new(
        ErrorCode::Unavailable,
        format!("blob request did not complete: {e}"),
      )
    })?;
    let status = response.status();
    let body = response
      .into_body()
      .collect()
      .await
      .map_err(|e| {
        XvfsError::new(
          ErrorCode::Unavailable,
          format!("blob body did not complete: {e}"),
        )
      })?
      .to_bytes();

    if !status.is_success() {
      return Err(http_error(status, &body));
    }
    Ok(body.to_vec())
  }
}

/// Turn a non-2xx blob response into the same error vocabulary the gRPC path
/// produces, preferring the server's own JSON `code` over the status line.
///
/// The status alone is not enough: 503 is both `UNAVAILABLE` (retry) and
/// `SNAPSHOT_BUILDING` (retry, but a different message), and the retry policy in
/// `ErrorCode::retryable` is written against the codes, not against HTTP.
fn http_error(status: http::StatusCode, body: &[u8]) -> XvfsError {
  #[derive(serde::Deserialize)]
  struct Body {
    code: String,
    message: String,
  }
  if let Ok(parsed) = serde_json::from_slice::<Body>(body) {
    if let Some(code) = code_from_wire(&parsed.code) {
      return XvfsError::new(code, parsed.message);
    }
  }
  let code = match status.as_u16() {
    400 => ErrorCode::InvalidArgument,
    401 => ErrorCode::Unauthenticated,
    403 => ErrorCode::PermissionDenied,
    404 => ErrorCode::NotFound,
    409 => ErrorCode::Conflict,
    422 => ErrorCode::FailedPrecondition,
    429 => ErrorCode::ResourceLimit,
    503 => ErrorCode::Unavailable,
    504 => ErrorCode::DeadlineExceeded,
    _ => ErrorCode::Internal,
  };
  XvfsError::new(code, format!("blob request failed with {status}"))
}

/// The inverse of [`ErrorCode::as_str`].
///
/// Written out rather than derived from `serde` so that an unknown code from a
/// newer server is `None` and falls back to the status mapping, instead of
/// failing to deserialize the whole body and losing the message too.
fn code_from_wire(s: &str) -> Option<ErrorCode> {
  Some(match s {
    "INVALID_ARGUMENT" => ErrorCode::InvalidArgument,
    "NOT_FOUND" => ErrorCode::NotFound,
    "PERMISSION_DENIED" => ErrorCode::PermissionDenied,
    "UNAUTHENTICATED" => ErrorCode::Unauthenticated,
    "EXPIRED" => ErrorCode::Expired,
    "FAILED_PRECONDITION" => ErrorCode::FailedPrecondition,
    "CONFLICT" => ErrorCode::Conflict,
    "RESOURCE_LIMIT" => ErrorCode::ResourceLimit,
    "SNAPSHOT_BUILDING" => ErrorCode::SnapshotBuilding,
    "NOT_INDEXABLE" => ErrorCode::NotIndexable,
    "UNSUPPORTED_REPOSITORY_FORMAT" => ErrorCode::UnsupportedRepositoryFormat,
    "RESERVED_NAMESPACE" => ErrorCode::ReservedNamespace,
    "UNAVAILABLE" => ErrorCode::Unavailable,
    "DEADLINE_EXCEEDED" => ErrorCode::DeadlineExceeded,
    "CANCELLED" => ErrorCode::Cancelled,
    "INTERNAL" => ErrorCode::Internal,
    _ => return None,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn every_error_code_round_trips_through_the_wire_name() {
    // The client's retry policy reads `ErrorCode`, so a code that does not
    // round-trip silently degrades to the status-line fallback and a retryable
    // failure becomes a permanent one.
    for code in [
      ErrorCode::InvalidArgument,
      ErrorCode::NotFound,
      ErrorCode::PermissionDenied,
      ErrorCode::Unauthenticated,
      ErrorCode::Expired,
      ErrorCode::FailedPrecondition,
      ErrorCode::Conflict,
      ErrorCode::ResourceLimit,
      ErrorCode::SnapshotBuilding,
      ErrorCode::NotIndexable,
      ErrorCode::UnsupportedRepositoryFormat,
      ErrorCode::ReservedNamespace,
      ErrorCode::Unavailable,
      ErrorCode::DeadlineExceeded,
      ErrorCode::Cancelled,
      ErrorCode::Internal,
    ] {
      assert_eq!(code_from_wire(code.as_str()), Some(code), "{code:?}");
    }
  }

  #[test]
  fn an_unknown_code_falls_back_to_the_status_line() {
    let body = br#"{"code":"SOMETHING_NEWER","message":"from a future server"}"#;
    let e = http_error(http::StatusCode::SERVICE_UNAVAILABLE, body);
    assert_eq!(e.code, ErrorCode::Unavailable);
    assert!(e.is_retryable());
  }

  #[test]
  fn a_json_body_wins_over_the_status_code() {
    // 503 is both UNAVAILABLE and SNAPSHOT_BUILDING; only the body distinguishes.
    let body = br#"{"code":"SNAPSHOT_BUILDING","message":"index is building"}"#;
    let e = http_error(http::StatusCode::SERVICE_UNAVAILABLE, body);
    assert_eq!(e.code, ErrorCode::SnapshotBuilding);
  }

  #[test]
  fn a_non_json_body_still_produces_a_typed_error() {
    let e = http_error(http::StatusCode::FORBIDDEN, b"<html>nope</html>");
    assert_eq!(e.code, ErrorCode::PermissionDenied);
  }
}
