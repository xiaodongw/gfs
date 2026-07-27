//! `xvfsd`: the client daemon that owns a mount.
//!
//! # Ordering is the whole design
//!
//! `CreateMount` **first**, then `mount.json`, then the FUSE session, then
//! publication. DESIGN.md section 7.1 makes `CreateMount` one atomic server-side
//! operation — resolve, authorize, write a `PREPARING` lease, create the
//! reachability anchor, mark it `ACTIVE` — so that there is no client-visible gap
//! between resolving a selector and pinning what it resolved to. A daemon that
//! mounted first and leased afterwards would have a window in which a force push
//! prunes the commit it is about to serve.
//!
//! Teardown runs in the opposite order: unpublish, unmount, release. Releasing
//! the lease while a mount can still read through it is the same window with the
//! sign flipped.
//!
//! # Generations
//!
//! `xvfs refresh` does not mutate a live mount. It creates a whole new generation
//! — a new `CreateMount`, a new FUSE session at a new mount point — and swaps the
//! publication over atomically. The old generation and *its lease* stay alive
//! until every handle opened through it closes.
//!
//! PLAN.md M2.1 is explicit that a refresh must never mutate the pinned base
//! under existing kernel dentries. This is why: a kernel that has cached
//! `src/main.rs` at inode 42 for the old commit must keep resolving it there
//! until the last reader lets go, and the only way to guarantee that is to leave
//! the old filesystem mounted.
//!
//! # Failure surfaces, it does not accumulate
//!
//! A renewal failure is counted and reported (ADR 0006: warn at two consecutive
//! failures). It does not stop the heartbeat, because the whole point of grace is
//! that a transient control-plane outage should be survivable.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use xvfs_overlay::{Binding, Overlay, OverlayConfig};
use xvfs_proto::v1;
use xvfs_types::error::{ErrorCode, XvfsError};
use xvfs_types::{
  HashAlgorithm, LeasePolicy, LeaseState, MountId, ObjectId, RepositoryId, Timestamp,
};

use crate::cache::BlobCache;
use crate::client::{MountBinding, SnapshotClient};
use crate::control::{MountReport, RefreshReport, Request, Response};
use crate::fs::{FsConfig, Xvfs, XvfsFilesystem};
use crate::gitdir::{GitDir, GitDirFacts};
use crate::lease::{LeaseHealth, LeaseMonitor};
use crate::publish::{MountPublisher, SymlinkPublisher};
use crate::session::MountConfig;
use crate::state::{prepare_state_dir, LeaseRecord, MountState};

#[derive(Clone, Debug)]
pub struct DaemonConfig {
  pub state_dir: PathBuf,
  pub workspace: PathBuf,
  pub cache_dir: PathBuf,
  pub grpc_endpoint: String,
  pub http_endpoint: String,
  pub token: String,
  pub repository_id: RepositoryId,
  pub revision_selector: String,
  pub cache_quota_bytes: u64,
  pub fs: FsConfig,
  pub overlay: OverlayConfig,
  pub mount: MountConfig,
  pub lease_policy: LeasePolicy,
  /// How long a retiring generation may keep handles open before it is torn down
  /// anyway. Bounded because a process that leaks a descriptor must not leak a
  /// lease and a mount with it for the life of the job.
  pub retire_timeout: Duration,
}

/// One mounted generation.
struct Generation {
  number: u64,
  mountpoint: PathBuf,
  overlay_dir: PathBuf,
  fs: Arc<Xvfs>,
  overlay: Arc<Overlay>,
  client: Arc<SnapshotClient>,
  monitor: Arc<LeaseMonitor>,
  session: Option<fuser::BackgroundSession>,
  commit: ObjectId,
  tree: ObjectId,
  ref_name: Option<String>,
  snapshot_time: Timestamp,
}

impl std::fmt::Debug for Generation {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Generation")
      .field("number", &self.number)
      .field("commit", &self.commit)
      .finish_non_exhaustive()
  }
}

