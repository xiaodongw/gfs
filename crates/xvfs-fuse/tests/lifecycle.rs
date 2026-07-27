//! M2.1: the mount lifecycle.
//!
//! The daemon runs in-process here rather than as a subprocess. What M2.1 is
//! accountable for is the *ordering* — lease before mount, unpublish before
//! unmount, release last — and the generation model, none of which is a property
//! of process boundaries. `scripts/dev-stack.sh` exercises the subprocess path.

mod harness;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use harness::{on_fs, Backend};
use xvfs_fuse::control::{self, Request, Response};
use xvfs_fuse::daemon::{Daemon, DaemonConfig};
use xvfs_fuse::state::MountState;
use xvfs_fuse::{FsConfig, MountConfig};
use xvfs_types::{LeasePolicy, RepositoryId};

struct Job {
  daemon: Arc<Daemon>,
  workspace: PathBuf,
  state_dir: PathBuf,
  _tmp: tempfile::TempDir,
  _cache: tempfile::TempDir,
}

impl Job {
  async fn start(backend: &Backend, revision: &str) -> Job {
    let tmp = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("ws");
    let state_dir = tmp.path().join("ws.xvfs");

    let daemon = Daemon::start(DaemonConfig {
      state_dir: state_dir.clone(),
      workspace: workspace.clone(),
      cache_dir: cache.path().to_path_buf(),
      grpc_endpoint: backend.grpc.clone(),
      http_endpoint: backend.http.clone(),
      token: harness::TOKEN.to_owned(),
      repository_id: backend.repo_id.clone(),
      revision_selector: revision.to_owned(),
      cache_quota_bytes: 1 << 30,
      fs: FsConfig::default(),
      overlay: xvfs_overlay::OverlayConfig::default(),
      mount: MountConfig::default(),
      lease_policy: LeasePolicy::adr_0006(),
      // Short, so the retirement case does not take five minutes to fail.
      retire_timeout: Duration::from_secs(5),
    })
    .await
    .unwrap();

    tokio::spawn(Arc::clone(&daemon).serve_control());
    // The socket exists only once `serve_control` has bound it.
    let socket = MountState::control_socket(&state_dir);
    for _ in 0..200 {
      if control::is_live(&socket) {
        break;
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }

    Job {
      daemon,
      workspace,
      state_dir,
      _tmp: tmp,
      _cache: cache,
    }
  }

  async fn call(&self, request: Request) -> Response {
    let socket = MountState::control_socket(&self.state_dir);
    on_fs(move || control::call(&socket, &request).unwrap())
      .await
      .into_result()
      .unwrap()
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mounted_workspace_is_published_and_described() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  let workspace = job.workspace.clone();
  let content = on_fs(move || std::fs::read(workspace.join("README.md")).unwrap()).await;
  assert_eq!(content, b"# basic\n");

  // Published through the seam, so the workspace is a symlink into the state
  // directory's generation 1 rather than the mount point itself.
  let workspace = job.workspace.clone();
  let target = on_fs(move || std::fs::read_link(&workspace).unwrap()).await;
  assert_eq!(target, MountState::generation_dir(&job.state_dir, 1));

  let Response::Inspect(report) = job.call(Request::Inspect).await else {
    panic!("expected an inspect report");
  };
  assert_eq!(report.generation, 1);
  assert!(!report.read_only, "the workspace is writable from M3");
  assert_eq!(report.overlay.entries, 0, "a fresh mount has no edits");
  assert!(report.retiring_generations.is_empty());
  assert!(report.publication.starts_with("symlink("));
  assert_eq!(report.commit, job.daemon.inspect().commit);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mount_state_records_the_pinned_commit_and_is_not_world_readable() {
  use std::os::unix::fs::PermissionsExt;

  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  let state = MountState::load(&job.state_dir).unwrap();
  assert_eq!(state.revision_selector, "main");
  assert_eq!(state.generation, 1);
  assert_eq!(state.api_version, xvfs_types::API_VERSION);
  assert_eq!(state.state_format_version, xvfs_types::STATE_FORMAT_VERSION);
  assert!(
    !state.lease.capability.is_empty(),
    "a restart must be able to renew"
  );

  let mode = std::fs::metadata(MountState::path(&job.state_dir))
    .unwrap()
    .permissions()
    .mode();
  assert_eq!(mode & 0o777, 0o600);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_reports_a_renewing_lease_and_a_renewal_extends_it() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  let Response::Health(health) = job.call(Request::Health).await else {
    panic!("expected health");
  };
  assert!(health.is_healthy());
  assert_eq!(health.consecutive_failures, 0);
  let first_expiry = health.lease_expiry;

  // The heartbeat's own interval is five minutes, so the renewal is driven
  // directly rather than waited for.
  job.daemon.renew_all().await;
  let Response::Health(health) = job.call(Request::Health).await else {
    panic!("expected health");
  };
  assert!(health.is_healthy());
  assert!(health.last_renewal.is_some());
  assert!(
    health.lease_expiry >= first_expiry,
    "a renewal must not shorten the lease"
  );

  // And the renewal is durable: a restarted daemon reads the new expiry.
  let state = MountState::load(&job.state_dir).unwrap();
  assert_eq!(state.lease.expires_at, health.lease_expiry);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_renewal_failure_is_surfaced_rather_than_swallowed() {
  // ADR 0006: "lease renewal failing -- warn at 2 failures ... never silent."
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  backend.stop();
  tokio::time::sleep(Duration::from_millis(200)).await;

  job.daemon.renew_all().await;
  job.daemon.renew_all().await;

  let health = job.daemon.health();
  assert_eq!(health.consecutive_failures, 2);
  assert_eq!(health.state, xvfs_fuse::HealthState::Warning);
  assert!(health.last_error.is_some());
  assert!(
    !health.is_healthy(),
    "`xvfs health` exits non-zero on exactly this"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refresh_swaps_generations_and_keeps_open_handles_on_the_old_one() {
  // M2's exit criterion: refresh exposes only the old or the new generation,
  // open old-generation handles remain valid until close, and no kernel-cached
  // path mixes generations.
  use std::io::Read;

  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  let workspace = job.workspace.clone();
  let mut open = on_fs(move || std::fs::File::open(workspace.join("README.md")).unwrap()).await;

  let Response::Refresh(report) = job.call(Request::Refresh).await else {
    panic!("expected a refresh report");
  };
  assert_eq!(report.previous_generation, 1);
  assert_eq!(report.generation, 2);
  assert!(
    report.unchanged,
    "the selector still resolves to the same commit"
  );

  // The workspace now resolves into generation 2.
  let workspace = job.workspace.clone();
  let target = on_fs(move || std::fs::read_link(&workspace).unwrap()).await;
  assert_eq!(target, MountState::generation_dir(&job.state_dir, 2));

  // The descriptor opened before the swap still reads, from generation 1.
  let content = on_fs(move || {
    let mut buffer = Vec::new();
    open.read_to_end(&mut buffer).unwrap();
    buffer
  })
  .await;
  assert_eq!(content, b"# basic\n");

  // And a fresh read through the workspace works too, from generation 2.
  let workspace = job.workspace.clone();
  let fresh = on_fs(move || std::fs::read(workspace.join("README.md")).unwrap()).await;
  assert_eq!(fresh, b"# basic\n");

  // Generation 1 retires once its last handle closes.
  for _ in 0..100 {
    let Response::Inspect(report) = job.call(Request::Inspect).await else {
      panic!("expected an inspect report");
    };
    if report.retiring_generations.is_empty() {
      return;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
  panic!("generation 1 was never retired");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unmount_unpublishes_releases_and_leaves_no_live_mount_behind() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let workspace = job.workspace.clone();
  let w = workspace.clone();
  on_fs(move || assert!(w.join("README.md").exists())).await;

  assert!(matches!(
    job.call(Request::Unmount).await,
    Response::Unmounted
  ));

  // The workspace is gone rather than present-but-broken. ADR 0003 measured that
  // a mount point outliving its daemon returns ENOTCONN for every operation, so
  // "removed" is the outcome an orchestrator can distinguish from a leak.
  let w = workspace.clone();
  on_fs(move || assert!(!w.exists())).await;

  // `mount.json` is removed, so cleanup can tell a released mount from a live one.
  assert!(MountState::load(&job.state_dir).is_err());
  assert!(!control::is_live(&MountState::control_socket(
    &job.state_dir
  )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_daemon_over_one_state_directory_is_refused() {
  // Two daemons would each believe they own the lease, and each would release it
  // on teardown -- so the first teardown would unpin a commit the second is
  // still serving.
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  let cache = tempfile::tempdir().unwrap();
  let error = Daemon::start(DaemonConfig {
    state_dir: job.state_dir.clone(),
    workspace: job.workspace.with_extension("second"),
    cache_dir: cache.path().to_path_buf(),
    grpc_endpoint: backend.grpc.clone(),
    http_endpoint: backend.http.clone(),
    token: harness::TOKEN.to_owned(),
    repository_id: backend.repo_id.clone(),
    revision_selector: "main".to_owned(),
    cache_quota_bytes: 1 << 20,
    fs: FsConfig::default(),
    overlay: xvfs_overlay::OverlayConfig::default(),
    mount: MountConfig::default(),
    lease_policy: LeasePolicy::adr_0006(),
    retire_timeout: Duration::from_secs(5),
  })
  .await
  .unwrap_err();
  assert_eq!(error.code, xvfs_types::ErrorCode::Conflict);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_repository_fails_before_anything_is_mounted() {
  // The ordering guarantee from the other side: `CreateMount` runs first, so a
  // rejected mount leaves no mount point, no publication, and no state file.
  let backend = Backend::start("basic").await;
  let tmp = tempfile::tempdir().unwrap();
  let cache = tempfile::tempdir().unwrap();
  let state_dir = tmp.path().join("ws.xvfs");

  let error = Daemon::start(DaemonConfig {
    state_dir: state_dir.clone(),
    workspace: tmp.path().join("ws"),
    cache_dir: cache.path().to_path_buf(),
    grpc_endpoint: backend.grpc.clone(),
    http_endpoint: backend.http.clone(),
    token: harness::TOKEN.to_owned(),
    repository_id: RepositoryId::parse("r-absent").unwrap(),
    revision_selector: "main".to_owned(),
    cache_quota_bytes: 1 << 20,
    fs: FsConfig::default(),
    overlay: xvfs_overlay::OverlayConfig::default(),
    mount: MountConfig::default(),
    lease_policy: LeasePolicy::adr_0006(),
    retire_timeout: Duration::from_secs(5),
  })
  .await
  .unwrap_err();

  assert!(
    matches!(
      error.code,
      xvfs_types::ErrorCode::NotFound | xvfs_types::ErrorCode::PermissionDenied
    ),
    "{error:?}"
  );
  assert!(!tmp.path().join("ws").exists(), "nothing was published");
  assert!(
    MountState::load(&state_dir).is_err(),
    "no state was written"
  );
}
