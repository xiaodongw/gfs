//! The `gfs-fuse` process: one host, many mounts.
//!
//! Every mount is still a whole [`Mount`] with its own lease, its own generations,
//! and its own control socket in the runtime directory (named in its
//! `.git/gfs.json`; see [`crate::state::workspace_control_socket`]). What changed
//! is only how many of them a process owns. Nothing that speaks
//! [`crate::control::Request`] can tell the difference, which is the point: a job
//! finds its workspace by walking up from the current directory to its `.git`,
//! and that walk resolves to exactly one answer whether the process behind it
//! serves one mount or twenty.
//!
//! # Why the per-mount socket survived
//!
//! It was never an operating-system requirement. The kernel talks to a FUSE
//! filesystem over a `/dev/fuse` descriptor obtained by `fusermount3` and held in
//! the process's file table; it never opens the control socket, and a job doing
//! ordinary file I/O never does either. The control socket is a *name*, and one
//! process can bind as many names as it likes. Consolidating them into a single
//! mount-addressed endpoint would have meant a selector on all fifteen request
//! variants and a new discovery rule, for no capability the host does not already
//! have.
//!
//! # What one process buys, and what it costs
//!
//! Bought: one blob cache per repository instead of one per mount, so
//! `--cache-quota` means "per host" rather than silently multiplying by the mount
//! count; one tokio runtime; one process to supervise. Not bought: FUSE threads.
//! `fuser` spawns its own per session, so N mounts is still N × `--fuse-threads`.
//!
//! Cost: the failure blast radius. Under one process per mount, "the daemon died"
//! and "the mount is gone" were the same event, which is what ADR 0006's failure
//! policy assumed. They are no longer the same event, so this module keeps them as
//! close as it can — every mount's state is per-instance, so a poisoned lock or a
//! failed lease is contained to the workspace it belongs to, and a mount whose
//! control task ends for any reason is torn down and deregistered rather than left
//! half-alive in the registry.
//!
//! # One host per socket, enforced by the kernel
//!
//! Two hosts sharing a socket path would each believe they owned it, and the
//! second would unlink the first's socket out from under its clients. The lock is
//! an `flock` held for the process's lifetime rather than a pid file: the kernel
//! releases it when the process dies by any means, so there is no staleness case
//! to get wrong.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::{HashAlgorithm, LeasePolicy, RepositoryId};
use tracing::Instrument;

use crate::cache::BlobCache;
use crate::control::{
  HostInfo, HostRequest, HostResponse, MountReport, MountRequest, MountSummary,
};
use crate::fs::FsConfig;
use crate::mount::{Mount, MountSpec};
use crate::session::MountConfig;

/// Everything the host is, independent of any mount it happens to be serving.
#[derive(Clone, Debug)]
pub struct HostConfig {
  pub socket: PathBuf,
  /// The gateway a `CreateMount` reaches unless it names another one.
  pub grpc_endpoint: String,
  pub http_endpoint: String,
  pub token: String,
  pub lease_policy: LeasePolicy,
  pub fs: FsConfig,
  /// Bytes each repository's odb projection may hold on local disk before it
  /// evicts and re-fetches (ADR 0009's residency budget). Zero — the default —
  /// is unbounded: the budget exists for small disks and is opt-in, unlike the
  /// hydration budget, because eviction degrades to slowness rather than
  /// refusal and a host with disk to spare should not pay re-fetches for it.
  pub odb_residency_bytes: u64,
}

/// Where a host listens when nobody says otherwise.
///
/// Under `XDG_RUNTIME_DIR` because that is a per-user directory the system already
/// clears on logout, which is the right lifetime for a socket naming a process.
/// The fallback is uid-qualified so two users on one machine never collide, and
/// both are kept short: a Unix socket path is capped near 108 bytes, and the
/// failure when it is exceeded is an opaque bind error.
pub fn default_socket() -> PathBuf {
  crate::state::runtime_dir().join("host.sock")
}

/// A mount, plus the tasks that keep it alive.
#[derive(Debug)]
struct MountEntry {
  mount: Arc<Mount>,
  heartbeat: tokio::task::JoinHandle<()>,
  /// The task serving this mount's own control socket. Held so an externally
  /// driven teardown can stop it; deliberately *not* aborted when the task
  /// deregisters itself, because a task cannot cancel itself and then finish.
  control: tokio::task::JoinHandle<()>,
}

