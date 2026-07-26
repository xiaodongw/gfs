//! Structured error codes shared by every XVFS surface.
//!
//! PLAN.md M1.1 requires structured error codes, and M1.4 requires golden tests
//! over the Protobuf and JSON error representations. Both need one enumeration
//! that the gRPC service, the HTTP endpoints, the CLI, and the FUSE client agree
//! on, so the code is defined here and the per-transport mappings are pure
//! functions over it rather than a `match` repeated in three crates.
//!
//! Three of these codes exist because DESIGN.md section 9 insists they be
//! distinguishable rather than collapsed into one failure: `SnapshotBuilding`,
//! `NotIndexable`, and `ResourceLimit` are *request* errors, not snapshot
//! states, and an agent that cannot tell them apart cannot tell "ask again in a
//! moment" from "this will never work".
//!
//! # Messages are safe to log
//!
//! Every `XvfsError` message is constructed to be safe for a client *and* for a
//! log line: no file content, no credential material, and no unvalidated path.
//! ADR 0006 fixes that rule for audit records, and the cheapest way to keep it
//! is to make it true of all errors rather than to maintain a second set of
//! log-safe strings. Use [`crate::redact`] when a value must appear at all.

use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
  /// Malformed request: a bad path, an unparseable object ID, an out-of-range
  /// page size, a selector outside the supported grammar.
  InvalidArgument,
  /// The named resource does not exist *or* the caller may not know whether it
  /// exists. See the masking rule below.
  NotFound,
  /// The caller is known and may see that the resource exists, but not perform
  /// this operation on it.
  PermissionDenied,
  /// No usable credential was presented.
  Unauthenticated,
  /// A credential or capability was valid but has expired.
  Expired,
  /// The request is well-formed but the resource is in a state that forbids it:
  /// a quarantined repository, a lease that is `PREPARING`, a released mount.
  FailedPrecondition,
  /// A concurrent modification lost a compare-and-swap.
  Conflict,
  /// A per-request or per-caller quota stopped the work: result count, byte
  /// budget, time budget, or regex complexity. Distinct from `Unavailable`
  /// because retrying without changing the request will fail the same way.
  ResourceLimit,
  /// The snapshot manifest or index generation is not ready yet. Retryable, and
  /// specifically *not* an empty result.
  SnapshotBuilding,
  /// The content is outside the searchable corpus by policy: binary, oversized,
  /// or excluded. Not a failure of the query.
  NotIndexable,
  /// The repository's on-disk format is one XVFS cannot serve. ADR 0001 rejects
  /// `reftable`, SHA-256, and unrecognized `extensions.*` at ingest rather than
  /// serving a partial view.
  UnsupportedRepositoryFormat,
  /// The caller named `refs/xvfs/`, which is an internal reachability namespace
  /// and never a user-supplied revision. A distinct code because ADR 0006 makes
  /// this a rule enforced at the lowest layer, and a test asserts it fires.
  ReservedNamespace,
  /// A dependency is temporarily unusable. Retryable.
  Unavailable,
  DeadlineExceeded,
  Cancelled,
  /// A bug. Never carries detail from the failure to the client.
  Internal,
}

impl ErrorCode {
  /// The gRPC status code, as the numeric value from the gRPC specification.
  ///
  /// Returned as a number so `xvfs-types` does not depend on `tonic`; the
  /// server converts.
  pub fn grpc_code(self) -> i32 {
    match self {
      ErrorCode::InvalidArgument => 3,
      ErrorCode::NotFound => 5,
      ErrorCode::PermissionDenied => 7,
      ErrorCode::Unauthenticated => 16,
      // Unauthenticated rather than a dedicated code: an expired credential is
      // a credential problem, and clients already re-authenticate on 16.
      ErrorCode::Expired => 16,
      ErrorCode::FailedPrecondition => 9,
      ErrorCode::Conflict => 10,         // ABORTED
      ErrorCode::ResourceLimit => 8,     // RESOURCE_EXHAUSTED
      ErrorCode::SnapshotBuilding => 14, // UNAVAILABLE: retryable
      ErrorCode::NotIndexable => 9,
      ErrorCode::UnsupportedRepositoryFormat => 9,
      ErrorCode::ReservedNamespace => 3,
      ErrorCode::Unavailable => 14,
      ErrorCode::DeadlineExceeded => 4,
      ErrorCode::Cancelled => 1,
      ErrorCode::Internal => 13,
    }
  }

