//! The Git smart-HTTP gateway.
//!
//! DESIGN.md section 7.2 and ADR 0001 fix the shape: XVFS does **not**
//! reimplement `upload-pack`. A Rust gateway authenticates, authorizes, limits,
//! and streams to a sandboxed stock `git upload-pack` child. Git owns the
//! protocol -- pkt-line framing, `ls-refs`, want/have negotiation, shallow
//! behaviour, filters, deltas, sideband -- and XVFS owns everything around it.
//!
//! The split between the files here follows the trust boundary rather than the
//! request flow:
//!
//! * [`upload_pack`] is everything the gateway decides *about the child*: its
//!   executable, arguments, working directory, environment, configuration, and
//!   resource limits. Nothing user-supplied reaches any of them.
//! * [`pkt`] is the only place the gateway looks at Git's wire bytes, and it
//!   does so for exactly two reasons that cannot be delegated to the child: the
//!   exact partial-clone filter, and the reserved `refs/xvfs/` namespace.
//! * this file is the HTTP surface: routing, credentials, the version-dependent
//!   framing, bounded request bodies, and streamed responses.
//!
//! # Two routes, and the framing difference between them
//!
//! `GET .../info/refs?service=git-upload-pack` runs
//! `upload-pack --http-backend-info-refs`, which emits **only** the
//! advertisement body. For protocol v0/v1 the gateway prepends the
//! `# service=git-upload-pack` pkt-line and a flush packet, exactly as
//! `git-http-backend` does. For protocol v2 it must not: the response begins
//! directly with upload-pack's own `version 2` pkt-line. Getting this backwards
//! produces a client error that names neither the preamble nor the version.
//!
//! `POST .../git-upload-pack` runs `upload-pack --stateless-rpc`. The request is
//! read under a byte cap, decompressed under an output and ratio cap when it is
//! gzipped, and **validated before the child is spawned**; the response streams.
//!
//! # Streaming, backpressure, and reaping
//!
//! A clone of the M0.1 worst case transfers gigabytes, so nothing here
//! buffers a response. Bytes move through a bounded channel: when the client
//! stops reading, the channel fills, the pump stops reading the child's stdout,
//! and the child blocks on its own pipe. That is the backpressure. When the
//! client *disconnects*, the response body is dropped, the channel closes, the
//! pump exits, and `kill_on_drop` reaps the child -- without which a
//! `git pack-objects` would keep burning CPU on a pack nobody will read.
//!
//! # What this gateway does not claim
//!
//! ADR 0002 is the load-bearing scope limit. M0.3 measured that protocol v2
//! serves any object in a repository's object database by object ID regardless
//! of `uploadpack.allowAnySHA1InWant`, so a repository reader can always reach a
//! lease-retained commit through Git. **One bare repository is one authorization
//! domain.** The gateway enforces repository authorization -- the same
//! [`Authorizer`](crate::auth::Authorizer) the snapshot, blob, and search APIs
//! use -- and does not claim object authorization. PLAN.md M1.5 says it plainly:
//! do not write an acceptance test that expects the Git path to deny it.

pub mod pkt;
pub mod upload_pack;

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_stream::StreamExt;
use xvfs_types::error::{ErrorCode, XvfsError};
use xvfs_types::RepositoryId;

use crate::audit::{self, Action, AuditRecord};
use crate::auth::{Authorizer, Identity};
use crate::observability::{self, RequestId};
use crate::registry::Registry;

pub use upload_pack::{
  FilterPolicy, GitProtocol, Mode, ResourceLimits, UploadPack, UploadPackPolicy,
};

/// The advertisement content type. Byte-exact, because Git checks it and falls
/// back to the dumb protocol when it does not match.
const ADVERTISEMENT_TYPE: &str = "application/x-git-upload-pack-advertisement";
const RESULT_TYPE: &str = "application/x-git-upload-pack-result";

