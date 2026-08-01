//! The gateway's LFS object store (ADR 0012).
//!
//! Content-addressed by the expanded content's SHA-256 — the digest a spec v1
//! pointer's `oid` line carries, spelled `lfs-sha256:{hex}` everywhere GFS
//! handles it. The store is what lets entry metadata substitute expanded sizes
//! and the immutable blob endpoint serve expanded content: an entry is
//! presented as expanded exactly when its object is here, and degrades to its
//! pointer otherwise.
//!
//! Unlike the search index, this is not derived data the server can rebuild
//! from Git alone — refetching needs upstream and a caller's credential — so
//! writes take the catalog's durability posture: bytes are verified against
//! their address before publication, published by atomic rename, and fsynced
//! (file and directory) before the rename is trusted.

mod batch;
mod store;

pub use batch::{BatchClient, DownloadReport, WantedObject};
pub use store::LfsStore;

/// How much of an upstream's LFS working set ingest fetches (ADR 0012 requires
/// this policy to be surfaced rather than buried).
///
/// The store can only be populated while a caller's upstream credential is in
/// hand — the catalog stores credential *references*, never secrets — so
/// "lazily, later" is not an available default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LfsPrefetch {
  /// Fetch the objects reachable from the default branch's tip: what a fresh
  /// mount will see. Objects on other branches degrade to pointers until a
  /// sync with a credential brings them in.
  #[default]
  Tip,
  /// Fetch nothing; every LFS entry degrades to its pointer.
  None,
}