#[derive(Debug)]
pub struct MountHost {
  config: HostConfig,
  /// Keyed by absolute workspace path, which is what identifies a mount:
  /// `adopt_or_refuse_workspace` already treats it as the thing at most one
  /// mount may own.
  mounts: Mutex<HashMap<PathBuf, MountEntry>>,
  /// One odb projection per `(cache directory, repository)`, the same sharing
  /// rule as the blob cache below and for the same reason (ADR 0009: every file
  /// in the store is immutable by name, so mounts can only agree).
  odbs: Mutex<HashMap<(PathBuf, String), Weak<crate::odb::OdbProjection>>>,
  /// One blob cache per `(cache directory, repository)`, shared by every mount
  /// of that repository. Weak so that a repository nobody mounts any more stops
  /// costing an in-memory index.
  caches: Mutex<HashMap<(PathBuf, String), Weak<BlobCache>>>,
  /// One opened clone per canonical path (local mode, ADR 0013), shared by
  /// every workspace mounted from it; weak for the same reason as the rest.
  locals: Mutex<HashMap<PathBuf, Weak<crate::local::LocalRepository>>>,
  _lock: SingletonLock,
}

impl MountHost {
  /// Take the singleton lock and bind the host socket.
  ///
  /// Returns the listener rather than storing it, for the same reason
  /// [`Mount::bind_control`] does: binding is the moment the host becomes
  /// reachable, and a caller that has not yet started serving still needs to be
  /// able to say so.
  pub fn bind(config: HostConfig) -> Result<(Arc<Self>, tokio::net::UnixListener), GfsError> {
    if let Some(parent) = config.socket.parent() {
      std::fs::create_dir_all(parent)
        .map_err(|e| GfsError::internal(format!("creating the host socket directory: {e}")))?;
      // 0700: the directory holds a socket that accepts `CreateMount`, and a
      // mount request carries a gateway token.
      let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }

    let lock = SingletonLock::acquire(&config.socket.with_extension("lock"))?;

    // Safe only because the lock is held: another host may be between its own
    // liveness check and its bind, and removing a socket it is about to bind
    // would leave its clients connecting to an unlinked inode.
    if config.socket.exists() {
      if crate::control::is_live(&config.socket) {
        return Err(GfsError::new(
          ErrorCode::Conflict,
          format!(
            "a GFS host is already listening on {}",
            config.socket.display()
          ),
        ));
      }
      let _ = std::fs::remove_file(&config.socket);
    }

    let listener = tokio::net::UnixListener::bind(&config.socket)
      .map_err(|e| GfsError::internal(format!("binding the host socket: {e}")))?;
    std::fs::set_permissions(&config.socket, std::fs::Permissions::from_mode(0o600))
      .map_err(|e| GfsError::internal(format!("restricting the host socket: {e}")))?;

    let host = Arc::new(MountHost {
      config,
      mounts: Mutex::new(HashMap::new()),
      caches: Mutex::new(HashMap::new()),
      odbs: Mutex::new(HashMap::new()),
      locals: Mutex::new(HashMap::new()),
      _lock: lock,
    });
    Ok((host, listener))
  }

  pub fn config(&self) -> &HostConfig {
    &self.config
  }

  pub fn info(&self) -> HostInfo {
    HostInfo {
      version: env!("CARGO_PKG_VERSION").to_owned(),
      state_format_version: gfs_types::STATE_FORMAT_VERSION,
      api_version: gfs_types::API_VERSION.to_owned(),
      pid: std::process::id(),
      socket: self.config.socket.clone(),
      grpc_endpoint: self.config.grpc_endpoint.clone(),
      http_endpoint: self.config.http_endpoint.clone(),
      mounts: self.mounts.lock().expect("mounts").len(),
    }
  }

  pub fn list(&self) -> Vec<MountSummary> {
    let mut summaries: Vec<_> = self
      .mounts
      .lock()
      .expect("mounts")
      .values()
      .map(|entry| entry.mount.summary())
      .collect();
    // Stable order, so an operator diffing two `gfs daemon status` runs sees
    // only what changed.
    summaries.sort_by(|a, b| a.workspace.cmp(&b.workspace));
    summaries
  }

