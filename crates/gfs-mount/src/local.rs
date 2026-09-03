//! Local mode: a pinned commit served from a clone on this machine.
//!
//! The use case is a developer who already has a full clone of a monorepo and
//! wants one working tree per change. `git worktree add` copies the tree every
//! time — seconds and hundreds of megabytes on vscode, more on a real monorepo.
//! The mount already presents a commit lazily; this module lets it do so from
//! the clone's own object database, with no server, no lease, and nothing on
//! `PATH`.
//!
//! # What is local and what is not
//!
//! Everything. The tree, blob bytes, the shipped index, the ref view, history,
//! diff, blame, and search all come from `gfs-git` over the clone. The workspace
//! borrows the clone's object store outright — `objects/info/alternates` names
//! its `objects` directory, which is what `git worktree` does — so there is no
//! projection, no block cache, and no hydration budget: nothing crosses a
//! network, and the disk the bytes would be copied to is the disk they are
//! already on.
//!
//! # The pin is still a pin
//!
//! The selector is resolved once, at mount time, and the workspace never moves
//! with the branch. A lease anchor under `refs/gfs/mounts/` is written into the
//! clone so `git gc` there cannot prune the commit a workspace is standing on;
//! it is removed at unmount. That is the one thing local mode writes into the
//! clone, and it is the same reserved-namespace ref the server writes.
//!
//! # Blobs are served from memory
//!
//! The verified on-disk cache exists to make a network fetch happen once. Here a
//! read inflates the blob from the pack, holds it for the life of the
//! descriptor, and keeps a bounded LRU of recently inflated blobs so a build
//! re-opening the same headers thousands of times pays the inflate once. No
//! second copy of any file is written to disk.
//!
//! # Search is a scan, not an index
//!
//! The server answers search from a trigram index it builds per snapshot. Local
//! mode walks the tree and scans every eligible blob straight from the pack,
//! across as many threads as the handle pool allows, with the same scanner the
//! overlay half already uses. No index means no `SNAPSHOT_BUILDING`; the answer
//! reports coverage and truncation exactly as the server's does.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gfs_git::repository::{AsyncRepository, DiffRequest, LogOptions, WalkEntry};
use gfs_git::Libgit2Repository;
use gfs_search::{
  Completion, CorpusPolicy, Coverage, ExecutionStatus, IgnoreRules, LocalBudget, LocalOutcome,
  LocalPath, Match, Query, SearchOutcome, SearchResult, TruncationReason,
};
use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::{
  limits, BytePath, CommitMeta, HashAlgorithm, MountId, ObjectId, RepositoryId, RevisionExpression,
  Timestamp, TreeEntryInfo, RESERVED_REF_PREFIX,
};

use crate::source::{
  Blame, DiffQuery, DirectoryPage, LogQuery, MountBinding, RevDiff, SnapshotSource, TreePage,
};

/// Bytes of inflated blobs kept in memory per clone.
///
/// A build's hot set — the headers and modules it opens again and again — fits
/// comfortably; a whole-tree read streams through it. Sized so that ten mounted
/// clones on one host stay well inside a developer machine's memory.
const BLOB_MEMORY_BYTES: u64 = 256 * 1024 * 1024;

/// How long a local search may run before it reports itself truncated.
///
/// Generous against the overlay scanner's five seconds, because this is the
/// *whole* tree and a monorepo's worth of source at pack-inflate speed is tens
/// of seconds on the worst case; and finite, because an answer that never comes
/// is worse than one that says it stopped.
const SEARCH_TIME_BUDGET: Duration = Duration::from_secs(120);

/// One clone opened by this host, shared by every workspace mounted from it.
pub struct LocalRepository {
  clone: PathBuf,
  repository_id: RepositoryId,
  objects: PathBuf,
  repo: AsyncRepository,
  blobs: Mutex<BlobMemory>,
}

impl std::fmt::Debug for LocalRepository {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("LocalRepository")
      .field("clone", &self.clone)
      .field("repository_id", &self.repository_id)
      .finish_non_exhaustive()
  }
}

