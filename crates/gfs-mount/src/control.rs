//! The control socket: how `gfs` talks to `gfs-fuse`.
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
//!
//! # Two sockets, two vocabularies
//!
//! [`Request`] is what a job asks *its own workspace*, and it is answered on the
//! per-mount socket at `<workspace>.gfs/control.sock`. It carries no mount
//! selector because the socket it arrives on is the selector — which is what lets
//! `gfs rg` work by walking up from the current directory with no flags at all.
//!
//! [`HostRequest`] is what the CLI asks the `gfs-fuse` *process*: create a mount,
//! list them, tear one down. It is answered on a single host socket shared by
//! every mount, because none of those questions belong to a workspace.
//!
//! Keeping them apart is what makes one process able to serve many mounts without
//! a protocol change: a host binds N per-mount sockets, and everything that speaks
//! [`Request`] is unaware there was ever one process per mount.

use std::path::{Path, PathBuf};

use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::Timestamp;

use crate::lease::{HealthState, LeaseHealth};

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
  /// Re-point the view at another revision and publish it.
  ///
  /// The selector is a *commit* rather than a branch name, because unpushed
  /// work lives in the reserved namespace and ADR 0006 forbids naming that as a
  /// revision. The CLI resolves the branch through the gateway and sends what it
  /// resolved to; `branch` travels alongside only so reports can name it.
  Switch {
    selector: String,
    branch: Option<String>,
  },
  /// What the workspace has changed, from the journal alone.
  Status,
  /// The same change set, rendered as a Git-compatible patch.
  Diff,
  /// Write an atomic, checksummed export bundle.
  Export { bundle: std::path::PathBuf },
  /// Search the merged workspace: the pinned commit's index, minus what the
  /// overlay changed, plus what it has instead.
  Search(Box<crate::search::SearchRequest>),
  /// The pinned commit's ancestry, newest first. Answered by the server's
  /// revwalk: the workspace has no object database to walk.
  ///
  /// `from` is a revision expression -- `HEAD~3`, a branch, a commit id -- and
  /// `None` means the pin. `HEAD` inside it means the **pinned commit**, not the
  /// repository's default branch, because after `gfs switch -c` those are not
  /// the same place and the caller is standing on the former.
  Log {
    skip: u32,
    limit: u32,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    first_parent: bool,
    /// Root-relative paths, base64url-encoded. A Git path is bytes and this is
    /// a JSON protocol.
    #[serde(default)]
    paths_b64url: Vec<String>,
  },
  /// What changed between two revisions, rendered by the server.
  ///
  /// The thing a workspace cannot answer for itself: ADR 0005 left the mount
  /// with no object database, so without this a caller reviewing a commit had to
  /// walk trees and fetch blobs -- exactly the hydration the partial clone was
  /// rejected for. The gateway renders it and the mount reads nothing.
  DiffRevs {
    /// The old side. `None` means "the first parent of `to`", which the daemon
    /// resolves; a root commit then diffs against the empty tree.
    from: Option<String>,
    to: String,
    /// Which parent to use when `from` is `None` and `to` is a merge. 1-based,
    /// as `git show -m` numbers them.
    #[serde(default)]
    parent: Option<u32>,
    format: gfs_types::DiffFormat,
    #[serde(default)]
    context_lines: Option<u32>,
    #[serde(default)]
    paths_b64url: Vec<String>,
  },
  /// One directory of any revision the workspace can reach.
  ///
  /// Routed through the daemon rather than read from the server directly, and
  /// that is the whole reason it exists: after `gfs switch -c` the pin is a
  /// commit on a branch in the reserved namespace, reachable from no visible
  /// ref, and only this mount's capability opens it.
  Ls {
    #[serde(default)]
    rev: Option<String>,
    path_b64url: String,
    page_size: u32,
  },
  /// One file's raw bytes from any revision the workspace can reach.
  Cat {
    #[serde(default)]
    rev: Option<String>,
    path_b64url: String,
  },
  /// Line-by-line attribution for one path at one revision.
  Blame {
    #[serde(default)]
    rev: Option<String>,
    /// base64url, for the reason the diff's paths are.
    path_b64url: String,
  },
  /// Filename search over the merged workspace.
  Find(Box<crate::find::FindRequest>),
  /// Everything the workspace changed, with the bytes, for the CLI to commit.
  ///
  /// The daemon collects but does not send: the control socket has no server
  /// credential, so the CLI is what talks to the gateway. See `CommitPlan`.
  CommitPlan,
  /// Adopt a commit that was just made, and re-pin the view to it.
  AdoptCommit { commit: String },
  /// Release the lease, unmount, and exit.
  Unmount,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MountReport {
  pub mount_id: String,
  pub repository_id: String,
  pub revision_selector: String,
  /// The work branch this view is on, when `gfs switch` put it on one.
  #[serde(default)]
  pub work_branch: Option<String>,
  pub commit: String,
  pub tree: String,
  pub ref_name: Option<String>,
  pub snapshot_time: Timestamp,
  pub workspace: String,
  pub publication: String,
  /// How many times this mount has been re-pinned. Not a generation: nothing
  /// is kept alive alongside it, and `gfs switch` replaces the pin in place.
  pub generation: u64,
  pub state_dir: String,
  pub daemon_pid: u32,
  /// The UID the mount reports as owner. An operator comparing this with the
  /// job's UID is the fastest way to diagnose a `safe.directory` refusal.
  pub owner_uid: u32,
  /// False from M3 onwards. Kept in the report because an orchestrator reading
  /// it should not have to infer writability from the absence of a field.
  pub read_only: bool,
  pub overlay: gfs_overlay::OverlayStats,
  pub health: LeaseHealth,
  pub stats: crate::fs::FsStats,
  pub cache: crate::cache::CacheStats,
  /// The hydration budget's ledger (ADR 0009). `refusals > 0` is the first thing
  /// to check when a job reports `EDQUOT`.
  #[serde(default)]
  pub budget: crate::budget::BudgetReport,
  pub live_inodes: usize,
  pub assigned_inodes: usize,
}

