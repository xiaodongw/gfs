//! The GFS server: repository catalog, mount retention leases, and the snapshot
//! and blob APIs.
//!
//! Three durable systems have to agree, and the ordering between them is the
//! substance of this crate:
//!
//! * the **catalog** ([`catalog`]) records repositories, refs, cataloged commit
//!   times, and leases;
//! * the **Git ref anchor** under `refs/gfs/` is what actually keeps a pinned
//!   commit reachable through `git gc`;
//! * the **repository lock** ([`locks`]) makes the two consistent, by ensuring
//!   nothing moves a repository's refs between resolving a selector and anchoring
//!   the resulting commit.
//!
//! [`mounts`] is the only place all three are sequenced. Its ordering argument --
//! why `PREPARING` exists, and why a crash at each step is recoverable -- is
//! written out there rather than distributed across handlers.

pub mod audit;
pub mod auth;
pub mod catalog;
pub mod gateway;
pub mod ingest;
pub mod locks;
pub mod mirror;
pub mod mounts;
pub mod observability;
pub mod odb;
pub mod registry;
pub mod search;
pub mod service;
pub mod util;

pub use auth::{Authorizer, CapabilityKey};
pub use catalog::Catalog;
pub use locks::RepositoryLocks;
pub use mounts::{MountManager, ReconcileOutcome};
pub use registry::Registry;
pub use search::{IndexManager, PrepareOutcome};
pub use service::Server;