impl LocalRepository {
  /// Open a clone. Blocking: libgit2 opens its handles here.
  pub fn open(clone: &Path) -> Result<Arc<Self>, GfsError> {
    let clone = clone.canonicalize().map_err(|e| {
      GfsError::new(
        ErrorCode::NotFound,
        format!("no clone at {}: {e}", clone.display()),
      )
    })?;
    let repo = Libgit2Repository::open(
      &clone,
      limits::DEFAULT_REPO_HANDLES,
      limits::DEFAULT_TREE_CACHE_ENTRIES * 512,
    )?;
    let objects = repo.objects_directory()?;
    let repository_id = repository_id_for(&clone);
    Ok(Arc::new(LocalRepository {
      clone,
      repository_id,
      objects,
      repo: AsyncRepository::new(Arc::new(repo), limits::DEFAULT_REPO_HANDLES),
      blobs: Mutex::new(BlobMemory::new(BLOB_MEMORY_BYTES)),
    }))
  }

  pub fn clone_path(&self) -> &Path {
    &self.clone
  }

  /// The identifier the blob cache, the overlay binding, and `mount.json` use
  /// for this clone. Derived from the canonical path so two workspaces of one
  /// clone agree and two clones never collide.
  pub fn repository_id(&self) -> &RepositoryId {
    &self.repository_id
  }

  /// The clone's object store, for `objects/info/alternates`.
  pub fn objects_directory(&self) -> &Path {
    &self.objects
  }

  /// Pin a selector: resolve it, anchor the commit in the clone, and build the
  /// source that reads through it.
  pub async fn pin(
    self: &Arc<Self>,
    selector: &str,
    workspace: &Path,
  ) -> Result<LocalPin, GfsError> {
    let expression = RevisionExpression::parse(selector, HashAlgorithm::Sha1)?;
    let resolved = self.repo.resolve_expression(expression).await?;
    let mount_id = new_mount_id(workspace);
    let anchor = format!("{RESERVED_REF_PREFIX}mounts/{}", mount_id.as_str());
    {
      let anchor = anchor.clone();
      let commit = resolved.commit.clone();
      self
        .repo
        .run(move |r| r.create_lease_anchor(&anchor, &commit))
        .await?;
    }
    let source = Arc::new(LocalSource {
      repo: Arc::clone(self),
      binding: MountBinding {
        repository_id: self.repository_id.clone(),
        commit: resolved.commit.clone(),
        algorithm: HashAlgorithm::Sha1,
        snapshot_time: resolved.snapshot_time,
      },
      anchor,
    });
    Ok(LocalPin {
      source,
      mount_id,
      commit: resolved.commit,
      tree: resolved.tree,
      ref_name: resolved.ref_name,
      snapshot_time: resolved.snapshot_time,
    })
  }

  /// A blob's bytes, inflated once and shared.
  async fn blob(&self, oid: &ObjectId) -> Result<Arc<Vec<u8>>, GfsError> {
    let key = oid.to_hex();
    if let Some(bytes) = self.blobs.lock().expect("blob memory").get(&key) {
      return Ok(bytes);
    }
    let bytes = Arc::new(self.repo.read_blob(oid.clone()).await?);
    self
      .blobs
      .lock()
      .expect("blob memory")
      .insert(key, Arc::clone(&bytes));
    Ok(bytes)
  }
}

/// What pinning a selector produced.
#[derive(Debug)]
pub struct LocalPin {
  pub source: Arc<LocalSource>,
  pub mount_id: MountId,
  pub commit: ObjectId,
  pub tree: ObjectId,
  pub ref_name: Option<String>,
  pub snapshot_time: Timestamp,
}

/// `local-<digest of the canonical clone path>`.
fn repository_id_for(clone: &Path) -> RepositoryId {
  use sha1::Digest;
  use std::os::unix::ffi::OsStrExt;
  let digest = sha1::Sha1::digest(clone.as_os_str().as_bytes());
  let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
  RepositoryId::parse(&format!("local-{hex}")).expect("a hex digest is a valid repository id")
}

/// A mount id nobody else on this host will produce: the workspace path, the
/// clock, and the process, digested. Interpolated into a ref name, so it is
/// limited to the characters `MountId` allows.
fn new_mount_id(workspace: &Path) -> MountId {
  use sha1::Digest;
  use std::os::unix::ffi::OsStrExt;
  let mut hasher = sha1::Sha1::new();
  hasher.update(workspace.as_os_str().as_bytes());
  let now = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_nanos())
    .unwrap_or_default();
  hasher.update(now.to_le_bytes());
  hasher.update(std::process::id().to_le_bytes());
  let hex: String = hasher
    .finalize()
    .iter()
    .take(10)
    .map(|b| format!("{b:02x}"))
    .collect();
  MountId::parse(&format!("local-{hex}")).expect("a hex digest is a valid mount id")
}

