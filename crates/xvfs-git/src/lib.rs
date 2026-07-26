//! The `GitRepository` abstraction and its libgit2-backed implementation.
//!
//! DESIGN.md section 6.1 requires libgit2 to sit behind a trait so FFI lifetimes,
//! blocking work, library upgrades, and format quirks stay out of HTTP, search,
//! and FUSE code. ADR 0001 measured that the wrapper can be thin -- libgit2 agreed
//! with stock Git on every check across the fixture matrix and all three corpus
//! mirrors, including 101,052 Linux tree entries compared for path bytes, mode,
//! and object ID -- so this crate is a boundary, not a compatibility project.
//!
//! What it does own, because each is a place M0 found a real hazard:
//!
//! * the **format gate** ([`mod@format`]), which reads `config` directly so it can
//!   produce a verdict for a repository libgit2 cannot open at all;
//! * the **handle model** ([`mod@pool`]), where the bound is admission control rather
//!   than reuse;
//! * **Git's tree ordering** ([`mod@tree`]), where paginating on raw names instead of
//!   sort keys silently drops entries at page boundaries;
//! * **lease anchors** ([`repository::GitRepository::create_lease_anchor`]), which
//!   are refused outside the reserved namespace so a caller's bug cannot create a
//!   publicly advertised ref.

pub mod format;
pub mod libgit2;
pub mod pool;
pub mod repository;
pub mod tree;

pub use format::{check, read_format, verdict, FormatVerdict, RepositoryFormat};
pub use libgit2::Libgit2Repository;
pub use pool::RepoPool;
pub use repository::{AsyncRepository, DirectoryPage, EntryLookup, GitRepository};
pub use tree::{DecodedTree, TreeCache, TreeCacheStats};