  /// Mount a repository, publish it, and start serving its control socket.
  ///
  /// Returns only once the workspace is usable *and* its socket is bound, so a
  /// caller may act on the reply immediately. That ordering is why
  /// [`Mount::bind_control`] exists.
  pub async fn create_mount(
    self: &Arc<Self>,
    request: MountRequest,
  ) -> Result<MountReport, GfsError> {
    if request.state_format_version != gfs_types::STATE_FORMAT_VERSION {
      return Err(GfsError::new(
        ErrorCode::FailedPrecondition,
        format!(
          "this client speaks state format {} and the running host speaks {}; \
           restart the host (`gfs daemon stop`) to pick up the new build",
          request.state_format_version,
          gfs_types::STATE_FORMAT_VERSION
        ),
      ));
    }

    let spec = MountSpec {
      workspace: absolute(&request.workspace)?,
      cache_dir: absolute(&request.cache_dir)?,
      grpc_endpoint: request
        .grpc_endpoint
        .unwrap_or_else(|| self.config.grpc_endpoint.clone()),
      http_endpoint: request
        .http_endpoint
        .unwrap_or_else(|| self.config.http_endpoint.clone()),
      token: request.token.unwrap_or_else(|| self.config.token.clone()),
      // In local mode the identity comes from the clone, once it is opened;
      // the placeholder is replaced in `mount` before anything reads it.
      repository_id: if request.local_clone.is_some() {
        RepositoryId::parse("local-pending")?
      } else {
        RepositoryId::parse(&request.repository_id)?
      },
      local_clone: request.local_clone,
      revision_selector: request.revision_selector,
      cache_quota_bytes: request.cache_quota_bytes,
      fs: FsConfig {
        writeback_cache: request.writeback_cache,
        ..self.config.fs.clone()
      },
      overlay: gfs_overlay::OverlayConfig {
        quota_bytes: request.overlay_quota_bytes,
        ..gfs_overlay::OverlayConfig::default()
      },
      mount: MountConfig {
        threads: request
          .fuse_threads
          .unwrap_or_else(|| MountConfig::default().threads),
        allow_other: request.allow_other,
        // Only with `allow_other`, which `fuser` enforces anyway.
        auto_unmount: request.allow_other,
      },
      lease_policy: self.config.lease_policy,
    };
    Ok(self.mount(spec).await?.inspect())
  }

  /// Create a mount from a fully-formed spec.
  ///
  /// [`MountHost::create_mount`] is this plus the translation from the wire
  /// request, which deliberately does not carry everything a spec has — lease
  /// policy and `FsConfig` are the host's, not a caller's. In-process embedders
  /// and the test harness use this to set what the wire form leaves out.
  pub async fn mount(self: &Arc<Self>, spec: MountSpec) -> Result<Arc<Mount>, GfsError> {
    // Absolute up front so that the registry key and the cache key are the same
    // strings a second call would produce from a relative path.
    let spec = MountSpec {
      workspace: absolute(&spec.workspace)?,
      cache_dir: absolute(&spec.cache_dir)?,
      ..spec
    };
    let workspace = spec.workspace.clone();

    // Checked before anything is created, so a duplicate request is refused
    // rather than half-applied. `Mount::start` checks the same thing against the
    // filesystem, which is what catches a mount owned by a *different* process.
    if self.mounts.lock().expect("mounts").contains_key(&workspace) {
      return Err(GfsError::new(
        ErrorCode::Conflict,
        format!(
          "this host already serves a mount at {}; unmount it first",
          workspace.display()
        ),
      ));
    }

    let (spec, backing) = match spec.local_clone.clone() {
      Some(clone) => {
        let local = self.local_for(&clone).await?;
        let spec = MountSpec {
          repository_id: local.repository_id().clone(),
          local_clone: Some(local.clone_path().to_path_buf()),
          ..spec
        };
        (spec, crate::mount::Backing::Local(local))
      }
      None => {
        let odb = self
          .odb_for(
            &spec.cache_dir,
            &spec.repository_id,
            &spec.http_endpoint,
            &spec.token,
          )
          .await?;
        (spec, crate::mount::Backing::Remote(odb))
      }
    };
    let cache = self.cache_for(&spec.cache_dir, &spec.repository_id, spec.cache_quota_bytes)?;
    let mount = Mount::start(spec, cache, backing).await?;
    let listener = match mount.bind_control() {
      Ok(listener) => listener,
      Err(e) => {
        // A mount nobody can reach is worse than no mount: it holds a lease and
        // publishes a workspace whose control socket does not answer.
        mount.shutdown().await;
        return Err(e);
      }
    };
    self.register(workspace, Arc::clone(&mount), listener);
    Ok(mount)
  }