/// One commit, rendered for a log.
///
/// Strings and raw bytes rather than domain types: this crosses the control
/// socket as JSON, and a commit message and an author name are both allowed to be
/// non-UTF-8 (DESIGN.md's rule for `Signature.name`), so they travel as bytes and
/// are decoded — or escaped — by whatever prints them.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LogEntry {
  pub commit: String,
  /// The commit's tree, so `--format=%T` needs no second round trip.
  #[serde(default)]
  pub tree: String,
  pub parents: Vec<String>,
  pub author_name: Vec<u8>,
  pub author_email: Vec<u8>,
  pub author_time: i64,
  pub author_tz_offset_minutes: i32,
  pub committer_name: Vec<u8>,
  pub committer_email: Vec<u8>,
  pub committer_time: i64,
  /// Carried alongside the committer time because `%cd` has to print the offset
  /// the commit was made at, not the reader's.
  #[serde(default)]
  pub committer_tz_offset_minutes: i32,
  pub message: Vec<u8>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LogReport {
  /// The commit the walk started from: the workspace's pin, or whatever `from`
  /// resolved to.
  pub base_commit: String,
  pub ref_name: Option<String>,
  pub commits: Vec<LogEntry>,
  /// True when ancestry remains past this page.
  pub has_more: bool,
}

/// One file's line in a diff summary, as the control socket carries it.
///
/// Paths are base64url rather than `BytePath`, for the reason [`LogEntry`]'s
/// fields are bytes: this is JSON and a Git path is not required to be UTF-8.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DiffFileEntry {
  pub path_b64url: String,
  pub old_path_b64url: Option<String>,
  pub status: gfs_types::DiffStatus,
  pub additions: u32,
  pub deletions: u32,
  pub binary: bool,
  pub old_mode: u32,
  pub new_mode: u32,
}

/// A rendered diff between two revisions.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RevDiffReport {
  /// The old side, or `None` when the new side is a root commit.
  pub base_commit: Option<String>,
  pub commit: String,
  /// The rendered diff, base64url-encoded. It carries file content and paths,
  /// neither of which is required to be UTF-8, and it has to stay byte-exact to
  /// be applied.
  pub rendered_b64url: String,
  pub files: Vec<DiffFileEntry>,
  /// True when the rendering hit the server's byte ceiling. An answer, not a
  /// failure -- but one a caller must not mistake for the whole diff.
  pub truncated: bool,
}

/// One blame hunk, as the control socket carries it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct BlameHunkEntry {
  pub final_start_line: u32,
  pub lines: u32,
  pub commit: String,
  pub orig_path_b64url: String,
  pub orig_start_line: u32,
  pub author_name: Vec<u8>,
  pub author_email: Vec<u8>,
  pub author_time: i64,
  pub author_tz_offset_minutes: i32,
  pub boundary: bool,
}

