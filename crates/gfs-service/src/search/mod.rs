//! Snapshot preparation and the search index, on the server side.
//!
//! `gfs-search` owns the representation and is deliberately network-free and
//! synchronous. This module is the half that knows about Git, Tokio, and the
//! repository catalog: it walks trees through [`AsyncRepository`], runs the
//! blocking indexer inside the bounded pool, and turns the durable claim state
//! machine into an RPC answer.
//!
//! # Two ways to build a manifest, and the cheap one is the common one
//!
//! A commit whose **first parent** already has a READY manifest is built from
//! `diff_commits`, which ADR 0004 measured as ~4 (vscode) to ~39 (linux) changed
//! blobs per commit against ~94 000 unchanged entries. Everything else is a full
//! tree walk, fanned out one task per top-level directory so several libgit2
//! handles work at once under the pool's existing admission control.
//!
//! Both paths must describe the same tree, and an incremental build that drifted
//! from a full one would return *wrong search results* rather than slow ones, so
//! it is asserted twice: `manifest.rs` pins byte-for-byte equality of the two
//! constructions within one registry, and `tests/search_index.rs` compares
//! `(path, mode, blob OID)` across two independent indexes. The second is the
//! weaker claim on purpose — a blob *key* is an allocation within one registry,
//! so two stores legitimately number the same content differently.
//!
//! # Waiting is bounded, and "still building" is an answer
//!
//! `PrepareSnapshot` returns READY when preparation finishes inside the request
//! deadline, and BUILDING with an operation ID when it does not — the build keeps
//! running in a background task. ADR 0006 targets under 5 seconds to READY, so
//! the wait is short and the fallback is a real state rather than a timeout
//! error. A timeout error would be the wrong answer: nothing failed.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gfs_git::{AsyncRepository, GitRepository, TreeDelta};
use gfs_search::manifest::{Manifest, ManifestDelta, PathEntry};
use gfs_search::postings::{PostingBatch, PostingStore};
use gfs_search::snapshots::{Claim, PreparePolicy, Progress, SnapshotRecord, SnapshotStore};
use gfs_search::{
  BlobFact, BlobRegistry, BlobSource, Cancel, CorpusPolicy, GcReport, IngestBudget, SearchStore,
};
use gfs_types::error::GfsError;
use gfs_types::{BytePath, EntryKind, ObjectId, RepositoryId, SnapshotState};

use crate::registry::Registry;

/// How long `PrepareSnapshot` waits before answering BUILDING.
///
/// ADR 0006's target is under 5 seconds to READY. Waiting materially longer
/// would make a client's own deadline the thing that decides the answer, which
/// is how one slow repository turns into a wall of `DeadlineExceeded` errors
/// that say nothing about what is actually happening.
const PREPARE_WAIT: Duration = Duration::from_secs(5);

/// How often the waiter re-reads the durable record.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How many subtree walks run at once during a full build.
///
/// Matched to the repository handle pool rather than to the CPU count: the pool
/// is the real bound, and exceeding it would only queue tasks on a semaphore
/// while holding their own state alive.
const WALK_FANOUT: usize = gfs_types::limits::DEFAULT_REPO_HANDLES;

/// What preparation produced.
#[derive(Clone, Debug)]
pub enum PrepareOutcome {
  Ready(Box<SnapshotRecord>),
  /// Still running, in a background task. The ID identifies that build.
  Building {
    operation_id: String,
  },
  Failed {
    reason: String,
  },
}

impl PrepareOutcome {
  pub fn state(&self) -> SnapshotState {
    match self {
      PrepareOutcome::Ready(_) => SnapshotState::Ready,
      PrepareOutcome::Building { .. } => SnapshotState::Building,
      PrepareOutcome::Failed { .. } => SnapshotState::Failed,
    }
  }
}

