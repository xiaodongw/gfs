//! One mounted workspace: its lease, its pinned commit, and its control socket.
//!
//! A [`Mount`] is everything that belongs to one workspace and nothing that
//! belongs to the process. [`crate::host`] owns however many of these a
//! `gfs-fuse` process is serving, which is why none of the state here is global:
//! every lock and cache handle is per-instance, so a mount that fails does so
//! alone.
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
//! # Re-pinning, and why it happens in place
//!
//! `gfs switch`, `gfs refresh`, and the re-pin after `gfs commit` all do the same
//! thing: resolve a selector, take out a lease on what it resolved to, and point
//! this filesystem at it. The mount does not move, the workspace path does not
//! change, and no second FUSE session is created.
//!
//! M2 did the opposite — a whole new generation at a new mount point, published
//! by swapping a symlink — for a guarantee that no reader ever observes a mixture
//! of two commits. ADR 0003's second amendment withdraws it. The guarantee was
//! strictly stronger than the one Git gives (`git switch` rewrites a working tree
//! in place and offers open descriptors nothing at all), and it was paid for with
//! the property this project exists to provide: a workspace that behaves like a
//! Git checkout. A symlinked workspace means `getcwd(2)` reports the generation
//! directory, so every tool that resolves its own working directory — which is
//! every tool that is not a shell — ends up pinned to a generation that the next
//! switch retires underneath it.
//!
//! What survives from the old model is the part that was load-bearing:
//!
//! * an **open descriptor keeps reading what it opened**, because a `FileState`
//!   holds a materialized cache file or overlay content file and never re-reads
//!   through the client. This costs nothing and needs no lease, which is why the
//!   old generation's lease can be released as soon as the swap is done.
//! * the kernel is **told** what changed, rather than left to time out. Every
//!   dentry the mount handed out is invalidated after the swap, so the next
//!   access re-resolves against the new commit.
//!
//! What is genuinely given up: a job whose working directory exists on the old
//! commit and not on the new one gets `ENOENT` afterwards, and for up to
//! `negative_ttl` a path that the new commit adds may still read as absent. Both
//! are what `git switch` does.
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

use gfs_overlay::{Binding, Overlay, OverlayConfig};
use gfs_proto::v1;
use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::{
  HashAlgorithm, LeasePolicy, LeaseState, MountId, ObjectId, RepositoryId, Timestamp,
};

use crate::cache::BlobCache;
use crate::client::{MountBinding, SnapshotClient};
use crate::control::{MountReport, RefreshReport, Request, Response};
use crate::fs::{FsConfig, Gfs, GfsFilesystem};
use crate::gitdir::GitDirFacts;
use crate::lease::{LeaseHealth, LeaseMonitor};
use crate::publish::{DirectMountPublisher, MountPublisher};
use crate::session::MountConfig;
use crate::state::{prepare_state_dir, LeaseRecord, MountState};

#[derive(Clone, Debug)]
pub struct MountSpec {
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
}

/// One path's contribution to a commit.
#[derive(Clone, Debug)]
pub enum CommitChange {
  Upsert {
    path: gfs_types::BytePath,
    mode: u32,
    content: Vec<u8>,
  },
  Delete {
    path: gfs_types::BytePath,
  },
}

/// Everything a commit needs from the workspace.
#[derive(Clone, Debug)]
pub struct PendingCommit {
  /// What the workspace diverged from. The server refuses if the branch has
  /// moved past this, rather than committing over someone else's work.
  pub base_commit: ObjectId,
  pub changes: Vec<CommitChange>,
  /// Directories the overlay deleted without per-file rows. Expanded by the
  /// server, which has the base tree; the workspace does not.
  pub deleted_directories: Vec<gfs_types::BytePath>,
}

/// Read one changed path's bytes out of the overlay.
///
/// A symlink's target is its content, and it is held in the entry rather than in
/// the content store -- reading it as a file would fail on the missing local id.
fn read_overlay_content(
  overlay: &Overlay,
  entry: &gfs_overlay::OverlayEntry,
) -> Result<Vec<u8>, GfsError> {
  if let Some(target) = &entry.symlink_target {
    return Ok(target.clone());
  }
  use std::io::Read;
  let mut file = overlay
    .open_content(entry)
    .map_err(crate::fs::overlay_as_service_error)?;
  let mut bytes = Vec::with_capacity(entry.size as usize);
  file
    .read_to_end(&mut bytes)
    .map_err(|e| GfsError::internal(format!("reading workspace content: {e}")))?;
  Ok(bytes)
}

/// What the mount is currently looking at.
///
/// The filesystem, the FUSE session, and the mount point are deliberately *not*
/// in here: they outlive every pin, which is the whole point of re-pinning in
/// place. This is only what a `gfs switch` replaces.
struct Pin {
  /// Counts re-pins. Not a generation — nothing is kept alive alongside it — but
  /// the `.git` surface reports it and an operator watching `gfs inspect` can
  /// tell one switch from the next.
  epoch: u64,
  overlay_dir: PathBuf,
  overlay: Arc<Overlay>,
  client: Arc<SnapshotClient>,
  monitor: Arc<LeaseMonitor>,
  commit: ObjectId,
  tree: ObjectId,
  ref_name: Option<String>,
  snapshot_time: Timestamp,
}

impl std::fmt::Debug for Pin {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Pin")
      .field("epoch", &self.epoch)
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
async fn unmount_session(session: fuser::BackgroundSession, mountpoint: PathBuf) {
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
    tracing::error!("the FUSE session thread did not exit even after a lazy unmount");
  }
}

impl Pin {
  /// Release the lease this pin held, and drop its overlay directory.
  ///
  /// Called *after* the filesystem has been re-pointed at the replacement, so
  /// nothing is reading through it any more. There is no unmount here and no
  /// waiting for handles to drain: a descriptor opened before the swap reads a
  /// materialized file, not the pinned commit, so the lease it was created under
  /// is already unreferenced.
  async fn release(self) {
    if let Err(e) = self.client.release_mount(self.monitor.mount_id()).await {
      // Not fatal: the lease expires on its own. Logged because a release that
      // keeps failing means orphan leases are accumulating somewhere.
      tracing::warn!(epoch = self.epoch, error = %e.message, "releasing the lease failed");
    }
    // Removed unconditionally. The guard used to be `is_empty()`, on the stated
    // grounds that a superseded overlay is always empty -- true for `switch` and
    // `refresh`, which refuse a dirty workspace, and false for `commit`, which
    // does not empty the overlay but gives the new pin a *fresh* one. So the one
    // path that reliably leaves rows behind was the one path that never cleaned
    // up, and a job that committed repeatedly accumulated one SQLite database
    // per commit.
    //
    // Nothing is discarded by removing it. This runs only after the re-pin has
    // succeeded, so the filesystem is already reading the new overlay and no
    // caller can reach this one again; for `commit` its contents are durable in
    // the commit that was just made. A descriptor still open on overlay content
    // keeps working, because it holds the file rather than the name -- the same
    // property `FileState::Local` relies on when the overlay unlinks content
    // under a live reader.
    let _ = std::fs::remove_dir_all(&self.overlay_dir);
  }
}