  pub fn http_status(self) -> u16 {
    match self {
      ErrorCode::InvalidArgument | ErrorCode::ReservedNamespace => 400,
      ErrorCode::Unauthenticated | ErrorCode::Expired => 401,
      ErrorCode::PermissionDenied => 403,
      ErrorCode::NotFound => 404,
      ErrorCode::Conflict => 409,
      ErrorCode::FailedPrecondition
      | ErrorCode::NotIndexable
      | ErrorCode::UnsupportedRepositoryFormat => 422,
      ErrorCode::ResourceLimit => 429,
      ErrorCode::Internal => 500,
      ErrorCode::Unavailable | ErrorCode::SnapshotBuilding => 503,
      ErrorCode::DeadlineExceeded => 504,
      // 499, nginx's client-closed-request. There is no standard status for it
      // and the response usually never reaches the client anyway.
      ErrorCode::Cancelled => 499,
    }
  }

  /// Whether an identical retry could plausibly succeed.
  ///
  /// The FUSE client's retry policy (M2.3) reads this. `ResourceLimit` is
  /// deliberately false: the same request will exhaust the same budget.
  pub fn retryable(self) -> bool {
    matches!(
      self,
      ErrorCode::Unavailable | ErrorCode::SnapshotBuilding | ErrorCode::DeadlineExceeded
    )
  }

  /// The stable wire name. Used in JSON error bodies and gRPC error details, so
  /// it is part of the API surface and must not be renamed without a version
  /// bump (ADR 0006's additive-only policy).
  pub fn as_str(self) -> &'static str {
    match self {
      ErrorCode::InvalidArgument => "INVALID_ARGUMENT",
      ErrorCode::NotFound => "NOT_FOUND",
      ErrorCode::PermissionDenied => "PERMISSION_DENIED",
      ErrorCode::Unauthenticated => "UNAUTHENTICATED",
      ErrorCode::Expired => "EXPIRED",
      ErrorCode::FailedPrecondition => "FAILED_PRECONDITION",
      ErrorCode::Conflict => "CONFLICT",
      ErrorCode::ResourceLimit => "RESOURCE_LIMIT",
      ErrorCode::SnapshotBuilding => "SNAPSHOT_BUILDING",
      ErrorCode::NotIndexable => "NOT_INDEXABLE",
      ErrorCode::UnsupportedRepositoryFormat => "UNSUPPORTED_REPOSITORY_FORMAT",
      ErrorCode::ReservedNamespace => "RESERVED_NAMESPACE",
      ErrorCode::Unavailable => "UNAVAILABLE",
      ErrorCode::DeadlineExceeded => "DEADLINE_EXCEEDED",
      ErrorCode::Cancelled => "CANCELLED",
      ErrorCode::Internal => "INTERNAL",
    }
  }
}

impl fmt::Display for ErrorCode {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

/// An error with a stable code and a message that is safe to return and to log.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct XvfsError {
  pub code: ErrorCode,
  pub message: String,
}