/// The server's search index.
pub struct IndexManager {
  store: Arc<SearchStore>,
  registry: Arc<Registry>,
  corpus: CorpusPolicy,
  prepare_policy: PreparePolicy,
  ingest_budget: IngestBudget,
  operations: AtomicU64,
}

impl std::fmt::Debug for IndexManager {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("IndexManager").finish_non_exhaustive()
  }
}

impl IndexManager {
  pub fn new(store: Arc<SearchStore>, registry: Arc<Registry>) -> IndexManager {
    IndexManager {
      store,
      registry,
      corpus: CorpusPolicy::default(),
      prepare_policy: PreparePolicy::default(),
      ingest_budget: IngestBudget::default(),
      operations: AtomicU64::new(0),
    }
  }

  pub fn corpus_policy(&self) -> &CorpusPolicy {
    &self.corpus
  }

  pub fn snapshots(&self, repository: &RepositoryId) -> SnapshotStore {
    SnapshotStore::new(
      Arc::clone(&self.store),
      repository.clone(),
      self.prepare_policy,
    )
  }

  pub fn blobs(&self, repository: &RepositoryId) -> BlobRegistry {
    BlobRegistry::new(
      Arc::clone(&self.store),
      repository.clone(),
      self.corpus.clone(),
    )
  }

  /// Prepare a snapshot, waiting up to five seconds (ADR 0006's target to READY)
  /// for it.
  ///
  /// `retained` marks a snapshot kept by policy — a configured branch tip —
  /// rather than one that expires on a TTL.
  pub async fn prepare(
    self: &Arc<Self>,
    repository: &RepositoryId,
    commit: &ObjectId,
    retained: bool,
  ) -> Result<PrepareOutcome, GfsError> {
    let snapshots = self.snapshots(repository);
    let hex = commit.to_hex();
    let operation_id = format!(
      "prep-{}-{}",
      // Hex is ASCII, but slicing a `String` by byte index is a habit worth not
      // having in a codebase where most strings are paths that are not.
      hex.chars().take(8).collect::<String>(),
      self.operations.fetch_add(1, Ordering::Relaxed)
    );

    match snapshots.claim(commit, &operation_id)? {
      Claim::Ready(record) => return Ok(PrepareOutcome::Ready(record)),
      Claim::Failed { reason, .. } => return Ok(PrepareOutcome::Failed { reason }),
      Claim::Building { .. } => {}
      Claim::Claimed { .. } => {
        // Spawned rather than awaited inline: the build must survive this
        // request's deadline, or a client that gave up at four seconds would
        // leave a BUILDING row for the next caller to reclaim ten minutes later.
        let manager = Arc::clone(self);
        let repository = repository.clone();
        let commit = commit.clone();
        tokio::spawn(async move {
          let cancel = Cancel::new();
          if let Err(e) = manager.build(&repository, &commit, retained, cancel).await {
            tracing::warn!(
              repository_id = %repository,
              commit = %commit.to_qualified(),
              error = %e,
              "snapshot preparation failed"
            );
          }
        });
      }
    }

    self.wait_for(repository, commit, PREPARE_WAIT).await
  }

  /// Poll the durable record until it leaves BUILDING or the deadline passes.
  async fn wait_for(
    &self,
    repository: &RepositoryId,
    commit: &ObjectId,
    limit: Duration,
  ) -> Result<PrepareOutcome, GfsError> {
    let snapshots = self.snapshots(repository);
    let deadline = tokio::time::Instant::now() + limit;
    loop {
      match snapshots.get(commit)? {
        Some(record) => match record.state {
          SnapshotState::Ready => return Ok(PrepareOutcome::Ready(Box::new(record))),
          SnapshotState::Failed => {
            return Ok(PrepareOutcome::Failed {
              reason: record
                .failure_reason
                .unwrap_or_else(|| "snapshot preparation failed".to_owned()),
            })
          }
          SnapshotState::Building => {
            if tokio::time::Instant::now() >= deadline {
              return Ok(PrepareOutcome::Building {
                operation_id: record.operation_id.unwrap_or_default(),
              });
            }
          }
        },
        // The row vanished: a cancelled build abandoned it. Report BUILDING with
        // no owner rather than inventing a failure -- nothing failed, and the
        // next caller will claim it.
        None => {
          return Ok(PrepareOutcome::Building {
            operation_id: String::new(),
          })
        }
      }
      tokio::time::sleep(POLL_INTERVAL).await;
    }
  }

