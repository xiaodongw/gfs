//! The server's request surfaces and the process that hosts them.
//!
//! Two listeners on two ports rather than one multiplexed port. gRPC needs HTTP/2
//! with prior knowledge and the HTTP surface serves ordinary HTTP/1.1 clients and
//! CDNs; multiplexing them means sniffing the protocol per connection, which is a
//! source of subtle failures with proxies in front. Two ports is honest and costs a
//! line of configuration.

pub mod http;
pub mod search;
pub mod snapshot;

use std::sync::Arc;

use xvfs_types::error::{ErrorCode, XvfsError};
use xvfs_types::LeasePolicy;

use crate::auth::{Authorizer, CapabilityKey};
use crate::catalog::Catalog;
use crate::locks::RepositoryLocks;
use crate::mounts::MountManager;
use crate::registry::Registry;

static PROMETHEUS: std::sync::OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
  std::sync::OnceLock::new();

/// Install the Prometheus recorder, once per process.
pub fn install_metrics() {
  if PROMETHEUS.get().is_some() {
    return;
  }
  let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
  let handle = recorder.handle();
  // A failure here means another recorder is already installed, which in a test
  // process is normal and not worth failing over.
  if metrics::set_global_recorder(recorder).is_ok() {
    let _ = PROMETHEUS.set(handle);
  }
}

pub fn prometheus_handle() -> Option<&'static metrics_exporter_prometheus::PrometheusHandle> {
  PROMETHEUS.get()
}

/// Everything the request surfaces need, assembled once.
pub struct Server {
  pub catalog: Arc<Catalog>,
  pub registry: Arc<Registry>,
  pub mounts: Arc<MountManager>,
  pub authz: Arc<Authorizer>,
  pub search: Arc<crate::search::IndexManager>,
  pub policy: LeasePolicy,
}

impl std::fmt::Debug for Server {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Server")
      .field("policy", &self.policy)
      .finish_non_exhaustive()
  }
}

impl Server {
  /// Assemble the server from a catalog and an authorization configuration.
  ///
  /// The capability key is shared between the mount manager, which mints, and the
  /// authorizer, which verifies. Passing one key rather than two removes the
  /// failure where they are configured differently and every capability is rejected
  /// as forged.
  pub fn new(
    catalog: Arc<Catalog>,
    authenticator: Arc<dyn crate::auth::Authenticator>,
    policy: Arc<dyn crate::auth::RepositoryPolicy>,
    key: CapabilityKey,
    lease_policy: LeasePolicy,
  ) -> Self {
    let registry = Arc::new(Registry::new(Arc::clone(&catalog)));
    let authz = Arc::new(Authorizer::new(
      authenticator,
      policy,
      Arc::clone(&registry),
      key.clone(),
      lease_policy,
    ));
    let mounts = Arc::new(MountManager::new(
      Arc::clone(&catalog),
      Arc::clone(&registry),
      Arc::new(RepositoryLocks::new()),
      lease_policy,
      key,
    ));
    // An in-memory index by default. The search index is derived data that is
    // rebuildable from Git, so a server that has not been told where to keep it
    // is *usable* rather than broken -- it simply re-prepares after a restart.
    // `with_search_index` gives it a home; the binary always does.
    let store = Arc::new(
      xvfs_search::SearchStore::open_in_memory().expect("an in-memory search index always opens"),
    );
    let search = Arc::new(crate::search::IndexManager::new(
      store,
      Arc::clone(&registry),
    ));

    Server {
      catalog,
      registry,
      mounts,
      authz,
      search,
      policy: lease_policy,
    }
  }

  /// Keep the search index on disk instead of in memory.
  ///
  /// Called before the server is shared. Persisting it does not change any
  /// answer -- every manifest and posting list is a function of a commit -- it
  /// changes how much work a restart costs.
  pub fn with_search_index(mut self, path: &std::path::Path) -> Result<Self, XvfsError> {
    let store = Arc::new(xvfs_search::SearchStore::open(path)?);
    self.search = Arc::new(crate::search::IndexManager::new(
      store,
      Arc::clone(&self.registry),
    ));
    Ok(self)
  }

