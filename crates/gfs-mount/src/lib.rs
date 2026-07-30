//! The GFS FUSE client: a pinned Git commit presented as a lazy workspace with
//! a crash-safe copy-on-write overlay over it.
//!
//! Owned by M2, extended by M3. The shape of the crate follows the constraints
//! M0 measured and M1 left in place, and each is enforced in one identifiable
//! module rather than distributed as a convention:
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
//! * **the overlay is the only writer.** Nothing in this crate mutates the base:
//!   a write copies the blob into `gfs-overlay`'s journal and content store and
//!   every later read of that path comes from there. That is what makes the
//!   pinned commit safe to share between mounts and what makes `status` derivable
//!   without scanning the tree.
//!
//! # What is deliberately not here
//!
//! Hard links (`EPERM`, because Git has no hard links to model), device nodes and
//! xattrs (documented unsupported in DESIGN.md section 8.2), and any permission
//! bit other than `+x` — Git records one, and an export could not reproduce the
//! rest.

pub mod attr;
pub mod blobs;
pub mod budget;
pub mod cache;
pub mod client;
pub mod control;
pub mod find;
pub mod fs;
pub mod gitdir;
pub mod host;
pub mod inode;
pub mod lease;
pub mod mount;
pub mod publish;
pub mod search;
pub mod session;
pub mod state;

pub use cache::{BlobCache, CacheStats, Hydration};
pub use client::{MountBinding, SnapshotClient};
pub use fs::{root_entry, FsConfig, FsStats, Gfs, GfsFilesystem};
pub use gitdir::{GitDir, GitDirFacts};
pub use host::{HostConfig, MountHost};
pub use lease::{HealthState, LeaseHealth, LeaseMonitor};
pub use mount::{Mount, MountSpec};
pub use publish::{DirectMountPublisher, MountPublisher};
pub use session::{spawn_mount, MountConfig};
pub use state::MountState;
