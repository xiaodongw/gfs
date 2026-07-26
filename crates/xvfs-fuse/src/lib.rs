//! The XVFS FUSE client: a pinned Git commit presented as a lazy, read-only
//! workspace.
//!
//! Owned by M2. The shape of the crate follows the three constraints M0 measured
//! and M1 left in place, and each is enforced in one identifiable module rather
//! than distributed as a convention:
//!
//! * **[`fs`] never blocks a FUSE callback.** ADR 0003 measured a blocking
//!   callback turning 64 parallel reads into 64 serial ones (1321 ms against
//!   123 ms). Every callback that can reach the network hands its reply to the
//!   tokio runtime and returns. [`session`] supplies the other half of that rule,
//!   more than one event-loop thread.
//! * **[`cache`] verifies before it publishes.** A blob is hashed as
//!   `blob <size>\0<content>` and compared with its object ID before it is
//!   renamed into place, so a truncated response or a corrupted disk fails
//!   loudly instead of reaching a compiler as a silently wrong source file.
//! * **[`gitdir`] synthesizes six entries, not four.** ADR 0005 measured that
//!   DESIGN.md's list does not form a repository at all, and that the shim is a
//!   correctness requirement because `ls-files` and `diff` against the raw
//!   surface exit 0 with empty output.
//!
//! # What M2 deliberately does not do
//!
//! Nothing here is writable. Every mutation is `EROFS`, and a `git diff` through
//! the shim is empty — both correct answers for a read-only mount of an immutable
//! commit, and both of which M3 rewires to the overlay journal. The overlay quota
//! is *reported* by `statfs` and not yet enforced, because there is nothing yet
//! to write.

pub mod attr;
pub mod cache;
pub mod client;
pub mod fs;
pub mod gitdir;
pub mod inode;
pub mod session;

pub use cache::{BlobCache, CacheStats, Hydration};
pub use client::{MountBinding, SnapshotClient};
pub use fs::{root_entry, FsConfig, FsStats, Xvfs, XvfsFilesystem};
pub use gitdir::{GitDir, GitDirFacts};
pub use session::{spawn_mount, MountConfig};