// ---------------------------------------------------------------------------
// The source
// ---------------------------------------------------------------------------

/// One pin's view of a local clone.
pub struct LocalSource {
  repo: Arc<LocalRepository>,
  binding: MountBinding,
  /// The ref holding the pinned commit reachable, deleted on release.
  anchor: String,
}

impl std::fmt::Debug for LocalSource {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("LocalSource")
      .field("clone", &self.repo.clone)
      .field("binding", &self.binding)
      .finish_non_exhaustive()
  }
}

fn page_limit(requested: u32, default: usize, max: usize) -> usize {
  if requested == 0 {
    default
  } else {
    (requested as usize).min(max)
  }
}

fn page_after(token: Vec<u8>) -> Option<Vec<u8>> {
  (!token.is_empty()).then_some(token)
}

#[async_trait::async_trait]
impl SnapshotSource for LocalSource {
  fn binding(&self) -> &MountBinding {
    &self.binding
  }

  fn leased(&self) -> bool {
    false
  }

  fn serves_blobs_in_memory(&self) -> bool {
    true
  }

  async fn get_entry_at(
    &self,
    commit: &ObjectId,
    path: &BytePath,
    _want_blob_ticket: bool,
  ) -> Result<Option<TreeEntryInfo>, GfsError> {
    self.repo.repo.entry(commit.clone(), path.clone()).await
  }

  async fn list_directory_at(
    &self,
    commit: &ObjectId,
    path: &BytePath,
    page_token: Vec<u8>,
    page_size: u32,
    _want_blob_tickets: bool,
  ) -> Result<DirectoryPage, GfsError> {
    let limit = page_limit(
      page_size,
      limits::DEFAULT_DIRECTORY_PAGE_SIZE,
      limits::MAX_DIRECTORY_PAGE_SIZE,
    );
    let page = self
      .repo
      .repo
      .list_directory(commit.clone(), path.clone(), page_after(page_token), limit)
      .await?;
    Ok(DirectoryPage {
      entries: page.entries,
      next_page_token: page.next_page_token.unwrap_or_default(),
    })
  }

  async fn list_tree(
    &self,
    root: &BytePath,
    page_token: Vec<u8>,
    max_entries: u32,
  ) -> Result<TreePage, GfsError> {
    let limit = page_limit(
      max_entries,
      limits::DEFAULT_TREE_PAGE_ENTRIES,
      limits::MAX_TREE_PAGE_ENTRIES,
    );
    let page = self
      .repo
      .repo
      .list_tree(
        self.binding.commit.clone(),
        root.clone(),
        page_after(page_token),
        limit,
      )
      .await?;
    Ok(TreePage {
      entries: page.entries,
      directories: page.directories,
      next_page_token: page.next_page_token.unwrap_or_default(),
    })
  }

  async fn batch_get_entry(
    &self,
    paths: &[BytePath],
    _want_blob_tickets: bool,
  ) -> Result<Vec<Option<TreeEntryInfo>>, GfsError> {
    let looked_up = self
      .repo
      .repo
      .batch_entries(self.binding.commit.clone(), paths.to_vec())
      .await?;
    Ok(
      looked_up
        .into_iter()
        .map(|result| result.ok().flatten())
        .collect(),
    )
  }

  async fn read_blob(&self, oid: &ObjectId, _ticket: &str) -> Result<Vec<u8>, GfsError> {
    Ok(self.repo.blob(oid).await?.as_ref().clone())
  }

  async fn read_blob_shared(
    &self,
    oid: &ObjectId,
    _ticket: &str,
  ) -> Result<Arc<Vec<u8>>, GfsError> {
    self.repo.blob(oid).await
  }

  fn forget_blob(&self, oid: &ObjectId) {
    self
      .repo
      .blobs
      .lock()
      .expect("blob memory")
      .remove(&oid.to_hex());
  }