  pub fn snapshot_api(&self) -> snapshot::SnapshotApi {
    snapshot::SnapshotApi {
      registry: Arc::clone(&self.registry),
      catalog: Arc::clone(&self.catalog),
      mounts: Arc::clone(&self.mounts),
      authz: Arc::clone(&self.authz),
      search: Arc::clone(&self.search),
    }
  }

  pub fn search_api(&self) -> search::SearchApi {
    search::SearchApi {
      registry: Arc::clone(&self.registry),
      authz: Arc::clone(&self.authz),
      index: Arc::clone(&self.search),
    }
  }

  pub fn http_router(&self) -> axum::Router {
    http::router(http::HttpState {
      registry: Arc::clone(&self.registry),
      catalog: Arc::clone(&self.catalog),
      authz: Arc::clone(&self.authz),
    })
  }

  /// Reconcile leases and refs after a restart.
  ///
  /// Run **before** accepting traffic. A `PREPARING` lease left by a crashed
  /// process must be resolved before a new mount can collide with its anchor, and an
  /// `ACTIVE` lease with a missing anchor must be repaired before its daemon's next
  /// read.
  pub async fn recover(&self) -> Result<(), XvfsError> {
    let outcome = self.mounts.reconcile().await?;
    if !outcome.abandoned.is_empty()
      || !outcome.repaired.is_empty()
      || !outcome.orphaned_anchors.is_empty()
    {
      tracing::warn!(
        abandoned = outcome.abandoned.len(),
        repaired = outcome.repaired.len(),
        orphaned_anchors = outcome.orphaned_anchors.len(),
        "restart reconciliation changed lease state"
      );
    }

    // Ref reconciliation recovers a webhook missed while the process was down.
    for record in self.catalog.list_repositories()? {
      if !record.state.is_servable() {
        continue;
      }
      let Ok(repo) = self.registry.repository(&record.repository_id) else {
        continue;
      };
      let actual = repo.visible_refs().await?;
      let catalog = Arc::clone(&self.catalog);
      let id = record.repository_id.clone();
      let changes = tokio::task::spawn_blocking(move || catalog.reconcile_refs(&id, &actual))
        .await
        .map_err(crate::util::join_error)??;
      if !changes.is_empty() {
        tracing::info!(
          repository_id = %record.repository_id,
          changes = changes.len(),
          "reconciled ref state after restart"
        );
      }
    }
    Ok(())
  }

  /// Run the lease sweep on a timer until cancelled.
  ///
  /// The interval is the heartbeat interval rather than something shorter: a sweep
  /// only has work when a lease has crossed a deadline, and the deadlines are
  /// measured in minutes.
  pub async fn run_lease_sweeper(self: Arc<Self>, shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut shutdown = shutdown;
    let mut ticker = tokio::time::interval(self.policy.heartbeat_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
      tokio::select! {
        _ = ticker.tick() => {
          match self.mounts.sweep().await {
            Ok(outcome) => {
              metrics::gauge!(crate::observability::metric::LEASES_IN_GRACE)
                .set(outcome.in_grace.len() as f64);
            }
            Err(e) => tracing::error!(error = %e, "lease sweep failed"),
          }
        }
        _ = shutdown.changed() => {
          if *shutdown.borrow() {
            return;
          }
        }
      }
    }
  }

  /// Assert the pinned tool versions at startup.
  ///
  /// ADR 0001 makes the supported-repository-format boundary a property of the exact
  /// libgit2 build, so a mismatch changes which repositories the server accepts. It
  /// is refused at startup rather than discovered when a mirror is rejected.
  pub fn check_pinned_versions() -> Result<(), XvfsError> {
    const EXPECTED_LIBGIT2: &str = "1.9.6";
    let actual = xvfs_git::format::libgit2_version();
    if actual != EXPECTED_LIBGIT2 {
      return Err(XvfsError::new(
        ErrorCode::FailedPrecondition,
        format!(
          "linked libgit2 is {actual} but ADR 0001 pins {EXPECTED_LIBGIT2}; the \
           supported-repository-format boundary is a property of that exact build"
        ),
      ));
    }
    tracing::info!(libgit2 = actual, "pinned versions verified");
    Ok(())
  }
}
