//! One `gfs-fuse` process serving several mounts.
//!
//! What changed when the host arrived is not what a mount does — that is
//! `lifecycle.rs` — but what happens when there is more than one of them in a
//! process: whether they are independent, whether they share what they should,
//! and whether the host socket can drive them.

use std::sync::Arc;

use gfs_mount::control::{self, HostRequest, HostResponse, Request, Response};
use gfs_mount::MountHost;
use gfs_test::mount::{host_config, on_fs, Backend, Job};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_workspaces_on_one_host_are_served_independently() {
  // The case `gfs clone repo1 && gfs clone repo2` produces. Each workspace has to
  // be readable through its own path and answerable on its own socket, because
  // that is what every command below `gfs mount` assumes.
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let (second, second_workspace, _second_state) = job.alongside(&backend, "main", "other").await;

  let (first_bytes, second_bytes) = {
    let a = job.workspace.clone();
    let b = second_workspace.clone();
    on_fs(move || {
      (
        std::fs::read(a.join("README.md")).unwrap(),
        std::fs::read(b.join("README.md")).unwrap(),
      )
    })
    .await
  };
  assert_eq!(first_bytes, b"# basic\n");
  assert_eq!(second_bytes, first_bytes);

  // Two sockets, two mount identities, one process.
  assert!(control::is_live(&job.socket()));
  assert!(control::is_live(
    &gfs_mount::state::workspace_control_socket(&second_workspace)
  ));
  assert_ne!(job.daemon.inspect().mount_id, second.inspect().mount_id);
  assert_eq!(job.daemon.inspect().daemon_pid, second.inspect().daemon_pid);

  // Unmounting one must not disturb the other. This is the property the old
  // one-process-per-mount model got for free and the host has to earn.
  let socket = gfs_mount::state::workspace_control_socket(&second_workspace);
  let reply = on_fs(move || control::call(&socket, &Request::Unmount).unwrap()).await;
  assert!(matches!(reply, Response::Unmounted));

  let still_there = {
    let a = job.workspace.clone();
    on_fs(move || std::fs::read(a.join("README.md")).unwrap()).await
  };
  assert_eq!(still_there, b"# basic\n");
  // The other workspace holds only its own `.git` again (ADR 0011). The mount
  // point survives the unmount -- see `lifecycle.rs` -- so "just .git" rather
  // than "absent" is what distinguishes a released mount from a live one.
  let released = second_workspace.clone();
  on_fs(move || {
    let names: Vec<_> = std::fs::read_dir(&released)
      .unwrap()
      .map(|e| e.unwrap().file_name())
      .collect();
    assert_eq!(
      names,
      vec![std::ffi::OsString::from(".git")],
      "the other workspace stopped serving its commit"
    );
  })
  .await;

  // And the host forgot it, rather than keeping a dead entry that `gfs daemon
  // status` would report as live.
  for _ in 0..200 {
    if job.host.list().len() == 1 {
      break;
    }
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
  }
  let listed = job.host.list();
  assert_eq!(listed.len(), 1, "{listed:?}");
  assert_eq!(listed[0].workspace, job.workspace);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_mounts_of_one_repository_share_a_blob_cache() {
  // Under one process per mount each daemon opened its own cache over the same
  // directory, so `--cache-quota` silently meant "per mount" and the same blob
  // was downloaded once per workspace. One host, one cache per repository.
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let (second, second_workspace, _) = job.alongside(&backend, "main", "other").await;

  let first = job.workspace.clone();
  on_fs(move || std::fs::read(first.join("README.md")).unwrap()).await;
  let after_first = job.daemon.inspect().cache;
  assert_eq!(after_first.fetches, 1);
  assert!(after_first.bytes_fetched > 0);

  let b = second_workspace.clone();
  on_fs(move || std::fs::read(b.join("README.md")).unwrap()).await;
  let after_second = second.inspect().cache;

  assert_eq!(
    after_second.bytes_fetched, after_first.bytes_fetched,
    "the second workspace re-downloaded a blob the first had already cached"
  );
  assert_eq!(
    after_second.fetches, 1,
    "still one fetch across both mounts"
  );
  assert!(
    after_second.hits > after_first.hits,
    "the second read should have been a cache hit"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_host_socket_reports_and_tears_down_its_mounts() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let (_, second_workspace, _) = job.alongside(&backend, "main", "other").await;
  let socket = job.host.config().socket.clone();

  let HostResponse::Info(info) = call(&socket, HostRequest::Info).await else {
    panic!("the host answered an info request with something else");
  };
  assert_eq!(info.pid, std::process::id());
  assert_eq!(info.mounts, 2);

  let HostResponse::Mounts { mounts } = call(&socket, HostRequest::ListMounts).await else {
    panic!("the host answered a list request with something else");
  };
  assert_eq!(mounts.len(), 2);
  assert!(mounts
    .iter()
    .all(|m| m.commit == job.daemon.inspect().commit));

  let destroy = HostRequest::DestroyMount {
    workspace: second_workspace.clone(),
  };
  assert!(matches!(
    call(&socket, destroy).await,
    HostResponse::Destroyed
  ));
  // Holding only its own `.git` again (ADR 0011): the projected tree is gone.
  let released: Vec<_> = std::fs::read_dir(&second_workspace)
    .unwrap()
    .map(|e| e.unwrap().file_name())
    .collect();
  assert_eq!(
    released,
    vec![std::ffi::OsString::from(".git")],
    "the destroyed mount stopped serving its commit"
  );

  let HostResponse::Mounts { mounts } = call(&socket, HostRequest::ListMounts).await else {
    panic!("the host answered a list request with something else");
  };
  assert_eq!(mounts.len(), 1);
  assert_eq!(mounts[0].workspace, job.workspace);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_host_on_one_socket_is_refused() {
  // Without the lock the loser would unlink the winner's socket and both would
  // believe they were serving, leaving clients connecting to an unlinked inode.
  let backend = Backend::start("basic").await;
  let tmp = tempfile::tempdir().unwrap();

  let (first, listener) = MountHost::bind(host_config(&backend, tmp.path())).unwrap();
  tokio::spawn(Arc::clone(&first).serve(listener));

  let error = MountHost::bind(host_config(&backend, tmp.path())).unwrap_err();
  assert_eq!(error.code, gfs_types::ErrorCode::Conflict);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_from_another_state_format_is_refused_rather_than_served() {
  // The skew a long-lived host makes possible: a rebuilt CLI against a host from
  // the previous build. Refusing names the fix; serving would write a state
  // directory the client cannot read back.
  let backend = Backend::start("basic").await;
  let tmp = tempfile::tempdir().unwrap();
  let (host, listener) = MountHost::bind(host_config(&backend, tmp.path())).unwrap();
  let socket = host.config().socket.clone();
  tokio::spawn(Arc::clone(&host).serve(listener));

  let request = gfs_mount::control::MountRequest {
    state_format_version: gfs_types::STATE_FORMAT_VERSION + 1,
    workspace: tmp.path().join("ws"),
    cache_dir: tmp.path().join("cache"),
    repository_id: backend.repo_id.as_str().to_owned(),
    revision_selector: "main".to_owned(),
    cache_quota_bytes: 1 << 20,
    overlay_quota_bytes: 1 << 20,
    allow_other: false,
    fuse_threads: None,
    grpc_endpoint: None,
    http_endpoint: None,
    token: None,
    local_clone: None,
    writeback_cache: false,
  };
  let HostResponse::Error { code, .. } =
    call(&socket, HostRequest::CreateMount(Box::new(request))).await
  else {
    panic!("a version-skewed client was served");
  };
  assert_eq!(code, "FAILED_PRECONDITION");
  assert!(host.list().is_empty());
}

/// The control protocol is synchronous, so it runs off the runtime's workers for
/// the same reason every filesystem call in this suite does.
async fn call(socket: &std::path::Path, request: HostRequest) -> HostResponse {
  let socket = socket.to_path_buf();
  on_fs(move || control::call_host(&socket, &request).unwrap()).await
}

/// A mount created straight from a spec is the same thing the wire request builds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mount_created_over_the_host_socket_is_usable_immediately() {
  // `CreateMount` must not answer before the per-mount socket is bound: the CLI
  // prints an inspect report the moment it returns, and `gfs status` may run
  // straight after.
  let backend = Backend::start("basic").await;
  let tmp = tempfile::tempdir().unwrap();
  let (host, listener) = MountHost::bind(host_config(&backend, tmp.path())).unwrap();
  let socket = host.config().socket.clone();
  tokio::spawn(Arc::clone(&host).serve(listener));

  let workspace = tmp.path().join("ws");
  let request = gfs_mount::control::MountRequest {
    state_format_version: gfs_types::STATE_FORMAT_VERSION,
    workspace: workspace.clone(),
    cache_dir: tmp.path().join("cache"),
    repository_id: backend.repo_id.as_str().to_owned(),
    revision_selector: "main".to_owned(),
    cache_quota_bytes: 1 << 30,
    overlay_quota_bytes: 1 << 20,
    allow_other: false,
    fuse_threads: None,
    grpc_endpoint: None,
    http_endpoint: None,
    token: None,
    local_clone: None,
    writeback_cache: false,
  };
  let HostResponse::Mounted(report) =
    call(&socket, HostRequest::CreateMount(Box::new(request))).await
  else {
    panic!("the host refused a well-formed mount request");
  };

  // No sleep, no polling: both of these must hold the instant the reply lands.
  assert!(control::is_live(
    &gfs_mount::state::workspace_control_socket(&workspace)
  ));
  let content = {
    let w = workspace.clone();
    on_fs(move || std::fs::read(w.join("README.md")).unwrap()).await
  };
  assert_eq!(content, b"# basic\n");
  assert_eq!(report.workspace, workspace.display().to_string());
}