  async fn commit_index(&self, commit: &ObjectId) -> Result<Vec<u8>, GfsError> {
    self
      .repo
      .repo
      .index_for_commit(commit.clone(), self.binding.snapshot_time)
      .await
  }

  async fn get_commit_at(&self, commit: &ObjectId) -> Result<CommitMeta, GfsError> {
    self.repo.repo.read_commit(commit.clone()).await
  }

  async fn log(
    &self,
    from: Option<&ObjectId>,
    options: &LogQuery,
  ) -> Result<(Vec<CommitMeta>, bool), GfsError> {
    self
      .repo
      .repo
      .log(
        from.unwrap_or(&self.binding.commit).clone(),
        LogOptions {
          skip: options.skip as usize,
          limit: page_limit(
            options.limit,
            limits::DEFAULT_LOG_LIMIT,
            limits::MAX_LOG_LIMIT,
          ),
          first_parent: options.first_parent,
          paths: options.paths.clone(),
        },
      )
      .await
  }

  async fn resolve(&self, selector: &str) -> Result<ObjectId, GfsError> {
    let expression = RevisionExpression::parse(selector, self.binding.algorithm)?;
    Ok(self.repo.repo.resolve_expression(expression).await?.commit)
  }

  async fn list_refs(&self) -> Result<Vec<gfs_types::RefTarget>, GfsError> {
    self.repo.repo.visible_ref_targets().await
  }

  async fn diff_commits(
    &self,
    from: Option<&ObjectId>,
    to: &ObjectId,
    query: &DiffQuery,
  ) -> Result<RevDiff, GfsError> {
    let output = self
      .repo
      .repo
      .diff(DiffRequest {
        from: from.cloned(),
        to: to.clone(),
        paths: query.paths.clone(),
        format: query.format,
        context_lines: query
          .context_lines
          .unwrap_or(gfs_types::diff::DEFAULT_CONTEXT_LINES)
          .min(limits::MAX_DIFF_CONTEXT_LINES),
        max_bytes: if query.max_bytes == 0 {
          limits::DEFAULT_DIFF_BYTES
        } else {
          (query.max_bytes as usize).min(limits::MAX_DIFF_BYTES)
        },
      })
      .await?;
    Ok(RevDiff {
      rendered: output.rendered,
      files: output.files,
      truncated: output.truncated,
    })
  }

  async fn blame(&self, commit: &ObjectId, path: &BytePath) -> Result<Blame, GfsError> {
    let output = self
      .repo
      .repo
      .blame(
        commit.clone(),
        path.clone(),
        limits::MAX_SEARCHABLE_BLOB_BYTES,
      )
      .await?;
    Ok(Blame {
      hunks: output.hunks,
      content: output.content,
      truncated: output.truncated,
    })
  }

  /// A scan needs no preparation.
  async fn prepare_snapshot(&self) -> Result<bool, GfsError> {
    Ok(true)
  }

  async fn search(&self, query: &Query, max_results: u32) -> Result<SearchOutcome, GfsError> {
    let started = Instant::now();
    let commit = self.binding.commit.clone();
    let entries = self.searchable_entries(&commit, &query.scope).await?;
    let repo = Arc::clone(&self.repo);
    let query = query.clone();
    tokio::task::spawn_blocking(move || scan(&repo, &commit, &query, entries, max_results, started))
      .await
      .map_err(|e| GfsError::internal(format!("the local search task failed: {e}")))?
  }

  async fn renew_mount(&self, _mount_id: &MountId) -> Result<Timestamp, GfsError> {
    Ok(far_future())
  }

  async fn release_mount(&self, _mount_id: &MountId) -> Result<(), GfsError> {
    let anchor = self.anchor.clone();
    self
      .repo
      .repo
      .run(move |r| r.delete_lease_anchor(&anchor))
      .await
  }
}

/// A lease expiry that never arrives, for the monitor of an unleased pin.
pub fn far_future() -> Timestamp {
  let now = Timestamp::now();
  Timestamp::new(now.secs + 100 * 365 * 24 * 3600, 0)
}