  /// Build one snapshot's manifest. The caller must already hold the claim.
  pub async fn build(
    &self,
    repository: &RepositoryId,
    commit: &ObjectId,
    retained: bool,
    cancel: Cancel,
  ) -> Result<SnapshotRecord, GfsError> {
    let snapshots = self.snapshots(repository);
    match self
      .build_inner(repository, commit, retained, &cancel, &snapshots)
      .await
    {
      Ok(record) => Ok(record),
      Err(e) if e.code == gfs_types::ErrorCode::Cancelled => {
        snapshots.abandon(commit)?;
        Err(e)
      }
      Err(e) => {
        snapshots.fail(commit, &e.message)?;
        Err(e)
      }
    }
  }

  async fn build_inner(
    &self,
    repository: &RepositoryId,
    commit: &ObjectId,
    retained: bool,
    cancel: &Cancel,
    snapshots: &SnapshotStore,
  ) -> Result<SnapshotRecord, GfsError> {
    let ctx = BuildContext {
      repo: self.registry.repository(repository)?,
      blobs: Arc::new(self.blobs(repository)),
      postings: Arc::new(self.postings(repository)),
    };

    // Detected once per build and applied to both construction paths, so the
    // incremental and full manifests agree byte-for-byte. Two jobs (ADR 0012):
    // undo the entry-metadata expansion — the index interns by the *pointer
    // blob*, the identity trees actually reference — and mark the facts so the
    // registry classifies them `Lfs` without reading anything.
    let lfs: std::collections::HashMap<BytePath, (ObjectId, u64)> = ctx
      .repo
      .lfs_pointers(commit.clone())
      .await?
      .into_iter()
      .map(|e| (e.path, (e.blob_oid, e.blob_size)))
      .collect();

    cancel.check()?;
    let manifest = match self.incremental_base(&ctx.repo, commit, snapshots).await? {
      Some((parent, base)) => {
        tracing::debug!(
          commit = %commit.to_qualified(),
          parent = %parent.to_qualified(),
          "building the manifest from its first parent"
        );
        self
          .build_incremental(&ctx, commit, &parent, &base, &lfs, cancel)
          .await?
      }
      None => self.build_full(&ctx, commit, &lfs, cancel, snapshots).await?,
    };

    cancel.check()?;
    let generation = snapshots.generation()?;
    snapshots.complete(&manifest, generation, retained)
  }

  /// The first parent's manifest, when it is READY.
  ///
  /// Only the *first* parent, and only when already prepared. Searching for any
  /// prepared ancestor would be cheaper still on a busy repository, but the
  /// saving is bounded by the diff and the risk is not: a distant ancestor's
  /// diff can be larger than the tree.
  async fn incremental_base(
    &self,
    repo: &AsyncRepository,
    commit: &ObjectId,
    snapshots: &SnapshotStore,
  ) -> Result<Option<(ObjectId, Manifest)>, GfsError> {
    let meta = repo.read_commit(commit.clone()).await?;
    let Some(parent) = meta.parents.first().cloned() else {
      return Ok(None);
    };
    match snapshots.manifest(&parent)? {
      Some(base) => Ok(Some((parent, base))),
      None => Ok(None),
    }
  }

