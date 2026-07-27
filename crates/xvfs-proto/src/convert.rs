//! Conversions between wire types and `xvfs-types` domain types.
//!
//! The wire is deliberately loose: object IDs are strings, paths are byte
//! vectors, modes are `uint32`, and a Protobuf message field is always optional
//! whether or not the schema treats it as required. The domain types are strict.
//! Every conversion in the request direction therefore validates, and the
//! validation cannot be skipped, because there is no other constructor a handler
//! can reach.
//!
//! The asymmetry is on purpose:
//!
//! * **Request direction** (`try_*`) validates and returns [`XvfsError`], so a
//!   malformed request becomes a typed `INVALID_ARGUMENT` at the boundary rather
//!   than a surprise several layers down.
//! * **Response direction** (`From`) is infallible, because a domain value that
//!   exists has already been validated and cannot fail to serialize.

use xvfs_types::error::{ErrorCode, XvfsError};
use xvfs_types::{
  BytePath, CommitMeta, EntryKind, HashAlgorithm, ObjectId, ResolvedRevision, RevisionSelector,
  Signature, SnapshotState, Timestamp, TreeEntryInfo,
};

use crate::v1;

// ---------------------------------------------------------------------------
// Timestamps
// ---------------------------------------------------------------------------

impl From<Timestamp> for v1::Timestamp {
  fn from(t: Timestamp) -> Self {
    v1::Timestamp {
      secs: t.secs,
      nanos: t.nanos,
    }
  }
}

impl From<v1::Timestamp> for Timestamp {
  fn from(t: v1::Timestamp) -> Self {
    // `nanos` is clamped rather than rejected. A peer that sends 2_000_000_000
    // nanoseconds is buggy, but the value's *meaning* is unambiguous, and
    // failing a whole request over a field the receiver only displays would be a
    // worse outcome than normalizing it.
    Timestamp::new(t.secs, t.nanos.min(999_999_999))
  }
}

/// A message field that the schema treats as required.
///
/// Protobuf 3 gives every message field optional presence on the wire, so
/// "required" is a schema convention the receiver has to enforce. Doing it
/// through one named helper keeps the error message consistent and makes the
/// enforcement points greppable.
fn required<T>(value: Option<T>, field: &str) -> Result<T, XvfsError> {
  value.ok_or_else(|| XvfsError::invalid(format!("missing required field `{field}`")))
}

// ---------------------------------------------------------------------------
// Entry kinds
// ---------------------------------------------------------------------------

impl From<EntryKind> for v1::EntryKind {
  fn from(k: EntryKind) -> Self {
    match k {
      EntryKind::Regular => v1::EntryKind::Regular,
      EntryKind::Executable => v1::EntryKind::Executable,
      EntryKind::Symlink => v1::EntryKind::Symlink,
      EntryKind::Directory => v1::EntryKind::Directory,
      EntryKind::Gitlink => v1::EntryKind::Gitlink,
      EntryKind::Unsupported(_) => v1::EntryKind::Unsupported,
    }
  }
}

/// Reconstruct an entry kind from the wire.
///
/// The raw `mode` is authoritative and the enum is not consulted, because
/// `ENTRY_KIND_UNSUPPORTED` carries no information on its own -- the mode is
/// where the actual value lives -- and because the mode is what Git stores. A
/// peer whose enum disagrees with its own mode is reporting something
/// inconsistent, and deriving from one field means the two cannot drift.
fn entry_kind_from_wire(mode: u32) -> EntryKind {
  EntryKind::from_mode(mode)
}

// ---------------------------------------------------------------------------
// Object IDs and paths
// ---------------------------------------------------------------------------

/// Parse a qualified object ID and require it to match the repository's
/// algorithm.
///
/// The algorithm check is not redundant with parsing. A well-formed `sha256:...`
/// ID is meaningless against a SHA-1 repository, and accepting it would push the
/// failure down to a libgit2 lookup whose error says nothing about the real
/// problem.
pub fn try_oid(s: &str, algorithm: HashAlgorithm, field: &str) -> Result<ObjectId, XvfsError> {
  if s.is_empty() {
    return Err(XvfsError::invalid(format!(
      "missing required field `{field}`"
    )));
  }
  let oid = if s.contains(':') {
    ObjectId::parse_qualified(s)?
  } else {
    // A bare hex digest is accepted only because the repository context supplies
    // the algorithm, exactly as ADR 0006 allows.
    ObjectId::from_hex(algorithm, s)?
  };
  if oid.algorithm() != algorithm {
    return Err(XvfsError::invalid(format!(
      "`{field}` is {} but this repository is {algorithm}",
      oid.algorithm()
    )));
  }
  Ok(oid)
}