  /// The mount serving a workspace, if this host has one.
  pub fn mount_at(&self, workspace: &Path) -> Option<Arc<Mount>> {
    let workspace = absolute(workspace).ok()?;
    self
      .mounts
      .lock()
      .expect("mounts")
      .get(&workspace)
      .map(|entry| Arc::clone(&entry.mount))
  }

  /// The cache handle for one repository on one cache root, creating it if this
  /// is the first mount to ask.
  fn cache_for(
    &self,
    cache_dir: &Path,
    repository_id: &RepositoryId,
    quota_bytes: u64,
  ) -> Result<Arc<BlobCache>, GfsError> {
    let key = (cache_dir.to_path_buf(), repository_id.as_str().to_owned());
    let mut caches = self.caches.lock().expect("caches");
    if let Some(existing) = caches.get(&key).and_then(Weak::upgrade) {
      return Ok(existing);
    }
    let cache = BlobCache::open(cache_dir, repository_id, HashAlgorithm::Sha1, quota_bytes)?;
    caches.insert(key, Arc::downgrade(&cache));
    caches.retain(|_, handle| handle.strong_count() > 0);
    Ok(cache)
  }

  /// The object-database projection for one repository, opening it if this is
  /// the first workspace to ask (ADR 0009). No mount any more: the workspace
  /// filesystem serves the store itself (ADR 0011).
  ///
  /// Weak for the same reason the cache map is: the projection lives exactly as
  /// long as some mount borrows it.
  async fn odb_for(
    &self,
    cache_dir: &Path,
    repository_id: &RepositoryId,
    http_endpoint: &str,
    token: &str,
  ) -> Result<Arc<crate::odb::OdbProjection>, GfsError> {
    let key = (cache_dir.to_path_buf(), repository_id.as_str().to_owned());
    {
      let mut odbs = self.odbs.lock().expect("odb projections");
      if let Some(existing) = odbs.get(&key).and_then(Weak::upgrade) {
        return Ok(existing);
      }
      odbs.retain(|_, handle| handle.strong_count() > 0);
    }
    // Opened outside the lock: it fetches the manifest over the network, and a
    // second concurrent asker at worst opens a duplicate that drops on insert.
    let client = crate::odb::OdbClient::new(http_endpoint, token, repository_id.clone());
    let root = cache_dir.join(repository_id.as_str()).join("odb");
    let projection =
      crate::odb::OdbProjection::open(client, &root, self.config.odb_residency_bytes).await?;
    let mut odbs = self.odbs.lock().expect("odb projections");
    if let Some(existing) = odbs.get(&key).and_then(Weak::upgrade) {
      return Ok(existing);
    }
    odbs.insert(key, Arc::downgrade(&projection));
    Ok(projection)
  }

  /// The opened clone at a path (local mode), opening it if this is the first
  /// workspace to ask. Keyed by canonical path, so two spellings of one clone
  /// share one handle pool and one blob memory.
  async fn local_for(&self, clone: &Path) -> Result<Arc<crate::local::LocalRepository>, GfsError> {
    let key = clone.canonicalize().map_err(|e| {
      GfsError::new(
        ErrorCode::NotFound,
        format!("no clone at {}: {e}", clone.display()),
      )
    })?;
    {
      let mut locals = self.locals.lock().expect("local clones");
      if let Some(existing) = locals.get(&key).and_then(Weak::upgrade) {
        return Ok(existing);
      }
      locals.retain(|_, handle| handle.strong_count() > 0);
    }
    // Opened outside the lock and off the runtime: libgit2 opens its handles
    // synchronously, and a second concurrent asker at worst opens a duplicate
    // that drops on insert.
    let opened = {
      let key = key.clone();
      tokio::task::spawn_blocking(move || crate::local::LocalRepository::open(&key))
        .await
        .map_err(|e| GfsError::internal(format!("opening the clone: {e}")))??
    };
    let mut locals = self.locals.lock().expect("local clones");
    if let Some(existing) = locals.get(&key).and_then(Weak::upgrade) {
      return Ok(existing);
    }
    locals.insert(key, Arc::downgrade(&opened));
    Ok(opened)
  }

