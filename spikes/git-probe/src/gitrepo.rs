//! The `GitRepository` abstraction and its libgit2-backed proof of concept.
//!
//! DESIGN.md section 6.1 requires libgit2 to sit behind a trait so FFI
//! lifetimes, blocking work, and format quirks do not leak into HTTP, search,
//! or FUSE code. Two constraints drive the shape of what follows:
//!
//! * `git2::Repository` is `Send` but not `Sync`, and libgit2 documents that a
//!   repository handle must not be used concurrently from multiple threads. So
//!   a handle is checked out for the duration of one request and returned, and
//!   is never shared.
//! * Every libgit2 call is synchronous and can block on disk for an unbounded
//!   time (a cold pack read, a large delta chain). Running one on a Tokio worker
//!   stalls unrelated requests, which is the "libgit2 blocks an async worker"
//!   risk in DESIGN.md section 13. The pool below is therefore sized and
//!   bounded, and the async wrapper is what callers get.

use crate::model::{BytePath, EntryKind, HashAlgorithm, ObjectId};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

#[derive(Debug, Clone, serde::Serialize)]
pub struct ResolvedRevision {
  pub commit: ObjectId,
  pub tree: ObjectId,
  /// Present when the selector named a ref rather than a raw object ID.
  pub ref_name: Option<String>,
  /// The raw committer time. The sanitized `snapshot_time` of DESIGN.md
  /// section 8.2 is a catalog decision, not a repository fact, so this layer
  /// reports the unsanitized value and refuses to invent the other one.
  pub committer_time: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TreeEntryInfo {
  pub path: BytePath,
  pub kind: EntryKind,
  pub mode: u32,
  pub oid: ObjectId,
  pub size: u64,
  pub symlink_target: Option<BytePath>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CommitMeta {
  pub commit: ObjectId,
  pub tree: ObjectId,
  pub parents: Vec<ObjectId>,
  pub author: String,
  pub committer: String,
  pub message: String,
  pub committer_time: i64,
}

/// What a mirror must satisfy before the catalog will serve it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RepositoryFormat {
  pub algorithm: HashAlgorithm,
  pub ref_backend: String,
  pub repository_format_version: i64,
  pub extensions: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FormatVerdict {
  Supported,
  /// The mirror must be rejected at creation. Serving it would expose a
  /// partial view, which DESIGN.md section 5.1 forbids.
  Rejected {
    reason: String,
  },
}

pub trait GitRepository: Send + Sync {
  fn format(&self) -> Result<RepositoryFormat>;
  fn resolve_revision(&self, selector: &str) -> Result<ResolvedRevision>;
  fn read_commit(&self, commit: &ObjectId) -> Result<CommitMeta>;
  /// Look up one path in a commit's tree, walking a tree object per component.
  fn entry(&self, commit: &ObjectId, path: &BytePath) -> Result<Option<TreeEntryInfo>>;
  /// One directory page. `after` is the last name returned by the prior page.
  fn list_directory(
    &self,
    commit: &ObjectId,
    path: &BytePath,
    after: Option<&[u8]>,
    limit: usize,
  ) -> Result<(Vec<TreeEntryInfo>, Option<Vec<u8>>)>;
  fn read_blob(&self, blob: &ObjectId) -> Result<Vec<u8>>;
  fn list_refs(&self) -> Result<Vec<(String, ObjectId)>>;
}

// ---------------------------------------------------------------------------
// Handle pool
// ---------------------------------------------------------------------------

/// A bounded pool of libgit2 repository handles.
///
/// Bounded rather than one-per-request because opening a repository re-reads
/// config and re-scans the object database, and bounded rather than unbounded
/// because the whole point is admission control: when every handle is busy a
/// caller waits instead of adding another concurrent pack read.
pub struct RepoPool {
  path: PathBuf,
  inner: Mutex<PoolInner>,
  available: Condvar,
}

struct PoolInner {
  idle: VecDeque<git2::Repository>,
  live: usize,
  max: usize,
}

impl RepoPool {
  pub fn open(path: impl AsRef<Path>, max: usize) -> Result<Arc<Self>> {
    let path = path.as_ref().to_path_buf();
    // Fail fast: an unopenable or unsupported repository must not become a
    // per-request error later.
    let probe = open_repo(&path)?;
    drop(probe);
    Ok(Arc::new(RepoPool {
      path,
      inner: Mutex::new(PoolInner {
        idle: VecDeque::new(),
        live: 0,
        max: max.max(1),
      }),
      available: Condvar::new(),
    }))
  }

  pub fn path(&self) -> &Path {
    &self.path
  }

  /// Check out a handle for the life of one request.
  fn checkout(self: &Arc<Self>) -> Result<PooledRepo> {
    let mut inner = self.inner.lock().unwrap();
    loop {
      if let Some(repo) = inner.idle.pop_front() {
        return Ok(PooledRepo {
          pool: Arc::clone(self),
          repo: Some(repo),
        });
      }
      if inner.live < inner.max {
        inner.live += 1;
        drop(inner);
        let repo = match open_repo(&self.path) {
          Ok(r) => r,
          Err(e) => {
            self.inner.lock().unwrap().live -= 1;
            return Err(e);
          }
        };
        return Ok(PooledRepo {
          pool: Arc::clone(self),
          repo: Some(repo),
        });
      }
      inner = self.available.wait(inner).unwrap();
    }
  }
}

struct PooledRepo {
  pool: Arc<RepoPool>,
  repo: Option<git2::Repository>,
}

impl std::ops::Deref for PooledRepo {
  type Target = git2::Repository;
  fn deref(&self) -> &git2::Repository {
    self.repo.as_ref().unwrap()
  }
}

impl Drop for PooledRepo {
  fn drop(&mut self) {
    let mut inner = self.pool.inner.lock().unwrap();
    if let Some(repo) = self.repo.take() {
      inner.idle.push_back(repo);
    } else {
      inner.live -= 1;
    }
    self.pool.available.notify_one();
  }
}

/// The key Git sorts a tree entry by: the name, with `/` appended for trees.
///
/// This is the ordering `git ls-tree` emits and the one a directory page token
/// must be expressed in.
pub fn tree_sort_key(name: &[u8], mode: u32) -> Vec<u8> {
  let mut k = name.to_vec();
  if mode == 0o040000 {
    k.push(b'/');
  }
  k
}

fn open_repo(path: &Path) -> Result<git2::Repository> {
  git2::Repository::open_bare(path)
    .or_else(|_| git2::Repository::open(path))
    .with_context(|| format!("libgit2 could not open {}", path.display()))
}

// ---------------------------------------------------------------------------
// libgit2 implementation
// ---------------------------------------------------------------------------

pub struct Libgit2Repository {
  pool: Arc<RepoPool>,
}

impl Libgit2Repository {
  pub fn open(path: impl AsRef<Path>, max_handles: usize) -> Result<Self> {
    Ok(Libgit2Repository {
      pool: RepoPool::open(path, max_handles)?,
    })
  }

  pub fn path(&self) -> &Path {
    self.pool.path()
  }

  fn algorithm(repo: &git2::Repository) -> HashAlgorithm {
    // libgit2 exposes the OID length through any object ID it produces. A
    // 32-byte raw ID can only be SHA-256.
    let len = repo
      .head()
      .ok()
      .and_then(|h| h.target())
      .map(|o| o.as_bytes().len())
      .unwrap_or(0);
    match len {
      32 => HashAlgorithm::Sha256,
      _ => HashAlgorithm::Sha1,
    }
  }

  fn to_oid(repo: &git2::Repository, oid: git2::Oid) -> Result<ObjectId> {
    let algo = Self::algorithm(repo);
    ObjectId::from_raw(algo, oid.as_bytes()).map_err(|e| anyhow!("object id: {e}"))
  }

  fn find_oid(id: &ObjectId) -> Result<git2::Oid> {
    git2::Oid::from_bytes(id.as_bytes()).with_context(|| format!("invalid object id {id}"))
  }

  fn entry_info(
    repo: &git2::Repository,
    path: BytePath,
    mode: u32,
    oid: git2::Oid,
  ) -> Result<TreeEntryInfo> {
    let kind = EntryKind::from_mode(mode);
    let (size, symlink_target) = match kind {
      EntryKind::Symlink => {
        let blob = repo.find_blob(oid)?;
        let content = blob.content().to_vec();
        (content.len() as u64, Some(BytePath::new(content)))
      }
      EntryKind::Regular | EntryKind::Executable => {
        // odb().read_header avoids inflating the blob just to size it.
        let (size, _) = repo.odb()?.read_header(oid)?;
        (size as u64, None)
      }
      _ => (0, None),
    };
    Ok(TreeEntryInfo {
      path,
      kind,
      mode,
      oid: Self::to_oid(repo, oid)?,
      size,
      symlink_target,
    })
  }

  /// Walk to the tree that contains `path`, returning it plus the final name.
  fn walk_to_parent<'r>(
    repo: &'r git2::Repository,
    commit: &ObjectId,
    path: &BytePath,
  ) -> Result<Option<(git2::Tree<'r>, Vec<u8>)>> {
    let commit_oid = Self::find_oid(commit)?;
    let mut tree = repo.find_commit(commit_oid)?.tree()?;
    let comps: Vec<&[u8]> = path.components().collect();
    let Some((last, parents)) = comps.split_last() else {
      return Ok(None);
    };
    for c in parents {
      // The entry borrows `tree`, so the child OID is copied out before
      // `tree` is reassigned.
      let next = {
        let Some(entry) = tree.get_name_bytes(c) else {
          return Ok(None);
        };
        if entry.filemode() != 0o040000 {
          // A path component that is not a directory: the path does
          // not exist, rather than being an error.
          return Ok(None);
        }
        entry.id()
      };
      tree = repo.find_tree(next)?;
    }
    Ok(Some((tree, last.to_vec())))
  }
}

impl GitRepository for Libgit2Repository {
  fn format(&self) -> Result<RepositoryFormat> {
    let repo = self.pool.checkout()?;
    read_format(&repo)
  }