  async fn build_incremental(
    &self,
    ctx: &BuildContext,
    commit: &ObjectId,
    parent: &ObjectId,
    base: &Manifest,
    lfs: &std::collections::HashMap<BytePath, (ObjectId, u64)>,
    cancel: &Cancel,
  ) -> Result<Manifest, GfsError> {
    let deltas = ctx
      .repo
      .diff_commits(parent.clone(), commit.clone())
      .await?;
    cancel.check()?;

    let upserts: Vec<(BytePath, u32, BlobFact)> = deltas
      .iter()
      .filter_map(|d| match d {
        TreeDelta::Upserted {
          path,
          mode,
          oid,
          size,
        } => {
          let fact = match lfs.get(path) {
            Some((blob_oid, blob_size)) => BlobFact {
              oid: blob_oid.clone(),
              size: *blob_size,
              lfs: true,
            },
            None => BlobFact {
              oid: oid.clone(),
              size: *size,
              lfs: false,
            },
          };
          Some((path.clone(), *mode, fact))
        }
        TreeDelta::Removed { .. } => None,
      })
      .collect();

    let facts: Vec<BlobFact> = upserts.iter().map(|(_, _, f)| f.clone()).collect();
    let keys = self.ingest(ctx, facts, cancel).await?;

    let mut manifest_deltas = Vec::with_capacity(deltas.len());
    for delta in &deltas {
      if let TreeDelta::Removed { path } = delta {
        manifest_deltas.push(ManifestDelta::Removed { path: path.clone() });
      }
    }
    for ((path, mode, _), key) in upserts.iter().zip(keys.iter()) {
      manifest_deltas.push(ManifestDelta::Upserted {
        path: path.clone(),
        mode: *mode,
        key: *key,
      });
    }
    Ok(base.apply(commit.clone(), &manifest_deltas))
  }

  async fn build_full(
    &self,
    ctx: &BuildContext,
    commit: &ObjectId,
    lfs: &std::collections::HashMap<BytePath, (ObjectId, u64)>,
    cancel: &Cancel,
    snapshots: &SnapshotStore,
  ) -> Result<Manifest, GfsError> {
    // The root listing decides the fan-out. Paging it rather than assuming one
    // page: a monorepo root with more than the page limit of top-level entries
    // is unusual but not impossible, and a silently truncated fan-out would
    // produce a manifest missing whole directories.
    let mut directories = Vec::new();
    let mut files: Vec<(BytePath, u32, BlobFact)> = Vec::new();
    let repo = &ctx.repo;
    let mut after: Option<Vec<u8>> = None;
    loop {
      let page = repo
        .list_directory(
          commit.clone(),
          BytePath::root(),
          after.clone(),
          gfs_types::limits::MAX_DIRECTORY_PAGE_SIZE,
        )
        .await?;
      for entry in &page.entries {
        match entry.kind {
          EntryKind::Directory => directories.push(entry.path.clone()),
          EntryKind::Regular | EntryKind::Executable => files.push((
            entry.path.clone(),
            entry.mode,
            BlobFact {
              oid: entry.oid.clone(),
              size: entry.size,
              lfs: false,
            },
          )),
          // Symlinks, gitlinks, and unsupported modes are outside the searchable
          // corpus; see `gfs_git::WalkEntry`.
          _ => {}
        }
      }
      match page.next_page_token {
        Some(token) => after = Some(token),
        None => break,
      }
    }

    cancel.check()?;

    // One task per top-level directory, at most WALK_FANOUT in flight. The
    // pool's semaphore is the real bound; this bound stops a repository with
    // three thousand top-level directories from allocating three thousand tasks
    // to wait on it.
    let mut walked: Vec<(BytePath, u32, BlobFact)> = files;
    let mut queue = directories.into_iter();
    let mut running = tokio::task::JoinSet::new();
    loop {
      while running.len() < WALK_FANOUT {
        let Some(dir) = queue.next() else { break };
        let repo = repo.clone();
        let commit = commit.clone();
        running.spawn(async move { repo.walk_tree(commit, dir).await });
      }
      let Some(joined) = running.join_next().await else {
        break;
      };
      let entries =
        joined.map_err(|e| GfsError::internal(format!("a subtree walk task failed: {e}")))??;
      for entry in entries {
        walked.push((
          entry.path,
          entry.mode,
          BlobFact {
            oid: entry.oid,
            size: entry.size,
            lfs: false,
          },
        ));
      }
      snapshots.record_progress(
        commit,
        Progress {
          paths_walked: walked.len() as u64,
          blobs_classified: 0,
        },
      )?;
      cancel.check()?;
    }

    // The LFS overwrite is applied to the merged list so both sources agree:
    // root entries arrived through `list_directory`, whose metadata expansion
    // must be undone here, and subtree entries arrived raw from `walk_tree`.
    for (path, _, fact) in walked.iter_mut() {
      if let Some((blob_oid, blob_size)) = lfs.get(path) {
        *fact = BlobFact {
          oid: blob_oid.clone(),
          size: *blob_size,
          lfs: true,
        };
      }
    }

    let facts: Vec<BlobFact> = walked.iter().map(|(_, _, f)| f.clone()).collect();
    let keys = self.ingest(ctx, facts, cancel).await?;

    let entries = walked
      .into_iter()
      .zip(keys)
      .map(|((path, mode, _), key)| PathEntry { path, mode, key })
      .collect();
    Ok(Manifest::build(commit.clone(), entries))
  }

