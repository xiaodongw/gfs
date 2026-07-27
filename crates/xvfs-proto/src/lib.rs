//! Generated gRPC types for the XVFS snapshot API, plus conversions to and from
//! the `xvfs-types` domain types.
//!
//! The conversions live here rather than in `xvfs-server` because both the server
//! and the client need them, and because this is where the wire's loose types
//! (`String` object IDs, `bytes` paths, `uint32` modes) become the domain's
//! validated ones. That makes the trust boundary easy to audit: a handler that
//! works with `xvfs_types` values received input that was checked, because there
//! is no other way to obtain one from a request.

pub mod convert;

/// The generated `xvfs.v1` module.
pub mod v1 {
  tonic::include_proto!("xvfs.v1");

  /// The compiled Protobuf descriptor set.
  ///
  /// Read by the golden tests to assert ADR 0006's compatibility rules, and
  /// available for gRPC server reflection if that is ever enabled.
  pub const FILE_DESCRIPTOR_SET: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/xvfs_descriptor.bin"));
}

pub use v1::{
  search_service_client::SearchServiceClient,
  search_service_server::{SearchService, SearchServiceServer},
  snapshot_service_client::SnapshotServiceClient,
  snapshot_service_server::{SnapshotService, SnapshotServiceServer},
};