  /// Start a mount's tasks and record it.
  fn register(
    self: &Arc<Self>,
    key: PathBuf,
    mount: Arc<Mount>,
    listener: tokio::net::UnixListener,
  ) {
    let workspace = mount.config().workspace.display().to_string();
    mount.spawn_index_warmup();

    let heartbeat = tokio::spawn(
      Arc::clone(&mount)
        .run_heartbeat()
        .instrument(tracing::info_span!("mount", workspace = %workspace)),
    );

    // Every log line from a shared host has to say which workspace it is about;
    // under one process per mount the process itself was the answer.
    let span = tracing::info_span!("mount", workspace = %workspace);
    let host = Arc::downgrade(self);
    let forget_key = key.clone();
    let served = Arc::clone(&mount);
    let control = tokio::spawn(
      async move {
        match Arc::clone(&served).serve_control(listener).await {
          Ok(()) => tracing::info!("unmount requested over the control socket"),
          Err(e) => tracing::error!(error = %e.message, "the control socket stopped serving"),
        }
        // Either the job asked to unmount or the socket failed. A mount that
        // cannot be reached must not stay in the registry claiming to be live,
        // and its lease has to be released either way.
        if let Some(host) = host.upgrade() {
          host.forget(&forget_key).await;
        }
      }
      .instrument(span),
    );

    self.mounts.lock().expect("mounts").insert(
      key,
      MountEntry {
        mount,
        heartbeat,
        control,
      },
    );
  }

  /// Drop a mount whose own control task has finished.
  ///
  /// Called from inside that task, which is why the control handle is dropped
  /// rather than aborted: aborting it here would cancel the caller mid-teardown.
  async fn forget(&self, workspace: &Path) {
    let entry = self.mounts.lock().expect("mounts").remove(workspace);
    if let Some(entry) = entry {
      entry.heartbeat.abort();
      // Idempotent, and already done when this came from `Request::Unmount`.
      entry.mount.shutdown().await;
    }
  }

  /// Tear one mount down from the host socket.
  pub async fn destroy_mount(&self, workspace: &Path) -> Result<(), GfsError> {
    let workspace = absolute(workspace)?;
    let entry = {
      let mut mounts = self.mounts.lock().expect("mounts");
      let key = mounts
        .iter()
        .find(|(_, entry)| entry.mount.config().workspace == workspace)
        .map(|(key, _)| key.clone());
      key.and_then(|key| mounts.remove(&key))
    };
    let Some(entry) = entry else {
      return Err(GfsError::new(
        ErrorCode::NotFound,
        format!("this host serves no mount at {}", workspace.display()),
      ));
    };
    entry.control.abort();
    entry.heartbeat.abort();
    entry.mount.shutdown().await;
    Ok(())
  }

  /// Tear down every mount, then stop answering.
  ///
  /// Concurrently, because each mount's unmount is bounded by its own timeout
  /// and a host with twenty workspaces would otherwise take twenty times as long
  /// to answer a `SIGTERM` as one with a single workspace.
  pub async fn shutdown(&self) {
    let entries: Vec<MountEntry> = std::mem::take(&mut *self.mounts.lock().expect("mounts"))
      .into_values()
      .collect();
    let teardowns: Vec<_> = entries
      .into_iter()
      .map(|entry| {
        tokio::spawn(async move {
          entry.control.abort();
          entry.heartbeat.abort();
          entry.mount.shutdown().await;
        })
      })
      .collect();
    for teardown in teardowns {
      let _ = teardown.await;
    }
    let _ = std::fs::remove_file(&self.config.socket);
  }

