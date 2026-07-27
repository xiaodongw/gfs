//! The Git smart-HTTP gateway.
//!
//! DESIGN.md section 7.2 and ADR 0001 fix the shape: XVFS does **not**
//! reimplement `upload-pack`. A Rust gateway authenticates, authorizes, limits,
//! and streams to a sandboxed stock `git upload-pack` child. Git owns the
//! protocol -- pkt-line framing, `ls-refs`, want/have negotiation, shallow
//! behaviour, filters, deltas, sideband -- and XVFS owns everything around it.
//!
//! The split between the two files here follows the trust boundary rather than
//! the request flow:
//!
//! * [`upload_pack`] is everything the gateway decides *about the child*: its
//!   executable, arguments, working directory, environment, configuration, and
//!   resource limits. Nothing user-supplied reaches any of them.
//! * [`pkt`] is the only place the gateway looks at Git's wire bytes, and it
//!   does so for exactly two reasons that cannot be delegated to the child: the
//!   exact partial-clone filter, and the reserved `refs/xvfs/` namespace.
//!
//! # What this gateway does not claim
//!
//! ADR 0002 is the load-bearing scope limit. M0.3 measured that protocol v2
//! serves any object in a repository's object database by object ID regardless
//! of `uploadpack.allowAnySHA1InWant`, so a repository reader can always reach a
//! lease-retained commit through Git. **One bare repository is one authorization
//! domain.** The gateway enforces repository authorization -- the same
//! `Authorizer` the snapshot, blob, and search APIs use -- and does not claim
//! object authorization. PLAN.md M1.5 says it plainly: do not write an
//! acceptance test that expects the Git path to deny it.

pub mod pkt;
pub mod upload_pack;

pub use upload_pack::{
  FilterPolicy, GitProtocol, Mode, ResourceLimits, UploadPack, UploadPackPolicy,
};