  fn resolve_revision(&self, selector: &str) -> Result<ResolvedRevision> {
    // `refs/gfs/` is an internal reachability namespace, never a user
    // revision (DESIGN.md section 7.1). Rejecting it here, at the lowest
    // layer, means no caller can forget to.
    if selector.starts_with("refs/gfs/") || selector.contains("/gfs/mounts/") {
      bail!("selector names the reserved internal namespace refs/gfs/");
    }
    let repo = self.pool.checkout()?;
    let object = repo
      .revparse_single(selector)
      .with_context(|| format!("resolving revision {selector:?}"))?;
    // An annotated tag must be peeled to its commit; returning the tag OID
    // as a commit would produce a snapshot nobody can read.
    let commit = object
      .peel_to_commit()
      .with_context(|| format!("{selector:?} does not peel to a commit"))?;
    let ref_name = repo
      .resolve_reference_from_short_name(selector)
      .ok()
      .and_then(|r| r.name().map(str::to_string));
    Ok(ResolvedRevision {
      commit: Self::to_oid(&repo, commit.id())?,
      tree: Self::to_oid(&repo, commit.tree_id())?,
      ref_name,
      committer_time: commit.time().seconds(),
    })
  }

  fn read_commit(&self, commit: &ObjectId) -> Result<CommitMeta> {
    let pooled = self.pool.checkout()?;
    // The inner scope ends every libgit2 borrow before the pooled handle is
    // returned to the pool by its `Drop`.
    let meta = {
      let repo: &git2::Repository = &pooled;
      let c = repo.find_commit(Self::find_oid(commit)?)?;
      // Signatures borrow the commit, so they are made owned before the
      // struct literal extends their temporaries past the block.
      let author = c.author().to_string();
      let committer = c.committer().to_string();
      CommitMeta {
        commit: Self::to_oid(repo, c.id())?,
        tree: Self::to_oid(repo, c.tree_id())?,
        parents: c
          .parent_ids()
          .map(|p| Self::to_oid(repo, p))
          .collect::<Result<_>>()?,
        author,
        committer,
        message: String::from_utf8_lossy(c.message_bytes()).into_owned(),
        committer_time: c.time().seconds(),
      }
    };
    Ok(meta)
  }