  /// Answer the host socket until `Shutdown` is received or the future is dropped.
  pub async fn serve(self: Arc<Self>, listener: tokio::net::UnixListener) -> Result<(), GfsError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    loop {
      let Ok((stream, _)) = listener.accept().await else {
        continue;
      };
      let host = Arc::clone(&self);
      let (reader, mut writer) = tokio::io::split(stream);
      let mut lines = BufReader::new(reader).lines();
      let Ok(Some(line)) = lines.next_line().await else {
        continue;
      };
      let response = match serde_json::from_str::<HostRequest>(&line) {
        Ok(request) => host.handle(request).await,
        Err(e) => HostResponse::from_error(&GfsError::invalid(format!(
          "unrecognized host request: {e}"
        ))),
      };
      let stop = matches!(response, HostResponse::ShuttingDown);
      // A response that will not encode still has to produce a line. Dropping it
      // silently closes the connection, and the caller sees only "the daemon
      // closed the control connection without replying" — which names the
      // transport for a fault that is entirely in the payload.
      let encoded = serde_json::to_string(&response).unwrap_or_else(|e| {
        tracing::error!(error = %e, "a host response could not be encoded");
        serde_json::to_string(&HostResponse::from_error(&GfsError::internal(format!(
          "the host could not encode its response: {e}"
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

  async fn handle(self: &Arc<Self>, request: HostRequest) -> HostResponse {
    match request {
      HostRequest::Info => HostResponse::Info(Box::new(self.info())),
      HostRequest::CreateMount(request) => match self.create_mount(*request).await {
        Ok(report) => HostResponse::Mounted(Box::new(report)),
        Err(e) => HostResponse::from_error(&e),
      },
      HostRequest::ListMounts => HostResponse::Mounts {
        mounts: self.list(),
      },
      HostRequest::DestroyMount { workspace } => match self.destroy_mount(&workspace).await {
        Ok(()) => HostResponse::Destroyed,
        Err(e) => HostResponse::from_error(&e),
      },
      // Answered before the teardown runs: the caller asked the host to stop and
      // should not wait out every unmount to learn that it agreed. The reply is
      // what ends `serve`, and the binary tears down after that returns.
      HostRequest::Shutdown => HostResponse::ShuttingDown,
    }
  }
}

/// An `flock` held for the life of the process.
///
/// A pid file would need a staleness rule, and every staleness rule is wrong for
/// the case where the pid was reused. The kernel drops this when the process
/// dies, however it dies.
#[derive(Debug)]
struct SingletonLock {
  _file: std::fs::File,
}

impl SingletonLock {
  fn acquire(path: &Path) -> Result<Self, GfsError> {
    use std::os::fd::AsRawFd;

    let file = std::fs::OpenOptions::new()
      .create(true)
      .truncate(false)
      .write(true)
      .open(path)
      .map_err(|e| GfsError::internal(format!("opening the host lock {}: {e}", path.display())))?;

    // The workspace denies `unsafe_code`; this is the documented opt-out, on the
    // same reasoning as `attr::Ownership::current`. `flock` takes a descriptor
    // and a flag word, touches no memory, and reports failure through `errno`.
    // The descriptor is owned by the `File` above and outlives the call.
    #[allow(unsafe_code)]
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if locked != 0 {
      return Err(GfsError::new(
        ErrorCode::Conflict,
        format!(
          "another GFS host holds {} ({})",
          path.display(),
          std::io::Error::last_os_error()
        ),
      ));
    }
    Ok(SingletonLock { _file: file })
  }
}

/// Make a path absolute without requiring it to exist.
fn absolute(path: &Path) -> Result<PathBuf, GfsError> {
  std::path::absolute(path)
    .map_err(|e| GfsError::invalid(format!("{} cannot be made absolute: {e}", path.display())))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn one_host_per_socket_and_the_lock_outlives_a_stale_socket() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("host.lock");

    let first = SingletonLock::acquire(&path).expect("the first host takes the lock");
    let second = SingletonLock::acquire(&path).unwrap_err();
    assert_eq!(second.code, ErrorCode::Conflict);

    // Released by dropping the file, which is what process death does too.
    drop(first);
    SingletonLock::acquire(&path).expect("the lock is free once its holder is gone");
  }

  #[test]
  fn the_default_socket_is_per_user_and_short_enough_to_bind() {
    let socket = default_socket();
    assert!(socket.is_absolute(), "{}", socket.display());
    // A Unix socket path is capped near 108 bytes and the failure is opaque.
    assert!(socket.as_os_str().len() < 100, "{}", socket.display());
  }
}