/// Validate a caller-supplied path.
pub fn try_path(bytes: Vec<u8>) -> Result<BytePath, XvfsError> {
  let p = BytePath::new(bytes);
  p.validate()?;
  Ok(p)
}

/// Parse and validate a revision selector.
pub fn try_selector(s: &str, algorithm: HashAlgorithm) -> Result<RevisionSelector, XvfsError> {
  RevisionSelector::parse(s, algorithm)
}

// ---------------------------------------------------------------------------
// Tree entries
// ---------------------------------------------------------------------------

impl From<TreeEntryInfo> for v1::TreeEntry {
  fn from(e: TreeEntryInfo) -> Self {
    v1::TreeEntry {
      path: e.path.into_bytes(),
      kind: v1::EntryKind::from(e.kind) as i32,
      mode: e.mode,
      oid: e.oid.to_qualified(),
      size: e.size,
      symlink_target: e.symlink_target,
      blob_ticket: e.blob_ticket,
    }
  }
}

impl v1::TreeEntry {
  /// Convert a received entry into the domain type.
  ///
  /// The client side of the boundary. A client must not trust a server's entry
  /// any more than a server trusts a client's request: a malformed path or object
  /// ID from a buggy or compromised server would otherwise reach the FUSE layer,
  /// where a path is used to build kernel dentries.
  pub fn try_into_domain(self, algorithm: HashAlgorithm) -> Result<TreeEntryInfo, XvfsError> {
    let oid = try_oid(&self.oid, algorithm, "entry.oid")?;
    // Validated, not merely wrapped: a server returning `../etc` inside a
    // directory listing is exactly what DESIGN.md section 10's traversal rules
    // exist for.
    let path = try_path(self.path)?;
    Ok(TreeEntryInfo {
      path,
      kind: entry_kind_from_wire(self.mode),
      mode: self.mode,
      oid,
      size: self.size,
      symlink_target: self.symlink_target,
      blob_ticket: self.blob_ticket,
    })
  }
}

// ---------------------------------------------------------------------------
// Commits
// ---------------------------------------------------------------------------

impl From<Signature> for v1::Signature {
  fn from(s: Signature) -> Self {
    v1::Signature {
      name: s.name,
      email: s.email,
      time: Some(s.time.into()),
      tz_offset_minutes: s.tz_offset_minutes,
    }
  }
}

impl v1::Signature {
  pub fn try_into_domain(self, field: &str) -> Result<Signature, XvfsError> {
    Ok(Signature {
      name: self.name,
      email: self.email,
      time: required(self.time, &format!("{field}.time"))?.into(),
      tz_offset_minutes: self.tz_offset_minutes,
    })
  }
}

impl From<CommitMeta> for v1::GetCommitResponse {
  fn from(c: CommitMeta) -> Self {
    v1::GetCommitResponse {
      commit_oid: c.commit.to_qualified(),
      tree_oid: c.tree.to_qualified(),
      parent_oids: c.parents.iter().map(ObjectId::to_qualified).collect(),
      author: Some(c.author.into()),
      committer: Some(c.committer.into()),
      message: c.message,
      snapshot_time: Some(c.snapshot_time.into()),
    }
  }
}

impl From<CommitMeta> for v1::LogCommit {
  fn from(c: CommitMeta) -> Self {
    v1::LogCommit {
      commit_oid: c.commit.to_qualified(),
      parent_oids: c.parents.iter().map(ObjectId::to_qualified).collect(),
      author: Some(c.author.into()),
      committer: Some(c.committer.into()),
      message: c.message,
    }
  }
}

impl v1::LogCommit {
  /// The tree and snapshot time a [`CommitMeta`] carries are not on the wire for
  /// a log entry, so they are filled from the commit itself: the tree is set to
  /// the commit's own ID as a placeholder no caller reads, and the time comes
  /// from the committer signature, which is what `git log` orders and prints by.
  pub fn try_into_domain(self, algorithm: HashAlgorithm) -> Result<CommitMeta, XvfsError> {
    let commit = try_oid(&self.commit_oid, algorithm, "commit_oid")?;
    let committer: Signature = required(self.committer, "committer")?.try_into_domain("committer")?;
    Ok(CommitMeta {
      tree: commit.clone(),
      commit,
      parents: self
        .parent_oids
        .iter()
        .map(|p| try_oid(p, algorithm, "parent_oids"))
        .collect::<Result<_, _>>()?,
      author: required(self.author, "author")?.try_into_domain("author")?,
      snapshot_time: committer.time,
      committer,
      message: self.message,
    })
  }
}