/// How long an unmount may wait for the kernel before it is forced.
///
/// A plain `umount` of a filesystem with an open descriptor fails with `EBUSY`,
/// and `fuser`'s `umount_and_join` then waits for a session thread that will not
/// exit — so an unbounded teardown hangs for as long as one process holds one
/// file. That is the wrong failure for job cleanup: ADR 0003 already establishes
/// that a leaked mount point is visible and inert (`ENOTCONN`), while a cleanup
/// step that never returns strands the whole job.
const UNMOUNT_TIMEOUT: Duration = Duration::from_secs(5);

/// Unmount, and force it if the kernel will not cooperate in time.
///
/// The lazy unmount detaches the mount point immediately and lets the kernel
/// release it once the last descriptor closes, which is exactly the semantics a
/// teardown wants: the workspace stops being reachable now, and the process that
/// is still reading finishes reading.
async fn unmount_session(session: fuser::BackgroundSession, mountpoint: PathBuf, generation: u64) {
  let mut joined = Box::pin(tokio::task::spawn_blocking(move || {
    session.umount_and_join()
  }));
  if tokio::time::timeout(UNMOUNT_TIMEOUT, &mut joined)
    .await
    .is_ok()
  {
    return;
  }
  tracing::warn!(
    generation,
    mountpoint = %mountpoint.display(),
    "the mount is still busy after the unmount timeout; forcing a lazy unmount"
  );
  let _ = std::process::Command::new("fusermount3")
    .args(["-u", "-z"])
    .arg(&mountpoint)
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status();
  if tokio::time::timeout(UNMOUNT_TIMEOUT, joined).await.is_err() {
    tracing::error!(
      generation,
      "the FUSE session thread did not exit even after a lazy unmount"
    );
  }
}

impl Generation {
  /// Unmount and release, in that order. See the module docs on ordering.
  async fn tear_down(mut self) {
    if let Some(session) = self.session.take() {
      unmount_session(session, self.mountpoint.clone(), self.number).await;
    }
    if let Err(e) = self.client.release_mount(self.monitor.mount_id()).await {
      // Not fatal: the lease expires on its own. Logged because a release that
      // keeps failing means orphan leases are accumulating somewhere.
      tracing::warn!(generation = self.number, error = %e.message, "releasing the lease failed");
    }
    let _ = std::fs::remove_dir(&self.mountpoint);
    // A retired generation's overlay is empty by construction -- `xvfs refresh`
    // refuses a dirty workspace -- so removing it discards nothing. Leaving it
    // would accumulate one SQLite database per refresh for the life of the job.
    if self.overlay.is_empty() {
      let _ = std::fs::remove_dir_all(&self.overlay_dir);
    }
  }
}

pub struct Daemon {
  config: DaemonConfig,
  cache: Arc<BlobCache>,
  publisher: Box<dyn MountPublisher>,
  current: Mutex<Generation>,
  /// Generations kept alive only until their last handle closes.
  retiring: Mutex<Vec<Arc<Mutex<Option<Generation>>>>>,
  shutting_down: AtomicBool,
}

impl std::fmt::Debug for Daemon {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Daemon")
      .field("workspace", &self.config.workspace)
      .finish_non_exhaustive()
  }
}

impl Daemon {
  /// Create the lease, mount, and publish. Returns once the workspace is usable.
  pub async fn start(config: DaemonConfig) -> Result<Arc<Self>, XvfsError> {
    // Absolute before anything else. The workspace is published as a symlink,
    // and a symlink target is resolved relative to the *link's* directory, not
    // to the daemon's working directory -- so a relative `--state-dir` produces
    // a link that points at a path that does not exist. It also means the daemon
    // keeps working if something later changes its working directory.
    let config = DaemonConfig {
      state_dir: absolute(&config.state_dir)?,
      workspace: absolute(&config.workspace)?,
      cache_dir: absolute(&config.cache_dir)?,
      ..config
    };
    prepare_state_dir(&config.state_dir)?;
    adopt_or_refuse_state_dir(&config.state_dir)?;

    let cache = BlobCache::open(
      &config.cache_dir,
      &config.repository_id,
      HashAlgorithm::Sha1,
      config.cache_quota_bytes,
    )?;
    let publisher: Box<dyn MountPublisher> =
      Box::new(SymlinkPublisher::new(config.workspace.clone())?);

    let generation = mount_generation(&config, &cache, 1).await?;
    publisher.publish(&generation.mountpoint)?;

    let daemon = Arc::new(Daemon {
      cache,
      publisher,
      current: Mutex::new(generation),
      retiring: Mutex::new(Vec::new()),
      shutting_down: AtomicBool::new(false),
      config,
    });
    daemon.persist()?;
    Ok(daemon)
  }

