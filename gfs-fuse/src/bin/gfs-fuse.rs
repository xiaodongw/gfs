//! `gfs-fuse`: the host daemon, serving however many mounts it is asked for.
//!
//! Started on demand by `gfs clone` and `gfs mount`, or in the foreground for
//! debugging. It binds one host socket, answers `CreateMount` on it, and from
//! there each mount runs its own lease, its own generations, and its own control
//! socket beside its workspace — see [`gfs_mount::host`] for why the per-mount
//! socket survived the consolidation.
//!
//! The process owns no mount-specific state of its own. That is deliberate: it is
//! what keeps a failing workspace from being a failing host.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use gfs_mount::host::{HostConfig, MountHost};
use gfs_mount::FsConfig;
use gfs_types::LeasePolicy;

#[derive(Parser, Debug)]
#[command(name = "gfs-fuse", version, about = "The GFS mount host daemon")]
struct Args {
  /// Where to listen. One host per socket, enforced by an `flock` beside it.
  #[arg(long, env = "GFS_HOST_SOCKET")]
  socket: Option<PathBuf>,

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

  /// Bytes a job may hydrate from the server before reads fail with EDQUOT.
  /// 0 disables the budget. Mandatory-by-default per ADR 0009: the Git
  /// configuration that keeps a workspace cheap is overridable per invocation,
  /// so this is the only enforcement a mount actually has.
  #[arg(long, env = "GFS_HYDRATION_BUDGET", default_value_t = FsConfig::default().hydration_budget_bytes)]
  hydration_budget: u64,

  /// Bytes each repository's odb projection may hold on local disk before it
  /// evicts and re-fetches instead of growing (ADR 0009's residency budget).
  /// 0 — the default — is unbounded; set it on hosts whose disk is smaller
  /// than the repositories they mount. Eviction degrades to re-fetching, never
  /// to refusal.
  #[arg(long, env = "GFS_ODB_RESIDENCY_BUDGET", default_value_t = 0)]
  odb_residency_budget: u64,
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
  let config = HostConfig {
    socket: args.socket.unwrap_or_else(gfs_mount::host::default_socket),
    grpc_endpoint: args.endpoint,
    http_endpoint: args.http_endpoint,
    token: args.token,
    lease_policy: LeasePolicy::adr_0006(),
    fs: FsConfig {
      hydration_budget_bytes: args.hydration_budget,
      ..FsConfig::default()
    },
    odb_residency_bytes: args.odb_residency_budget,
  };

  let (host, listener) =
    MountHost::bind(config).map_err(|e| anyhow::anyhow!("{}: {}", e.code.as_str(), e.message))?;
  tracing::info!(
    socket = %host.config().socket.display(),
    pid = std::process::id(),
    "the GFS host is listening"
  );

  // A signal is a *request* to tear down cleanly, not a reason to abandon leases.
  // ADR 0003 measured that a mount point survives its daemon and returns ENOTCONN
  // until something unmounts it, so exiting without this leaves every workspace
  // this host was serving failing every operation.
  let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    .context("installing the SIGTERM handler")?;
  let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
    .context("installing the SIGINT handler")?;

  // `serve` resolving is not by itself good news: it returns `Ok` when a client
  // asked the host to stop and `Err` when it could not serve at all. Reporting a
  // bind failure as a clean shutdown is how a cause ends up recorded nowhere.
  let mut failure = None;
  let served = tokio::spawn(Arc::clone(&host).serve(listener));
  tokio::select! {
    _ = sigterm.recv() => tracing::info!("SIGTERM: tearing down every mount"),
    _ = sigint.recv() => tracing::info!("SIGINT: tearing down every mount"),
    outcome = served => match outcome {
      Ok(Ok(())) => tracing::info!("shutdown requested over the host socket"),
      Ok(Err(e)) => {
        tracing::error!(error = %e.message, "the host socket stopped serving");
        failure = Some(anyhow::anyhow!("{}: {}", e.code.as_str(), e.message));
      }
      Err(e) => {
        tracing::error!(error = %e, "the host task did not finish");
        failure = Some(anyhow::anyhow!("the host task did not finish: {e}"));
      }
    },
  }

  // Teardown happens either way: a host socket that failed still leaves mount
  // points that return ENOTCONN until something unmounts them (ADR 0003).
  host.shutdown().await;
  match failure {
    Some(e) => Err(e),
    None => Ok(()),
  }
}
