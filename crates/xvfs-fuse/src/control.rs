//! The control socket: how `xvfs` talks to `xvfsd`.
//!
//! DESIGN.md section 8.1 gives a job "only a mount handle and scoped CLI socket",
//! and ADR 0006's threat model fixes the socket at mode 0600 carrying the job's
//! scoped capability rather than repository credentials. That is the whole
//! security story here: the socket cannot be opened by another user, and nothing
//! that crosses it is a repository credential.
//!
//! # Why a socket rather than signals or a pid file
//!
//! `unmount`, `refresh`, and `health` all need a *reply* — whether the workspace
//! was clean, which generation is now published, whether the lease is renewing.
//! A signal carries no reply, and a status file cannot report the outcome of an
//! action that has not happened yet.
//!
//! # The protocol
//!
//! One JSON request per line, one JSON response per line, then the connection
//! closes. Line-delimited rather than length-prefixed because the messages are
//! small, the peer is on the same host, and a protocol a human can drive with
//! `socat` is worth something during an incident.

use std::path::Path;

use xvfs_types::error::{ErrorCode, XvfsError};
use xvfs_types::Timestamp;

use crate::lease::LeaseHealth;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
  /// Everything about the mount, for humans and for the orchestrator.
  Inspect,
  /// Just the lease and daemon health. Separate from `Inspect` because a
  /// liveness probe runs constantly and should not pay for the rest.
  Health,
  /// Replace the published generation with a freshly resolved one.
  Refresh,
  /// What the workspace has changed, from the journal alone.
  Status,
  /// The same change set, rendered as a Git-compatible patch.
  Diff,
  /// Write an atomic, checksummed export bundle.
  Export { bundle: std::path::PathBuf },
  /// Release the lease, unmount, and exit.
  Unmount,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MountReport {
  pub mount_id: String,
  pub repository_id: String,
  pub revision_selector: String,
  pub commit: String,
  pub tree: String,
  pub ref_name: Option<String>,
  pub snapshot_time: Timestamp,
  pub workspace: String,
  pub publication: String,
  pub generation: u64,
  /// Generations still alive because handles opened through them are still open.
  pub retiring_generations: Vec<u64>,
  pub state_dir: String,
  pub daemon_pid: u32,
  /// The UID the mount reports as owner. An operator comparing this with the
  /// job's UID is the fastest way to diagnose a `safe.directory` refusal.
  pub owner_uid: u32,
  /// False from M3 onwards. Kept in the report because an orchestrator reading
  /// it should not have to infer writability from the absence of a field.
  pub read_only: bool,
  pub overlay: xvfs_overlay::OverlayStats,
  pub health: LeaseHealth,
  pub stats: crate::fs::FsStats,
  pub cache: crate::cache::CacheStats,
  pub live_inodes: usize,
  pub assigned_inodes: usize,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RefreshReport {
  pub previous_generation: u64,
  pub generation: u64,
  pub previous_commit: String,
  pub commit: String,
  /// True when the refresh resolved to the same commit, so nothing moved.
  pub unchanged: bool,
}

/// A change set plus the commit it is relative to.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct StatusReport {
  pub base_commit: String,
  pub ref_name: Option<String>,
  #[serde(flatten)]
  pub status: xvfs_overlay::Status,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
  Inspect(Box<MountReport>),
  Health(LeaseHealth),
  Refresh(RefreshReport),
  Status(Box<StatusReport>),
  /// The patch, base64url-encoded.
  ///
  /// A patch is bytes -- it contains the workspace's file content, which is not
  /// required to be UTF-8, and paths that are not either. Encoding it keeps the
  /// one-JSON-object-per-line protocol intact without a lossy conversion in the
  /// middle of the one artifact that has to be byte-exact.
  Diff {
    patch_b64url: String,
  },
  Export(xvfs_overlay::ExportReport),
  Unmounted,
  Error {
    code: String,
    message: String,
  },
}

impl Response {
  pub fn from_error(e: &XvfsError) -> Self {
    Response::Error {
      code: e.code.as_str().to_owned(),
      message: e.message.clone(),
    }
  }

  /// Turn an error response back into a typed error, so the CLI's exit code and
  /// message come from the daemon's own vocabulary rather than being re-invented.
  pub fn into_result(self) -> Result<Response, XvfsError> {
    match self {
      Response::Error { code, message } => Err(XvfsError::new(code_from_wire(&code), message)),
      other => Ok(other),
    }
  }
}

