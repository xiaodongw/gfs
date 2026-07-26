//! An in-process server plus a real FUSE mount.
//!
//! The tests drive the mount through ordinary syscalls -- `std::fs`, `openat`,
//! `readdir` -- rather than by calling the `Filesystem` trait directly. That is
//! the point: everything M2 is accountable for lives between the syscall and the
//! server, and a test that called `lookup` itself would exercise none of the
//! kernel's caching, offset handling, or permission checks.
//!
//! ADR 0003's amendment fixes the UID model: the daemon and the reader are the
//! same UID here, because `user_allow_other` is a privileged host action and
//! requiring it would make `cargo test` need one.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use xvfs_fuse::client::MountBinding;
use xvfs_fuse::{BlobCache, FsConfig, GitDir, GitDirFacts, MountConfig, SnapshotClient, Xvfs};
use xvfs_proto::v1;
use xvfs_server::auth::{AllowList, CapabilityKey, StaticTokens};
use xvfs_server::catalog::repositories::NewRepository;
use xvfs_server::{Catalog, Server};
use xvfs_types::{
  DisplayName, HashAlgorithm, LeasePolicy, MountId, ObjectId, RepositoryId, SubjectId,
};

pub const TOKEN: &str = "token-mount";

/// A running server, with no mount attached.
pub struct Backend {
  pub grpc: String,
  pub http: String,
  pub repo_id: RepositoryId,
  pub repo_path: PathBuf,
  pub server: Arc<Server>,
  shutdown: tokio::sync::watch::Sender<bool>,
  _tmp: tempfile::TempDir,
}

impl Backend {
  pub async fn start(fixture: &str) -> Backend {
    let (tmp, repo_path) = xvfs_test::scratch_clone(fixture).unwrap();
    let catalog = Arc::new(Catalog::open_in_memory().unwrap());
    let repo_id = RepositoryId::parse("r-mount").unwrap();
    catalog
      .create_repository(&NewRepository {
        repository_id: repo_id.clone(),
        display_name: DisplayName::parse("acme/mounted").unwrap(),
        repo_path: repo_path.clone(),
        algorithm: HashAlgorithm::Sha1,
        upstream_url: None,
        credential_ref: None,
      })
      .unwrap();

    let subject = SubjectId::parse("job-mount").unwrap();
    let authenticator = Arc::new(StaticTokens::new().with_token(TOKEN, subject.clone()));
    let policy = Arc::new(AllowList::new().allow(&subject, &repo_id));
    let server = Arc::new(Server::new(
      Arc::clone(&catalog),
      authenticator,
      policy,
      CapabilityKey::generate().unwrap(),
      LeasePolicy::adr_0006(),
    ));
    server.registry.activate(&repo_id).unwrap();
    server.recover().await.unwrap();

    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);

    let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = http_listener.local_addr().unwrap();
    let router = server.http_router();
    let mut http_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
      axum::serve(http_listener, router)
        .with_graceful_shutdown(async move {
          let _ = http_shutdown.changed().await;
        })
        .await
    });

    let grpc_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_addr = grpc_listener.local_addr().unwrap();
    let api = server.snapshot_api();
    let mut grpc_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
      tonic::transport::Server::builder()
        .add_service(xvfs_proto::SnapshotServiceServer::new(api))
        .serve_with_incoming_shutdown(
          tokio_stream::wrappers::TcpListenerStream::new(grpc_listener),
          async move {
            let _ = grpc_shutdown.changed().await;
          },
        )
        .await
    });

    Backend {
      grpc: format!("http://{grpc_addr}"),
      http: format!("http://{http_addr}"),
      repo_id,
      repo_path,
      server,
      shutdown,
      _tmp: tmp,
    }
  }

  /// Stop answering. Used by the server-loss cases.
  pub fn stop(&self) {
    let _ = self.shutdown.send(true);
  }

  pub async fn grpc_client(
    &self,
  ) -> v1::snapshot_service_client::SnapshotServiceClient<tonic::transport::Channel> {
    v1::snapshot_service_client::SnapshotServiceClient::connect(self.grpc.clone())
      .await
      .unwrap()
  }
}

/// A mounted workspace.
pub struct Mount {
  pub path: PathBuf,
  pub fs: Arc<Xvfs>,
  pub commit: ObjectId,
  pub mount_id: MountId,
  pub capability: String,
  session: Option<fuser::BackgroundSession>,
  _cache_dir: tempfile::TempDir,
  _mount_dir: tempfile::TempDir,
}