/// Metric action labels. The two routes are separated because their latencies
/// are unrelated -- an advertisement is milliseconds and a clone is minutes --
/// and one histogram over both would describe neither.
const METRIC_ADVERTISE: &str = "git_advertise";
const METRIC_RPC: &str = "git_upload_pack";

/// Chunks in flight between the child and the socket.
///
/// The number that sets the backpressure window: 16 chunks of at most 64 KiB is
/// a megabyte of slack, enough that an ordinary jittery reader never stalls the
/// child and small enough that a thousand abandoned clones cannot become a
/// gigabyte of server memory.
const STREAM_CHANNEL_DEPTH: usize = 16;
const READ_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct GatewayState {
  pub registry: Arc<Registry>,
  pub authz: Arc<Authorizer>,
  pub policy: Arc<UploadPackPolicy>,
  /// Admission control across the whole process. PLAN.md M5.3's "process count"
  /// limit: an unbounded number of concurrent clones is an unbounded number of
  /// `pack-objects` children, which is the cheapest way to take the server down.
  pub admission: Arc<tokio::sync::Semaphore>,
}

impl std::fmt::Debug for GatewayState {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("GatewayState")
      .field("policy", &self.policy)
      .field("permits", &self.admission.available_permits())
      .finish_non_exhaustive()
  }
}

impl GatewayState {
  pub fn new(registry: Arc<Registry>, authz: Arc<Authorizer>, policy: UploadPackPolicy) -> Self {
    let admission = Arc::new(tokio::sync::Semaphore::new(policy.max_concurrent_processes));
    GatewayState {
      registry,
      authz,
      policy: Arc::new(policy),
      admission,
    }
  }
}

/// The gateway's routes.
///
/// Built as its own `Router` and merged into the HTTP surface rather than added
/// to it. The blob and webhook routes carry a 64 KiB body limit and a
/// request timeout, and both are wrong here: a negotiation body is megabytes and
/// a clone legitimately runs for an hour. Keeping the two routers separate keeps
/// each set of layers scoped to the routes it was reasoned about for.
///
/// The paths are what stock Git derives from a clone URL of
/// `http://host/v1/repos/<repository-id>`: it appends `/info/refs` for the
/// advertisement and `/git-upload-pack` for the RPC.
pub fn router(state: GatewayState) -> Router {
  let max_body = state.policy.max_body_bytes;
  Router::new()
    .route("/v1/repos/{repository_id}/info/refs", get(info_refs))
    .route(
      "/v1/repos/{repository_id}/git-upload-pack",
      post(upload_pack_rpc),
    )
    // The cap applies to the compressed body; `decompress_body` caps what it may
    // expand to. Both are needed: neither bound implies the other.
    .layer(tower_http::limit::RequestBodyLimitLayer::new(max_body))
    .with_state(state)
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

/// Extract a bearer token from either scheme Git can present.
///
/// Bearer is what an XVFS-configured client sends through
/// `http.extraHeader`. Basic is what Git itself produces from a credential
/// helper or a URL userinfo, and it is accepted because refusing it would mean
/// every ordinary `git clone` fails in a way the user cannot fix from the Git
/// side. The token is the password; the username is ignored, matching the
/// `x-access-token:<token>` convention Git hosts already use.
fn credential(headers: &HeaderMap) -> Option<String> {
  let value = headers
    .get(header::AUTHORIZATION)
    .and_then(|v| v.to_str().ok())?;
  if let Some(token) = value.strip_prefix("Bearer ") {
    return Some(token.to_owned());
  }
  let encoded = value.strip_prefix("Basic ")?;
  let decoded = base64_decode(encoded.trim())?;
  let text = String::from_utf8(decoded).ok()?;
  // `user:password`; a password-less form is read as the token itself, because
  // `https://<token>@host/...` is a shape people write.
  match text.split_once(':') {
    Some((_user, password)) if !password.is_empty() => Some(password.to_owned()),
    Some((user, _)) => Some(user.to_owned()),
    None => Some(text),
  }
}

/// Standard (padded) base64, which is what HTTP Basic uses.
///
/// `xvfs_types::b64url_decode` is the *url* alphabet and unpadded, so it cannot
/// be reused here: `+` and `/` would be rejected and padding would be a parse
/// error. Twenty lines rather than a dependency at the trust boundary.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
  const INVALID: u8 = 255;
  fn value(byte: u8) -> u8 {
    match byte {
      b'A'..=b'Z' => byte - b'A',
      b'a'..=b'z' => byte - b'a' + 26,
      b'0'..=b'9' => byte - b'0' + 52,
      b'+' => 62,
      b'/' => 63,
      _ => INVALID,
    }
  }
  let input = input.trim_end_matches('=').as_bytes();
  let mut out = Vec::with_capacity(input.len() * 3 / 4);
  let mut accumulator: u32 = 0;
  let mut bits = 0u32;
  for &byte in input {
    let v = value(byte);
    if v == INVALID {
      return None;
    }
    accumulator = (accumulator << 6) | u32::from(v);
    bits += 6;
    if bits >= 8 {
      bits -= 8;
      out.push((accumulator >> bits) as u8);
    }
  }
  Some(out)
}