  pub fn config(&self) -> &DaemonConfig {
    &self.config
  }

  pub fn control_socket(&self) -> PathBuf {
    MountState::control_socket(&self.config.state_dir)
  }

  fn persist(&self) -> Result<(), XvfsError> {
    let current = self.current.lock().expect("current generation");
    let health = current.monitor.health();
    MountState {
      state_format_version: xvfs_types::STATE_FORMAT_VERSION,
      api_version: xvfs_types::API_VERSION.to_owned(),
      mount_id: current.monitor.mount_id().as_str().to_owned(),
      repository_id: self.config.repository_id.as_str().to_owned(),
      revision_selector: self.config.revision_selector.clone(),
      commit: current.commit.to_qualified(),
      tree: current.tree.to_qualified(),
      ref_name: current.ref_name.clone(),
      snapshot_time: current.snapshot_time,
      grpc_endpoint: self.config.grpc_endpoint.clone(),
      http_endpoint: self.config.http_endpoint.clone(),
      workspace: self.config.workspace.clone(),
      generation: current.number,
      lease: LeaseRecord {
        state: LeaseState::Active,
        expires_at: health.lease_expiry,
        heartbeat_interval_secs: health.heartbeat_interval_secs,
        capability: current.client.capability_for_persistence(),
      },
      daemon_pid: std::process::id(),
    }
    .store(&self.config.state_dir)
  }

  pub fn health(&self) -> LeaseHealth {
    self
      .current
      .lock()
      .expect("current generation")
      .monitor
      .health()
  }

  pub fn inspect(&self) -> MountReport {
    let current = self.current.lock().expect("current generation");
    let retiring = self
      .retiring
      .lock()
      .expect("retiring generations")
      .iter()
      .filter_map(|slot| {
        slot
          .lock()
          .expect("retiring slot")
          .as_ref()
          .map(|g| g.number)
      })
      .collect();
    MountReport {
      mount_id: current.monitor.mount_id().as_str().to_owned(),
      repository_id: self.config.repository_id.as_str().to_owned(),
      revision_selector: self.config.revision_selector.clone(),
      commit: current.commit.to_qualified(),
      tree: current.tree.to_qualified(),
      ref_name: current.ref_name.clone(),
      snapshot_time: current.snapshot_time,
      workspace: self.config.workspace.display().to_string(),
      publication: self.publisher.describe(),
      generation: current.number,
      retiring_generations: retiring,
      state_dir: self.config.state_dir.display().to_string(),
      daemon_pid: std::process::id(),
      owner_uid: crate::attr::Ownership::current().uid,
      read_only: false,
      overlay: current.overlay.stats(),
      health: current.monitor.health(),
      stats: current.fs.stats(),
      cache: self.cache.stats(),
      live_inodes: current.fs.inode_counts().0,
      assigned_inodes: current.fs.inode_counts().1,
    }
  }

  /// Renew every live generation's lease.
  ///
  /// Every generation, not only the published one: a retiring generation still
  /// has open descriptors reading through its pinned commit, and letting its
  /// lease lapse would prune the objects those reads depend on.
  pub async fn renew_all(&self) {
    let entries: Vec<(Arc<SnapshotClient>, Arc<LeaseMonitor>)> = {
      let current = self.current.lock().expect("current generation");
      let mut entries = vec![(Arc::clone(&current.client), Arc::clone(&current.monitor))];
      for slot in self.retiring.lock().expect("retiring generations").iter() {
        if let Some(generation) = slot.lock().expect("retiring slot").as_ref() {
          entries.push((
            Arc::clone(&generation.client),
            Arc::clone(&generation.monitor),
          ));
        }
      }
      entries
    };

    for (client, monitor) in entries {
      match client.renew_mount(monitor.mount_id()).await {
        Ok(expiry) => monitor.record_success(expiry),
        Err(e) => {
          let failures = monitor.record_failure(e.message.clone());
          if failures >= self.config.lease_policy.alert_after_failures {
            // ADR 0006's alert threshold. At `error` level because this is the
            // point at which a human has roughly ten minutes to act before the
            // grace period starts running out.
            tracing::error!(
              mount_id = monitor.mount_id().as_str(),
              failures,
              error = %e.message,
              "lease renewal is failing; the workspace will stop serving uncached reads after grace"
            );
          } else {
            tracing::warn!(
              mount_id = monitor.mount_id().as_str(),
              failures,
              error = %e.message,
              "lease renewal failed"
            );
          }
        }
      }
    }
    let _ = self.persist();
  }