impl Mount {
  pub async fn new(backend: &Backend, revision: &str) -> Mount {
    Mount::with_config(backend, revision, FsConfig::default()).await
  }

  pub async fn with_config(backend: &Backend, revision: &str, config: FsConfig) -> Mount {
    let mut grpc = backend.grpc_client().await;
    let mut request = tonic::Request::new(v1::CreateMountRequest {
      repository_id: backend.repo_id.as_str().to_owned(),
      revision_selector: revision.to_owned(),
      requested_ttl_seconds: 0,
    });
    request
      .metadata_mut()
      .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    let grant = grpc.create_mount(request).await.unwrap().into_inner();

    let commit = ObjectId::parse_qualified(&grant.commit_oid).unwrap();
    let tree = ObjectId::parse_qualified(&grant.tree_oid).unwrap();
    let snapshot_time = grant
      .snapshot_time
      .map(|t| xvfs_types::Timestamp::new(t.secs, t.nanos))
      .unwrap();
    let mount_id = MountId::parse(&grant.mount_id).unwrap();

    let client = SnapshotClient::connect(
      &backend.grpc,
      &backend.http,
      TOKEN,
      MountBinding {
        repository_id: backend.repo_id.clone(),
        commit: commit.clone(),
        algorithm: HashAlgorithm::Sha1,
        snapshot_time,
      },
      grant.mount_capability.clone(),
    )
    .await
    .unwrap();

    let cache_dir = tempfile::tempdir().unwrap();
    let cache = BlobCache::open(
      cache_dir.path(),
      &backend.repo_id,
      HashAlgorithm::Sha1,
      1 << 30,
    )
    .unwrap();

    let gitdir = Arc::new(GitDir::new(&GitDirFacts {
      repository_id: backend.repo_id.clone(),
      commit: commit.clone(),
      tree: tree.clone(),
      ref_name: grant.ref_name.clone(),
      mount_id: mount_id.clone(),
      snapshot_time,
      grpc_endpoint: backend.grpc.clone(),
      http_endpoint: backend.http.clone(),
      generation: 1,
    }));

    let fs = Xvfs::new(client, cache, gitdir, xvfs_fuse::root_entry(tree), config);

    let mount_dir = tempfile::tempdir().unwrap();
    let path = mount_dir.path().join("workspace");
    std::fs::create_dir(&path).unwrap();

    let session = xvfs_fuse::spawn_mount(
      xvfs_fuse::XvfsFilesystem::new(Arc::clone(&fs), tokio::runtime::Handle::current()),
      &path,
      &MountConfig::default(),
    )
    .unwrap();

    Mount {
      path,
      fs,
      commit,
      mount_id,
      capability: grant.mount_capability,
      session: Some(session),
      _cache_dir: cache_dir,
      _mount_dir: mount_dir,
    }
  }

  pub fn join(&self, relative: &str) -> PathBuf {
    self.path.join(relative)
  }

  /// Unmount explicitly, so a test can assert on what happens afterwards.
  pub fn unmount(&mut self) {
    if let Some(session) = self.session.take() {
      let _ = session.umount_and_join();
    }
  }
}

impl Drop for Mount {
  fn drop(&mut self) {
    // Before the temporary directories go, or the unmount races the removal of
    // its own mount point.
    self.unmount();
  }
}

/// Run blocking filesystem work off the runtime's worker threads.
///
/// Every syscall against the mount blocks until the daemon answers, and the
/// daemon answers on this runtime. Blocking a worker with the reply still queued
/// is how a test deadlocks instead of failing.
pub async fn on_fs<T, F>(f: F) -> T
where
  F: FnOnce() -> T + Send + 'static,
  T: Send + 'static,
{
  tokio::task::spawn_blocking(f).await.unwrap()
}

/// The entries of a directory, sorted, as raw bytes.
pub fn read_dir_names(path: &Path) -> Vec<Vec<u8>> {
  use std::os::unix::ffi::OsStrExt;
  let mut names: Vec<Vec<u8>> = std::fs::read_dir(path)
    .unwrap()
    .map(|e| e.unwrap().file_name().as_bytes().to_vec())
    .collect();
  names.sort();
  names
}