impl v1::GetCommitResponse {
  pub fn try_into_domain(self, algorithm: HashAlgorithm) -> Result<CommitMeta, XvfsError> {
    Ok(CommitMeta {
      commit: try_oid(&self.commit_oid, algorithm, "commit_oid")?,
      tree: try_oid(&self.tree_oid, algorithm, "tree_oid")?,
      parents: self
        .parent_oids
        .iter()
        .map(|p| try_oid(p, algorithm, "parent_oids"))
        .collect::<Result<_, _>>()?,
      author: required(self.author, "author")?.try_into_domain("author")?,
      committer: required(self.committer, "committer")?.try_into_domain("committer")?,
      message: self.message,
      snapshot_time: required(self.snapshot_time, "snapshot_time")?.into(),
    })
  }
}

// ---------------------------------------------------------------------------
// Revision resolution
// ---------------------------------------------------------------------------

impl From<ResolvedRevision> for v1::ResolveRevisionResponse {
  fn from(r: ResolvedRevision) -> Self {
    v1::ResolveRevisionResponse {
      commit_oid: r.commit.to_qualified(),
      tree_oid: r.tree.to_qualified(),
      ref_name: r.ref_name,
      ref_version: r.ref_version,
      snapshot_time: Some(r.snapshot_time.into()),
    }
  }
}

impl v1::ResolveRevisionResponse {
  pub fn try_into_domain(self, algorithm: HashAlgorithm) -> Result<ResolvedRevision, XvfsError> {
    Ok(ResolvedRevision {
      commit: try_oid(&self.commit_oid, algorithm, "commit_oid")?,
      tree: try_oid(&self.tree_oid, algorithm, "tree_oid")?,
      ref_name: self.ref_name,
      ref_version: self.ref_version,
      snapshot_time: required(self.snapshot_time, "snapshot_time")?.into(),
    })
  }
}

// ---------------------------------------------------------------------------
// Snapshot state
// ---------------------------------------------------------------------------

impl From<SnapshotState> for v1::SnapshotState {
  fn from(s: SnapshotState) -> Self {
    match s {
      SnapshotState::Ready => v1::SnapshotState::Ready,
      SnapshotState::Building => v1::SnapshotState::Building,
      SnapshotState::Failed => v1::SnapshotState::Failed,
    }
  }
}

impl TryFrom<v1::SnapshotState> for SnapshotState {
  type Error = XvfsError;