  /// Run the heartbeat until shutdown.
  pub async fn run_heartbeat(self: Arc<Self>) {
    loop {
      let interval = self
        .current
        .lock()
        .expect("current generation")
        .monitor
        .interval();
      tokio::time::sleep(interval).await;
      if self.shutting_down.load(Ordering::SeqCst) {
        return;
      }
      self.renew_all().await;
    }
  }

  /// Replace the published generation with a freshly resolved one.
  pub async fn refresh(self: &Arc<Self>) -> Result<RefreshReport, XvfsError> {
    // PLAN.md M2.1: refuse when the overlay is non-empty; three-way refresh is
    // out of scope. A new generation is a new pinned commit, and an overlay is
    // bound to the commit it diverged from -- carrying edits across would make
    // every `status` answer be about a base that is no longer mounted.
    if !self.overlay_is_empty() {
      return Err(XvfsError::new(
        ErrorCode::FailedPrecondition,
        "the workspace has local changes; export them or discard them before refreshing \
         (three-way refresh is out of scope)",
      ));
    }

    let (previous_number, previous_commit) = {
      let current = self.current.lock().expect("current generation");
      (current.number, current.commit.clone())
    };
    let next_number = previous_number + 1;
    let fresh = mount_generation(&self.config, &self.cache, next_number).await?;
    let commit = fresh.commit.clone();

    // Published before the old generation is retired, so there is never a moment
    // with no workspace at all.
    self.publisher.publish(&fresh.mountpoint)?;

    let retired = {
      let mut current = self.current.lock().expect("current generation");
      std::mem::replace(&mut *current, fresh)
    };
    self.persist()?;
    self.retire(retired);

    Ok(RefreshReport {
      previous_generation: previous_number,
      generation: next_number,
      previous_commit: previous_commit.to_qualified(),
      commit: commit.to_qualified(),
      unchanged: previous_commit == commit,
    })
  }

  /// The change set, from the journal alone.
  pub async fn status(&self) -> Result<crate::control::StatusReport, XvfsError> {
    let (overlay, commit, ref_name) = {
      let current = self.current.lock().expect("current generation");
      (
        Arc::clone(&current.overlay),
        current.commit.clone(),
        current.ref_name.clone(),
      )
    };
    let algorithm = commit.algorithm();
    let status = tokio::task::spawn_blocking(move || overlay.status(algorithm))
      .await
      .map_err(|e| XvfsError::internal(format!("the status task failed: {e}")))?
      .map_err(crate::fs::overlay_as_service_error)?;
    Ok(crate::control::StatusReport {
      base_commit: commit.to_qualified(),
      ref_name,
      status,
    })
  }

  pub async fn diff(&self) -> Result<Vec<u8>, XvfsError> {
    let (status, base, overlay, commit) = self.export_inputs().await?;
    let exporter = xvfs_overlay::Exporter::new(
      &overlay,
      &base,
      commit.algorithm(),
      self.config.repository_id.as_str(),
      commit.to_qualified(),
    );
    exporter
      .patch(&status)
      .map_err(crate::fs::overlay_as_service_error)
  }