pub struct Mount {
  config: MountSpec,
  /// The selector the next re-pin resolves.
  ///
  /// Held here rather than in `config` because `switch` changes it: a view is
  /// re-pointed at another branch by resolving a *different* selector, which is
  /// the same machinery `refresh` uses. The config's value is the starting one.
  selector: Mutex<String>,
  /// The work branch this view is on, when it is on one.
  ///
  /// Separate from `selector` because the selector is a resolved commit -- the
  /// reserved namespace cannot be named as a revision -- so the branch name
  /// would otherwise be lost the moment it was resolved, and `gfs commit` needs
  /// to know which ref to advance.
  work_branch: Mutex<Option<String>>,
  cache: Arc<BlobCache>,
  /// The per-repository object-database projection this workspace borrows.
  /// Held so the projection outlives every workspace that references it
  /// (ADR 0009).
  odb: Arc<crate::odb::OdbProjection>,
  /// This workspace's own window onto the shared store. `objects/info/
  /// alternates` names the view's mountpoint rather than the shared one, so
  /// odb traffic is attributed to this job by construction.
  odb_view: Arc<crate::odb::OdbView>,
  publisher: Box<dyn MountPublisher>,
  /// The filesystem, created once and re-pointed by every switch. Outlives every
  /// [`Pin`], which is what makes the workspace path stable.
  fs: Arc<Gfs>,
  /// The one FUSE session. `None` only after shutdown has taken it.
  session: Mutex<Option<fuser::BackgroundSession>>,
  current: Mutex<Pin>,
  /// Serializes re-pins against each other.
  ///
  /// A `tokio` lock rather than a `std` one because the critical section awaits:
  /// two concurrent switches that interleaved their `CreateMount` and their swap
  /// would leave the filesystem on one commit and `mount.json` naming another.
  repinning: tokio::sync::Mutex<()>,
  shutting_down: AtomicBool,
}

impl std::fmt::Debug for Mount {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Mount")
      .field("workspace", &self.config.workspace)
      .finish_non_exhaustive()
  }
}

impl Mount {
  /// Create the lease, mount, and publish. Returns once the workspace is usable.
  ///
  /// The blob cache is passed in rather than opened here because it is the one
  /// thing a mount does *not* own alone: two mounts of the same repository on one
  /// host share it, and a cache opened per mount would make `--cache-quota` mean
  /// "per mount" rather than "per host". [`crate::host::MountHost`] keeps the
  /// registry that decides which mounts get the same handle.
  pub async fn start(
    config: MountSpec,
    cache: Arc<BlobCache>,
    odb: Arc<crate::odb::OdbProjection>,
  ) -> Result<Arc<Self>, GfsError> {
    // Absolute before anything else, so the daemon keeps working if something
    // later changes its working directory, and so `mount.json` names a path an
    // orchestrator cleaning up after a crash can act on without knowing where
    // the daemon was started.
    let config = MountSpec {
      state_dir: absolute(&config.state_dir)?,
      workspace: absolute(&config.workspace)?,
      cache_dir: absolute(&config.cache_dir)?,
      ..config
    };
    prepare_state_dir(&config.state_dir)?;
    adopt_or_refuse_state_dir(&config.state_dir, &config.workspace)?;

    let publisher: Box<dyn MountPublisher> =
      Box::new(DirectMountPublisher::new(config.workspace.clone())?);

    let resolved = resolve_pin(&config, &config.revision_selector.clone(), 1).await?;
    // The real `.git` (ADR 0009): seeded on local disk, named by a synthesized
    // `.git` *file* in the mount. The index the gateway built is fetched once;
    // a failure there degrades to no index -- `git status` would report every
    // file deleted, which is wrong -- so it fails the mount instead.
    let git_dir_path = config.state_dir.join("git");
    let index = odb.client.commit_index(&resolved.pin.commit).await?;
    // The workspace's own view of the shared store (per-job odb attribution).
    // In the state directory, so its lifetime and cleanup follow the mount's.
    let odb_view =
      crate::odb::OdbView::mount(Arc::clone(&odb.store), &config.state_dir.join("odb"))?;
    crate::gitdir::seed_git_dir(&crate::gitdir::SeedSpec {
      git_dir: &git_dir_path,
      workspace: &config.workspace,
      odb_mountpoint: &odb_view.mountpoint,
      facts: &resolved.facts,
      index: Some(&index),
    })?;
    let fs = Gfs::new(
      Arc::clone(&resolved.pin.client),
      Arc::clone(&cache),
      Arc::new(crate::gitdir::git_file(&git_dir_path)),
      Arc::clone(&resolved.pin.overlay),
      resolved.root,
      config.fs.clone(),
    );

    let mountpoint = publisher.mountpoint().to_path_buf();
    std::fs::create_dir_all(&mountpoint)
      .map_err(|e| GfsError::internal(format!("creating the mount point: {e}")))?;
    let session = crate::session::spawn_mount(
      GfsFilesystem::new(Arc::clone(&fs), tokio::runtime::Handle::current()),
      &mountpoint,
      &config.mount,
    )?;
    publisher.publish()?;

    let mount = Arc::new(Mount {
      cache,
      odb,
      odb_view,
      publisher,
      fs,
      session: Mutex::new(Some(session)),
      current: Mutex::new(resolved.pin),
      repinning: tokio::sync::Mutex::new(()),
      shutting_down: AtomicBool::new(false),
      selector: Mutex::new(config.revision_selector.clone()),
      work_branch: Mutex::new(None),
      config,
    });
    mount.persist()?;
    Ok(mount)
  }

  pub fn config(&self) -> &MountSpec {
    &self.config
  }

  /// A one-line view of this mount, for the host's `ListMounts`.
  pub fn summary(&self) -> crate::control::MountSummary {
    let current = self.current.lock().expect("current pin");
    crate::control::MountSummary {
      workspace: self.config.workspace.clone(),
      state_dir: self.config.state_dir.clone(),
      repository_id: self.config.repository_id.as_str().to_owned(),
      mount_id: current.monitor.mount_id().as_str().to_owned(),
      revision_selector: self.selector.lock().expect("selector").clone(),
      commit: current.commit.to_qualified(),
      generation: current.epoch,
      health: current.monitor.health().state,
    }
  }

  pub fn control_socket(&self) -> PathBuf {
    MountState::control_socket(&self.config.state_dir)
  }