/// Authenticate and authorize, returning the repository's on-disk path.
///
/// The path comes from the catalog, keyed by a parsed [`RepositoryId`]. That is
/// the whole defence against path traversal on this surface: no part of the
/// request is ever joined onto a filesystem path, so there is no traversal to
/// normalize away. `RepositoryId::parse` additionally refuses the shapes a URL
/// router could smuggle through.
async fn resolve(
  state: &GatewayState,
  repository_id: &str,
  headers: &HeaderMap,
) -> Result<(Identity, RepositoryId, std::path::PathBuf), XvfsError> {
  let token = credential(headers)
    .ok_or_else(|| XvfsError::new(ErrorCode::Unauthenticated, "no credential presented"))?;
  let identity = state.authz.authenticate(&token)?;
  let repo_id = RepositoryId::parse(repository_id)?;
  // The same call the snapshot, blob, and search surfaces make. M1.5's
  // "enforce repository permissions uniformly" is only true if it is literally
  // the same function, so it is.
  state
    .authz
    .authorize_repository(&identity.subject, &repo_id)?;
  let record = state.registry.require_servable(&repo_id)?;
  Ok((identity, repo_id, record.repo_path))
}

// ---------------------------------------------------------------------------
// GET /info/refs
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
pub struct ServiceQuery {
  #[serde(default)]
  service: String,
}

async fn info_refs(
  State(state): State<GatewayState>,
  Path(repository_id): Path<String>,
  Query(query): Query<ServiceQuery>,
  headers: HeaderMap,
) -> Response {
  let rid = request_id(&headers);
  let started = std::time::Instant::now();

  let prepared = async {
    // Dumb HTTP is not served at all. A missing or unknown `service` is refused
    // rather than falling through, because the fallback exposes the object
    // directory as static files and bypasses every check in this module.
    match query.service.as_str() {
      "git-upload-pack" => {}
      "git-receive-pack" => {
        return Err(XvfsError::new(
          ErrorCode::PermissionDenied,
          "this repository is read-only over Git; push is not supported",
        ))
      }
      _ => {
        return Err(XvfsError::new(
          ErrorCode::InvalidArgument,
          "only service=git-upload-pack is supported",
        ))
      }
    }
    let (identity, repo_id, repo_path) = resolve(&state, &repository_id, &headers).await?;
    let pack = UploadPack::new(&repo_path, (*state.policy).clone())?;
    Ok((identity, repo_id, pack))
  }
  .await;

  let (identity, repo_id, pack) = match prepared {
    Ok(v) => v,
    Err(e) => return fail(METRIC_ADVERTISE, &e, &rid, started),
  };

  let protocol = GitProtocol::from_header(header_str(&headers, "git-protocol"));
  // v0/v1 only. `upload-pack --http-backend-info-refs` emits the advertisement
  // and nothing else, so the service preamble is the gateway's to write; for v2
  // writing it would corrupt a response that must begin with `version 2`.
  let preamble = match protocol {
    GitProtocol::V0 => {
      let mut bytes = pkt::pkt_line(b"# service=git-upload-pack\n");
      bytes.extend_from_slice(pkt::FLUSH_PKT);
      bytes
    }
    GitProtocol::V2 => Vec::new(),
  };

  let body = match stream_child(
    &state,
    &pack,
    protocol,
    Mode::Advertise,
    None,
    preamble,
    // Scanned: an advertisement is pkt-lines of ref names and never carries
    // object content, so a reserved-namespace check here cannot false-positive.
    true,
    &rid,
  )
  .await
  {
    Ok(body) => body,
    Err(e) => return fail(METRIC_ADVERTISE, &e, &rid, started),
  };

  audit::success(
    Action::GitFetch,
    &AuditRecord {
      subject: Some(&identity.subject),
      repository_id: Some(&repo_id),
      request_id: Some(rid.as_str()),
      ..Default::default()
    },
  );
  observability::record_request(METRIC_ADVERTISE, None, started.elapsed());

  let mut response = (StatusCode::OK, body).into_response();
  git_response_headers(response.headers_mut(), ADVERTISEMENT_TYPE, &rid);
  response
}