  /// Search the merged workspace.
  ///
  /// Reads the generation's client and overlay under the lock and then releases
  /// it: a search can take seconds, and holding the generation lock for its
  /// duration would block `refresh`, `status`, and every other control command
  /// behind one agent's query.
  pub async fn search(
    &self,
    request: &crate::search::SearchRequest,
  ) -> Result<crate::search::SearchReport, XvfsError> {
    let (client, overlay, commit, ref_name) = {
      let current = self.current.lock().expect("current generation");
      (
        Arc::clone(&current.client),
        Arc::clone(&current.overlay),
        current.commit.clone(),
        current.ref_name.clone(),
      )
    };
    let (outcome, local_matches) = crate::search::search(&client, &overlay, request).await?;
    Ok(crate::search::SearchReport {
      base_commit: commit.to_qualified(),
      ref_name,
      local_matches,
      outcome,
    })
  }

  pub async fn export(&self, bundle: &Path) -> Result<xvfs_overlay::ExportReport, XvfsError> {
    let (status, base, overlay, commit) = self.export_inputs().await?;
    let exporter = xvfs_overlay::Exporter::new(
      &overlay,
      &base,
      commit.algorithm(),
      self.config.repository_id.as_str(),
      commit.to_qualified(),
    );
    exporter
      .write_bundle(&status, bundle)
      .map_err(crate::fs::overlay_as_service_error)
  }

  /// Compute the status and pre-fetch every base blob a diff or export needs.
  ///
  /// The fetch happens here, in async code, because the exporter is synchronous
  /// and the blob path is not. Pre-fetching *only the changed paths* is the
  /// property DESIGN.md section 8.5 asks for -- "diff reads only changed overlay
  /// files and their base blobs" -- and doing it in one place makes that
  /// checkable rather than incidental.
  async fn export_inputs(
    &self,
  ) -> Result<
    (
      xvfs_overlay::Status,
      crate::blobs::PreloadedBase,
      Arc<Overlay>,
      ObjectId,
    ),
    XvfsError,
  > {
    let report = self.status().await?;
    let (overlay, client, commit) = {
      let current = self.current.lock().expect("current generation");
      (
        Arc::clone(&current.overlay),
        Arc::clone(&current.client),
        current.commit.clone(),
      )
    };
    let base =
      crate::blobs::PreloadedBase::fetch(&client, &self.cache, &report.status, &overlay).await?;
    Ok((report.status, base, overlay, commit))
  }

  /// Whether the published generation's overlay holds anything.
  fn overlay_is_empty(&self) -> bool {
    self
      .current
      .lock()
      .expect("current generation")
      .overlay
      .is_empty()
  }

  /// Keep a generation alive until its last handle closes, then tear it down.
  fn retire(self: &Arc<Self>, generation: Generation) {
    let slot = Arc::new(Mutex::new(Some(generation)));
    self
      .retiring
      .lock()
      .expect("retiring generations")
      .push(Arc::clone(&slot));

    let daemon = Arc::clone(self);
    tokio::spawn(async move {
      let deadline = tokio::time::Instant::now() + daemon.config.retire_timeout;
      loop {
        let idle = {
          let guard = slot.lock().expect("retiring slot");
          match guard.as_ref() {
            Some(generation) => generation.fs.open_handles() == 0,
            None => return,
          }
        };
        if idle || tokio::time::Instant::now() >= deadline {
          if !idle {
            tracing::warn!(
              "a retiring mount generation still had open handles after the retire timeout; \
               tearing it down anyway"
            );
          }
          break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
      }
      let generation = slot.lock().expect("retiring slot").take();
      if let Some(generation) = generation {
        generation.tear_down().await;
      }
      daemon
        .retiring
        .lock()
        .expect("retiring generations")
        .retain(|other| other.lock().expect("retiring slot").is_some());
    });
  }

  /// Unpublish, unmount every generation, release every lease, and remove the
  /// state that says a mount is live here.
  pub async fn shutdown(self: &Arc<Self>) {
    if self.shutting_down.swap(true, Ordering::SeqCst) {
      return;
    }
    // Unpublish first: the job must stop being able to reach the mount before
    // the mount stops being able to serve it, or a read in flight sees ENOTCONN
    // rather than a missing workspace.
    let _ = self.publisher.unpublish();

    let retiring: Vec<_> =
      std::mem::take(&mut *self.retiring.lock().expect("retiring generations"));
    for slot in retiring {
      let generation = slot.lock().expect("retiring slot").take();
      if let Some(generation) = generation {
        generation.tear_down().await;
      }
    }

    // Swapped out under the lock and torn down outside it: `tear_down` awaits,
    // and holding a `std::sync::Mutex` across an await is how a daemon deadlocks
    // on its own shutdown.
    let (placeholder, generation, point) = {
      let mut current = self.current.lock().expect("current generation");
      (
        current.session.take(),
        current.number,
        current.mountpoint.clone(),
      )
    };
    if let Some(session) = placeholder {
      unmount_session(session, point, generation).await;
    }
    let (client, monitor, mountpoint) = {
      let current = self.current.lock().expect("current generation");
      (
        Arc::clone(&current.client),
        Arc::clone(&current.monitor),
        current.mountpoint.clone(),
      )
    };
    if let Err(e) = client.release_mount(monitor.mount_id()).await {
      tracing::warn!(error = %e.message, "releasing the lease failed during shutdown");
    }
    let _ = std::fs::remove_dir(&mountpoint);
    let _ = std::fs::remove_file(MountState::path(&self.config.state_dir));
    let _ = std::fs::remove_file(self.control_socket());
  }

  /// Serve the control socket until `Unmount` is received or the future is
  /// dropped.
  pub async fn serve_control(self: Arc<Self>) -> Result<(), XvfsError> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let path = self.control_socket();
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path)
      .map_err(|e| XvfsError::internal(format!("binding the control socket: {e}")))?;
    // 0600 (ADR 0006). Set after bind, which is the only order available; the
    // window is one syscall wide and inside a 0700 directory.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
      .map_err(|e| XvfsError::internal(format!("restricting the control socket: {e}")))?;