/// One entry of a snapshot directory listing.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LsEntry {
  /// Root-relative, always — including when a subdirectory was listed. That is
  /// what `Cat` takes, so the output of one is the input of the other.
  pub path_b64url: String,
  pub mode: u32,
  pub size: u64,
  pub oid: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LsReport {
  pub commit: String,
  pub entries: Vec<LsEntry>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct BlameReport {
  pub commit: String,
  pub path_b64url: String,
  pub hunks: Vec<BlameHunkEntry>,
  /// The file's bytes, base64url-encoded. Empty when `truncated`.
  pub content_b64url: String,
  pub truncated: bool,
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
/// What a commit would contain, as the daemon sees it.
///
/// Content travels base64url-encoded for the same reason the diff does: this is
/// a JSON protocol and a Git blob is arbitrary bytes.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CommitPlan {
  pub base_commit: String,
  pub work_branch: Option<String>,
  pub changes: Vec<PlannedChange>,
  /// Path prefixes the workspace deleted wholesale, for the server to expand.
  pub deleted_directories: Vec<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlannedChange {
  Upsert {
    path: String,
    mode: u32,
    content_b64url: String,
  },
  Delete {
    path: String,
  },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct StatusReport {
  pub base_commit: String,
  pub ref_name: Option<String>,
  /// The work branch this view is on, when `gfs switch` put it on one.
  ///
  /// Separate from `ref_name`, which is the *base* ref the commit was resolved
  /// through. After a switch the view is pinned to a bare commit, so `ref_name`
  /// is `None` and only this says where the work is going.
  #[serde(default)]
  pub work_branch: Option<String>,
  #[serde(flatten)]
  pub status: gfs_overlay::Status,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
  Inspect(Box<MountReport>),
  Health(LeaseHealth),
  Refresh(RefreshReport),
  CommitPlan(Box<CommitPlan>),
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
  Export(gfs_overlay::ExportReport),
  Search(Box<crate::search::SearchReport>),
  Log(Box<LogReport>),
  RevDiff(Box<RevDiffReport>),
  Blame(Box<BlameReport>),
  Ls(Box<LsReport>),
  /// One file's bytes, base64url-encoded. Bytes for the reason the patch is:
  /// file content is not required to be UTF-8, and this is a JSON protocol.
  Cat {
    commit: String,
    content_b64url: String,
  },
  Find(Box<crate::find::FindReport>),
  Unmounted,
  Error {
    code: String,
    message: String,
  },
}

impl Response {
  pub fn from_error(e: &GfsError) -> Self {
    Response::Error {
      code: e.code.as_str().to_owned(),
      message: e.message.clone(),
    }
  }

  /// Turn an error response back into a typed error, so the CLI's exit code and
  /// message come from the daemon's own vocabulary rather than being re-invented.
  pub fn into_result(self) -> Result<Response, GfsError> {
    match self {
      Response::Error { code, message } => Err(GfsError::new(code_from_wire(&code), message)),
      other => Ok(other),
    }
  }
}

/// Everything a host needs to create one mount.
///
/// A flattened wire form of `MountSpec` rather than the spec itself: the spec
/// holds a `LeasePolicy` and an `FsConfig`, which are the host's business and not
/// a caller's, and letting a client set them over a socket would put policy on
/// the wrong side of the boundary.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MountRequest {
  /// Refused rather than misread when it does not match the host's build. The
  /// state directory format is the thing a CLI and a host have to agree about,
  /// and a long-lived host is exactly where a version skew shows up.
  pub state_format_version: u32,
  pub workspace: PathBuf,
  pub state_dir: PathBuf,
  pub cache_dir: PathBuf,
  pub repository_id: String,
  pub revision_selector: String,
  pub cache_quota_bytes: u64,
  pub overlay_quota_bytes: u64,
  pub allow_other: bool,
  /// `None` takes the host's default, which is where ADR 0003's "more than one
  /// event-loop thread" rule is expressed. A caller overrides it only to
  /// experiment.
  pub fuse_threads: Option<usize>,
  /// Where this mount's gateway is. `None` means the host's own default, which
  /// is the ordinary case; a caller sends one only to reach a different gateway
  /// than the host was started against.
  pub grpc_endpoint: Option<String>,
  pub http_endpoint: Option<String>,
  pub token: Option<String>,
}

/// What the CLI asks the `gfs-fuse` process, as opposed to one workspace.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum HostRequest {
  /// Which host is this, and what is it serving. Also the liveness probe.
  Info,
  /// Mount a repository and publish it. Answers once the workspace is usable and
  /// its per-mount control socket is bound.
  CreateMount(Box<MountRequest>),
  ListMounts,
  /// Tear one mount down. `gfs unmount` reaches the same code through the mount's
  /// own socket; this exists for an orchestrator holding only the host socket.
  DestroyMount {
    workspace: PathBuf,
  },
  /// Tear down every mount and exit.
  Shutdown,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct HostInfo {
  pub version: String,
  pub state_format_version: u32,
  pub api_version: String,
  pub pid: u32,
  pub socket: PathBuf,
  pub grpc_endpoint: String,
  pub http_endpoint: String,
  pub mounts: usize,
}

/// One mount, as the host lists it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MountSummary {
  pub workspace: PathBuf,
  pub state_dir: PathBuf,
  pub repository_id: String,
  pub mount_id: String,
  pub revision_selector: String,
  pub commit: String,
  pub generation: u64,
  pub health: HealthState,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum HostResponse {
  Info(Box<HostInfo>),
  /// The same report `gfs inspect` prints, so `gfs mount` needs no second round
  /// trip to say what it just created.
  Mounted(Box<MountReport>),
  /// A struct variant rather than `Mounts(Vec<_>)`: this enum is internally
  /// tagged, and serde cannot put a tag on a newtype variant wrapping a sequence
  /// — it fails at serialization time, where the only symptom is a reply that
  /// never arrives.
  Mounts {
    mounts: Vec<MountSummary>,
  },
  Destroyed,
  ShuttingDown,
  Error {
    code: String,
    message: String,
  },
}

impl HostResponse {
  pub fn from_error(e: &GfsError) -> Self {
    HostResponse::Error {
      code: e.code.as_str().to_owned(),
      message: e.message.clone(),
    }
  }

  pub fn into_result(self) -> Result<HostResponse, GfsError> {
    match self {
      HostResponse::Error { code, message } => Err(GfsError::new(code_from_wire(&code), message)),
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

/// Send one request to a mount's own socket and read its reply.
///
/// Synchronous and dependency-free on purpose: this is what the CLI calls, and
/// the CLI should not need a runtime to ask a local daemon a question.
pub fn call(socket: &Path, request: &Request) -> Result<Response, GfsError> {
  roundtrip(socket, request)
}

/// Send one request to a host socket and read its reply.
pub fn call_host(socket: &Path, request: &HostRequest) -> Result<HostResponse, GfsError> {
  roundtrip(socket, request)
}

/// One line out, one line back. Shared by both vocabularies because the framing
/// is the protocol — the only thing that differs is what the line decodes to.
fn roundtrip<Req, Res>(socket: &Path, request: &Req) -> Result<Res, GfsError>
where
  Req: serde::Serialize,
  Res: serde::de::DeserializeOwned,
{
  use std::io::{BufRead, BufReader, Write};

  let mut stream = std::os::unix::net::UnixStream::connect(socket).map_err(|e| {
    let code = match e.kind() {
      std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => ErrorCode::NotFound,
      std::io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
      _ => ErrorCode::Unavailable,
    };
    GfsError::new(
      code,
      format!(
        "no GFS daemon is listening on {}: {}",
        socket.display(),
        e.kind()
      ),
    )
  })?;
  let mut line = serde_json::to_string(request)
    .map_err(|e| GfsError::internal(format!("encoding a control request: {e}")))?;
  line.push('\n');
  stream.write_all(line.as_bytes()).map_err(|e| {
    GfsError::new(
      ErrorCode::Unavailable,
      format!("control write: {}", e.kind()),
    )
  })?;
  stream.flush().map_err(|e| {
    GfsError::new(
      ErrorCode::Unavailable,
      format!("control flush: {}", e.kind()),
    )
  })?;

  let mut reply = String::new();
  BufReader::new(&stream).read_line(&mut reply).map_err(|e| {
    GfsError::new(
      ErrorCode::Unavailable,
      format!("control read: {}", e.kind()),
    )
  })?;
  if reply.trim().is_empty() {
    return Err(GfsError::new(
      ErrorCode::Unavailable,
      "the daemon closed the control connection without replying",
    ));
  }
  serde_json::from_str::<Res>(&reply)
    .map_err(|e| GfsError::internal(format!("decoding a control response: {e}")))
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

    let response = Response::from_error(&GfsError::not_found("nothing here"));
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
      std::path::Path::new("/nonexistent/gfs/control.sock"),
      &Request::Health,
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
    assert!(!is_live(std::path::Path::new(
      "/nonexistent/gfs/control.sock"
    )));
  }
}