impl LocalSource {
  /// Every blob-bearing entry a search over `scope` could read.
  ///
  /// A scope naming a directory walks only that subtree; one naming a file
  /// yields that file; one naming nothing yields nothing. The whole tree is
  /// walked only for the whole-tree question.
  async fn searchable_entries(
    &self,
    commit: &ObjectId,
    scope: &[u8],
  ) -> Result<Vec<WalkEntry>, GfsError> {
    let root = if scope.is_empty() {
      BytePath::root()
    } else {
      let path = BytePath::new(scope.to_vec());
      match self.repo.repo.entry(commit.clone(), path.clone()).await? {
        None => return Ok(Vec::new()),
        Some(entry) if entry.kind.is_dir_like() => path,
        Some(entry) => {
          return Ok(vec![WalkEntry {
            path,
            mode: entry.mode,
            oid: entry.oid,
            size: entry.size,
          }])
        }
      }
    };
    self.repo.repo.walk_tree(commit.clone(), root).await
  }
}

/// The scan: every regular file in `entries`, across as many threads as the
/// handle pool has handles, each running the overlay's own scanner over its
/// slice of the sorted path list and reading straight from the pack.
fn scan(
  repo: &LocalRepository,
  commit: &ObjectId,
  query: &Query,
  entries: Vec<WalkEntry>,
  max_results: u32,
  started: Instant,
) -> Result<SearchOutcome, GfsError> {
  let policy = CorpusPolicy::default();
  let mut files: Vec<(LocalPath, ObjectId)> = entries
    .into_iter()
    .filter(|e| matches!(e.mode, 0o100644 | 0o100755))
    .map(|e| {
      (
        LocalPath {
          path: e.path.as_bytes().to_vec(),
          tracked_in_base: true,
          size: e.size,
        },
        e.oid,
      )
    })
    .collect();
  files.sort_by(|a, b| a.0.path.cmp(&b.0.path));
  let candidates = files.len() as u64;

  let max_results = (max_results.max(1)) as usize;
  let budget = LocalBudget {
    max_time: SEARCH_TIME_BUDGET,
    max_bytes_read: u64::MAX,
    max_results,
  };
  let threads = std::thread::available_parallelism()
    .map(|n| n.get())
    .unwrap_or(1)
    .clamp(1, limits::DEFAULT_REPO_HANDLES);
  let chunk = files.len().div_ceil(threads).max(1);
  let ignore = IgnoreRules::default();
  let blocking = repo.repo.blocking();

  // Contiguous slices of one sorted list, so concatenating the outcomes in
  // slice order is already in path order.
  let outcomes: Vec<Result<LocalOutcome, GfsError>> = std::thread::scope(|scope| {
    let handles: Vec<_> = files
      .chunks(chunk)
      .map(|slice| {
        let (paths, oids): (Vec<&LocalPath>, Vec<&ObjectId>) =
          slice.iter().map(|(p, o)| (p, o)).unzip();
        let by_path: HashMap<&[u8], &ObjectId> = paths
          .iter()
          .zip(oids.iter())
          .map(|(p, o)| (p.path.as_slice(), *o))
          .collect();
        let owned: Vec<LocalPath> = paths.into_iter().cloned().collect();
        let ignore = &ignore;
        let policy = &policy;
        scope.spawn(move || {
          gfs_search::search_local(&owned, query, policy, ignore, &budget, true, |local| {
            let Some(oid) = by_path.get(local.path.as_slice()) else {
              return Ok(None);
            };
            match blocking.read_blob(oid) {
              Ok(bytes) => Ok(Some(bytes)),
              // Missing from the pack: a gap, not a miss.
              Err(e) if e.code == ErrorCode::NotFound => Ok(None),
              Err(e) => Err(e),
            }
          })
        })
      })
      .collect();
    handles
      .into_iter()
      .map(|h| {
        h.join()
          .unwrap_or_else(|_| Err(GfsError::internal("a search thread panicked")))
      })
      .collect()
  });

  let mut matches: Vec<Match> = Vec::new();
  let mut excluded: BTreeMap<String, u64> = BTreeMap::new();
  let mut eligible_paths = 0u64;
  let mut bytes_read = 0u64;
  let mut truncated = false;
  for outcome in outcomes {
    let outcome = outcome?;
    matches.extend(outcome.matches);
    for (reason, count) in outcome.excluded {
      *excluded.entry(reason.as_str().to_owned()).or_default() += count;
    }
    eligible_paths += outcome.eligible_paths;
    bytes_read += outcome.bytes_read;
    truncated |= outcome.truncated;
  }
  let over_limit = matches.len() > max_results;
  if over_limit {
    matches.truncate(max_results);
    truncated = true;
  }

  Ok(SearchOutcome::Completed(SearchResult {
    matches,
    completion: Completion {
      execution_status: if truncated {
        ExecutionStatus::Truncated
      } else {
        ExecutionStatus::Complete
      },
      truncation: truncated.then_some(if over_limit {
        TruncationReason::ResultLimit
      } else {
        TruncationReason::TimeBudget
      }),
      coverage: Coverage {
        scope: query.scope.clone(),
        eligible_paths,
        excluded,
        declared_exclusions: policy
          .declared_exclusions()
          .into_iter()
          .map(|r| r.as_str().to_owned())
          .collect(),
      },
      // No index: every answer is computed against the pack as it is.
      index_generation: 0,
      commit: commit.to_qualified(),
      stop_budget: truncated.then(|| {
        if over_limit {
          "the result limit".to_owned()
        } else {
          "the local scan's time budget".to_owned()
        }
      }),
      candidates_considered: candidates,
      bytes_read,
      elapsed_ms: started.elapsed().as_millis() as u64,
    },
  }))
}