    loop {
      let Ok((stream, _)) = listener.accept().await else {
        continue;
      };
      let daemon = Arc::clone(&self);
      let (reader, mut writer) = tokio::io::split(stream);
      let mut lines = BufReader::new(reader).lines();
      let Ok(Some(line)) = lines.next_line().await else {
        continue;
      };
      let response = match serde_json::from_str::<Request>(&line) {
        Ok(request) => daemon.handle(request).await,
        Err(e) => Response::from_error(&XvfsError::invalid(format!(
          "unrecognized control request: {e}"
        ))),
      };
      let stop = matches!(response, Response::Unmounted);
      if let Ok(mut encoded) = serde_json::to_string(&response) {
        encoded.push('\n');
        let _ = writer.write_all(encoded.as_bytes()).await;
        let _ = writer.flush().await;
      }
      if stop {
        return Ok(());
      }
    }
  }

  async fn handle(self: &Arc<Self>, request: Request) -> Response {
    match request {
      Request::Inspect => Response::Inspect(Box::new(self.inspect())),
      Request::Health => Response::Health(self.health()),
      Request::Refresh => match self.refresh().await {
        Ok(report) => Response::Refresh(report),
        Err(e) => Response::from_error(&e),
      },
      Request::Status => match self.status().await {
        Ok(report) => Response::Status(Box::new(report)),
        Err(e) => Response::from_error(&e),
      },
      Request::Diff => match self.diff().await {
        Ok(patch) => Response::Diff {
          patch_b64url: xvfs_types::path::b64url_encode(&patch),
        },
        Err(e) => Response::from_error(&e),
      },
      Request::Export { bundle } => match self.export(&bundle).await {
        Ok(report) => Response::Export(report),
        Err(e) => Response::from_error(&e),
      },
      Request::Search(request) => match self.search(&request).await {
        Ok(report) => Response::Search(Box::new(report)),
        Err(e) => Response::from_error(&e),
      },
      Request::Unmount => {
        self.shutdown().await;
        Response::Unmounted
      }
    }
  }
}

/// Where one generation's overlay lives.
fn overlay_dir(state_dir: &Path, generation: u64) -> PathBuf {
  state_dir.join("overlay").join(generation.to_string())
}

