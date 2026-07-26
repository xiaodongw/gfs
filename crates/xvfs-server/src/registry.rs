//! Open repositories, keyed by repository ID.
//!
//! Sits between the catalog (which knows a repository exists and where it is) and
//! `xvfs-git` (which can read it). Two jobs:
//!
//! * hold one [`Libgit2Repository`] per repository, because opening one re-reads
//!   config and re-scans the object database, and a per-request open would pay that
//!   on every `getattr`;
//! * enforce that a repository is *servable* before anything reads it, so the
//!   lifecycle state machine in the catalog is not merely advisory.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use xvfs_git::{AsyncRepository, GitRepository, Libgit2Repository};
use xvfs_types::error::{ErrorCode, XvfsError};
use xvfs_types::{limits, RepositoryId};

use crate::catalog::{Catalog, RepositoryRecord};

pub struct Registry {
  catalog: Arc<Catalog>,
  open: RwLock<HashMap<RepositoryId, Arc<Libgit2Repository>>>,
  max_handles: usize,
  tree_cache_bytes: usize,
}

impl std::fmt::Debug for Registry {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let open = self.open.read().unwrap_or_else(|e| e.into_inner());
    f.debug_struct("Registry")
      .field("open", &open.len())
      .field("max_handles", &self.max_handles)
      .finish()
  }
}

impl Registry {
  pub fn new(catalog: Arc<Catalog>) -> Self {
    Registry {
      catalog,
      open: RwLock::new(HashMap::new()),
      max_handles: limits::DEFAULT_REPO_HANDLES,
      tree_cache_bytes: limits::DEFAULT_TREE_CACHE_ENTRIES * 512,
    }
  }

  pub fn with_limits(mut self, max_handles: usize, tree_cache_bytes: usize) -> Self {
    self.max_handles = max_handles.max(1);
    self.tree_cache_bytes = tree_cache_bytes;
    self
  }

  /// The record for a repository that may be served.
  ///
  /// A repository that exists but is not servable reports `NOT_FOUND`, not a
  /// distinct "quarantined" error. The reason is the M1 exit criterion about
  /// existence inference: a caller who may not read a repository must not be able
  /// to learn that it exists, and a distinct status for "exists but quarantined"
  /// would leak exactly that. Operators see the real state through the admin
  /// surface, where they are already authorized to know it.
  pub fn require_servable(&self, id: &RepositoryId) -> Result<RepositoryRecord, XvfsError> {
    let record = self
      .catalog
      .get_repository(id)?
      .ok_or_else(|| XvfsError::masked_denial("no such repository"))?;
    if !record.state.is_servable() {
      return Err(XvfsError::masked_denial("no such repository"));
    }
    Ok(record)
  }

  /// The blocking repository handle for a servable repository.
  pub fn blocking_repository(
    &self,
    id: &RepositoryId,
  ) -> Result<Arc<dyn GitRepository>, XvfsError> {
    Ok(self.opened(id)?)
  }

  /// The async, admission-controlled facade for a servable repository.
  pub fn repository(&self, id: &RepositoryId) -> Result<AsyncRepository, XvfsError> {
    let repo = self.opened(id)?;
    Ok(AsyncRepository::new(repo, self.max_handles))
  }

  fn opened(&self, id: &RepositoryId) -> Result<Arc<Libgit2Repository>, XvfsError> {
    if let Some(repo) = self
      .open
      .read()
      .unwrap_or_else(|e| e.into_inner())
      .get(id)
      .cloned()
    {
      return Ok(repo);
    }
    let record = self.require_servable(id)?;

    let mut open = self.open.write().unwrap_or_else(|e| e.into_inner());
    // Re-check under the write lock: two callers can miss the read path at once,
    // and opening twice would create two handle pools for one repository, doubling
    // the concurrency the pool bound is supposed to cap.
    if let Some(repo) = open.get(id).cloned() {
      return Ok(repo);
    }
    let repo = Arc::new(Libgit2Repository::open(
      &record.repo_path,
      self.max_handles,
      self.tree_cache_bytes,
    )?);
    open.insert(id.clone(), Arc::clone(&repo));
    Ok(repo)
  }

  /// Drop a cached handle, so the next access reopens.
  ///
  /// Needed after maintenance replaces packs, and after a repository leaves and
  /// re-enters the servable state.
  pub fn evict(&self, id: &RepositoryId) {
    self
      .open
      .write()
      .unwrap_or_else(|e| e.into_inner())
      .remove(id);
  }

  /// Open a repository and confirm its format, moving it from `CREATING` to
  /// `ACTIVE`.
  ///
  /// The format check is the ingest gate: ADR 0001 rejects `reftable`, SHA-256, and
  /// unrecognized extensions at creation rather than serving a partial view. A
  /// failure leaves the repository in `CREATING` with the reason recorded, so an
  /// operator sees why rather than finding an inert row.
  pub fn activate(&self, id: &RepositoryId) -> Result<RepositoryRecord, XvfsError> {
    let record = self
      .catalog
      .get_repository(id)?
      .ok_or_else(|| XvfsError::not_found("no such repository"))?;

    match Libgit2Repository::open(&record.repo_path, self.max_handles, self.tree_cache_bytes) {
      Ok(repo) => {
        let actual = repo.format().algorithm;
        if actual != record.algorithm {
          let reason = format!(
            "on-disk object format is {actual} but the catalog records {}",
            record.algorithm
          );
          self.catalog.set_repository_state(
            id,
            crate::catalog::RepositoryState::Quarantined,
            Some(&reason),
          )?;
          return Err(XvfsError::new(
            ErrorCode::UnsupportedRepositoryFormat,
            reason,
          ));
        }
        self
          .open
          .write()
          .unwrap_or_else(|e| e.into_inner())
          .insert(id.clone(), Arc::new(repo));
        self
          .catalog
          .set_repository_state(id, crate::catalog::RepositoryState::Active, None)?;
        self
          .catalog
          .get_repository(id)?
          .ok_or_else(|| XvfsError::not_found("no such repository"))
      }
      Err(e) => {
        // Quarantined rather than deleted: the mirror may be recoverable, and
        // ADR 0001's rejections are about refusing to *serve*, not about
        // discarding data.
        self.catalog.set_repository_state(
          id,
          crate::catalog::RepositoryState::Quarantined,
          Some(&e.message),
        )?;
        Err(e)
      }
    }
  }
}
