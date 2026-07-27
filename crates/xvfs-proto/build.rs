//! Generate the gRPC client and server from `proto/`.
//!
//! `protoc` comes from `protoc-bin-vendored` rather than from the host. See the
//! Cargo.toml note: pinning the compiler through Cargo is the same decision ADR
//! 0001 made for libgit2, for the same reason -- the artifact has to be ours, or
//! an environment difference silently becomes a behaviour difference.

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let protoc = protoc_bin_vendored::protoc_bin_path()?;
  // `PROTOC` is how prost-build locates the compiler. Setting it here rather
  // than documenting "install protoc" is what makes a fresh clone build.
  std::env::set_var("PROTOC", &protoc);

  let protos = [
    "proto/xvfs/v1/common.proto",
    "proto/xvfs/v1/snapshot.proto",
    "proto/xvfs/v1/search.proto",
  ];

  // Emit the descriptor set as well as the Rust code. `golden.rs` reads it to
  // assert ADR 0006's compatibility rules -- that no field was renumbered and no
  // enum value repurposed -- which is checkable from the descriptor and not from
  // the generated structs.
  let descriptor_path =
    std::path::PathBuf::from(std::env::var("OUT_DIR")?).join("xvfs_descriptor.bin");

  tonic_prost_build::configure()
    .build_client(true)
    .build_server(true)
    .file_descriptor_set_path(&descriptor_path)
    .compile_protos(&protos, &["proto"])?;

  for p in protos {
    println!("cargo:rerun-if-changed={p}");
  }
  Ok(())
}