  /// Intern, classify, and index, looping until the batch budget stops asking
  /// for more.
  ///
  /// Returns one key per input fact, in order. The loop is the reason
  /// `IngestReport::budget_exhausted` exists: a caller that ran one batch and
  /// stopped would leave part of the corpus unclassified, and every path in it
  /// would show up as an index gap forever.
  ///
  /// # The order of the last three steps is the correctness argument
  ///
  /// Postings are merged **before** `mark_indexed`. `indexed` is what a query
  /// trusts, so setting it before the posting lists exist would make a blob
  /// answer "no match" for trigrams that are simply not written yet — an
  /// unreported wrong answer, which is the one outcome this milestone treats as
  /// unacceptable. In the other order the worst case is a blob correctly
  /// reported as an index gap for a moment.
  async fn ingest(
    &self,
    ctx: &BuildContext,
    facts: Vec<BlobFact>,
    cancel: &Cancel,
  ) -> Result<Vec<gfs_search::BlobKey>, GfsError> {
    let BuildContext {
      repo,
      blobs,
      postings,
    } = ctx;
    let keys = {
      let blobs = Arc::clone(blobs);
      let facts = facts.clone();
      tokio::task::spawn_blocking(move || blobs.intern(&facts))
        .await
        .map_err(crate::util::join_error)??
    };

    let mut indexed_any = false;
    loop {
      cancel.check()?;
      let (report, batch) = {
        let blobs = Arc::clone(blobs);
        let facts = facts.clone();
        let budget = self.ingest_budget;
        repo
          .run(move |r| {
            let source = RepoBlobSource(r);
            let mut batch = PostingBatch::new();
            let report = blobs.ingest(&source, &facts, &budget, |key, content| {
              batch.add(key, content);
              Ok(())
            })?;
            Ok((report, batch))
          })
          .await?
      };

      if !batch.is_empty() {
        indexed_any = true;
        let blobs = Arc::clone(blobs);
        let postings = Arc::clone(postings);
        tokio::task::spawn_blocking(move || {
          postings.merge(&batch)?;
          blobs.mark_indexed(batch.keys())
        })
        .await
        .map_err(crate::util::join_error)??;
      }

      if !report.budget_exhausted {
        break;
      }
    }

    if indexed_any {
      // The shared index changed, so two answers computed either side of this
      // point saw different data and must be distinguishable.
      self.snapshots(blobs.repository()).bump_generation()?;
    }
    Ok(keys)
  }

