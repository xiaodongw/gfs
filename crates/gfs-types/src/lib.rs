//! Shared value types for GFS: object IDs, byte paths, revision selectors,
//! identities, error codes, limits, and the timestamp policy.
//!
//! This crate is the bottom of the dependency graph. Every other `gfs-*` crate
//! depends on it, and it depends on almost nothing, so that adding a dependency
//! here is visibly a decision to add one to the server, the CLI, and the FUSE
//! client at once.
//!
//! Four rules are enforced here rather than documented and hoped for, each
//! because M0 found a way for it to go wrong:
//!
//! * an object ID always carries its algorithm, so a SHA-256 digest cannot be
//!   silently truncated into a SHA-1 ([`oid`]);
//! * a path is bytes and is validated before use, so no layer round-trips it
//!   through `String` or accepts a traversal ([`path`]);
//! * a revision selector is one of four closed shapes, so `revparse`'s
//!   expression language never reaches libgit2 ([`revision`]);
//! * `refs/gfs/` is rejected as a user revision at the lowest layer, in both
//!   the qualified and short-name spellings, so no caller can forget to
//!   ([`revision::RESERVED_REF_PREFIX`]).

pub mod diff;
pub mod entry;
pub mod error;
pub mod glob;
pub mod ids;
pub mod lfs;
pub mod limits;
pub mod oid;
pub mod path;
pub mod redact;
pub mod revision;
pub mod snapshot;
pub mod time;

pub use diff::{BlameHunk, DiffFileChange, DiffFormat, DiffStatus};
pub use entry::{mode, EntryKind};
pub use error::{ErrorCode, GfsError};
pub use ids::{DisplayName, MountId, RepositoryId, SubjectId};
pub use limits::LeasePolicy;
pub use oid::{HashAlgorithm, ObjectId, OidError};
pub use path::BytePath;
pub use revision::{AncestryStep, RevisionExpression, RevisionSelector, RESERVED_REF_PREFIX};
pub use snapshot::{
  CommitMeta, LeaseState, MountGrant, RefTarget, ResolvedRevision, Signature, SnapshotState,
  TreeEntryInfo,
};
pub use time::Timestamp;

/// The API version this build speaks.
///
/// ADR 0006's versioning policy: gRPC is additive-only within a major version,
/// HTTP is path-versioned as `/v1/...`, and a client must interoperate with a
/// server one minor version newer or older.
pub const API_VERSION: &str = "v1";

/// The on-disk format version for client and server state.
///
/// Bumped when a persisted layout changes incompatibly. Recorded in `mount.json`
/// (DESIGN.md section 8.3) and in the catalog so a mismatch is refused rather
/// than misread.
///
/// 2: ADR 0011 — state moved inside the workspace (`.git/gfs`), the mount
/// request lost its state-dir field, and the control socket moved to the
/// runtime directory.
pub const STATE_FORMAT_VERSION: u32 = 2;

pub type Result<T> = std::result::Result<T, GfsError>;
