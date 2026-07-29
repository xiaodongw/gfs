//! M2.1: the mount lifecycle.
//!
//! The daemon runs in-process here rather than as a subprocess. What M2.1 is
//! accountable for is the *ordering* — lease before mount, unpublish before
//! unmount, release last — and the re-pin model, none of which is a property of
//! process boundaries. `scripts/dev-stack.sh` exercises the subprocess path.

use std::time::Duration;

use gfs_mount::control::{self, Request, Response};
use gfs_mount::state::MountState;
use gfs_mount::MountHost;
use gfs_test::mount::{host_config, mount_spec, on_fs, Backend, Job};
use gfs_types::RepositoryId;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mounted_workspace_is_published_and_described() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  let workspace = job.workspace.clone();
  let content = on_fs(move || std::fs::read(workspace.join("README.md")).unwrap()).await;
  assert_eq!(content, b"# basic\n");

  // The workspace *is* the mount, not a symlink to one. This is the property the
  // whole re-pin model exists for: a tool that resolves its own working
  // directory -- which is anything calling `getcwd(2)`, so anything that is not
  // a shell -- sees the path it was given rather than an implementation detail
  // that the next switch invalidates.
  let workspace = job.workspace.clone();
  let resolved = on_fs(move || std::fs::canonicalize(&workspace).unwrap()).await;
  assert_eq!(resolved, job.workspace);
  let workspace = job.workspace.clone();
  let link = on_fs(move || std::fs::read_link(&workspace)).await;
  assert!(link.is_err(), "the workspace is a directory, not a symlink");

  let Response::Inspect(report) = job.call(Request::Inspect).await else {
    panic!("expected an inspect report");
  };
  assert_eq!(report.generation, 1);
  assert!(!report.read_only, "the workspace is writable from M3");
  assert_eq!(report.overlay.entries, 0, "a fresh mount has no edits");
  assert!(report.publication.starts_with("direct("));
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
  assert_eq!(state.api_version, gfs_types::API_VERSION);
  assert_eq!(state.state_format_version, gfs_types::STATE_FORMAT_VERSION);
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
  assert_eq!(health.state, gfs_mount::HealthState::Warning);
  assert!(health.last_error.is_some());
  assert!(
    !health.is_healthy(),
    "`gfs health` exits non-zero on exactly this"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refresh_repins_in_place_and_keeps_the_workspace_path_and_open_handles() {
  // M2's exit criterion, as ADR 0003's second amendment restates it: a re-pin
  // does not move the mount, a descriptor opened before it keeps reading what it
  // opened, and a working directory inside the workspace stays valid.
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

  // The mount did not move. This is the assertion the old generation model could
  // not make, and the reason for the change: a process that resolved this path
  // before the re-pin is still standing in a live workspace after it.
  let workspace = job.workspace.clone();
  let resolved = on_fs(move || std::fs::canonicalize(&workspace).unwrap()).await;
  assert_eq!(resolved, job.workspace);

  // The descriptor opened before the swap still reads what it opened.
  let content = on_fs(move || {
    let mut buffer = Vec::new();
    open.read_to_end(&mut buffer).unwrap();
    buffer
  })
  .await;
  assert_eq!(content, b"# basic\n");

  // And a fresh read through the workspace works too, against the new pin.
  let workspace = job.workspace.clone();
  let fresh = on_fs(move || std::fs::read(workspace.join("README.md")).unwrap()).await;
  assert_eq!(fresh, b"# basic\n");

  // One pin, one lease. Nothing is left alive alongside it to be reaped, which
  // is what the retiring-generation machinery used to exist for.
  let Response::Inspect(report) = job.call(Request::Inspect).await else {
    panic!("expected an inspect report");
  };
  assert_eq!(report.generation, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switching_to_another_commit_changes_what_already_cached_paths_read() {
  // The correctness question the in-place model creates and the generation model
  // never had to answer: the kernel has cached dentries and attributes for the
  // old commit, and a re-pin that did not invalidate them would serve the old
  // tree indefinitely -- a wrong answer rather than a slow one.
  //
  // `basic` moves all three ways between `v1.0` and `main`:
  //   src/main.rs   modified  ("bye" on main, "hi" at v1.0)
  //   src/new.rs    added on main, so absent at v1.0
  //   docs/guide.md deleted on main, so present at v1.0
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  // Read first, so the kernel is actually holding what has to be invalidated.
  // Without this the test would pass against a daemon that invalidated nothing.
  let ws = job.workspace.clone();
  let before = on_fs(move || {
    (
      std::fs::read(ws.join("src/main.rs")).unwrap(),
      ws.join("src/new.rs").exists(),
      ws.join("docs/guide.md").exists(),
    )
  })
  .await;
  assert_eq!(before.0, b"fn main() { println!(\"bye\"); }\n");
  assert!(before.1, "src/new.rs is on main");
  assert!(!before.2, "docs/guide.md was deleted on main");

  let Response::Refresh(report) = job
    .call(Request::Switch {
      selector: "v1.0".to_owned(),
      branch: None,
    })
    .await
  else {
    panic!("expected a switch report");
  };
  assert!(!report.unchanged, "v1.0 is not main");

  let ws = job.workspace.clone();
  let after = on_fs(move || {
    (
      std::fs::read(ws.join("src/main.rs")).unwrap(),
      ws.join("src/new.rs").exists(),
    )
  })
  .await;
  assert_eq!(
    after.0, b"fn main() { println!(\"hi\"); }\n",
    "a cached path reads the new pin's bytes, not the old one's"
  );
  assert!(
    !after.1,
    "a path the new pin does not have stopped resolving"
  );

  // The one case that is *not* immediate. `docs/guide.md` was missed above, and
  // a miss is cached with `FsConfig::negative_ttl` -- a negative dentry is not
  // enumerable, so unlike a positive one it cannot be invalidated, only waited
  // out. This asserts the window is real and bounded rather than pretending it
  // is not there; `git switch` has no equivalent because it rewrites the tree
  // through the kernel rather than underneath it.
  let ws = job.workspace.clone();
  let appeared = on_fs(move || {
    for _ in 0..40 {
      if ws.join("docs/guide.md").exists() {
        return true;
      }
      std::thread::sleep(Duration::from_millis(100));
    }
    false
  })
  .await;
  assert!(
    appeared,
    "a path the new pin adds becomes visible once the negative entry expires"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_edited_back_to_its_original_bytes_does_not_block_a_switch() {
  // `switch` and `refresh` used to gate on `Overlay::is_empty`, which answers
  // "does the journal have rows" -- and opening a base file for writing leaves a
  // row whether or not the bytes end up different. So an edit that was undone
  // produced a workspace that `gfs status` called clean and `gfs switch` refused
  // as dirty, naming changes the user had no way to find or discard.
  //
  // Both now ask `Status::is_clean`, which is what `status` reports.
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  let ws = job.workspace.clone();
  let original = on_fs(move || std::fs::read(ws.join("src/main.rs")).unwrap()).await;

  let ws = job.workspace.clone();
  let edited = original.clone();
  on_fs(move || {
    std::fs::write(ws.join("src/main.rs"), b"scratch\n").unwrap();
    // Written back byte for byte. The copy-up row survives; the change does not.
    std::fs::write(ws.join("src/main.rs"), &edited).unwrap();
  })
  .await;

  let Response::Status(status) = job.call(Request::Status).await else {
    panic!("expected a status report");
  };
  assert!(
    status.status.is_clean(),
    "status is the definition of clean: {:?}",
    status.status.changes
  );

  let Response::Refresh(report) = job.call(Request::Refresh).await else {
    panic!("a refresh must not refuse a workspace status calls clean");
  };
  assert_eq!(report.generation, 2);

  let Response::Refresh(_) = job
    .call(Request::Switch {
      selector: "v1.0".to_owned(),
      branch: None,
    })
    .await
  else {
    panic!("a switch must not refuse a workspace status calls clean");
  };

  let ws = job.workspace.clone();
  let after = on_fs(move || std::fs::read(ws.join("src/main.rs")).unwrap()).await;
  assert_eq!(
    after, b"fn main() { println!(\"hi\"); }\n",
    "and the switch actually happened"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_superseded_overlay_directory_is_removed_rather_than_accumulated() {
  // Each pin gets its own overlay directory, so a pin that is superseded without
  // its directory being removed leaks one SQLite database per re-pin for the
  // life of the job. The guard used to be `Overlay::is_empty`, which is false
  // exactly when a `commit` has just put rows in it -- so the one path that
  // reliably leaves rows behind was the one path that never cleaned up.
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  let overlays = job.state_dir.join("overlay");
  let count = |dir: &std::path::Path| {
    std::fs::read_dir(dir)
      .map(|entries| entries.count())
      .unwrap_or(0)
  };
  assert_eq!(count(&overlays), 1, "one pin, one overlay");

  // Dirty it, so the superseded overlay has rows the old guard would have kept.
  let ws = job.workspace.clone();
  on_fs(move || std::fs::write(ws.join("src/main.rs"), b"local edit\n").unwrap()).await;

  // `AdoptCommit` is the re-pin `gfs commit` performs, and the only one that
  // does not require a clean workspace -- which is why it is the leaking path.
  let Response::Inspect(report) = job.call(Request::Inspect).await else {
    panic!("expected an inspect report");
  };
  let Response::Refresh(_) = job
    .call(Request::AdoptCommit {
      commit: report.commit.clone(),
    })
    .await
  else {
    panic!("expected a re-pin report");
  };

  for _ in 0..50 {
    if count(&overlays) == 1 {
      return;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
  panic!(
    "the superseded overlay directory was kept: {} remain",
    count(&overlays)
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_working_directory_inside_the_workspace_survives_a_repin() {
  // The flow this whole change exists to support: start a tool in the workspace,
  // switch underneath it, keep working. `git switch` behaves exactly this way,
  // and before ADR 0003's second amendment GFS did not: the workspace was a
  // symlink, so a process that had resolved its own working directory was left
  // standing in a generation the refresh had just retired.
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  // `canonicalize` is what a process does to itself at startup, and is the exact
  // operation that used to capture a doomed generation directory.
  let workspace = job.workspace.clone();
  let cwd = on_fs(move || std::fs::canonicalize(&workspace).unwrap()).await;

  let Response::Refresh(_) = job.call(Request::Refresh).await else {
    panic!("expected a refresh report");
  };

  let after = cwd.clone();
  let content = on_fs(move || std::fs::read(after.join("README.md")).unwrap()).await;
  assert_eq!(
    content, b"# basic\n",
    "the physical path a tool captured before the re-pin still serves reads"
  );
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

  // The workspace is an ordinary empty directory again, not a mount point
  // returning ENOTCONN for every operation -- which is what ADR 0003 measured a
  // mount outliving its daemon does, and what an orchestrator has to be able to
  // distinguish a clean release from.
  //
  // The directory itself stays. It is the path the caller named, `umount`
  // leaving its mount point behind is the Unix norm, and removing a directory
  // that a process may be standing in is a worse outcome than leaving an empty
  // one. `mount.json` below, not the directory, is the evidence of release.
  let w = workspace.clone();
  on_fs(move || {
    assert!(
      w.is_dir(),
      "the mount point is readable, so it is not stranded"
    );
    assert_eq!(
      std::fs::read_dir(&w).unwrap().count(),
      0,
      "the pinned commit is no longer visible through it"
    );
  })
  .await;

  // `mount.json` is removed, so cleanup can tell a released mount from a live one.
  assert!(MountState::load(&job.state_dir).is_err());
  assert!(!control::is_live(&MountState::control_socket(
    &job.state_dir
  )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_host_over_one_state_directory_is_refused() {
  // Two mounts would each believe they owned the lease, and each would release it
  // on teardown -- so the first teardown would unpin a commit the second is
  // still serving. Across *processes* this is caught by the live control socket
  // beside the workspace, which is the only evidence one host has of another.
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  let tmp = tempfile::tempdir().unwrap();
  let cache = tempfile::tempdir().unwrap();
  let (other, listener) = MountHost::bind(host_config(&backend, tmp.path())).unwrap();
  tokio::spawn(std::sync::Arc::clone(&other).serve(listener));

  let error = other
    .mount(mount_spec(
      &backend,
      "main",
      &job.workspace.with_extension("second"),
      &job.state_dir,
      cache.path(),
      gfs_overlay::OverlayConfig::default(),
    ))
    .await
    .unwrap_err();
  assert_eq!(error.code, gfs_types::ErrorCode::Conflict);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_host_refuses_to_mount_the_same_state_directory_twice() {
  // The same conflict from inside one process, where the registry answers before
  // anything touches the filesystem. Worth its own test because the two checks
  // are independent: the socket check cannot see a mount this host is still
  // creating, and the registry cannot see another process at all.
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  let cache = tempfile::tempdir().unwrap();
  let error = job
    .host
    .mount(mount_spec(
      &backend,
      "main",
      &job.workspace.with_extension("second"),
      &job.state_dir,
      cache.path(),
      gfs_overlay::OverlayConfig::default(),
    ))
    .await
    .unwrap_err();
  assert_eq!(error.code, gfs_types::ErrorCode::Conflict);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_repository_fails_before_anything_is_mounted() {
  // The ordering guarantee from the other side: `CreateMount` runs first, so a
  // rejected mount leaves no mount point, no publication, and no state file.
  let backend = Backend::start("basic").await;
  let tmp = tempfile::tempdir().unwrap();
  let cache = tempfile::tempdir().unwrap();
  let state_dir = tmp.path().join("ws.gfs");

  let (host, listener) = MountHost::bind(host_config(&backend, tmp.path())).unwrap();
  tokio::spawn(std::sync::Arc::clone(&host).serve(listener));

  let mut spec = mount_spec(
    &backend,
    "main",
    &tmp.path().join("ws"),
    &state_dir,
    cache.path(),
    gfs_overlay::OverlayConfig::default(),
  );
  spec.repository_id = RepositoryId::parse("r-absent").unwrap();
  let error = host.mount(spec).await.unwrap_err();

  assert!(
    matches!(
      error.code,
      gfs_types::ErrorCode::NotFound | gfs_types::ErrorCode::PermissionDenied
    ),
    "{error:?}"
  );
  assert!(!tmp.path().join("ws").exists(), "nothing was published");
  assert!(
    MountState::load(&state_dir).is_err(),
    "no state was written"
  );
}