  fn entry(&self, commit: &ObjectId, path: &BytePath) -> Result<Option<TreeEntryInfo>> {
    let pooled = self.pool.checkout()?;
    let found = {
      let repo: &git2::Repository = &pooled;
      if path.is_empty() {
        let tree = repo.find_commit(Self::find_oid(commit)?)?.tree()?;
        Some(Libgit2Repository::entry_info(
          repo,
          BytePath::new(Vec::new()),
          0o040000,
          tree.id(),
        )?)
      } else {
        match Self::walk_to_parent(repo, commit, path)? {
          None => None,
          Some((tree, name)) => match tree.get_name_bytes(&name) {
            None => None,
            Some(entry) => Some(Libgit2Repository::entry_info(
              repo,
              path.clone(),
              entry.filemode() as u32,
              entry.id(),
            )?),
          },
        }
      }
    };
    Ok(found)
  }

  fn list_directory(
    &self,
    commit: &ObjectId,
    path: &BytePath,
    after: Option<&[u8]>,
    limit: usize,
  ) -> Result<(Vec<TreeEntryInfo>, Option<Vec<u8>>)> {
    let pooled = self.pool.checkout()?;
    let result = {
      let repo: &git2::Repository = &pooled;
      let mut tree = repo.find_commit(Self::find_oid(commit)?)?.tree()?;
      if !path.is_empty() {
        let Some((parent, name)) = Self::walk_to_parent(repo, commit, path)? else {
          return Ok((Vec::new(), None));
        };
        let child = {
          let Some(entry) = parent.get_name_bytes(&name) else {
            return Ok((Vec::new(), None));
          };
          match entry.filemode() as u32 {
            0o040000 => entry.id(),
            // DESIGN.md section 8.2: a gitlink is presented as an
            // empty, read-only directory. Returning an empty page
            // here rather than an error keeps that rule in one
            // place instead of making every caller special-case it,
            // and GFS has no objects to list for it in any case —
            // submodule contents live in another repository.
            0o160000 => return Ok((Vec::new(), None)),
            _ => bail!("not a directory"),
          }
        };
        tree = repo.find_tree(child)?;
      }

      let mut out: Vec<TreeEntryInfo> = Vec::new();
      let mut truncated = false;
      let mut last_key: Vec<u8> = Vec::new();
      // Git stores tree entries sorted, so resuming after a key is a scan
      // rather than a sort, and a name-based token stays valid across tree
      // caching and re-reads in a way an index-based one would not.
      //
      // The sort key is NOT the raw name. Git orders a directory entry as
      // if its name ended in '/', so `byteorder.h` (0x2e) sorts before
      // `byteorder/` (0x2f) even though the raw names compare the other
      // way. Paginating on raw names therefore skips entries at page
      // boundaries: the Linux tree's `include/linux` returns 1597 of 1598.
      for e in tree.iter() {
        let key = tree_sort_key(e.name_bytes(), e.filemode() as u32);
        if let Some(after) = after {
          if key.as_slice() <= after {
            continue;
          }
        }
        if out.len() >= limit {
          truncated = true;
          break;
        }
        let mut child = path.as_bytes().to_vec();
        if !child.is_empty() && !child.ends_with(b"/") {
          child.push(b'/');
        }
        child.extend_from_slice(e.name_bytes());
        out.push(Libgit2Repository::entry_info(
          repo,
          BytePath::new(child),
          e.filemode() as u32,
          e.id(),
        )?);
        last_key = key;
      }
      let next_token = truncated.then_some(last_key);
      (out, next_token)
    };
    Ok(result)
  }

