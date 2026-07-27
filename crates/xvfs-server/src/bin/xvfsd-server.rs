//! The XVFS server process.
//!
//! Startup order matters and is not incidental:
//!
//! 1. assert the pinned libgit2 version, because ADR 0001 makes the
//!    supported-repository-format boundary a property of that exact build;
//! 2. open the catalog, which refuses a schema newer than this build understands;
//! 3. **reconcile** leases and refs, before accepting any traffic. A `PREPARING`
//!    lease left by a crashed process must be resolved before a new mount can
//!    collide with its anchor, and an `ACTIVE` lease with a missing anchor must be
//!    repaired before its daemon's next read;
//! 4. only then listen.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use xvfs_server::auth::{AllowList, CapabilityKey, StaticTokens};
use xvfs_server::{observability, service, Catalog, Server};
use xvfs_types::{LeasePolicy, SubjectId};

#[derive(Parser, Debug)]
#[command(
  name = "xvfsd-server",
  version,
  about = "XVFS repository and snapshot server"
)]
struct Args {
  /// Directory holding the catalog and bare repositories.
  #[arg(long, env = "XVFS_STATE_DIR", default_value = "/var/lib/xvfs")]
  state_dir: PathBuf,

  /// gRPC listen address.
  #[arg(long, env = "XVFS_GRPC_ADDR", default_value = "127.0.0.1:8431")]
  grpc_addr: String,

  /// HTTP listen address for blobs, health, and metrics.
  #[arg(long, env = "XVFS_HTTP_ADDR", default_value = "127.0.0.1:8430")]
  http_addr: String,

  /// Hex-encoded capability signing key, at least 32 bytes.
  ///
  /// Generated when absent, which is correct for local development and wrong for
  /// anything with more than one process or a restart: every outstanding capability
  /// becomes invalid when the key changes, so a restart would break live mounts.
  #[arg(long, env = "XVFS_CAPABILITY_KEY", hide_env_values = true)]
  capability_key: Option<String>,

  /// Register a repository at startup, as `id=/path/to/bare.git`. Repeatable.
  ///
  /// Idempotent, so the local development stack can be restarted without
  /// bookkeeping. This is a bootstrap convenience, not the admin API: M6.1 wires the
  /// orchestrator's own repository provisioning, and a real deployment creates
  /// repositories through the catalog rather than through argv.
  #[arg(long = "import", value_name = "ID=PATH")]
  imports: Vec<String>,

  /// Development bearer token, granting read access to every repository.
  ///
  /// Present because M1.5's OIDC integration is a declared seam. A deployment must
  /// supply a real authenticator instead; `--dev-token` makes the substitute
  /// obvious rather than hiding it behind a default.
  #[arg(long, env = "XVFS_DEV_TOKEN", hide_env_values = true)]
  dev_token: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
  let args = Args::parse();
  observability::init_tracing();
  service::install_metrics();

  Server::check_pinned_versions()?;

  let catalog_path = args.state_dir.join("catalog.sqlite");
  let catalog = Arc::new(Catalog::open(&catalog_path)?);
  tracing::info!(catalog = %catalog_path.display(), "catalog opened");

  let key = match &args.capability_key {
    Some(hex) => {
      let bytes = decode_hex(hex)?;
      CapabilityKey::new(bytes)?
    }
    None => {
      tracing::warn!(
        "no --capability-key supplied; generating an ephemeral one. Every \
         outstanding mount capability becomes invalid on restart, which will break \
         live mounts. Supply a persistent key outside local development."
      );
      CapabilityKey::generate()?
    }
  };

  let dev_subject = SubjectId::parse("dev")?;
  let (authenticator, policy) = match &args.dev_token {
    Some(token) => {
      tracing::warn!(
        "using the development static-token authenticator; M1.5's OIDC integration \
         is a declared seam and this is not production-safe"
      );
      let auth = Arc::new(StaticTokens::new().with_token(token, dev_subject.clone()));
      let policy = Arc::new(AllowList::new().allow_all_repositories(&dev_subject));
      (
        auth as Arc<dyn xvfs_server::auth::Authenticator>,
        policy as Arc<dyn xvfs_server::auth::RepositoryPolicy>,
      )
    }
    None => {
      // Fails closed. A server with no authenticator must serve nothing rather than
      // everything, and saying so at startup is better than a confusing 401 later.
      tracing::error!(
        "no authenticator configured. Supply --dev-token for local development, or \
         wire a real identity provider into the Authenticator seam."
      );
      (
        Arc::new(StaticTokens::new()) as Arc<dyn xvfs_server::auth::Authenticator>,
        Arc::new(xvfs_server::auth::policy::DenyAll)
          as Arc<dyn xvfs_server::auth::RepositoryPolicy>,
      )
    }
  };