/// Make a path absolute without requiring it to exist.
///
/// `canonicalize` would be stronger but demands that every component already
/// exists, which the workspace path deliberately does not: it is about to be
/// created as a symlink.
fn absolute(path: &Path) -> Result<PathBuf, XvfsError> {
  std::path::absolute(path)
    .map_err(|e| XvfsError::invalid(format!("{} cannot be made absolute: {e}", path.display())))
}

/// Create a lease and mount it, in that order.
async fn mount_generation(
  config: &DaemonConfig,
  cache: &Arc<BlobCache>,
  number: u64,
) -> Result<Generation, XvfsError> {
  let grant = create_mount(config).await?;

  let commit = ObjectId::parse_qualified(&grant.commit_oid)
    .map_err(|e| XvfsError::internal(format!("server returned an unparseable commit: {e}")))?;
  let tree = ObjectId::parse_qualified(&grant.tree_oid)
    .map_err(|e| XvfsError::internal(format!("server returned an unparseable tree: {e}")))?;
  let mount_id = MountId::parse(&grant.mount_id)?;
  let snapshot_time = grant
    .snapshot_time
    .map(|t| Timestamp::new(t.secs, t.nanos))
    .ok_or_else(|| XvfsError::internal("server returned no snapshot time"))?;
  let expiry = grant
    .lease_expiry
    .map(|t| Timestamp::new(t.secs, t.nanos))
    .ok_or_else(|| XvfsError::internal("server returned no lease expiry"))?;
  let interval = if grant.heartbeat_interval_seconds == 0 {
    config.lease_policy.heartbeat_interval
  } else {
    Duration::from_secs(grant.heartbeat_interval_seconds)
  };

  let client = SnapshotClient::connect(
    &config.grpc_endpoint,
    &config.http_endpoint,
    &config.token,
    MountBinding {
      repository_id: config.repository_id.clone(),
      commit: commit.clone(),
      algorithm: HashAlgorithm::Sha1,
      snapshot_time,
    },
    grant.mount_capability,
  )
  .await?;

  // One call, at mount time, so the `git` shim's `log -1` needs neither a
  // network round trip nor a credential. A failure here degrades `log -1` and
  // nothing else, so it must not fail the mount.
  let commit_meta = match client.get_commit().await {
    Ok(meta) => Some(meta),
    Err(e) => {
      tracing::warn!(error = %e.message, "commit metadata unavailable; `git log -1` will be unsupported");
      None
    }
  };

  let gitdir = Arc::new(GitDir::new(&GitDirFacts {
    repository_id: config.repository_id.clone(),
    commit: commit.clone(),
    tree: tree.clone(),
    ref_name: grant.ref_name.clone(),
    mount_id: mount_id.clone(),
    control_socket: MountState::control_socket(&config.state_dir),
    snapshot_time,
    grpc_endpoint: config.grpc_endpoint.clone(),
    http_endpoint: config.http_endpoint.clone(),
    generation: number,
    commit_meta,
  }));

  // One overlay per generation, in its own directory. The binding check inside
  // `Overlay::open` then does real work: a daemon restarted against a moved
  // branch cannot silently adopt the previous generation's edits.
  let overlay_dir = overlay_dir(&config.state_dir, number);
  let overlay = Arc::new(
    Overlay::open(
      &overlay_dir,
      &Binding {
        repository_id: config.repository_id.as_str().to_owned(),
        base_commit: commit.to_qualified(),
      },
      snapshot_time,
      config.overlay.clone(),
    )
    .map_err(crate::fs::overlay_as_service_error)?,
  );
  let recovery = overlay.recovery();
  if !recovery.is_clean() {
    tracing::warn!(
      orphan_files = recovery.orphan_files_removed,
      orphan_bytes = recovery.orphan_bytes_removed,
      temporaries = recovery.temporary_files_removed,
      missing = recovery.missing_content.len(),
      "recovered an overlay left behind by a previous process"
    );
  }
  if !recovery.missing_content.is_empty() {
    // The store's invariant says this cannot happen. Reported at `error` because
    // it means bytes a job was told were written are not on disk, and continuing
    // quietly would serve an empty file in their place.
    tracing::error!(
      ids = ?recovery.missing_content,
      "overlay content referenced by the journal is missing"
    );
  }

  let fs = Xvfs::new(
    Arc::clone(&client),
    Arc::clone(cache),
    gitdir,
    Arc::clone(&overlay),
    crate::fs::root_entry(tree.clone()),
    config.fs.clone(),
  );

  let mountpoint = MountState::generation_dir(&config.state_dir, number);
  std::fs::create_dir_all(&mountpoint)
    .map_err(|e| XvfsError::internal(format!("creating the mount point: {e}")))?;

  let session = crate::session::spawn_mount(
    XvfsFilesystem::new(Arc::clone(&fs), tokio::runtime::Handle::current()),
    &mountpoint,
    &config.mount,
  )?;

  Ok(Generation {
    number,
    mountpoint,
    overlay_dir,
    fs,
    overlay,
    client,
    monitor: LeaseMonitor::new(mount_id, expiry, interval, config.lease_policy),
    session: Some(session),
    commit,
    tree,
    ref_name: grant.ref_name,
    snapshot_time,
  })
}