  fn try_from(s: v1::SnapshotState) -> Result<Self, XvfsError> {
    match s {
      v1::SnapshotState::Ready => Ok(SnapshotState::Ready),
      v1::SnapshotState::Building => Ok(SnapshotState::Building),
      v1::SnapshotState::Failed => Ok(SnapshotState::Failed),
      // Not silently mapped to one of the three. A server that sends
      // UNSPECIFIED is either older or broken, and guessing `Failed` would make
      // a transient condition look permanent while guessing `Building` would
      // make a client poll forever.
      v1::SnapshotState::Unspecified => Err(XvfsError::new(
        ErrorCode::Internal,
        "server returned an unspecified snapshot state",
      )),
    }
  }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

impl From<&XvfsError> for v1::ErrorDetail {
  fn from(e: &XvfsError) -> Self {
    v1::ErrorDetail {
      code: e.code.as_str().to_owned(),
      message: e.message.clone(),
    }
  }
}

/// The metadata key carrying the stable XVFS error code.
pub const ERROR_CODE_METADATA_KEY: &str = "xvfs-error-code";

/// Convert a domain error into a gRPC status.
///
/// A free function rather than a `From` impl because both types are foreign to
/// this crate and the orphan rule forbids the impl.
///
/// The code travels twice: as the gRPC status code, which generic middleware and
/// retry policies already understand, and as the stable XVFS wire name in the
/// metadata, which distinguishes codes that share a gRPC code. Without the
/// second, `SNAPSHOT_BUILDING` and `UNAVAILABLE` are both gRPC 14 and therefore
/// indistinguishable to a client -- and DESIGN.md section 9 requires them to be
/// distinguishable, because one means "retry shortly" and the other means the
/// backend is down.
pub fn to_status(e: &XvfsError) -> tonic::Status {
  let code = tonic::Code::from_i32(e.code.grpc_code());
  let mut status = tonic::Status::new(code, e.message.clone());
  if let Ok(value) = e.code.as_str().parse() {
    status.metadata_mut().insert(ERROR_CODE_METADATA_KEY, value);
  }
  status
}

/// Recover the XVFS error code from a gRPC status, falling back to the gRPC code.
///
/// The fallback matters for statuses that did not come from an XVFS handler --
/// a proxy timeout, a transport reset -- which carry no metadata but still need to
/// map onto something a client can act on.
pub fn from_status(status: &tonic::Status) -> XvfsError {
  let code = status
    .metadata()
    .get(ERROR_CODE_METADATA_KEY)
    .and_then(|v| v.to_str().ok())
    .map(|name| {
      v1::ErrorDetail {
        code: name.to_owned(),
        message: String::new(),
      }
      .into_domain()
      .code
    })
    .unwrap_or_else(|| match status.code() {
      tonic::Code::InvalidArgument => ErrorCode::InvalidArgument,
      tonic::Code::NotFound => ErrorCode::NotFound,
      tonic::Code::PermissionDenied => ErrorCode::PermissionDenied,
      tonic::Code::Unauthenticated => ErrorCode::Unauthenticated,
      tonic::Code::FailedPrecondition => ErrorCode::FailedPrecondition,
      tonic::Code::Aborted => ErrorCode::Conflict,
      tonic::Code::ResourceExhausted => ErrorCode::ResourceLimit,
      tonic::Code::Unavailable => ErrorCode::Unavailable,
      tonic::Code::DeadlineExceeded => ErrorCode::DeadlineExceeded,
      tonic::Code::Cancelled => ErrorCode::Cancelled,
      _ => ErrorCode::Internal,
    });
  XvfsError::new(code, status.message().to_owned())
}

impl v1::ErrorDetail {
  /// Recover a domain error from a per-item `ErrorDetail`.
  pub fn into_domain(self) -> XvfsError {
    let code = match self.code.as_str() {
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
      // An unrecognized code from a newer server. `Internal` rather than a
      // guess: it is not retryable, so a client cannot spin on it, and the
      // message is preserved for a human.
      _ => ErrorCode::Internal,
    };
    XvfsError::new(code, self.message)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const SHA1: HashAlgorithm = HashAlgorithm::Sha1;

  fn oid(byte: &str) -> ObjectId {
    ObjectId::from_hex(SHA1, &byte.repeat(20)).unwrap()
  }

  #[test]
  fn a_non_utf8_path_survives_the_round_trip() {
    // The reason paths are `bytes` and not `string`. A `string` field would make
    // this value unrepresentable on the wire.
    let entry = TreeEntryInfo {
      path: BytePath::new(b"drivers/\xff\xfebad.c".to_vec()),
      kind: EntryKind::Regular,
      mode: 0o100644,
      oid: oid("ab"),
      size: 12,
      symlink_target: None,
      blob_ticket: None,
    };
    let wire = v1::TreeEntry::from(entry.clone());
    let back = wire.try_into_domain(SHA1).unwrap();
    assert_eq!(back.path, entry.path);
    assert_eq!(back, entry);
  }

  #[test]
  fn a_non_utf8_commit_message_and_author_survive_the_round_trip() {
    let sig = Signature {
      name: b"Ren\xe9 Descartes".to_vec(), // Latin-1, not UTF-8
      email: b"rene@example.invalid".to_vec(),
      time: Timestamp::from_secs(1_600_000_000),
      tz_offset_minutes: 120,
    };
    let commit = CommitMeta {
      commit: oid("11"),
      tree: oid("22"),
      parents: vec![oid("33")],
      author: sig.clone(),
      committer: sig,
      message: b"fix caf\xe9 handling\n".to_vec(),
      snapshot_time: Timestamp::from_secs(1_600_000_001),
    };
    let wire = v1::GetCommitResponse::from(commit.clone());
    assert_eq!(wire.try_into_domain(SHA1).unwrap(), commit);
  }

  #[test]
  fn an_unsupported_mode_keeps_its_raw_value_across_the_wire() {
    // `ENTRY_KIND_UNSUPPORTED` carries no information on its own, so the mode
    // has to be authoritative or the value is lost.
    let entry = TreeEntryInfo {
      path: BytePath::new("weird"),
      kind: EntryKind::Unsupported(0o100664),
      mode: 0o100664,
      oid: oid("cd"),
      size: 1,
      symlink_target: None,
      blob_ticket: None,
    };
    let back = v1::TreeEntry::from(entry.clone())
      .try_into_domain(SHA1)
      .unwrap();
    assert_eq!(back.kind, EntryKind::Unsupported(0o100664));
    assert_eq!(back.mode, 0o100664);
  }

  #[test]
  fn a_traversal_path_from_the_server_is_rejected_by_the_client() {
    // A client must not trust a server's entry: this path would otherwise reach
    // the FUSE layer and be used to build a kernel dentry.
    let wire = v1::TreeEntry {
      path: b"../../etc/passwd".to_vec(),
      kind: v1::EntryKind::Regular as i32,
      mode: 0o100644,
      oid: oid("ab").to_qualified(),
      size: 1,
      symlink_target: None,
      blob_ticket: None,
    };
    let err = wire.try_into_domain(SHA1).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
  }

  #[test]
  fn an_oid_from_the_wrong_algorithm_is_rejected_rather_than_looked_up() {
    let sha256 = ObjectId::from_hex(HashAlgorithm::Sha256, &"ab".repeat(32)).unwrap();
    let err = try_oid(&sha256.to_qualified(), SHA1, "commit_oid").unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert!(err.message.contains("sha256"));
  }

  #[test]
  fn a_bare_hex_digest_is_accepted_in_a_repository_context() {
    // ADR 0006 allows this exactly because the repository supplies the algorithm.
    let hex = "ab".repeat(20);
    assert_eq!(try_oid(&hex, SHA1, "commit_oid").unwrap(), oid("ab"));
  }

  #[test]
  fn a_missing_required_message_field_becomes_invalid_argument() {
    // Protobuf 3 cannot express "required", so the schema's requirement has to
    // be enforced on receipt.
    let wire = v1::ResolveRevisionResponse {
      commit_oid: oid("ab").to_qualified(),
      tree_oid: oid("cd").to_qualified(),
      ref_name: None,
      ref_version: 1,
      snapshot_time: None,
    };
    let err = wire.try_into_domain(SHA1).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert!(err.message.contains("snapshot_time"));
  }

  #[test]
  fn unspecified_snapshot_state_is_an_error_not_a_guess() {
    assert!(SnapshotState::try_from(v1::SnapshotState::Unspecified).is_err());
    for (wire, domain) in [
      (v1::SnapshotState::Ready, SnapshotState::Ready),
      (v1::SnapshotState::Building, SnapshotState::Building),
      (v1::SnapshotState::Failed, SnapshotState::Failed),
    ] {
      assert_eq!(SnapshotState::try_from(wire).unwrap(), domain);
      assert_eq!(v1::SnapshotState::from(domain), wire);
    }
  }

  #[test]
  fn error_codes_that_share_a_grpc_code_stay_distinguishable() {
    // SNAPSHOT_BUILDING and UNAVAILABLE are both gRPC 14. DESIGN.md section 9
    // requires a client to tell them apart, which the status code alone cannot
    // do -- hence the metadata entry.
    let building = to_status(&XvfsError::new(
      ErrorCode::SnapshotBuilding,
      "manifest building",
    ));
    let unavailable = to_status(&XvfsError::new(ErrorCode::Unavailable, "backend down"));
    assert_eq!(building.code(), unavailable.code());
    assert_eq!(building.code(), tonic::Code::Unavailable);

    // Recovered through the metadata, they are distinct again.
    assert_eq!(from_status(&building).code, ErrorCode::SnapshotBuilding);
    assert_eq!(from_status(&unavailable).code, ErrorCode::Unavailable);
  }

  #[test]
  fn a_status_without_xvfs_metadata_still_maps_to_a_usable_code() {
    // A proxy timeout or transport reset carries no XVFS metadata, but a client
    // still has to decide whether to retry.
    let raw = tonic::Status::new(tonic::Code::DeadlineExceeded, "upstream timed out");
    let e = from_status(&raw);
    assert_eq!(e.code, ErrorCode::DeadlineExceeded);
    assert!(e.is_retryable());

    let denied = tonic::Status::new(tonic::Code::PermissionDenied, "no");
    assert_eq!(from_status(&denied).code, ErrorCode::PermissionDenied);
    assert!(!from_status(&denied).is_retryable());
  }

  #[test]
  fn error_detail_round_trips_every_code() {
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
      let e = XvfsError::new(code, "detail");
      let wire = v1::ErrorDetail::from(&e);
      assert_eq!(wire.clone().into_domain(), e, "round trip for {code}");
    }
  }

  #[test]
  fn an_unknown_error_code_from_a_newer_server_is_not_retryable() {
    let unknown = v1::ErrorDetail {
      code: "SOMETHING_NEW".to_owned(),
      message: "from a newer server".to_owned(),
    };
    let e = unknown.into_domain();
    // A client must not spin on a code it does not understand.
    assert!(!e.is_retryable());
    assert_eq!(e.message, "from a newer server");
  }
}