  let index_path = args.state_dir.join("search.sqlite");
  let server = Arc::new(
    Server::new(catalog, authenticator, policy, key, LeasePolicy::adr_0006())
      .with_search_index(&index_path)?,
  );
  tracing::info!(index = %index_path.display(), "search index opened");

  for spec in &args.imports {
    let (id, path) = spec
      .split_once('=')
      .ok_or_else(|| anyhow::anyhow!("--import expects ID=PATH, got {spec:?}"))?;
    import_repository(&server, id, std::path::Path::new(path))?;
  }

  // Before listening, not after.
  server.recover().await?;
  tracing::info!("reconciliation complete");

  let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
  tokio::spawn(Arc::clone(&server).run_lease_sweeper(shutdown_rx.clone()));

  let http = tokio::net::TcpListener::bind(&args.http_addr).await?;
  // One listener for blobs, health, webhooks, and the Git gateway. A clone URL
  // is `http://<http-addr>/v1/repos/<repository-id>`.
  tracing::info!(addr = %args.http_addr, "http and git smart-HTTP listening");
  let http_router = server.http_router();
  let mut http_shutdown = shutdown_rx.clone();
  let http_task = tokio::spawn(async move {
    axum::serve(http, http_router)
      .with_graceful_shutdown(async move {
        let _ = http_shutdown.changed().await;
      })
      .await
  });

  let grpc_addr: std::net::SocketAddr = args.grpc_addr.parse()?;
  tracing::info!(addr = %grpc_addr, "grpc listening");
  let mut grpc_shutdown = shutdown_rx.clone();
  let grpc_task = tokio::spawn(
    tonic::transport::Server::builder()
      .add_service(xvfs_proto::SnapshotServiceServer::new(
        server.snapshot_api(),
      ))
      .add_service(xvfs_proto::SearchServiceServer::new(server.search_api()))
      .serve_with_shutdown(grpc_addr, async move {
        let _ = grpc_shutdown.changed().await;
      }),
  );

  // Unmount and lease release are the client's job; a server shutdown must not
  // release leases, because the mounts are still live and their commits still need
  // to be reachable when the server returns.
  tokio::signal::ctrl_c().await?;
  tracing::info!("shutting down");
  let _ = shutdown_tx.send(true);
  let _ = http_task.await;
  let _ = grpc_task.await;
  Ok(())
}

/// Register and activate one repository, tolerating a repeat run.
///
/// Activation applies ADR 0001's format gate, so an unsupported mirror is refused
/// here with a stated reason rather than at the first read.
fn import_repository(server: &Server, id: &str, path: &std::path::Path) -> anyhow::Result<()> {
  use xvfs_server::catalog::repositories::NewRepository;
  use xvfs_types::{DisplayName, RepositoryId};

  let repository_id = RepositoryId::parse(id)?;
  let path = path
    .canonicalize()
    .map_err(|e| anyhow::anyhow!("cannot resolve {}: {e}", path.display()))?;

  if server.catalog.get_repository(&repository_id)?.is_none() {
    // The on-disk format decides the algorithm; the catalog records what was found
    // rather than asserting a default that activation would then contradict.
    let format = xvfs_git::read_format(&path)?;
    server.catalog.create_repository(&NewRepository {
      repository_id: repository_id.clone(),
      display_name: DisplayName::parse(id)?,
      repo_path: path.clone(),
      algorithm: format.algorithm,
      upstream_url: None,
      credential_ref: None,
    })?;
  }
  match server.registry.activate(&repository_id) {
    Ok(record) => {
      tracing::info!(
        repository_id = %record.repository_id,
        algorithm = %record.algorithm,
        "repository imported"
      );
      Ok(())
    }
    Err(e) => {
      tracing::error!(repository_id = %repository_id, error = %e, "repository refused");
      Err(e.into())
    }
  }
}

fn decode_hex(s: &str) -> anyhow::Result<Vec<u8>> {
  if !s.len().is_multiple_of(2) {
    anyhow::bail!("capability key must be an even number of hex characters");
  }
  let mut out = Vec::with_capacity(s.len() / 2);
  let bytes = s.as_bytes();
  for pair in bytes.chunks(2) {
    let hi = (pair[0] as char)
      .to_digit(16)
      .ok_or_else(|| anyhow::anyhow!("capability key is not hex"))?;
    let lo = (pair[1] as char)
      .to_digit(16)
      .ok_or_else(|| anyhow::anyhow!("capability key is not hex"))?;
    out.push(((hi << 4) | lo) as u8);
  }
  Ok(out)
}
