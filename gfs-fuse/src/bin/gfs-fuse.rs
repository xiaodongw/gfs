//! `gfs-fuse`: the client daemon that owns one mounted workspace.
//!
//! Started by `gfs mount`, or directly in the foreground for debugging. It
//! creates the retention lease, mounts the pinned commit, publishes it at the
//! workspace path, renews the lease on a heartbeat, and answers the control
//! socket until told to unmount.
//!
//! Deliberately one mount per daemon. A daemon owning several would have to
//! decide what a partial failure means -- one mount lost, the others alive -- and
//! ADR 0006's failure policy has no answer for that. One process per mount makes
//! "the daemon died" and "the mount is gone" the same event, which is the
//! behaviour an orchestrator can reason about.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use gfs_mount::daemon::{Daemon, DaemonConfig};
use gfs_mount::{FsConfig, MountConfig};
use gfs_types::{LeasePolicy, RepositoryId};

#[derive(Parser, Debug)]
#[command(name = "gfs-fuse", version, about = "The GFS mount daemon")]
struct Args {
  /// Where the job's state lives: `mount.json`, the control socket, and the
  /// per-generation mount points.
  #[arg(long)]
  state_dir: PathBuf,

  /// The path the job uses. Published by the daemon; must not already exist as
  /// a real directory.
  #[arg(long)]
  workspace: PathBuf,

  /// The shared blob cache. Shared between mounts of the same repository on one
  /// host, and scoped to that repository (ADR 0002, ADR 0006).
  #[arg(long)]
  cache_dir: PathBuf,

  #[arg(long, env = "GFS_ENDPOINT", default_value = "http://127.0.0.1:8431")]
  endpoint: String,

  #[arg(
    long,
    env = "GFS_HTTP_ENDPOINT",
    default_value = "http://127.0.0.1:8430"
  )]
  http_endpoint: String,

  #[arg(long, env = "GFS_TOKEN", hide_env_values = true, default_value = "")]
  token: String,

  #[arg(long)]
  repo: String,

  /// A branch, tag, or object ID. Resolved once, by `CreateMount`.
  #[arg(long, default_value = "HEAD")]
  rev: String,

  /// Blob cache quota in bytes.
  #[arg(long, default_value_t = 8 * 1024 * 1024 * 1024)]
  cache_quota: u64,

  /// Overlay quota in bytes, reported by `statfs`. Enforced from M3.
  #[arg(long, default_value_t = 1024 * 1024 * 1024)]
  overlay_quota: u64,

  /// Allow a different UID to read the mount. Requires `user_allow_other` in
  /// `/etc/fuse.conf`, a privileged one-time host action (ADR 0003).
  #[arg(long)]
  allow_other: bool,

  /// FUSE event-loop threads. More than one is half of ADR 0003's dispatch rule.
  #[arg(long, default_value_t = 4)]
  fuse_threads: usize,

  /// Write this file once the workspace is usable. `gfs mount` waits for it,
  /// which is more reliable than polling for the socket: the socket exists
  /// before the first mount is published.
  #[arg(long)]
  ready_file: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
  tracing_subscriber::fmt()
    .with_env_filter(
      tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
    )
    .init();

  let args = Args::parse();
  let config = DaemonConfig {
    state_dir: args.state_dir.clone(),
    workspace: args.workspace.clone(),
    cache_dir: args.cache_dir,
    grpc_endpoint: args.endpoint,
    http_endpoint: args.http_endpoint,
    token: args.token,
    repository_id: RepositoryId::parse(&args.repo).context("invalid repository id")?,
    revision_selector: args.rev,
    cache_quota_bytes: args.cache_quota,
    fs: FsConfig::default(),
    overlay: gfs_overlay::OverlayConfig {
      quota_bytes: args.overlay_quota,
      ..gfs_overlay::OverlayConfig::default()
    },
    mount: MountConfig {
      threads: args.fuse_threads,
      allow_other: args.allow_other,
      // Only with `allow_other`, which `fuser` enforces anyway: `auto_unmount`
      // needs it, and asking for it without would fail the mount outright.
      auto_unmount: args.allow_other,
    },
    lease_policy: LeasePolicy::adr_0006(),
    retire_timeout: Duration::from_secs(300),
  };

  let daemon = Daemon::start(config)
    .await
    .map_err(|e| anyhow::anyhow!("{}: {}", e.code.as_str(), e.message))?;

  if let Some(ready) = &args.ready_file {
    std::fs::write(ready, format!("{}\n", std::process::id()))
      .with_context(|| format!("writing {}", ready.display()))?;
  }
  tracing::info!(
    workspace = %args.workspace.display(),
    state_dir = %args.state_dir.display(),
    "workspace is ready"
  );

  daemon.spawn_index_warmup();
  let heartbeat = tokio::spawn(Arc::clone(&daemon).run_heartbeat());
  let control = tokio::spawn(Arc::clone(&daemon).serve_control());

  // A signal is a *request* to tear down cleanly, not a reason to abandon a
  // lease. ADR 0003 measured that a mount point survives its daemon and returns
  // ENOTCONN until something unmounts it, so exiting without this leaves the job
  // with a workspace that fails every operation.
  let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    .context("installing the SIGTERM handler")?;
  let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
    .context("installing the SIGINT handler")?;

  // `serve_control` resolving is not by itself good news: it returns `Ok` when a
  // client asked to unmount and `Err` when it could not serve at all. Matching
  // only on the join handle reported a bind failure — an over-long socket path,
  // a permissions problem — as a clean unmount and exited 0, so `gfs mount`
  // said "gfs-fuse exited before the workspace was ready (exit status: 0)" and the
  // cause was recorded nowhere. The failure is distinguished and propagated.
  let mut control_failure = None;
  tokio::select! {
    _ = sigterm.recv() => tracing::info!("SIGTERM: tearing down"),
    _ = sigint.recv() => tracing::info!("SIGINT: tearing down"),
    outcome = control => match outcome {
      Ok(Ok(())) => tracing::info!("unmount requested over the control socket"),
      Ok(Err(e)) => {
        tracing::error!(error = %e.message, "the control socket stopped serving");
        control_failure = Some(anyhow::anyhow!("{}: {}", e.code.as_str(), e.message));
      }
      Err(e) => {
        tracing::error!(error = %e, "the control task did not finish");
        control_failure = Some(anyhow::anyhow!("the control task did not finish: {e}"));
      }
    },
  }

  // Teardown happens either way: a control socket that failed still leaves a
  // mount point that returns ENOTCONN until something unmounts it (ADR 0003).
  daemon.shutdown().await;
  heartbeat.abort();
  if let Some(ready) = &args.ready_file {
    let _ = std::fs::remove_file(ready);
  }
  match control_failure {
    Some(e) => Err(e),
    None => Ok(()),
  }
}
