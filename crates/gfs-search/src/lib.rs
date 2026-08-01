//! Revision-aware search indexing and query library.
//!
//! DESIGN.md section 6.5 and ADR 0004 fix the representation: every unique
//! searchable blob in a repository is indexed **once** under a stable numeric
//! `blob_key`, and a snapshot supplies a path table plus a Roaring bitmap of the
//! keys it contains. A query intersects trigram postings with that bitmap before
//! a single blob is inflated, which is what makes searching an arbitrary commit
//! cost the query rather than the commit.
//!
//! ADR 0004 priced it on the worst-case repository: postings at 0.15 bytes per
//! indexed byte, **1.99 MiB of manifest per snapshot**, and 0.39 GiB for 200
//! concurrently retained snapshots. That number is why on-demand search of an
//! arbitrary commit is affordable and does not need to be rationed.
//!
//! # The crate is network-free and synchronous
//!
//! Like `gfs-overlay`, and for the same reason: it knows nothing about gRPC,
//! libgit2, or FUSE. Content arrives through the [`BlobSource`] trait, so the
//! same engine indexes a server-side repository and searches a client-side
//! overlay, and a property test can drive it against an in-memory corpus.
//!
//! # Two kinds of classification, stored in two places
//!
//! This distinction is load-bearing and easy to get wrong. A blob's
//! *content* class — binary, oversized, invalid UTF-8 — is a property of the
//! bytes, so it is recorded once per blob key and shared by every path and
//! every snapshot that carries those bytes. A path's class — generated,
//! vendored — is a property of *where* the bytes are, and the same blob can sit
//! at a vendored path in one snapshot and a hand-written one in another.
//! Recording a path class against a blob key would let one snapshot's directory
//! layout silently exclude another snapshot's file.
//!
//! [`classify`] therefore has two entry points, and [`CorpusPolicy`] maps both
//! kinds of label onto one [`ExclusionReason`] vocabulary, because coverage
//! metadata has to report them side by side.
//!
//! # There is no tokenized search mode
//!
//! PLAN.md M4.4 offers one and says to skip it if literal/regex covers agent
//! workloads. It does, and the skip is recorded with its reasoning in
//! [ADR 0004's M4.4 amendment](../../../docs/adr/0004-search-representation.md).
//! The short version: substring matching inside identifiers — `authorize_re`
//! finding `authorize_request` — is a trigram strength and a tokenizer
//! weakness, and ranking, which trigrams genuinely cannot do, does not change
//! which files an agent opens.
//!
//! # Nothing is excluded silently
//!
//! PLAN.md M4.1 is explicit: generated and vendored files are *classifications*,
//! not default exclusions, and any exclusion must be declared in coverage
//! metadata. The policy object is the declaration — the same value decides what
//! is excluded and is reported in the completion message, so the two cannot
//! drift apart.

pub mod classify;
// The byte-glob moved to `gfs_types` so `gfs-git`'s attribute matching (ADR
// 0012) can share it without dragging this crate's index dependencies along;
// re-exported here so search-side callers keep their paths.
pub use gfs_types::glob;
pub mod lines;
pub mod local;
pub mod manifest;
pub mod postings;
pub mod query;
pub mod registry;
pub mod snapshots;
pub mod store;
pub mod trigram;

pub use classify::{
  classify_content, classify_path, ContentClass, CorpusPolicy, ExclusionReason, PathClass,
};
pub use glob::Glob;
pub use lines::{lines, Line};
pub use local::{search_local, IgnoreRules, LocalBudget, LocalOutcome, LocalPath};
pub use manifest::{Manifest, ManifestDelta, PathEntry, MANIFEST_FORMAT_VERSION};
pub use postings::{PostingBatch, PostingStore};
pub use query::{
  exit_code, search, Budget, Completion, Coverage, ExecutionStatus, Match, Query, SearchInputs,
  SearchOutcome, SearchResult, TruncationReason,
};
pub use registry::{BlobFact, BlobKey, BlobRecord, BlobRegistry, IngestBudget, IngestReport};
pub use snapshots::{
  Cancel, Claim, GcReport, PreparePolicy, Progress, SnapshotRecord, SnapshotStore,
};
pub use store::{SearchStore, SCHEMA_VERSION};
pub use trigram::{required_literals, trigrams, RequiredLiterals};

/// Where a blob's bytes come from.
///
/// The indexer and the query engine both read through this rather than through
/// libgit2 or an HTTP client, which is what keeps this crate free of both. The
/// server implements it over `gfs-git`; a test implements it over a `HashMap`.
///
/// `size` is separate from `read` because ADR 0004's oversized rule must be
/// applied *without* inflating the blob — a 300 MiB blob that will be excluded
/// for size should not cost 300 MiB of inflation to exclude.
pub trait BlobSource: Send + Sync {
  fn size(&self, oid: &gfs_types::ObjectId) -> Result<u64, gfs_types::GfsError>;
  fn read(&self, oid: &gfs_types::ObjectId) -> Result<Vec<u8>, gfs_types::GfsError>;
}