async fn create_mount(config: &DaemonConfig) -> Result<v1::CreateMountResponse, XvfsError> {
  let channel = tonic::transport::Endpoint::from_shared(config.grpc_endpoint.clone())
    .map_err(|e| XvfsError::invalid(format!("invalid gRPC endpoint: {e}")))?
    .connect()
    .await
    .map_err(|e| {
      XvfsError::new(
        ErrorCode::Unavailable,
        format!("connecting to the XVFS server: {e}"),
      )
    })?;
  let mut client = v1::snapshot_service_client::SnapshotServiceClient::new(channel);
  let mut request = tonic::Request::new(v1::CreateMountRequest {
    repository_id: config.repository_id.as_str().to_owned(),
    revision_selector: config.revision_selector.clone(),
    requested_ttl_seconds: 0,
  });
  if !config.token.is_empty() {
    let value = format!("Bearer {}", config.token)
      .parse()
      .map_err(|_| XvfsError::invalid("token is not a valid header value"))?;
    request.metadata_mut().insert("authorization", value);
  }
  Ok(
    client
      .create_mount(request)
      .await
      .map_err(|s| xvfs_proto::convert::from_status(&s))?
      .into_inner(),
  )
}

/// Refuse to start on top of a live daemon; clean up after a dead one.
///
/// ADR 0003 measured that a mount point survives its daemon and returns
/// `ENOTCONN` until something calls `fusermount3 -u`, so orphan cleanup is an
/// explicit responsibility rather than something the kernel does. This is the
/// daemon's half of it: whatever the previous occupant of this state directory
/// left behind is removed before a new mount is created in it.
fn adopt_or_refuse_state_dir(state_dir: &Path) -> Result<(), XvfsError> {
  let socket = MountState::control_socket(state_dir);
  if socket.exists() {
    if crate::control::is_live(&socket) {
      return Err(XvfsError::new(
        ErrorCode::Conflict,
        format!(
          "a live XVFS daemon already owns {}; unmount it before mounting again",
          state_dir.display()
        ),
      ));
    }
    let _ = std::fs::remove_file(&socket);
  }

  // Unmount anything a killed daemon left behind. Every operation on such a
  // mount point returns ENOTCONN, so a new mount over it would be unreachable.
  let generations = state_dir.join(crate::state::GENERATIONS_DIR);
  if let Ok(entries) = std::fs::read_dir(&generations) {
    for entry in entries.flatten() {
      let path = entry.path();
      let _ = std::process::Command::new("fusermount3")
        .args(["-u", "-z"])
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
      let _ = std::fs::remove_dir(&path);
    }
  }
  let _ = std::fs::remove_file(MountState::path(state_dir));
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_stale_socket_is_removed_and_a_live_one_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    prepare_state_dir(tmp.path()).unwrap();
    let socket = MountState::control_socket(tmp.path());

    // A stale socket file with nothing listening: adopted.
    std::fs::write(&socket, b"").unwrap();
    adopt_or_refuse_state_dir(tmp.path()).unwrap();
    assert!(!socket.exists());

    // A real listener: refused, because two daemons over one state directory
    // would each believe they own the lease.
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let e = adopt_or_refuse_state_dir(tmp.path()).unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict);
  }
}