  fn persist(&self) -> Result<(), GfsError> {
    let current = self.current.lock().expect("current pin");
    let health = current.monitor.health();
    MountState {
      state_format_version: gfs_types::STATE_FORMAT_VERSION,
      api_version: gfs_types::API_VERSION.to_owned(),
      mount_id: current.monitor.mount_id().as_str().to_owned(),
      repository_id: self.config.repository_id.as_str().to_owned(),
      revision_selector: self.selector.lock().expect("selector").clone(),
      commit: current.commit.to_qualified(),
      tree: current.tree.to_qualified(),
      ref_name: current.ref_name.clone(),
      snapshot_time: current.snapshot_time,
      grpc_endpoint: self.config.grpc_endpoint.clone(),
      http_endpoint: self.config.http_endpoint.clone(),
      workspace: self.config.workspace.clone(),
      generation: current.epoch,
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
    let current = self.current.lock().expect("current pin");
    MountReport {
      mount_id: current.monitor.mount_id().as_str().to_owned(),
      repository_id: self.config.repository_id.as_str().to_owned(),
      revision_selector: self.selector.lock().expect("selector").clone(),
      work_branch: self.work_branch.lock().expect("work branch").clone(),
      commit: current.commit.to_qualified(),
      tree: current.tree.to_qualified(),
      ref_name: current.ref_name.clone(),
      snapshot_time: current.snapshot_time,
      workspace: self.config.workspace.display().to_string(),
      publication: self.publisher.describe(),
      generation: current.epoch,
      state_dir: self.config.state_dir.display().to_string(),
      daemon_pid: std::process::id(),
      owner_uid: crate::attr::Ownership::current().uid,
      read_only: false,
      overlay: current.overlay.stats(),
      health: current.monitor.health(),
      stats: self.fs.stats(),
      cache: self.cache.stats(),
      budget: self.fs.budget_report(),
      odb: self.odb.store.stats(),
      odb_job: self.odb_view.stats(),
      live_inodes: self.fs.inode_counts().0,
      assigned_inodes: self.fs.inode_counts().1,
    }
  }

  /// Renew the pinned commit's lease.
  ///
  /// One lease, because there is one pin. The generation model had to renew
  /// superseded generations too — they still had descriptors reading through
  /// their pinned commits — but a descriptor now reads a materialized file and
  /// holds nothing on the server, so a superseded lease is released outright
  /// rather than kept warm.
  pub async fn renew_all(&self) {
    let entries: Vec<(Arc<SnapshotClient>, Arc<LeaseMonitor>)> = {
      let current = self.current.lock().expect("current pin");
      vec![(Arc::clone(&current.client), Arc::clone(&current.monitor))]
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

  /// Ask the server to start building the pinned commit's search index.
  ///
  /// Spawned rather than awaited at mount time: a mount is 0.06 s and indexing a
  /// large repository is not, so blocking one on the other would trade the
  /// property the project exists for against a query the job may never run. The
  /// search path prepares on demand as well, so this is a warm-up and not the
  /// mechanism — losing it costs the first search some latency and nothing else.
  ///
  /// Failure is logged and dropped for the same reason: a repository whose index
  /// cannot be built still serves every read, and refusing to mount over it
  /// would turn a degraded search into a job that cannot start.
  pub fn spawn_index_warmup(self: &Arc<Self>) {
    let client = {
      let current = self.current.lock().expect("current pin");
      Arc::clone(&current.client)
    };
    tokio::spawn(async move {
      match client.prepare_snapshot().await {
        Ok(true) => tracing::info!("the search index for the pinned commit is ready"),
        Ok(false) => tracing::info!("the search index for the pinned commit is building"),
        Err(e) => tracing::warn!(error = %e.message, "could not prepare the search index"),
      }
    });
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
  pub async fn refresh(self: &Arc<Self>) -> Result<RefreshReport, GfsError> {
    // PLAN.md M2.1: refuse when the overlay is non-empty; three-way refresh is
    // out of scope. A re-pin is a new pinned commit, and an overlay is bound to
    // the commit it diverged from -- carrying edits across would make every
    // `status` answer be about a base the workspace is no longer on. `git pull`
    // refuses a dirty tree for the same reason.
    if !self.workspace_is_clean().await? {
      return Err(GfsError::new(
        ErrorCode::FailedPrecondition,
        "the workspace has local changes; export them or discard them before refreshing \
         (three-way refresh is out of scope)",
      ));
    }
    self.repin().await
  }

  /// Resolve the current selector and point the filesystem at what it resolved
  /// to -- without asking whether the overlay is clean.
  ///
  /// Split out because the three callers disagree about that precondition and
  /// agree about everything else. `refresh` and `switch` require a clean
  /// workspace; `commit` requires a *dirty* one and is the reason the check
  /// cannot live in here.
  ///
  /// The new pin gets its own overlay directory, so a committed overlay is never
  /// reset in place -- it is left behind with the pin it belonged to and removed
  /// once nothing points at it. That is what makes commit-then-re-pin safe
  /// without a two-phase protocol: there is no window in which one overlay is
  /// half-cleared.
  ///
  /// # Order, and the window it leaves
  ///
  /// `CreateMount` runs before anything is swapped, so a failure to resolve the
  /// new selector leaves the mount exactly as it was — a `gfs switch` at a
  /// typo'd branch name changes nothing. Then, in order: swap the filesystem's
  /// pin, invalidate the kernel's dentries, persist, release the old lease.
  ///
  /// Between the swap and the end of invalidation the kernel may still answer
  /// from a dentry it cached under the old commit. This is a real window and it
  /// is the one Git also has — `git switch` writes a working tree file by file —
  /// so it is bounded and reported rather than designed away. Invalidation is
  /// bounded by the paths the job actually touched, not by the size of the tree.
  async fn repin(self: &Arc<Self>) -> Result<RefreshReport, GfsError> {
    // Held across the whole operation, so two switches cannot interleave and
    // leave the filesystem on one commit with `mount.json` naming another.
    let _serialized = self.repinning.lock().await;

    let (previous_epoch, previous_commit) = {
      let current = self.current.lock().expect("current pin");
      (current.epoch, current.commit.clone())
    };
    let next_epoch = previous_epoch + 1;
    let selector = self.selector.lock().expect("selector").clone();

    // Local commits are the agent's work and a re-seed would overwrite the
    // branch ref they hang from. `git push` (or an export) is how they leave;
    // until then the workspace stays where it is. The overlay-dirty check made
    // by the callers does not see these, because a commit empties nothing in
    // `.git`.
    let git_dir_path = self.config.state_dir.join("git");
    if let Some(local) = crate::gitdir::local_head(&git_dir_path) {
      if local != previous_commit.to_hex() {
        return Err(GfsError::new(
          ErrorCode::FailedPrecondition,
          format!(
            "the workspace has local commits ({} is ahead of the pinned {}); \
             push them before switching",
            local.get(..12).unwrap_or(&local),
            previous_commit.to_hex()
          ),
        ));
      }
    }

    let resolved = resolve_pin(&self.config, &selector, next_epoch).await?;
    let commit = resolved.pin.commit.clone();

    // Re-seed HEAD, the branch ref, and the index to the new pin. The index
    // fetch is the only network dependency; failing the switch here leaves the
    // old pin fully intact, which is the same guarantee resolve_pin's ordering
    // already gives.
    let index = self.odb.client.commit_index(&commit).await?;
    crate::gitdir::seed_git_dir(&crate::gitdir::SeedSpec {
      git_dir: &git_dir_path,
      workspace: &self.config.workspace,
      odb_mountpoint: &self.odb_view.mountpoint,
      facts: &resolved.facts,
      index: Some(&index),
    })?;

    let stale = self.fs.repin(
      crate::fs::Pinned {
        client: Arc::clone(&resolved.pin.client),
        gitdir: Arc::new(crate::gitdir::git_file(&git_dir_path)),
        overlay: Arc::clone(&resolved.pin.overlay),
        snapshot_time: resolved.pin.snapshot_time,
      },
      resolved.root,
    );
    let superseded = {
      let mut current = self.current.lock().expect("current pin");
      std::mem::replace(&mut *current, resolved.pin)
    };

    self.invalidate(stale).await;
    self.persist()?;
    superseded.release().await;

    Ok(RefreshReport {
      previous_generation: previous_epoch,
      generation: next_epoch,
      previous_commit: previous_commit.to_qualified(),
      commit: commit.to_qualified(),
      unchanged: previous_commit == commit,
    })
  }

  /// Tell the kernel to forget every name the old commit answered for.
  ///
  /// Runs on a blocking worker and never on a FUSE session thread.
  /// `FUSE_NOTIFY_INVAL_ENTRY` blocks until the kernel has finished with the
  /// dentry, and the kernel may need this filesystem to answer a request first —
  /// so issuing it from inside a callback is how a mount deadlocks against
  /// itself.
  ///
  /// Individual failures are dropped rather than propagated. `ENOENT` is the
  /// common one and means the kernel had already forgotten the entry, which is
  /// the outcome being asked for; the rest would each, at worst, leave one name
  /// to expire on its TTL, and none is a reason to fail a switch that has
  /// already happened.
  async fn invalidate(&self, entries: Vec<(u64, Vec<u8>)>) {
    if entries.is_empty() {
      return;
    }
    let notifier = {
      let session = self.session.lock().expect("fuse session");
      session.as_ref().map(|s| s.notifier())
    };
    let Some(notifier) = notifier else {
      return;
    };
    let count = entries.len();
    let _ = tokio::task::spawn_blocking(move || {
      use std::os::unix::ffi::OsStrExt;
      for (parent, name) in entries {
        let _ = notifier.inval_entry(fuser::INodeNo(parent), std::ffi::OsStr::from_bytes(&name));
      }
    })
    .await;
    tracing::debug!(entries = count, "invalidated cached names after a re-pin");
  }

  /// Re-point this view at another revision, and remember it.
  ///
  /// `refresh` re-resolves the *same* selector; this changes which one is
  /// resolved and is what `gfs switch` and the re-pin after `gfs commit` both
  /// use. The machinery underneath is identical, because a view changing branch
  /// and a view following a moved branch are the same operation with a different
  /// selector.
  ///
  /// Refuses a dirty workspace for the reason `refresh` does, and for the reason
  /// `git switch` does: an overlay is bound to the commit it diverged from, and
  /// carrying edits to a different base would make every later `status` describe
  /// a base the workspace is not on. `gfs commit` does not hit this, because
  /// committing empties the overlay before re-pinning.
  pub async fn switch_to(
    self: &Arc<Self>,
    selector: &str,
    branch: Option<String>,
  ) -> Result<RefreshReport, GfsError> {
    if !self.workspace_is_clean().await? {
      return Err(GfsError::new(
        ErrorCode::FailedPrecondition,
        "the workspace has local changes; commit or discard them before switching",
      ));
    }
    {
      let mut current = self.selector.lock().expect("selector");
      *current = selector.to_owned();
    }
    {
      let mut current = self.work_branch.lock().expect("work branch");
      *current = branch;
    }
    self.repin().await
  }

  /// The work branch this view is on, if any.
  pub fn work_branch(&self) -> Option<String> {
    self.work_branch.lock().expect("work branch").clone()
  }

  /// Everything the workspace changed, with the bytes, ready to commit.
  ///
  /// The overlay reports six kinds of change and a tree understands two, so this
  /// is where the mapping is decided:
  ///
  /// * `Added`, `Modified`, `TypeChanged` and `ModeChanged` all become an
  ///   upsert. A type change is an upsert because writing a path with a new mode
  ///   and new content *is* the change; a mode change is an upsert whose content
  ///   happens to be unchanged, and re-sending those bytes is what keeps this a
  ///   single uniform operation rather than a special case that has to look up
  ///   the old blob.
  /// * `Deleted` becomes a delete.
  /// * `Renamed` becomes **both**: a delete of the old path and an upsert of the
  ///   new one. The overlay keeps `renamed_from` so export can emit a rename
  ///   record, but a Git tree has no rename -- it is a content-addressed map, and
  ///   the pair is exactly what `git` itself stores.
  ///
  /// Directory deletions are *not* expanded here. The overlay records them
  /// without a per-file row, and expanding one needs the base tree, which the
  /// workspace does not have -- so they travel separately and the server, which
  /// does have it, expands them.
  pub async fn changes_for_commit(&self) -> Result<PendingCommit, GfsError> {
    let (overlay, commit) = {
      let current = self.current.lock().expect("current pin");
      (Arc::clone(&current.overlay), current.commit.clone())
    };
    let algorithm = commit.algorithm();

    tokio::task::spawn_blocking(move || {
      let status = overlay
        .status(algorithm)
        .map_err(crate::fs::overlay_as_service_error)?;
      let mut changes = Vec::with_capacity(status.changes.len());

      for change in &status.changes {
        use gfs_overlay::status::ChangeKind as K;
        match change.kind {
          K::Deleted => changes.push(CommitChange::Delete {
            path: change.path.clone(),
          }),
          K::Added | K::Modified | K::TypeChanged | K::ModeChanged | K::Renamed => {
            if change.kind == K::Renamed {
              if let Some(from) = &change.from {
                changes.push(CommitChange::Delete { path: from.clone() });
              }
            }
            let entry = overlay.get(&change.path).ok_or_else(|| {
              // The journal said this path changed and the state has no row for
              // it. That is an inconsistency, not an empty file: committing a
              // zero-byte blob here would silently destroy the content.
              GfsError::internal(format!(
                "the overlay reports {} as changed but holds no entry for it",
                change.path.escaped()
              ))
            })?;
            let mode = change.new_mode.ok_or_else(|| {
              GfsError::internal(format!(
                "{} changed with no new mode",
                change.path.escaped()
              ))
            })?;
            let content = read_overlay_content(&overlay, &entry)?;
            changes.push(CommitChange::Upsert {
              path: change.path.clone(),
              mode,
              content,
            });
          }
        }
      }

      Ok(PendingCommit {
        base_commit: commit,
        changes,
        deleted_directories: status.directory_deletions.clone(),
      })
    })
    .await
    .map_err(|e| GfsError::internal(format!("collecting changes failed: {e}")))?
  }

  /// The commit this view is pinned to.
  pub fn pinned_commit(&self) -> ObjectId {
    self
      .current
      .lock()
      .expect("current generation")
      .commit
      .clone()
  }

  /// The commit the CLI should ask the gateway to make.
  ///
  /// The daemon collects and the CLI sends, rather than the daemon talking to
  /// the gateway itself: the control socket carries no credential, and a commit
  /// has to be attributed to the caller rather than to whatever token the daemon
  /// was started with.
  pub async fn commit_plan(&self) -> Result<crate::control::CommitPlan, GfsError> {
    use gfs_types::path::b64url_encode;
    let pending = self.changes_for_commit().await?;
    if pending.changes.is_empty() && pending.deleted_directories.is_empty() {
      return Err(GfsError::new(
        ErrorCode::FailedPrecondition,
        "nothing to commit; the workspace is clean",
      ));
    }
    Ok(crate::control::CommitPlan {
      base_commit: pending.base_commit.to_qualified(),
      work_branch: self.work_branch(),
      changes: pending
        .changes
        .into_iter()
        .map(|c| match c {
          CommitChange::Upsert {
            path,
            mode,
            content,
          } => crate::control::PlannedChange::Upsert {
            path: b64url_encode(path.as_bytes()),
            mode,
            content_b64url: b64url_encode(&content),
          },
          CommitChange::Delete { path } => crate::control::PlannedChange::Delete {
            path: b64url_encode(path.as_bytes()),
          },
        })
        .collect(),
      deleted_directories: pending
        .deleted_directories
        .into_iter()
        .map(|p| b64url_encode(p.as_bytes()))
        .collect(),
    })
  }

  /// Re-pin to a commit that was just made, keeping the work branch.
  ///
  /// Deliberately does **not** check that the overlay is clean: it is not, and
  /// that is the point. The changes it holds are now in the commit being pinned,
  /// so the new pin's fresh overlay starts empty and correct while the old one
  /// is released with the pin it belonged to.
  pub async fn adopt_commit(self: &Arc<Self>, commit: &str) -> Result<RefreshReport, GfsError> {
    {
      let mut current = self.selector.lock().expect("selector");
      *current = commit.to_owned();
    }
    self.repin().await
  }

  /// The change set, from the journal alone.
  /// Answer the fsmonitor hook (ADR 0009).
  ///
  /// The contract with Git: return every path changed since the caller's
  /// token, or tell it to rescan. The overlay journal is cumulative for a
  /// generation, so the answer is *all* overlay-changed paths — a superset of
  /// changes since any token in the generation, and a superset is always safe
  /// because Git re-checks listed paths and trusts only the unlisted ones.
  ///
  /// The token is `gfs:<generation>`. A repin discards or re-bases the
  /// overlay, which changes what an unlisted path means, so a token from
  /// another generation forces the one full rescan that re-grounds Git.
  pub async fn fsmonitor_changes(
    &self,
    caller_token: &str,
  ) -> Result<crate::control::FsMonitorAnswer, GfsError> {
    let (overlay, commit, generation) = {
      let current = self.current.lock().expect("current pin");
      (
        Arc::clone(&current.overlay),
        current.commit.clone(),
        current.epoch,
      )
    };
    let token = format!("gfs:{generation}");
    let full_rescan = caller_token != token;
    let algorithm = commit.algorithm();
    let status = tokio::task::spawn_blocking(move || overlay.status(algorithm))
      .await
      .map_err(|e| GfsError::internal(format!("the fsmonitor task failed: {e}")))?
      .map_err(crate::fs::overlay_as_service_error)?;
    let mut paths = Vec::with_capacity(status.changes.len() * 2);
    for change in &status.changes {
      paths.push(String::from_utf8_lossy(change.path.as_bytes()).into_owned());
      // A rename changed both names: the new one exists, the old one is gone,
      // and Git must re-stat both.
      if let Some(from) = &change.from {
        paths.push(String::from_utf8_lossy(from.as_bytes()).into_owned());
      }
    }
    for dir in &status.directory_deletions {
      // Git's protocol: a trailing slash marks a directory whose contents all
      // changed.
      paths.push(format!("{}/", String::from_utf8_lossy(dir.as_bytes())));
    }
    Ok(crate::control::FsMonitorAnswer {
      token,
      paths,
      full_rescan,
    })
  }

  pub async fn status(&self) -> Result<crate::control::StatusReport, GfsError> {
    let (overlay, commit, ref_name) = {
      let current = self.current.lock().expect("current pin");
      (
        Arc::clone(&current.overlay),
        current.commit.clone(),
        current.ref_name.clone(),
      )
    };
    let algorithm = commit.algorithm();
    let status = tokio::task::spawn_blocking(move || overlay.status(algorithm))
      .await
      .map_err(|e| GfsError::internal(format!("the status task failed: {e}")))?
      .map_err(crate::fs::overlay_as_service_error)?;
    Ok(crate::control::StatusReport {
      base_commit: commit.to_qualified(),
      ref_name,
      work_branch: self.work_branch(),
      status,
    })
  }

  pub async fn diff(&self) -> Result<Vec<u8>, GfsError> {
    let (status, base, overlay, commit) = self.export_inputs().await?;
    let exporter = gfs_overlay::Exporter::new(
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
  ) -> Result<crate::search::SearchReport, GfsError> {
    let (client, overlay, commit, ref_name) = {
      let current = self.current.lock().expect("current pin");
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

  /// The pinned commit's ancestry.
  ///
  /// No overlay half. The overlay holds file changes, not commits — a workspace
  /// with local edits still has exactly the history its pin has — so there is
  /// nothing to merge and pretending otherwise would invent commits.
  pub async fn log(
    &self,
    from: Option<&str>,
    query: crate::client::LogQuery,
  ) -> Result<crate::control::LogReport, GfsError> {
    let (client, commit, ref_name) = {
      let current = self.current.lock().expect("current pin");
      (
        Arc::clone(&current.client),
        current.commit.clone(),
        current.ref_name.clone(),
      )
    };
    // A named start point is its own base, and it is not on the pinned ref.
    let (start, ref_name) = match from {
      Some(selector) => (self.resolve_here(&client, &commit, selector).await?, None),
      None => (commit, ref_name),
    };
    let (commits, has_more) = client.log(Some(&start), &query).await?;
    Ok(crate::control::LogReport {
      base_commit: start.to_qualified(),
      ref_name,
      commits: commits.into_iter().map(log_entry).collect(),
      has_more,
    })
  }

  /// Resolve a revision expression the way a caller standing in this workspace
  /// means it.
  ///
  /// `HEAD` is the **pin**, not the repository's default branch. After
  /// `gfs switch -c` those are different commits, and sending `HEAD` to the
  /// gateway would silently answer about a branch the caller is not on. The pin
  /// is substituted here and any `~n`/`^n` suffix travels with it, so the server
  /// walks the ancestry of the right commit.
  async fn resolve_here(
    &self,
    client: &crate::client::SnapshotClient,
    pin: &ObjectId,
    selector: &str,
  ) -> Result<ObjectId, GfsError> {
    let suffix_at = selector.find(['~', '^']).unwrap_or(selector.len());
    let (base, suffix) = selector.split_at(suffix_at);
    if base != "HEAD" {
      return client.resolve(selector).await;
    }
    if suffix.is_empty() {
      return Ok(pin.clone());
    }
    client
      .resolve(&format!("{}{suffix}", pin.to_qualified()))
      .await
  }

  /// What changed between two revisions, answered by the gateway.
  pub async fn diff_revs(
    &self,
    request: &crate::control::Request,
  ) -> Result<crate::control::RevDiffReport, GfsError> {
    let crate::control::Request::DiffRevs {
      from,
      to,
      parent,
      format,
      context_lines,
      paths_b64url,
    } = request
    else {
      return Err(GfsError::internal("not a diff request"));
    };

    let (client, pin) = {
      let current = self.current.lock().expect("current pin");
      (Arc::clone(&current.client), current.commit.clone())
    };
    let to_commit = self.resolve_here(&client, &pin, to).await?;

    // An absent `from` means "the parent this commit is being reviewed against",
    // which is what `git show` diffs. Resolved here rather than on the server
    // because it is a *presentation* choice -- which parent of a merge -- and the
    // server should be told the two commits, not asked to guess.
    let base = match from {
      Some(selector) => Some(self.resolve_here(&client, &pin, selector).await?),
      None => {
        let meta = client.get_commit_at(&to_commit).await?;
        let index = parent.unwrap_or(1).max(1) as usize - 1;
        match meta.parents.get(index) {
          Some(p) => Some(p.clone()),
          // A root commit has no parent and is diffed against the empty tree,
          // which is what `git show` does for the first commit in a history.
          None if index == 0 => None,
          None => {
            return Err(GfsError::invalid(format!(
              "that commit has {} parent(s), so there is no parent {}",
              meta.parents.len(),
              index + 1
            )))
          }
        }
      }
    };

    let mut paths = Vec::with_capacity(paths_b64url.len());
    for encoded in paths_b64url {
      paths.push(gfs_types::BytePath::new(gfs_types::path::b64url_decode(
        encoded,
      )?));
    }
    let diff = client
      .diff_commits(
        base.as_ref(),
        &to_commit,
        &crate::client::DiffQuery {
          paths,
          format: *format,
          context_lines: *context_lines,
          max_bytes: 0,
        },
      )
      .await?;

    Ok(crate::control::RevDiffReport {
      base_commit: base.map(|b| b.to_qualified()),
      commit: to_commit.to_qualified(),
      rendered_b64url: gfs_types::path::b64url_encode(&diff.rendered),
      files: diff
        .files
        .into_iter()
        .map(|f| crate::control::DiffFileEntry {
          path_b64url: gfs_types::path::b64url_encode(f.path.as_bytes()),
          old_path_b64url: f
            .old_path
            .map(|p| gfs_types::path::b64url_encode(p.as_bytes())),
          status: f.status,
          additions: f.additions,
          deletions: f.deletions,
          binary: f.binary,
          old_mode: f.old_mode,
          new_mode: f.new_mode,
        })
        .collect(),
      truncated: diff.truncated,
    })
  }

  /// One directory of a snapshot, answered by the gateway.
  pub async fn ls(
    &self,
    rev: Option<&str>,
    path: &gfs_types::BytePath,
    page_size: u32,
  ) -> Result<crate::control::LsReport, GfsError> {
    let (client, pin) = {
      let current = self.current.lock().expect("current pin");
      (Arc::clone(&current.client), current.commit.clone())
    };
    let commit = match rev {
      Some(selector) => self.resolve_here(&client, &pin, selector).await?,
      None => pin,
    };

    // Paged to the end here rather than by the caller. A directory listing is
    // one question, and a CLI that had to thread a page token would be
    // reimplementing what the daemon already does for the filesystem.
    let mut entries = Vec::new();
    let mut token = Vec::new();
    loop {
      let page = client
        .list_directory_at(&commit, path, token, page_size, false)
        .await?;
      for entry in page.entries {
        entries.push(crate::control::LsEntry {
          path_b64url: gfs_types::path::b64url_encode(entry.path.as_bytes()),
          mode: entry.mode,
          size: entry.size,
          oid: entry.oid.to_qualified(),
        });
      }
      if page.next_page_token.is_empty() {
        break;
      }
      token = page.next_page_token;
    }
    Ok(crate::control::LsReport {
      commit: commit.to_qualified(),
      entries,
    })
  }

  /// One file's raw bytes at a revision, answered by the gateway.
  pub async fn cat(
    &self,
    rev: Option<&str>,
    path: &gfs_types::BytePath,
  ) -> Result<(String, Vec<u8>), GfsError> {
    let (client, pin) = {
      let current = self.current.lock().expect("current pin");
      (Arc::clone(&current.client), current.commit.clone())
    };
    let commit = match rev {
      Some(selector) => self.resolve_here(&client, &pin, selector).await?,
      None => pin,
    };
    let entry = client
      .get_entry_at(&commit, path, true)
      .await?
      .ok_or_else(|| GfsError::not_found("no such path in this commit"))?;
    let ticket = entry
      .blob_ticket
      .clone()
      .ok_or_else(|| GfsError::invalid("that path has no file content to print"))?;
    // Read through the blob cache the mount already has, so catting a file at
    // the pinned commit costs nothing the second time and shares with the
    // filesystem's own reads.
    let content = client.read_blob(&entry.oid, &ticket).await?;
    Ok((commit.to_qualified(), content))
  }

  /// Line attribution for one path, answered by the gateway.
  pub async fn blame(
    &self,
    rev: Option<&str>,
    path: &gfs_types::BytePath,
  ) -> Result<crate::control::BlameReport, GfsError> {
    let (client, pin) = {
      let current = self.current.lock().expect("current pin");
      (Arc::clone(&current.client), current.commit.clone())
    };
    let commit = match rev {
      Some(selector) => self.resolve_here(&client, &pin, selector).await?,
      None => pin,
    };
    let blame = client.blame(&commit, path).await?;
    Ok(crate::control::BlameReport {
      commit: commit.to_qualified(),
      path_b64url: gfs_types::path::b64url_encode(path.as_bytes()),
      hunks: blame
        .hunks
        .into_iter()
        .map(|h| crate::control::BlameHunkEntry {
          final_start_line: h.final_start_line,
          lines: h.lines,
          commit: h.commit.to_qualified(),
          orig_path_b64url: gfs_types::path::b64url_encode(h.orig_path.as_bytes()),
          orig_start_line: h.orig_start_line,
          author_name: h.author.name,
          author_email: h.author.email,
          author_time: h.author.time.secs,
          author_tz_offset_minutes: h.author.tz_offset_minutes,
          boundary: h.boundary,
        })
        .collect(),
      content_b64url: gfs_types::path::b64url_encode(&blame.content),
      truncated: blame.truncated,
    })
  }

  pub async fn export(&self, bundle: &Path) -> Result<gfs_overlay::ExportReport, GfsError> {
    let (status, base, overlay, commit) = self.export_inputs().await?;
    let exporter = gfs_overlay::Exporter::new(
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
      gfs_overlay::Status,
      crate::blobs::PreloadedBase,
      Arc<Overlay>,
      ObjectId,
    ),
    GfsError,
  > {
    let report = self.status().await?;
    let (overlay, client, commit) = {
      let current = self.current.lock().expect("current pin");
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

  /// Whether the workspace has changes, by the same definition `gfs status`
  /// reports.
  ///
  /// **Not `Overlay::is_empty`.** That answers "does the journal have rows",
  /// which is a different question: opening a base file for writing copies it up
  /// and leaves a row whether or not the bytes end up different. A caller who
  /// edited a file and undid the edit is told `nothing to commit, working tree
  /// clean` by `status` — and was then refused by `switch` with "the workspace
  /// has local changes", naming a change the tool it had just run said did not
  /// exist and offering nothing to discard.
  ///
  /// `Status::is_clean` is the definition a user can act on, so it is the one the
  /// gate uses. `status` hashes local content rather than reading rows, which is
  /// why the cheap version was reached for; the cost is bounded by the number of
  /// *modified* paths, not by the tree, and this runs only on a re-pin.
  async fn workspace_is_clean(&self) -> Result<bool, GfsError> {
    let (overlay, algorithm) = {
      let current = self.current.lock().expect("current pin");
      (Arc::clone(&current.overlay), current.commit.algorithm())
    };
    let status = tokio::task::spawn_blocking(move || overlay.status(algorithm))
      .await
      .map_err(|e| GfsError::internal(format!("the status task failed: {e}")))?
      .map_err(crate::fs::overlay_as_service_error)?;
    Ok(status.is_clean())
  }

  /// Unpublish, unmount, release the lease, and remove the state that says a
  /// mount is live here.
  pub async fn shutdown(self: &Arc<Self>) {
    if self.shutting_down.swap(true, Ordering::SeqCst) {
      return;
    }
    // Unpublish first: the job must stop being able to reach the mount before
    // the mount stops being able to serve it, or a read in flight sees ENOTCONN
    // rather than a missing workspace. With a direct mount the unmount below is
    // what actually does it, and this stays because a privileged publisher's
    // unpublish is a real step that must keep happening in this order.
    let _ = self.publisher.unpublish();

    // Taken out under the lock and unmounted outside it: `unmount_session`
    // awaits, and holding a `std::sync::Mutex` across an await is how a daemon
    // deadlocks on its own shutdown.
    let session = self.session.lock().expect("fuse session").take();
    let mountpoint = self.publisher.mountpoint().to_path_buf();
    if let Some(session) = session {
      unmount_session(session, mountpoint.clone()).await;
    }
    // The view holds no state of its own; unmounted after the workspace so no
    // job read can land on a dead alternates path while the mount still serves.
    self.odb_view.unmount();
    let (client, monitor) = {
      let current = self.current.lock().expect("current pin");
      (Arc::clone(&current.client), Arc::clone(&current.monitor))
    };
    if let Err(e) = client.release_mount(monitor.mount_id()).await {
      tracing::warn!(error = %e.message, "releasing the lease failed during shutdown");
    }
    // The workspace directory is the mount point now, so removing it is removing
    // what the job was told to use. Only the daemon's own state goes.
    let _ = std::fs::remove_file(MountState::path(&self.config.state_dir));
    let _ = std::fs::remove_file(self.control_socket());
  }

  /// Bind the control socket, without serving it.
  ///
  /// Separate from [`Mount::serve_control`] because the two have to happen at
  /// different times: the host must be able to tell a caller "your workspace is
  /// ready" only once the socket it will use is *bound*, while serving it is an
  /// endless loop that has to run on its own task.
  ///
  /// Binding inside the serve loop instead left a race that the test harness had
  /// to poll around — `gfs mount` returned as soon as the daemon said it was
  /// ready and then sent `Inspect` to a socket the spawned task had not reached
  /// yet. Handing the bound listener back makes readiness mean what it says.
  pub fn bind_control(&self) -> Result<tokio::net::UnixListener, GfsError> {
    use std::os::unix::fs::PermissionsExt;

    let path = self.control_socket();
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path)
      .map_err(|e| GfsError::internal(format!("binding the control socket: {e}")))?;
    // 0600 (ADR 0006). Set after bind, which is the only order available; the
    // window is one syscall wide and inside a 0700 directory.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
      .map_err(|e| GfsError::internal(format!("restricting the control socket: {e}")))?;
    Ok(listener)
  }

  /// Serve the control socket until `Unmount` is received or the future is
  /// dropped.
  pub async fn serve_control(
    self: Arc<Self>,
    listener: tokio::net::UnixListener,
  ) -> Result<(), GfsError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

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
        Err(e) => Response::from_error(&GfsError::invalid(format!(
          "unrecognized control request: {e}"
        ))),
      };
      let stop = matches!(response, Response::Unmounted);
      // A response that will not encode still has to produce a line. Dropping it
      // silently closes the connection, and the caller sees only "the daemon
      // closed the control connection without replying" — which names the
      // transport for a fault that is entirely in the payload.
      let encoded = serde_json::to_string(&response).unwrap_or_else(|e| {
        tracing::error!(error = %e, "a control response could not be encoded");
        serde_json::to_string(&Response::from_error(&GfsError::internal(format!(
          "the daemon could not encode its response: {e}"
        ))))
        .expect("an error response encodes")
      });
      let _ = writer.write_all(format!("{encoded}\n").as_bytes()).await;
      let _ = writer.flush().await;
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
      Request::CommitPlan => match self.commit_plan().await {
        Ok(plan) => Response::CommitPlan(Box::new(plan)),
        Err(e) => Response::from_error(&e),
      },
      Request::AdoptCommit { commit } => match self.adopt_commit(commit.as_str()).await {
        Ok(report) => Response::Refresh(report),
        Err(e) => Response::from_error(&e),
      },
      Request::Switch { selector, branch } => {
        match self.switch_to(selector.as_str(), branch.clone()).await {
          Ok(report) => Response::Refresh(report),
          Err(e) => Response::from_error(&e),
        }
      }
      Request::Status => match self.status().await {
        Ok(report) => Response::Status(Box::new(report)),
        Err(e) => Response::from_error(&e),
      },
      Request::FsMonitorChanges { token } => match self.fsmonitor_changes(&token).await {
        Ok(answer) => Response::FsMonitorChanges(Box::new(answer)),
        Err(e) => Response::from_error(&e),
      },
      Request::Diff => match self.diff().await {
        Ok(patch) => Response::Diff {
          patch_b64url: gfs_types::path::b64url_encode(&patch),
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
      Request::Log {
        skip,
        limit,
        ref from,
        first_parent,
        ref paths_b64url,
      } => {
        let decoded: Result<Vec<gfs_types::BytePath>, GfsError> = paths_b64url
          .iter()
          .map(|p| Ok(gfs_types::BytePath::new(gfs_types::path::b64url_decode(p)?)))
          .collect();
        match decoded {
          Ok(paths) => {
            let query = crate::client::LogQuery {
              skip,
              limit,
              first_parent,
              paths,
            };
            match self.log(from.as_deref(), query).await {
              Ok(report) => Response::Log(Box::new(report)),
              Err(e) => Response::from_error(&e),
            }
          }
          Err(e) => Response::from_error(&e),
        }
      }
      Request::DiffRevs { .. } => match self.diff_revs(&request).await {
        Ok(report) => Response::RevDiff(Box::new(report)),
        Err(e) => Response::from_error(&e),
      },
      Request::Ls {
        ref rev,
        ref path_b64url,
        page_size,
      } => match gfs_types::path::b64url_decode(path_b64url) {
        Ok(path) => {
          match self
            .ls(rev.as_deref(), &gfs_types::BytePath::new(path), page_size)
            .await
          {
            Ok(report) => Response::Ls(Box::new(report)),
            Err(e) => Response::from_error(&e),
          }
        }
        Err(e) => Response::from_error(&e),
      },
      Request::Cat {
        ref rev,
        ref path_b64url,
      } => match gfs_types::path::b64url_decode(path_b64url) {
        Ok(path) => {
          match self
            .cat(rev.as_deref(), &gfs_types::BytePath::new(path))
            .await
          {
            Ok((commit, content)) => Response::Cat {
              commit,
              content_b64url: gfs_types::path::b64url_encode(&content),
            },
            Err(e) => Response::from_error(&e),
          }
        }
        Err(e) => Response::from_error(&e),
      },
      Request::Blame {
        ref rev,
        ref path_b64url,
      } => match gfs_types::path::b64url_decode(path_b64url) {
        Ok(path) => {
          match self
            .blame(rev.as_deref(), &gfs_types::BytePath::new(path))
            .await
          {
            Ok(report) => Response::Blame(Box::new(report)),
            Err(e) => Response::from_error(&e),
          }
        }
        Err(e) => Response::from_error(&e),
      },
      Request::Unmount => {
        self.shutdown().await;
        Response::Unmounted
      }
    }
  }
}

/// One commit, as the control socket carries it.
fn log_entry(c: gfs_types::CommitMeta) -> crate::control::LogEntry {
  crate::control::LogEntry {
    commit: c.commit.to_qualified(),
    tree: c.tree.to_qualified(),
    parents: c.parents.iter().map(|p| p.to_qualified()).collect(),
    author_name: c.author.name,
    author_email: c.author.email,
    author_time: c.author.time.secs,
    author_tz_offset_minutes: c.author.tz_offset_minutes,
    committer_name: c.committer.name,
    committer_email: c.committer.email,
    committer_time: c.committer.time.secs,
    committer_tz_offset_minutes: c.committer.tz_offset_minutes,
    message: c.message,
  }
}

/// Where one pin's overlay lives.
fn overlay_dir(state_dir: &Path, epoch: u64) -> PathBuf {
  state_dir.join("overlay").join(epoch.to_string())
}

/// Make a path absolute without requiring it to exist.
///
/// `canonicalize` would be stronger but demands that every component already
/// exists, which the workspace path deliberately does not: it is about to be
/// created as a directory to mount on.
fn absolute(path: &Path) -> Result<PathBuf, GfsError> {
  std::path::absolute(path)
    .map_err(|e| GfsError::invalid(format!("{} cannot be made absolute: {e}", path.display())))
}

/// A pin, plus the two things the filesystem needs that the pin does not hold.
///
/// `gitdir` and `root` belong to the *filesystem's* view of a commit rather than
/// to the mount's bookkeeping, so they are handed straight to [`Gfs::new`] or
/// [`Gfs::repin`] and not stored twice.
struct Resolved {
  pin: Pin,
  facts: GitDirFacts,
  root: gfs_types::TreeEntryInfo,
}

/// Take out a lease on what a selector resolves to, and build everything that
/// reads through it.
///
/// Creates no FUSE session and touches no live filesystem: the caller decides
/// whether this is the first pin (mount it) or a replacement (swap it in). That
/// split is what makes a failed `gfs switch` a no-op — the mount is only
/// disturbed once this has already succeeded.
async fn resolve_pin(config: &MountSpec, selector: &str, epoch: u64) -> Result<Resolved, GfsError> {
  let grant = create_mount(config, selector).await?;

  let commit = ObjectId::parse_qualified(&grant.commit_oid)
    .map_err(|e| GfsError::internal(format!("server returned an unparseable commit: {e}")))?;
  let tree = ObjectId::parse_qualified(&grant.tree_oid)
    .map_err(|e| GfsError::internal(format!("server returned an unparseable tree: {e}")))?;
  let mount_id = MountId::parse(&grant.mount_id)?;
  let snapshot_time = grant
    .snapshot_time
    .map(|t| Timestamp::new(t.secs, t.nanos))
    .ok_or_else(|| GfsError::internal("server returned no snapshot time"))?;
  let expiry = grant
    .lease_expiry
    .map(|t| Timestamp::new(t.secs, t.nanos))
    .ok_or_else(|| GfsError::internal("server returned no lease expiry"))?;
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

  let facts = GitDirFacts {
    repository_id: config.repository_id.clone(),
    commit: commit.clone(),
    tree: tree.clone(),
    ref_name: grant.ref_name.clone(),
    mount_id: mount_id.clone(),
    control_socket: MountState::control_socket(&config.state_dir),
    snapshot_time,
    grpc_endpoint: config.grpc_endpoint.clone(),
    http_endpoint: config.http_endpoint.clone(),
    generation: epoch,
    commit_meta,
    work_ref_root: (!grant.work_ref_root.is_empty()).then(|| grant.work_ref_root.clone()),
  };

  // One overlay per pin, in its own directory. The binding check inside
  // `Overlay::open` then does real work: a daemon restarted against a moved
  // branch cannot silently adopt the previous pin's edits.
  let overlay_dir = overlay_dir(&config.state_dir, epoch);
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

  let root = crate::fs::root_entry(tree.clone());

  Ok(Resolved {
    facts,
    root,
    pin: Pin {
      epoch,
      overlay_dir,
      overlay,
      client,
      monitor: LeaseMonitor::new(mount_id, expiry, interval, config.lease_policy),
      commit,
      tree,
      ref_name: grant.ref_name,
      snapshot_time,
    },
  })
}

async fn create_mount(
  config: &MountSpec,
  selector: &str,
) -> Result<v1::CreateMountResponse, GfsError> {
  let channel = tonic::transport::Endpoint::from_shared(config.grpc_endpoint.clone())
    .map_err(|e| GfsError::invalid(format!("invalid gRPC endpoint: {e}")))?
    .connect()
    .await
    .map_err(|e| {
      GfsError::new(
        ErrorCode::Unavailable,
        format!("connecting to the GFS server: {e}"),
      )
    })?;
  let mut client = v1::snapshot_service_client::SnapshotServiceClient::new(channel);
  let mut request = tonic::Request::new(v1::CreateMountRequest {
    repository_id: config.repository_id.as_str().to_owned(),
    revision_selector: selector.to_owned(),
    requested_ttl_seconds: 0,
  });
  if !config.token.is_empty() {
    let value = format!("Bearer {}", config.token)
      .parse()
      .map_err(|_| GfsError::invalid("token is not a valid header value"))?;
    request.metadata_mut().insert("authorization", value);
  }
  Ok(
    client
      .create_mount(request)
      .await
      .map_err(|s| gfs_proto::convert::from_status(&s))?
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
fn adopt_or_refuse_state_dir(state_dir: &Path, workspace: &Path) -> Result<(), GfsError> {
  let socket = MountState::control_socket(state_dir);
  if socket.exists() {
    if crate::control::is_live(&socket) {
      return Err(GfsError::new(
        ErrorCode::Conflict,
        format!(
          "a live GFS daemon already owns {}; unmount it before mounting again",
          state_dir.display()
        ),
      ));
    }
    let _ = std::fs::remove_file(&socket);
  }

  // Unmount anything a killed daemon left behind. Every operation on such a
  // mount point returns ENOTCONN, so a new mount over it would be unreachable --
  // and, now that the workspace *is* the mount point, so would the check
  // `DirectMountPublisher` makes for a non-empty directory: `read_dir` on a dead
  // FUSE mount fails, and the publisher would refuse a path that holds nothing.
  //
  // The workspace is swept rather than the old `generations/` tree. A daemon
  // killed before this change left its mounts there instead, so both are
  // cleaned: an upgrade must not require the operator to know that the layout
  // moved.
  let mut orphans = vec![workspace.to_path_buf()];
  let legacy = state_dir.join("generations");
  if let Ok(entries) = std::fs::read_dir(&legacy) {
    orphans.extend(entries.flatten().map(|entry| entry.path()));
  }
  for path in orphans {
    let _ = std::process::Command::new("fusermount3")
      .args(["-u", "-z"])
      .arg(&path)
      .stdout(std::process::Stdio::null())
      .stderr(std::process::Stdio::null())
      .status();
  }
  let _ = std::fs::remove_dir_all(&legacy);
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
    adopt_or_refuse_state_dir(tmp.path(), &tmp.path().join("ws")).unwrap();
    assert!(!socket.exists());

    // A real listener: refused, because two daemons over one state directory
    // would each believe they own the lease.
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let e = adopt_or_refuse_state_dir(tmp.path(), &tmp.path().join("ws")).unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict);
  }
}