impl XvfsError {
  pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
    XvfsError {
      code,
      message: message.into(),
    }
  }

  pub fn not_found(message: impl Into<String>) -> Self {
    XvfsError::new(ErrorCode::NotFound, message)
  }

  pub fn invalid(message: impl Into<String>) -> Self {
    XvfsError::new(ErrorCode::InvalidArgument, message)
  }

  pub fn internal(message: impl Into<String>) -> Self {
    XvfsError::new(ErrorCode::Internal, message)
  }

  /// A denial reported as absence.
  ///
  /// M1's exit criteria require that an unauthorized caller cannot infer
  /// existence "through status, timing within a defined tolerance, cache, or
  /// error differences". For any resource whose *existence* is itself
  /// privileged -- a repository, a commit retained for another subject's mount,
  /// a blob -- the denial and the absence must therefore be one response.
  ///
  /// This constructor exists so that requirement is expressed once, at the call
  /// site that makes the decision, instead of relying on every authorization
  /// check to remember to pick `NotFound` over the more natural
  /// `PermissionDenied`. `PermissionDenied` stays available for operations on a
  /// resource the caller is already known to be able to see.
  pub fn masked_denial(message: impl Into<String>) -> Self {
    XvfsError::new(ErrorCode::NotFound, message)
  }

  pub fn is_retryable(&self) -> bool {
    self.code.retryable()
  }
}

impl fmt::Display for XvfsError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}: {}", self.code, self.message)
  }
}

impl fmt::Debug for XvfsError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{self}")
  }
}

impl std::error::Error for XvfsError {}

impl From<crate::oid::OidError> for XvfsError {
  fn from(e: crate::oid::OidError) -> Self {
    XvfsError::new(ErrorCode::InvalidArgument, format!("object id: {e}"))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn wire_names_are_unique_and_stable() {
    // Golden list. A change here is an API change: ADR 0006's versioning policy
    // makes these names additive-only, so this test failing means either a new
    // code was added (extend the list) or an existing one was renamed (don't).
    let expected = [
      "INVALID_ARGUMENT",
      "NOT_FOUND",
      "PERMISSION_DENIED",
      "UNAUTHENTICATED",
      "EXPIRED",
      "FAILED_PRECONDITION",
      "CONFLICT",
      "RESOURCE_LIMIT",
      "SNAPSHOT_BUILDING",
      "NOT_INDEXABLE",
      "UNSUPPORTED_REPOSITORY_FORMAT",
      "RESERVED_NAMESPACE",
      "UNAVAILABLE",
      "DEADLINE_EXCEEDED",
      "CANCELLED",
      "INTERNAL",
    ];
    let actual: Vec<&str> = ALL_CODES.iter().map(|c| c.as_str()).collect();
    assert_eq!(actual, expected);
  }

  #[test]
  fn json_representation_uses_the_wire_name() {
    let e = XvfsError::new(ErrorCode::SnapshotBuilding, "manifest is building");
    let json = serde_json::to_string(&e).unwrap();
    assert_eq!(
      json,
      r#"{"code":"SNAPSHOT_BUILDING","message":"manifest is building"}"#
    );
    let back: XvfsError = serde_json::from_str(&json).unwrap();
    assert_eq!(back, e);
  }

  #[test]
  fn retryable_codes_are_exactly_the_transient_ones() {
    let retryable: Vec<&str> = ALL_CODES
      .iter()
      .filter(|c| c.retryable())
      .map(|c| c.as_str())
      .collect();
    // ResourceLimit is deliberately absent: an identical retry exhausts the
    // identical budget, and a client that retries it just burns quota.
    assert_eq!(
      retryable,
      ["SNAPSHOT_BUILDING", "UNAVAILABLE", "DEADLINE_EXCEEDED"]
    );
  }

  #[test]
  fn a_masked_denial_is_indistinguishable_from_an_absence() {
    let denied = XvfsError::masked_denial("repository not found");
    let absent = XvfsError::not_found("repository not found");
    assert_eq!(denied, absent);
    assert_eq!(denied.code.http_status(), absent.code.http_status());
    assert_eq!(denied.code.grpc_code(), absent.code.grpc_code());
  }

  const ALL_CODES: &[ErrorCode] = &[
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
  ];
}