// ---------------------------------------------------------------------------
// POST /git-upload-pack
// ---------------------------------------------------------------------------

async fn upload_pack_rpc(
  State(state): State<GatewayState>,
  Path(repository_id): Path<String>,
  headers: HeaderMap,
  body: Bytes,
) -> Response {
  let rid = request_id(&headers);
  let started = std::time::Instant::now();

  let prepared = async {
    let (identity, repo_id, repo_path) = resolve(&state, &repository_id, &headers).await?;
    let pack = UploadPack::new(&repo_path, (*state.policy).clone())?;

    // Decompress, then validate, then -- and only then -- spawn. Both steps run
    // before a subprocess exists to attack, which is the point of doing the
    // filter check here rather than trusting Git's coarser configuration.
    let request = if header_str(&headers, header::CONTENT_ENCODING.as_str())
      .is_some_and(|v| v.eq_ignore_ascii_case("gzip"))
    {
      pack.decompress_body(&body)?
    } else {
      body.to_vec()
    };
    pack.validate_request(&request)?;
    Ok((identity, repo_id, pack, request))
  }
  .await;

  let (identity, repo_id, pack, request) = match prepared {
    Ok(v) => v,
    Err(e) => return fail(METRIC_RPC, &e, &rid, started),
  };

  let protocol = GitProtocol::from_header(header_str(&headers, "git-protocol"));
  let body = match stream_child(
    &state,
    &pack,
    protocol,
    Mode::StatelessRpc,
    Some(request),
    Vec::new(),
    // Never scanned: this response carries a packfile, whose bytes are
    // arbitrary repository content. See the note in `pkt`.
    false,
    &rid,
  )
  .await
  {
    Ok(body) => body,
    Err(e) => return fail(METRIC_RPC, &e, &rid, started),
  };

  audit::success(
    Action::GitFetch,
    &AuditRecord {
      subject: Some(&identity.subject),
      repository_id: Some(&repo_id),
      request_id: Some(rid.as_str()),
      ..Default::default()
    },
  );
  observability::record_request(METRIC_RPC, None, started.elapsed());

  let mut response = (StatusCode::OK, body).into_response();
  git_response_headers(response.headers_mut(), RESULT_TYPE, &rid);
  response
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

/// Spawn the child and turn its stdout into a response body.
///
/// Returns an error -- and therefore a proper HTTP status -- when the child
/// fails before producing anything. That is why the first chunk is awaited here
/// rather than inside the pump: once one byte of a 200 response has been
/// written, a failure can only be reported by truncating the stream, and "the
/// repository is unreadable" deserves a status code.
#[allow(clippy::too_many_arguments)]
async fn stream_child(
  state: &GatewayState,
  pack: &UploadPack,
  protocol: GitProtocol,
  mode: Mode,
  stdin_bytes: Option<Vec<u8>>,
  preamble: Vec<u8>,
  scan_advertisement: bool,
  rid: &RequestId,
) -> Result<Body, XvfsError> {
  // Admission before spawn. `try_acquire_owned` rather than `acquire`: a client
  // that would have to queue is told to retry, which it can act on, instead of
  // holding a connection open behind an unknown number of gigabyte clones.
  let permit = Arc::clone(&state.admission)
    .try_acquire_owned()
    .map_err(|_| {
      XvfsError::new(
        ErrorCode::ResourceLimit,
        "too many concurrent Git transfers; retry shortly",
      )
    })?;

  let mut child = pack.spawn(protocol, mode)?;
  let mut stdout = child
    .stdout
    .take()
    .ok_or_else(|| XvfsError::new(ErrorCode::Internal, "upload-pack stdout was not piped"))?;
  let stderr = child.stderr.take();

  // Written concurrently with reading stdout, never before it. upload-pack can
  // begin answering while the request is still arriving, and a 16 MiB body
  // written first would deadlock against a full stdout pipe.
  if let Some(bytes) = stdin_bytes {
    if let Some(mut stdin) = child.stdin.take() {
      tokio::spawn(async move {
        let _ = stdin.write_all(&bytes).await;
        // Closing stdin is what tells upload-pack the request is complete.
        let _ = stdin.shutdown().await;
      });
    }
  }

  // Drained concurrently and bounded. An unread stderr pipe fills at 64 KiB and
  // blocks the child forever; retaining all of it would let a chatty failure
  // become the memory limit.
  let stderr_task = stderr.map(|mut stderr| {
    let cap = pack.policy().limits.max_stderr_bytes;
    tokio::spawn(async move {
      let mut kept = Vec::new();
      let mut buf = vec![0u8; 4096];
      while let Ok(n) = stderr.read(&mut buf).await {
        if n == 0 {
          break;
        }
        if kept.len() < cap {
          let room = cap - kept.len();
          kept.extend_from_slice(&buf[..n.min(room)]);
        }
      }
      String::from_utf8_lossy(&kept).into_owned()
    })
  });

  let (tx, mut rx) =
    tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(STREAM_CHANNEL_DEPTH);

  let limits = pack.policy().limits.clone();
  let hidden = pack.policy().hidden_ref_prefixes.clone();
  let pack_for_log = pack.clone();
  let rid_owned = rid.clone();

  tokio::spawn(async move {
    // The permit and the child both live here, so both are released when this
    // task ends -- including when it ends because the client hung up.
    let _permit = permit;
    let deadline = tokio::time::Instant::now() + limits.wall_clock;
    let mut scanner = scan_advertisement.then(|| pkt::AdvertisementScanner::new(&hidden));
    let mut sent: u64 = 0;

    if !preamble.is_empty() && tx.send(Ok(preamble.into())).await.is_err() {
      return;
    }

    let mut buf = vec![0u8; READ_CHUNK_BYTES];
    let outcome = loop {
      let read = tokio::select! {
        biased;
        _ = tokio::time::sleep_until(deadline) => break Err(timeout_error("wall-clock deadline")),
        read = tokio::time::timeout(limits.inactivity, stdout.read(&mut buf)) => read,
      };
      let n = match read {
        Ok(Ok(0)) => break Ok(()),
        Ok(Ok(n)) => n,
        Ok(Err(e)) => break Err(e),
        Err(_) => break Err(timeout_error("no output within the inactivity window")),
      };
      sent += n as u64;
      if sent > limits.max_output_bytes {
        break Err(timeout_error("output exceeded the per-transfer byte limit"));
      }
      let chunk = match scanner.as_mut() {
        Some(scanner) => match scanner.push(&buf[..n]) {
          Ok(bytes) => bytes,
          // Fail-closed: a reserved ref reached the wire, so the transfer is
          // aborted rather than completed with the leak in it.
          Err(e) => {
            tracing::error!(request_id = %rid_owned, error = %e, "aborting Git advertisement");
            break Err(std::io::Error::other(e.message));
          }
        },
        None => buf[..n].to_vec(),
      };
      if chunk.is_empty() {
        continue;
      }
      // The backpressure point: a full channel stops this loop, which stops
      // draining the child's stdout pipe, which blocks the child.
      if tx.send(Ok(chunk.into())).await.is_err() {
        // The client disconnected. Dropping `child` kills it.
        tracing::debug!(request_id = %rid_owned, "Git client disconnected mid-transfer");
        return;
      }
    };

    if let (Ok(()), Some(scanner)) = (&outcome, scanner.as_mut()) {
      match scanner.finish() {
        Ok(tail) if !tail.is_empty() => {
          let _ = tx.send(Ok(tail.into())).await;
        }
        Ok(_) => {}
        Err(e) => {
          let _ = tx.send(Err(std::io::Error::other(e.message))).await;
          return;
        }
      }
    }

    let status = match &outcome {
      Ok(()) => child.wait().await.ok(),
      Err(_) => {
        let _ = child.start_kill();
        child.wait().await.ok()
      }
    };
    let stderr = match stderr_task {
      Some(task) => task.await.unwrap_or_default(),
      None => String::new(),
    };

    let failed = outcome.is_err() || status.is_none_or(|s| !s.success());
    if failed {
      // Unredacted to the server's own log, redacted to nobody: the client gets
      // a truncated stream, not a message, because the 200 header is long gone.
      tracing::warn!(
        request_id = %rid_owned,
        status = ?status,
        stderr = %pack_for_log.redact(stderr.trim()),
        "upload-pack failed"
      );
      let message = match outcome {
        Err(e) => e.to_string(),
        Ok(()) => "upload-pack exited non-zero".to_owned(),
      };
      // An error item truncates the response body, which every Git client
      // reports as a failed transfer. Silently ending the stream would let a
      // failed clone look like an empty repository.
      let _ = tx.send(Err(std::io::Error::other(message))).await;
    }
  });

  // Wait for the first item so a child that dies immediately produces a status
  // code rather than an empty 200.
  let Some(first) = rx.recv().await else {
    return Err(XvfsError::new(
      ErrorCode::Unavailable,
      "upload-pack produced no output",
    ));
  };
  let first = first.map_err(|e| {
    XvfsError::new(
      ErrorCode::Unavailable,
      format!("upload-pack failed: {}", e.kind()),
    )
  })?;

  let stream = tokio_stream::once(Ok(first)).chain(tokio_stream::wrappers::ReceiverStream::new(rx));
  Ok(Body::from_stream(stream))
}

fn timeout_error(what: &str) -> std::io::Error {
  std::io::Error::new(std::io::ErrorKind::TimedOut, what.to_owned())
}

// ---------------------------------------------------------------------------
// Headers and errors
// ---------------------------------------------------------------------------

/// The cache headers `git-http-backend` sends, reproduced exactly.
///
/// Both routes are uncacheable: an advertisement is a snapshot of moving refs,
/// and an RPC result depends on the request body. Git and the proxies between
/// it and here have historically needed all four headers, not just
/// `Cache-Control`.
fn git_response_headers(headers: &mut HeaderMap, content_type: &str, rid: &RequestId) {
  set(headers, header::CONTENT_TYPE.as_str(), content_type);
  set(
    headers,
    header::CACHE_CONTROL.as_str(),
    "no-cache, max-age=0, must-revalidate",
  );
  set(
    headers,
    header::EXPIRES.as_str(),
    "Fri, 01 Jan 1980 00:00:00 GMT",
  );
  set(headers, header::PRAGMA.as_str(), "no-cache");
  set(headers, observability::REQUEST_ID_KEY, rid.as_str());
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
  headers.get(name).and_then(|v| v.to_str().ok())
}

fn set(headers: &mut HeaderMap, name: &str, value: &str) {
  if let (Ok(name), Ok(value)) = (
    header::HeaderName::from_bytes(name.as_bytes()),
    header::HeaderValue::from_str(value),
  ) {
    headers.insert(name, value);
  }
}

fn request_id(headers: &HeaderMap) -> RequestId {
  RequestId::from_client(header_str(headers, observability::REQUEST_ID_KEY))
}

/// Turn an error into a Git-appropriate HTTP response.
///
/// The body is deliberately terse. Git shows a server's error text to the user,
/// and this surface's errors can mention repository paths and object IDs, so the
/// detail goes to the audit log and the trace instead.
fn fail(
  metric: &'static str,
  error: &XvfsError,
  rid: &RequestId,
  started: std::time::Instant,
) -> Response {
  audit::failure(
    Action::GitFetch,
    &AuditRecord {
      request_id: Some(rid.as_str()),
      ..Default::default()
    },
    error.code,
  );
  observability::record_request(metric, Some(error.code), started.elapsed());

  let status =
    StatusCode::from_u16(error.code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
  let mut response = (status, format!("{}\n", error.message)).into_response();
  set(
    response.headers_mut(),
    header::CONTENT_TYPE.as_str(),
    "text/plain; charset=utf-8",
  );
  // Without this, Git reports "authentication failed" and never asks for a
  // credential, so a user with a working helper cannot get past a 401.
  if error.code == ErrorCode::Unauthenticated {
    set(
      response.headers_mut(),
      header::WWW_AUTHENTICATE.as_str(),
      "Basic realm=\"XVFS\"",
    );
  }
  set(
    response.headers_mut(),
    observability::REQUEST_ID_KEY,
    rid.as_str(),
  );
  response
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn both_credential_schemes_git_can_send_are_accepted() {
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, "Bearer tok-123".parse().unwrap());
    assert_eq!(credential(&headers).as_deref(), Some("tok-123"));

    // `x-access-token:<token>`, which is what a URL userinfo or a credential
    // helper produces.
    let encoded = base64_encode(b"x-access-token:tok-123");
    headers.insert(
      header::AUTHORIZATION,
      format!("Basic {encoded}").parse().unwrap(),
    );
    assert_eq!(credential(&headers).as_deref(), Some("tok-123"));

    // A userinfo with no password is read as the token itself.
    let encoded = base64_encode(b"tok-123:");
    headers.insert(
      header::AUTHORIZATION,
      format!("Basic {encoded}").parse().unwrap(),
    );
    assert_eq!(credential(&headers).as_deref(), Some("tok-123"));

    headers.insert(header::AUTHORIZATION, "Digest whatever".parse().unwrap());
    assert_eq!(credential(&headers), None);
    assert_eq!(credential(&HeaderMap::new()), None);
  }

  #[test]
  fn base64_decoding_handles_padding_and_rejects_the_url_alphabet() {
    for plain in [
      &b""[..],
      b"a",
      b"ab",
      b"abc",
      b"abcd",
      b"user:pass",
      &[0xfb, 0xff, 0xbf][..],
    ] {
      let encoded = base64_encode(plain);
      assert_eq!(base64_decode(&encoded).as_deref(), Some(plain), "{plain:?}");
    }
    // The url alphabet is a different encoding and must not silently decode to
    // different bytes than the sender meant.
    assert_eq!(base64_decode("a-b_"), None);
    assert_eq!(base64_decode("not base64!"), None);
  }

  fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
      let b = [
        chunk[0],
        chunk.get(1).copied().unwrap_or(0),
        chunk.get(2).copied().unwrap_or(0),
      ];
      let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
      for i in 0..4 {
        if i <= chunk.len() {
          out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
        } else {
          out.push('=');
        }
      }
    }
    out
  }
}