fn code_from_wire(s: &str) -> ErrorCode {
  match s {
    "INVALID_ARGUMENT" => ErrorCode::InvalidArgument,
    "NOT_FOUND" => ErrorCode::NotFound,
    "PERMISSION_DENIED" => ErrorCode::PermissionDenied,
    "UNAUTHENTICATED" => ErrorCode::Unauthenticated,
    "EXPIRED" => ErrorCode::Expired,
    "FAILED_PRECONDITION" => ErrorCode::FailedPrecondition,
    "CONFLICT" => ErrorCode::Conflict,
    "RESOURCE_LIMIT" => ErrorCode::ResourceLimit,
    "UNAVAILABLE" => ErrorCode::Unavailable,
    "DEADLINE_EXCEEDED" => ErrorCode::DeadlineExceeded,
    "CANCELLED" => ErrorCode::Cancelled,
    _ => ErrorCode::Internal,
  }
}

/// Send one request to a daemon and read its reply.
///
/// Synchronous and dependency-free on purpose: this is what the CLI calls, and
/// the CLI should not need a runtime to ask a local daemon a question.
pub fn call(socket: &Path, request: &Request) -> Result<Response, XvfsError> {
  use std::io::{BufRead, BufReader, Write};

  let mut stream = std::os::unix::net::UnixStream::connect(socket).map_err(|e| {
    let code = match e.kind() {
      std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => ErrorCode::NotFound,
      std::io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
      _ => ErrorCode::Unavailable,
    };
    XvfsError::new(
      code,
      format!(
        "no XVFS daemon is listening on {}: {}",
        socket.display(),
        e.kind()
      ),
    )
  })?;
  let mut line = serde_json::to_string(request)
    .map_err(|e| XvfsError::internal(format!("encoding a control request: {e}")))?;
  line.push('\n');
  stream.write_all(line.as_bytes()).map_err(|e| {
    XvfsError::new(
      ErrorCode::Unavailable,
      format!("control write: {}", e.kind()),
    )
  })?;
  stream.flush().map_err(|e| {
    XvfsError::new(
      ErrorCode::Unavailable,
      format!("control flush: {}", e.kind()),
    )
  })?;

  let mut reply = String::new();
  BufReader::new(&stream).read_line(&mut reply).map_err(|e| {
    XvfsError::new(
      ErrorCode::Unavailable,
      format!("control read: {}", e.kind()),
    )
  })?;
  if reply.trim().is_empty() {
    return Err(XvfsError::new(
      ErrorCode::Unavailable,
      "the daemon closed the control connection without replying",
    ));
  }
  serde_json::from_str::<Response>(&reply)
    .map_err(|e| XvfsError::internal(format!("decoding a control response: {e}")))
}

/// Whether a daemon is alive on this socket.
///
/// Used before adopting a state directory: a stale socket file left by a killed
/// daemon must be removed, and a live one must not be.
pub fn is_live(socket: &Path) -> bool {
  std::os::unix::net::UnixStream::connect(socket).is_ok()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn requests_and_responses_round_trip_as_one_line_each() {
    let request = Request::Refresh;
    let encoded = serde_json::to_string(&request).unwrap();
    assert!(!encoded.contains('\n'), "one request per line");
    assert!(matches!(
      serde_json::from_str::<Request>(&encoded).unwrap(),
      Request::Refresh
    ));

    let response = Response::from_error(&XvfsError::not_found("nothing here"));
    let encoded = serde_json::to_string(&response).unwrap();
    assert!(!encoded.contains('\n'));
    let decoded: Response = serde_json::from_str(&encoded).unwrap();
    let error = decoded.into_result().unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);
    assert_eq!(error.message, "nothing here");
  }

  #[test]
  fn a_missing_socket_is_not_found_rather_than_unavailable() {
    // The orchestrator distinguishes "there is no daemon here" (nothing to clean
    // up) from "the daemon is not answering" (retry, then escalate).
    let e = call(
      std::path::Path::new("/nonexistent/xvfs/control.sock"),
      &Request::Health,
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
    assert!(!is_live(std::path::Path::new(
      "/nonexistent/xvfs/control.sock"
    )));
  }
}
