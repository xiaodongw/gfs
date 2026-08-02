//! Snapshot and mount value types.
//!
//! These are what the snapshot API talks about, expressed in validated domain
//! types. They live in `gfs-types` rather than in `gfs-git` because three
//! layers need them and none of them should depend on the Git implementation:
//! `gfs-git` produces them, `gfs-proto` converts them to and from the wire, and
//! `gfs-server` authorizes and caches them.

use std::time::Duration;

use crate::ids::MountId;
use crate::oid::ObjectId;
use crate::path::BytePath;
use crate::time::Timestamp;
use crate::EntryKind;

/// A selector resolved to an immutable commit.
#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct ResolvedRevision {
  pub commit: ObjectId,
  pub tree: ObjectId,
  /// The ref the selector named, if it named one.
  pub ref_name: Option<String>,
  /// Monotonic per-ref version from the catalog. See the field comment on
  /// `ResolveRevisionResponse.ref_version`: comparing commit OIDs cannot
  /// distinguish "unchanged" from "moved and moved back".
  pub ref_version: u64,
  /// The stable sanitized time from ADR 0006, never the raw committer time.
  pub snapshot_time: Timestamp,
}

/// One ref a caller may see, with its target peeled if it names a tag object.
///
/// The peel is carried rather than left to the reader because the reader is a
/// workspace whose object database is a network projection: peeling a tag there
/// costs a pack lookup and a block fetch, and the server has the object open
/// already. It is what lets a mount write a `fully-peeled` `packed-refs` file,
/// so `git describe` and `git tag --list` answer without touching the
/// projection at all.
#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct RefTarget {
  /// The full ref name, `refs/heads/main` or `refs/tags/v1.0`.
  pub name: String,
  /// What the ref points at directly — a tag object for an annotated tag.
  pub target: ObjectId,
  /// The commit an annotated tag resolves to. `None` for a ref that already
  /// points at a commit, which is the lightweight-tag and branch case.
  pub peeled: Option<ObjectId>,
}

/// A Git identity line.
///
/// `name` and `email` are raw bytes because Git does not constrain them to UTF-8.
/// The M0 spike used `from_utf8_lossy`, which replaces invalid sequences with
/// U+FFFD -- that silently corrupts a contributor's name instead of reporting
/// that it is unusual, and the corruption is unrecoverable downstream.
#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct Signature {
  pub name: Vec<u8>,
  pub email: Vec<u8>,
  pub time: Timestamp,
  pub tz_offset_minutes: i32,
}

#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct CommitMeta {
  pub commit: ObjectId,
  pub tree: ObjectId,
  pub parents: Vec<ObjectId>,
  pub author: Signature,
  pub committer: Signature,
  /// Raw bytes, for the same reason as [`Signature::name`].
  pub message: Vec<u8>,
  pub snapshot_time: Timestamp,
}

#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct TreeEntryInfo {
  pub path: BytePath,
  pub kind: EntryKind,
  /// The raw Git mode, always present even when `kind` is
  /// [`EntryKind::Unsupported`].
  pub mode: u32,
  pub oid: ObjectId,
  pub size: u64,
  /// A symlink's target as stored in the blob.
  ///
  /// Deliberately `Vec<u8>` rather than [`BytePath`]. A `BytePath` carries the
  /// implication that [`BytePath::validate`] has been or can be applied, and a
  /// symlink target legitimately fails that validation: it may be absolute, may
  /// contain `..`, and may escape the mount. Whether such a link is *allowed* is
  /// a FUSE-layer policy decision (DESIGN.md section 10 item 10), and typing the
  /// target as a validated path would quietly move that decision to the wrong
  /// layer -- or invite someone to "fix" the validation failure by normalizing
  /// the target, which is how symlink escapes get built.
  pub symlink_target: Option<Vec<u8>>,
  /// Short-lived blob authorization, when one was requested.
  pub blob_ticket: Option<String>,
}

/// The three snapshot lifecycle states.
///
/// DESIGN.md section 7.3: this is the whole lifecycle. `NOT_INDEXABLE` and
/// `RESOURCE_LIMIT` are request errors, not states.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SnapshotState {
  Ready,
  Building,
  Failed,
}

/// What `CreateMount` returns: a pinned commit plus the lease that keeps it
/// readable.
#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct MountGrant {
  pub mount_id: MountId,
  pub commit: ObjectId,
  pub tree: ObjectId,
  pub ref_name: Option<String>,
  pub snapshot_time: Timestamp,
  /// The opaque capability token. Held by the daemon, never logged; use
  /// [`crate::redact::token_fingerprint`] if it must appear in a log line.
  pub capability: String,
  pub lease_expiry: Timestamp,
  /// Supplied by the server so the renewal cadence is not a constant compiled
  /// into each client.
  pub heartbeat_interval: Duration,
}

/// The lease lifecycle from DESIGN.md section 7.1.
///
/// `Preparing` exists because the catalog write and the Git ref anchor cannot
/// share one storage transaction. A lease in `Preparing` has a durable catalog
/// record and possibly no anchor, so restart reconciliation removes its anchor or
/// completes a provably safe transition. An `Active` lease is never returned
/// before its anchor is durable.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LeaseState {
  Preparing,
  Active,
  Released,
  Expired,
}

impl LeaseState {
  /// Whether this state keeps its commit reachable for garbage collection.
  ///
  /// `Preparing` counts. Its anchor may already exist, and treating it as a
  /// non-root would let maintenance prune a commit a `CreateMount` call is in the
  /// middle of pinning.
  pub fn is_reachability_root(self) -> bool {
    matches!(self, LeaseState::Preparing | LeaseState::Active)
  }

  pub fn as_str(self) -> &'static str {
    match self {
      LeaseState::Preparing => "PREPARING",
      LeaseState::Active => "ACTIVE",
      LeaseState::Released => "RELEASED",
      LeaseState::Expired => "EXPIRED",
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn preparing_leases_are_reachability_roots() {
    // The subtle half of DESIGN.md section 7.1's state machine. A `PREPARING`
    // lease may already have created its ref anchor, and a maintenance pass that
    // ignored it could prune the commit a CreateMount call is pinning right now.
    assert!(LeaseState::Preparing.is_reachability_root());
    assert!(LeaseState::Active.is_reachability_root());
    assert!(!LeaseState::Released.is_reachability_root());
    assert!(!LeaseState::Expired.is_reachability_root());
  }
}