  /// Run one query against a prepared snapshot.
  ///
  /// Fails with `SnapshotBuilding` when the manifest is not READY. That is a
  /// distinct, retryable code on purpose (DESIGN.md section 9): an agent must be
  /// able to tell "ask again shortly" from "there are no matches", and returning
  /// an empty result here would erase the difference.
  pub async fn search(
    &self,
    repository: &RepositoryId,
    commit: &ObjectId,
    query: gfs_search::Query,
  ) -> Result<gfs_search::SearchResult, GfsError> {
    let snapshots = self.snapshots(repository);
    let Some(manifest) = snapshots.manifest(commit)? else {
      return Err(GfsError::new(
        gfs_types::ErrorCode::SnapshotBuilding,
        "the snapshot is not prepared; call PrepareSnapshot and retry",
      ));
    };
    let generation = snapshots
      .get(commit)?
      .map(|r| r.index_generation)
      .unwrap_or(0);

    let repo = self.registry.repository(repository)?;
    let blobs = self.blobs(repository);
    let postings = Arc::new(self.postings(repository));
    let policy = self.corpus.clone();

    // Records for the whole manifest, fetched once. A per-path lookup would be
    // one SQLite round trip per file in scope, which on a monorepo directory is
    // thousands of round trips before a single blob is read.
    let keys: Vec<gfs_search::BlobKey> = manifest.members().iter().collect();
    let records = tokio::task::spawn_blocking(move || blobs.records_for_keys(&keys))
      .await
      .map_err(crate::util::join_error)??;
    let records = gfs_search::query::records_by_key(records);

    repo
      .run(move |r| {
        let source = RepoBlobSource(r);
        let inputs = gfs_search::SearchInputs {
          manifest: &manifest,
          postings: &postings,
          policy: &policy,
          index_generation: generation,
          records: &records,
        };
        gfs_search::search(&source, &inputs, &query)
      })
      .await
  }

  pub fn postings(&self, repository: &RepositoryId) -> PostingStore {
    PostingStore::new(Arc::clone(&self.store), repository.clone())
  }

  /// Prepare every configured branch tip eagerly, and collect what nothing pins.
  ///
  /// `pinned` is assembled by the caller from visible refs and live leases,
  /// because only the catalog knows them.
  pub async fn maintain(
    self: &Arc<Self>,
    repository: &RepositoryId,
    tips: &[ObjectId],
    pinned: &HashSet<String>,
  ) -> Result<GcReport, GfsError> {
    for tip in tips {
      match self.prepare(repository, tip, true).await {
        Ok(_) => {}
        Err(e) => tracing::warn!(
          repository_id = %repository,
          commit = %tip.to_qualified(),
          error = %e,
          "eager branch-tip preparation failed"
        ),
      }
    }
    self.snapshots(repository).gc(pinned)
  }
}

/// The three handles every build step needs.
///
/// Grouped rather than passed one at a time because they are only ever used
/// together, and threading five arguments through four functions is how one of
/// them ends up being the wrong repository's.
struct BuildContext {
  repo: AsyncRepository,
  blobs: Arc<BlobRegistry>,
  postings: Arc<PostingStore>,
}

/// A [`BlobSource`] over a checked-out repository handle.
///
/// Borrowed rather than owned so it lives entirely inside one
/// `AsyncRepository::run` closure — the indexer's blob reads then happen under
/// the same admission control as every other libgit2 call, instead of opening a
/// second, unbounded path into the object database.
struct RepoBlobSource<'a>(&'a dyn GitRepository);

impl BlobSource for RepoBlobSource<'_> {
  fn size(&self, oid: &ObjectId) -> Result<u64, GfsError> {
    self.0.blob_size(oid)
  }

  fn read(&self, oid: &ObjectId) -> Result<Vec<u8>, GfsError> {
    self.0.read_blob(oid)
  }
}