  fn read_blob(&self, blob: &ObjectId) -> Result<Vec<u8>> {
    let pooled = self.pool.checkout()?;
    let bytes = {
      let repo: &git2::Repository = &pooled;
      repo.find_blob(Self::find_oid(blob)?)?.content().to_vec()
    };
    Ok(bytes)
  }

  fn list_refs(&self) -> Result<Vec<(String, ObjectId)>> {
    let pooled = self.pool.checkout()?;
    let mut out = Vec::new();
    {
      let repo: &git2::Repository = &pooled;
      for r in repo.references()? {
        let r = r?;
        let Some(name) = r.name() else { continue };
        // The internal namespace never appears in an enumeration a
        // caller can see, matching the upload-pack hideRefs policy.
        if name.starts_with("refs/gfs/") {
          continue;
        }
        if let Some(t) = r.target() {
          out.push((name.to_string(), Self::to_oid(repo, t)?));
        }
      }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
  }
}

/// Read the on-disk repository format without trusting libgit2 to reject it.
///
/// libgit2 will happily open a repository whose `extensions.*` it does not
/// implement in some configurations, so ingest validation reads `config`
/// directly and decides for itself. This is the "detect at creation rather than
/// serve a partial view" requirement in PLAN.md M0.3.
pub fn read_format(repo: &git2::Repository) -> Result<RepositoryFormat> {
  let cfg = repo.config()?;
  let version = cfg.get_i64("core.repositoryformatversion").unwrap_or(0);

  let mut extensions = Vec::new();
  if let Ok(mut entries) = cfg.entries(Some("extensions.*")) {
    while let Some(Ok(e)) = entries.next() {
      if let (Some(n), Some(v)) = (e.name(), e.value()) {
        extensions.push((n.to_string(), v.to_string()));
      }
    }
  }

  let algorithm = extensions
    .iter()
    .find(|(n, _)| n.eq_ignore_ascii_case("extensions.objectformat"))
    .and_then(|(_, v)| HashAlgorithm::from_name(v))
    .unwrap_or(HashAlgorithm::Sha1);

  let ref_backend = extensions
    .iter()
    .find(|(n, _)| n.eq_ignore_ascii_case("extensions.refstorage"))
    .map(|(_, v)| v.clone())
    .unwrap_or_else(|| "files".to_string());

  Ok(RepositoryFormat {
    algorithm,
    ref_backend,
    repository_format_version: version,
    extensions,
  })
}

/// The ingest gate. Called before a mirror is ever served.
pub fn verdict(format: &RepositoryFormat, sha256_supported: bool) -> FormatVerdict {
  if format.ref_backend != "files" {
    return FormatVerdict::Rejected {
      reason: format!(
        "ref backend {:?} is unsupported; libgit2 reads only the `files` backend",
        format.ref_backend
      ),
    };
  }
  if format.algorithm == HashAlgorithm::Sha256 && !sha256_supported {
    return FormatVerdict::Rejected {
      reason: "SHA-256 object format requires a libgit2 built with \
                     GIT_EXPERIMENTAL_SHA256"
        .to_string(),
    };
  }
  // Any extension we do not recognize is a rejection, not a warning. A repo
  // with an unknown extension is by definition one whose on-disk meaning we
  // do not know.
  const KNOWN: &[&str] = &[
    "extensions.objectformat",
    "extensions.refstorage",
    "extensions.noop",
    "extensions.compatobjectformat",
  ];
  for (name, value) in &format.extensions {
    if !KNOWN.iter().any(|k| name.eq_ignore_ascii_case(k)) {
      return FormatVerdict::Rejected {
        reason: format!("unrecognized repository extension {name}={value}"),
      };
    }
  }
  FormatVerdict::Supported
}

/// Whether the linked libgit2 has experimental SHA-256 support compiled in.
///
/// Deliberately a runtime probe rather than `cfg!(libgit2_experimental_sha256)`.
/// libgit2-sys emits that cfg for its own compilation only, so a downstream
/// crate testing it always reads `false` and would report "no SHA-256" even in a
/// build that has it. Constructing a 32-byte raw object ID is the observable
/// difference: `GIT_OID_MAX_SIZE` is 20 in a default build and 32 in a
/// `GIT_EXPERIMENTAL_SHA256` build, and `git_oid_fromraw` length-checks against
/// it.
pub fn build_has_sha256() -> bool {
  git2::Oid::from_bytes(&[0u8; 32]).is_ok()
}

pub fn libgit2_version() -> String {
  let (major, minor, patch) = git2::Version::get().libgit2_version();
  format!("{major}.{minor}.{patch}")
}