// ---------------------------------------------------------------------------
// Blob memory
// ---------------------------------------------------------------------------

/// A bounded LRU of inflated blobs, keyed by object id.
struct BlobMemory {
  cap: u64,
  bytes: u64,
  tick: u64,
  entries: HashMap<String, (u64, Arc<Vec<u8>>)>,
  order: BTreeMap<u64, String>,
}

impl BlobMemory {
  fn new(cap: u64) -> Self {
    BlobMemory {
      cap,
      bytes: 0,
      tick: 0,
      entries: HashMap::new(),
      order: BTreeMap::new(),
    }
  }

  fn get(&mut self, key: &str) -> Option<Arc<Vec<u8>>> {
    let (tick, bytes) = self.entries.get_mut(key)?;
    self.order.remove(tick);
    self.tick += 1;
    *tick = self.tick;
    self.order.insert(self.tick, key.to_owned());
    Some(Arc::clone(bytes))
  }

  fn remove(&mut self, key: &str) {
    if let Some((tick, bytes)) = self.entries.remove(key) {
      self.order.remove(&tick);
      self.bytes -= bytes.len() as u64;
    }
  }

  fn insert(&mut self, key: String, bytes: Arc<Vec<u8>>) {
    let size = bytes.len() as u64;
    if size > self.cap || self.entries.contains_key(&key) {
      return;
    }
    while self.bytes + size > self.cap {
      let Some((_, oldest)) = self.order.pop_first() else {
        break;
      };
      if let Some((_, evicted)) = self.entries.remove(&oldest) {
        self.bytes -= evicted.len() as u64;
      }
    }
    self.tick += 1;
    self.order.insert(self.tick, key.clone());
    self.entries.insert(key, (self.tick, bytes));
    self.bytes += size;
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn blob_memory_evicts_least_recently_used_first() {
    let mut memory = BlobMemory::new(10);
    memory.insert("a".to_owned(), Arc::new(vec![0; 4]));
    memory.insert("b".to_owned(), Arc::new(vec![0; 4]));
    assert!(memory.get("a").is_some());
    // 4 + 4 + 4 > 10: the least recently used, `b`, goes.
    memory.insert("c".to_owned(), Arc::new(vec![0; 4]));
    assert!(memory.get("b").is_none());
    assert!(memory.get("a").is_some());
    assert!(memory.get("c").is_some());
    assert_eq!(memory.bytes, 8);
  }

  #[test]
  fn a_blob_larger_than_the_memory_is_not_kept() {
    let mut memory = BlobMemory::new(10);
    memory.insert("big".to_owned(), Arc::new(vec![0; 11]));
    assert!(memory.get("big").is_none());
    assert_eq!(memory.bytes, 0);
  }

  #[test]
  fn repository_ids_are_stable_and_distinct() {
    let a = repository_id_for(Path::new("/tmp/a"));
    let b = repository_id_for(Path::new("/tmp/b"));
    assert_eq!(a, repository_id_for(Path::new("/tmp/a")));
    assert_ne!(a, b);
    assert!(a.as_str().starts_with("local-"));
  }
}
