//! The libgit2-backed [`GitRepository`] implementation.
//!
//! Every method follows the same shape, dictated by the FFI lifetimes: check out a
//! pooled handle, do all libgit2 work inside an inner scope so every borrow ends
//! before the handle is returned by its `Drop`, and hand back owned values. The
//! compiler enforces that discipline, which is why the inner scopes are written
//! out rather than relying on care.

use std::sync::Arc;

use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::revision::{self, RevisionSelector};
use gfs_types::{
  limits, mode, AncestryStep, BlameHunk, BytePath, CommitMeta, DiffFileChange, DiffFormat,
  DiffStatus, EntryKind, HashAlgorithm, ObjectId, ResolvedRevision, Signature, Timestamp,
  TreeEntryInfo,
};

use crate::attributes::{self, AttributeFile};
use crate::format::{self, RepositoryFormat};
use crate::lfs::{self, LfsPointer};
use crate::pool::{PooledRepo, RepoPool};
use crate::repository::{
  BlameOutput, CommitSignature, DiffOutput, DiffRequest, DirectoryPage, EntryLookup, GitRepository,
  LfsEntry, LogOptions, TreeChange, TreeChangeKind, TreeDelta, TreePage, WalkEntry,
};

use crate::tree::{DecodedEntry, DecodedTree, TreeCache, TreeCacheStats};

/// Terminal width `--stat` lays its histogram out to.
///
/// Git uses the terminal's width; there is no terminal here, so this is the
/// conventional 80 — the width the output is expected to be pasted at.
const STAT_WIDTH: usize = 80;

/// The tree containing a path, plus that path's final component.
///
/// `None` when any intermediate component is missing or is not a directory,
/// which is an ordinary negative lookup rather than a failure.
type ParentTree = Option<(Arc<DecodedTree>, Vec<u8>)>;

pub struct Libgit2Repository {
  pool: Arc<RepoPool>,
  format: RepositoryFormat,
  trees: TreeCache,
  /// Parsed `.gitattributes` blobs, keyed by blob OID — immutable content, so
  /// entries never go stale. Attribute files are small and few; the bound is a
  /// wholesale clear rather than an LRU.
  attr_files: std::sync::Mutex<std::collections::HashMap<ObjectId, Arc<AttributeFile>>>,
  /// When present, entry metadata substitutes expanded size and `lfs-sha256:`
  /// key for LFS entries whose object this check confirms (ADR 0012). Absent
  /// means every LFS entry is served as its pointer, the pre-0012 behavior.
  lfs_check: Option<Arc<dyn lfs::LfsObjectCheck>>,
}

impl std::fmt::Debug for Libgit2Repository {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Libgit2Repository")
      .field("path", &self.pool.path())
      .field("algorithm", &self.format.algorithm)
      .finish_non_exhaustive()
  }
}

impl Libgit2Repository {
  /// Open a repository, rejecting an unsupported format before serving anything.
  pub fn open(
    path: impl AsRef<std::path::Path>,
    max_handles: usize,
    tree_cache_bytes: usize,
  ) -> Result<Self, GfsError> {
    let path = path.as_ref();
    // Format first: a `reftable` repository must produce the format verdict, not
    // a generic libgit2 open failure, and `format::check` reads config directly
    // so it works even when libgit2 cannot open the repository at all.
    let format = format::check(path)?;
    Ok(Libgit2Repository {
      pool: RepoPool::open(path, max_handles)?,
      format,
      trees: TreeCache::new(tree_cache_bytes),
      attr_files: std::sync::Mutex::new(std::collections::HashMap::new()),
      lfs_check: None,
    })
  }

  /// Enable ADR 0012 entry-metadata expansion, gated on `check`.
  pub fn with_lfs_check(mut self, check: Arc<dyn lfs::LfsObjectCheck>) -> Self {
    self.lfs_check = Some(check);
    self
  }

  pub fn path(&self) -> &std::path::Path {
    self.pool.path()
  }

  fn to_oid(&self, oid: git2::Oid) -> Result<ObjectId, GfsError> {
    // The algorithm comes from the validated repository format, not from the
    // length of whatever OID libgit2 happened to produce. The M0 spike derived it
    // from HEAD's OID length, which reports SHA-1 for an empty repository whose
    // config says otherwise -- harmless there, but it makes the algorithm a
    // property of the data rather than of the repository.
    ObjectId::from_raw(self.format.algorithm, oid.as_bytes()).map_err(GfsError::from)
  }

  fn git_oid(&self, id: &ObjectId) -> Result<git2::Oid, GfsError> {
    if id.algorithm() != self.format.algorithm {
      return Err(GfsError::invalid(format!(
        "object id is {} but this repository is {}",
        id.algorithm(),
        self.format.algorithm
      )));
    }
    git2::Oid::from_bytes(id.as_bytes()).map_err(|e| GfsError::invalid(e.message().to_owned()))
  }

  fn checkout(&self) -> Result<PooledRepo, GfsError> {
    self.pool.checkout()
  }

  /// Decode a tree, using the cache when possible.
  fn decoded_tree(
    &self,
    repo: &git2::Repository,
    tree_oid: git2::Oid,
  ) -> Result<Arc<DecodedTree>, GfsError> {
    let key = self.to_oid(tree_oid)?;
    if let Some(hit) = self.trees.get(&key) {
      return Ok(hit);
    }
    let tree = repo
      .find_tree(tree_oid)
      .map_err(|e| not_found(&e, "tree"))?;
    let mut entries = Vec::with_capacity(tree.len());
    for e in tree.iter() {
      entries.push(DecodedEntry {
        name: e.name_bytes().to_vec(),
        mode: e.filemode() as u32,
        oid: self.to_oid(e.id())?,
      });
    }
    Ok(self.trees.insert(key, DecodedTree::new(entries)))
  }

  /// Walk to the tree containing `path`, returning it and the final component.
  ///
  /// Returns `Ok(None)` when any intermediate component is missing or is not a
  /// directory. A path whose parent is a file does not exist; that is an ordinary
  /// negative lookup, not an error.
  fn walk_to_parent(
    &self,
    repo: &git2::Repository,
    commit: &ObjectId,
    path: &BytePath,
  ) -> Result<ParentTree, GfsError> {
    let commit_oid = self.git_oid(commit)?;
    let root = repo
      .find_commit(commit_oid)
      .map_err(|e| not_found(&e, "commit"))?
      .tree_id();
    let mut tree = self.decoded_tree(repo, root)?;

    let comps: Vec<&[u8]> = path.components().collect();
    let Some((last, parents)) = comps.split_last() else {
      return Ok(None);
    };
    for c in parents {
      let Some(entry) = tree.get(c) else {
        return Ok(None);
      };
      if entry.mode != mode::DIRECTORY {
        // Includes the gitlink case: a submodule has no tree in this repository,
        // so any path *inside* it is absent rather than an error.
        return Ok(None);
      }
      let child = self.git_oid(&entry.oid)?;
      tree = self.decoded_tree(repo, child)?;
    }
    Ok(Some((tree, last.to_vec())))
  }

  /// Parse (through the cache) the `.gitattributes` blob with this OID.
  fn attribute_file(
    &self,
    repo: &git2::Repository,
    blob_oid: &ObjectId,
  ) -> Result<Arc<AttributeFile>, GfsError> {
    if let Some(hit) = self.attr_files.lock().unwrap().get(blob_oid) {
      return Ok(hit.clone());
    }
    let oid = self.git_oid(blob_oid)?;
    let blob = repo.find_blob(oid).map_err(|e| not_found(&e, "blob"))?;
    let parsed = Arc::new(AttributeFile::parse(blob.content()));
    let mut cache = self.attr_files.lock().unwrap();
    if cache.len() >= 1024 {
      cache.clear();
    }
    cache.insert(blob_oid.clone(), parsed.clone());
    Ok(parsed)
  }

  /// The `.gitattributes` stack for `path`, root-first: element `i` is the
  /// attribute file of the directory made of `path`'s first `i` components,
  /// with an empty stand-in where a directory has none. Levels past a missing
  /// or non-directory component are empty too — the path then has no entry,
  /// and the caller's own lookup is what reports that.
  fn attribute_stack(
    &self,
    repo: &git2::Repository,
    commit: &ObjectId,
    path: &BytePath,
  ) -> Result<Vec<Arc<AttributeFile>>, GfsError> {
    let comps: Vec<&[u8]> = path.components().collect();
    let Some((_, parents)) = comps.split_last() else {
      return Ok(Vec::new());
    };
    let commit_oid = self.git_oid(commit)?;
    let root = self.find_commit(repo, commit_oid)?.tree_id();
    let mut tree = Some(self.decoded_tree(repo, root)?);
    let mut stack = Vec::with_capacity(parents.len() + 1);
    for step in std::iter::once(None).chain(parents.iter().map(Some)) {
      if let Some(component) = step {
        tree = match tree.as_ref().and_then(|t| t.get(component)) {
          Some(entry) if entry.mode == mode::DIRECTORY => {
            let child = self.git_oid(&entry.oid)?;
            Some(self.decoded_tree(repo, child)?)
          }
          _ => None,
        };
      }
      let file = match tree.as_ref().and_then(|t| t.get(b".gitattributes")) {
        Some(entry) if EntryKind::from_mode(entry.mode) == EntryKind::Regular => {
          self.attribute_file(repo, &entry.oid)?
        }
        _ => Arc::new(AttributeFile::default()),
      };
      stack.push(file);
    }
    Ok(stack)
  }

  /// Resolve an entry to its LFS pointer, or `None` when the entry is not an
  /// LFS entry: not a file, too large to be a pointer, not on a `filter=lfs`
  /// path at this revision, or its content is not a spec v1 pointer. This is
  /// ADR 0012's detection step; whether the pointer is *expanded* is the
  /// caller's decision, gated on the LFS store actually holding the object.
  pub(crate) fn lfs_pointer_for(
    &self,
    repo: &git2::Repository,
    commit: &ObjectId,
    path: &BytePath,
    kind: EntryKind,
    blob_oid: &ObjectId,
    size: u64,
  ) -> Result<Option<LfsPointer>, GfsError> {
    if !matches!(kind, EntryKind::Regular | EntryKind::Executable) {
      return Ok(None);
    }
    if size == 0 || size > lfs::MAX_POINTER_SIZE {
      return Ok(None);
    }
    let stack = self.attribute_stack(repo, commit, path)?;
    if !attributes::is_lfs(attributes::resolve_filter(&stack, path.as_bytes())) {
      return Ok(None);
    }
    let blob = repo
      .find_blob(self.git_oid(blob_oid)?)
      .map_err(|e| not_found(&e, "blob"))?;
    Ok(lfs::parse_pointer(blob.content()))
  }

  /// The decoded tree a directory path names, or `None` for a gitlink.
  ///
  /// DESIGN.md section 8.2 presents a submodule as an empty read-only
  /// directory, and GFS genuinely has nothing to list for one: its contents are
  /// in another repository. Keeping that rule here means `list_directory` and
  /// `list_tree` cannot disagree about it.
  fn directory_tree(
    &self,
    repo: &git2::Repository,
    commit: &ObjectId,
    path: &BytePath,
  ) -> Result<Option<Arc<DecodedTree>>, GfsError> {
    if path.is_empty() {
      let commit_oid = self.git_oid(commit)?;
      let root = self.find_commit(repo, commit_oid)?.tree_id();
      return Ok(Some(self.decoded_tree(repo, root)?));
    }
    let Some((parent, name)) = self.walk_to_parent(repo, commit, path)? else {
      return Err(GfsError::not_found("no such directory"));
    };
    let Some(entry) = parent.get(&name) else {
      return Err(GfsError::not_found("no such directory"));
    };
    match entry.mode {
      mode::DIRECTORY => {
        let child = self.git_oid(&entry.oid)?;
        Ok(Some(self.decoded_tree(repo, child)?))
      }
      mode::GITLINK => Ok(None),
      _ => Err(GfsError::new(ErrorCode::InvalidArgument, "not a directory")),
    }
  }

  /// Build the API-level entry for one decoded tree entry.
  ///
  /// This is where ADR 0012 expansion happens: an LFS entry whose object the
  /// store holds reports the expanded size and the `lfs-sha256:` content key,
  /// so every metadata consumer — FUSE attributes, `gfs cat`, WebDAV — expands
  /// without knowing it did. The commit is needed for the `.gitattributes`
  /// stack, which is a property of the revision, not of the blob.
  fn entry_info(
    &self,
    repo: &git2::Repository,
    commit: &ObjectId,
    path: BytePath,
    decoded: &DecodedEntry,
  ) -> Result<TreeEntryInfo, GfsError> {
    let kind = EntryKind::from_mode(decoded.mode);
    let oid = self.git_oid(&decoded.oid)?;
    let (size, symlink_target) = match kind {
      EntryKind::Symlink => {
        // A symlink's target *is* its blob content, and it is small. Reading it
        // here means a caller never has to fetch a blob to resolve a link, which
        // matters because `readlink` is on the hot path for any build system that
        // uses symlinked toolchains.
        let blob = repo.find_blob(oid).map_err(|e| not_found(&e, "blob"))?;
        let content = blob.content().to_vec();
        (content.len() as u64, Some(content))
      }
      EntryKind::Regular | EntryKind::Executable => {
        // `read_header` reads the object header only. `find_blob` would inflate
        // the entire blob just to learn its length, which on a large file is the
        // difference between a stat and a full read.
        let (size, _) = repo
          .odb()
          .and_then(|odb| odb.read_header(oid))
          .map_err(|e| not_found(&e, "blob"))?;
        (size as u64, None)
      }
      // A directory, gitlink, or unsupported mode has no blob content to size.
      _ => (0, None),
    };
    let (oid, size) = match &self.lfs_check {
      Some(check) => {
        match self.lfs_pointer_for(repo, commit, &path, kind, &decoded.oid, size)? {
          Some(pointer) if check.contains(&pointer.oid) => (pointer.oid, pointer.size),
          // Not an LFS entry, or its object is unfetchable: the entry degrades
          // to its pointer, which is the MVP behavior and stays truthful.
          _ => (decoded.oid.clone(), size),
        }
      }
      None => (decoded.oid.clone(), size),
    };
    Ok(TreeEntryInfo {
      path,
      kind,
      mode: decoded.mode,
      oid,
      size,
      symlink_target,
      blob_ticket: None,
    })
  }

  /// Resolve one entry given an already-checked-out handle, so a batch can share
  /// both the handle and the decoded trees.
  fn entry_with(
    &self,
    repo: &git2::Repository,
    commit: &ObjectId,
    path: &BytePath,
  ) -> Result<Option<TreeEntryInfo>, GfsError> {
    if path.is_empty() {
      // The root. Reported as a directory whose OID is the commit's tree.
      let commit_oid = self.git_oid(commit)?;
      let tree = self.find_commit(repo, commit_oid)?.tree_id();
      return Ok(Some(TreeEntryInfo {
        path: BytePath::root(),
        kind: EntryKind::Directory,
        mode: mode::DIRECTORY,
        oid: self.to_oid(tree)?,
        size: 0,
        symlink_target: None,
        blob_ticket: None,
      }));
    }
    let Some((tree, name)) = self.walk_to_parent(repo, commit, path)? else {
      return Ok(None);
    };
    let Some(decoded) = tree.get(&name) else {
      return Ok(None);
    };
    Ok(Some(self.entry_info(
      repo,
      commit,
      path.clone(),
      decoded,
    )?))
  }

  fn find_commit<'r>(
    &self,
    repo: &'r git2::Repository,
    oid: git2::Oid,
  ) -> Result<git2::Commit<'r>, GfsError> {
    repo.find_commit(oid).map_err(|e| not_found(&e, "commit"))
  }

  /// Whether a commit changed any of `paths`, relative to its parents.
  ///
  /// The rule is Git's own default history simplification, narrowed to the part
  /// that matters here: a commit is interesting when it is *not* TREESAME to any
  /// parent. For an ordinary commit that reads as "the path differs from the
  /// parent"; for a merge it means the merge itself touched the path, rather
  /// than only inheriting a side branch's change — which is the distinction that
  /// stops `log -- <path>` on a merge-heavy history from being all merges.
  ///
  /// Mode is compared along with the object id, so a `chmod +x` is a change.
  fn touches(
    &self,
    repo: &git2::Repository,
    oid: git2::Oid,
    paths: &[BytePath],
  ) -> Result<bool, GfsError> {
    let commit = self.find_commit(repo, oid)?;
    let tree = commit.tree().map_err(|e| not_found(&e, "tree"))?;
    if commit.parent_count() == 0 {
      return Ok(paths.iter().any(|p| tree_entry_at(&tree, p).is_some()));
    }
    for parent in commit.parents() {
      let parent_tree = parent.tree().map_err(|e| not_found(&e, "tree"))?;
      let same = paths
        .iter()
        .all(|p| tree_entry_at(&tree, p) == tree_entry_at(&parent_tree, p));
      if same {
        return Ok(false);
      }
    }
    Ok(true)
  }

  fn signature_of(sig: &git2::Signature<'_>) -> Signature {
    Signature {
      // Raw bytes: Git does not constrain these to UTF-8, and the M0 spike's
      // `from_utf8_lossy` replaced invalid sequences, corrupting a name rather
      // than reporting it as unusual.
      name: sig.name_bytes().to_vec(),
      email: sig.email_bytes().to_vec(),
      time: Timestamp::from_secs(sig.when().seconds()),
      tz_offset_minutes: sig.when().offset_minutes(),
    }
  }
}

/// A byte path as the `Path` libgit2's APIs take.
///
/// Sound on Unix, where a path *is* bytes; this crate is Linux-only (ADR 0003
/// builds on FUSE) so the lossless conversion exists and no `String` round trip
/// is needed.
fn as_path(path: &BytePath) -> &std::path::Path {
  use std::os::unix::ffi::OsStrExt;
  std::path::Path::new(std::ffi::OsStr::from_bytes(path.as_bytes()))
}

/// One path's `(object id, mode)` in a tree, or `None` when it is absent.
///
/// The mode travels with the id because a `chmod +x` changes a tree without
/// changing a blob, and comparing ids alone would report that commit as having
/// changed nothing.
fn tree_entry_at(tree: &git2::Tree<'_>, path: &BytePath) -> Option<(git2::Oid, i32)> {
  tree
    .get_path(as_path(path))
    .ok()
    .map(|e| (e.id(), e.filemode()))
}

/// libgit2's delta status as the reporting vocabulary.
fn diff_status(delta: git2::Delta) -> DiffStatus {
  match delta {
    git2::Delta::Added | git2::Delta::Untracked => DiffStatus::Added,
    git2::Delta::Deleted => DiffStatus::Deleted,
    git2::Delta::Renamed | git2::Delta::Copied => DiffStatus::Renamed,
    git2::Delta::Typechange => DiffStatus::TypeChanged,
    _ => DiffStatus::Modified,
  }
}

/// libgit2's `FileMode` as the raw Git mode.
///
/// `git2::FileMode` is an enum, so a mode Git wrote that libgit2 does not model
/// arrives as `Unreadable` (0). Mapped rather than cast because a cast of the
/// enum's discriminant is not the Git mode at all.
fn git_mode(m: git2::FileMode) -> u32 {
  match m {
    git2::FileMode::Blob => mode::REGULAR,
    git2::FileMode::BlobExecutable => mode::EXECUTABLE,
    git2::FileMode::Link => mode::SYMLINK,
    git2::FileMode::Tree => mode::DIRECTORY,
    git2::FileMode::Commit => mode::GITLINK,
    git2::FileMode::BlobGroupWritable => 0o100_664,
    git2::FileMode::Unreadable => 0,
  }
}

/// Map a libgit2 error, distinguishing a genuine absence from a real failure.
///
/// libgit2 reports both "no such object" and "the pack is corrupt" through
/// `git2::Error`, and collapsing them would make a corrupted repository look like
/// an empty one -- which is how a data-loss incident gets reported as a
/// not-found.
fn not_found(e: &git2::Error, what: &str) -> GfsError {
  match e.code() {
    git2::ErrorCode::NotFound => GfsError::not_found(format!("no such {what}")),
    _ => GfsError::new(
      ErrorCode::Internal,
      format!("reading {what} failed: {}", e.message()),
    ),
  }
}

impl GitRepository for Libgit2Repository {
  fn algorithm(&self) -> HashAlgorithm {
    self.format.algorithm
  }

  fn format(&self) -> &RepositoryFormat {
    &self.format
  }

  fn resolve(&self, selector: &RevisionSelector) -> Result<ResolvedRevision, GfsError> {
    let pooled = self.checkout()?;
    let repo: &git2::Repository = &pooled;

    let (commit, ref_name) = match selector {
      RevisionSelector::FullOid(oid) => {
        let git_oid = self.git_oid(oid)?;
        (self.find_commit(repo, git_oid)?, None)
      }
      RevisionSelector::Abbrev(hex) => {
        // `revparse_single` is used *only* here, for the one shape that genuinely
        // needs Git's prefix lookup. The input is already known to be pure hex of
        // a bounded length, so it cannot carry expression syntax.
        let object = repo.revparse_single(hex).map_err(|e| match e.code() {
          git2::ErrorCode::NotFound => GfsError::not_found("no object matches that abbreviation"),
          // libgit2 reports an ambiguous prefix distinctly. Reporting it rather
          // than picking a candidate is the whole reason abbreviations have a
          // minimum length.
          git2::ErrorCode::Ambiguous => {
            GfsError::invalid("abbreviated object id is ambiguous; supply more characters")
          }
          _ => GfsError::invalid(format!("cannot resolve abbreviation: {}", e.message())),
        })?;
        let commit = peel_to_commit(object)?;
        (commit, None)
      }
      RevisionSelector::FullRef(name) | RevisionSelector::ShortName(name) => {
        // Belt and braces. The selector grammar already rejected the reserved
        // namespace in both spellings, and this is the layer that would actually
        // hand out a lease anchor if that ever regressed.
        if revision::is_reserved_ref(name) {
          return Err(GfsError::new(
            ErrorCode::ReservedNamespace,
            "that ref is in the reserved internal namespace",
          ));
        }
        let reference =
          repo
            .resolve_reference_from_short_name(name)
            .map_err(|e| match e.code() {
              git2::ErrorCode::NotFound => GfsError::not_found("no such ref"),
              _ => GfsError::invalid(format!("cannot resolve ref: {}", e.message())),
            })?;
        let resolved_name = reference.name().map(str::to_owned);
        if resolved_name
          .as_deref()
          .is_some_and(revision::is_reserved_ref)
        {
          // The short-name case: `gfs/mounts/x` resolves to
          // `refs/gfs/mounts/x`. Caught at the *resolved* name, so it holds even
          // for a spelling the parser did not anticipate.
          return Err(GfsError::new(
            ErrorCode::ReservedNamespace,
            "that ref is in the reserved internal namespace",
          ));
        }
        let object = reference
          .peel(git2::ObjectType::Any)
          .map_err(|e| GfsError::invalid(format!("cannot peel ref: {}", e.message())))?;
        (peel_to_commit(object)?, resolved_name)
      }
    };

    Ok(ResolvedRevision {
      commit: self.to_oid(commit.id())?,
      tree: self.to_oid(commit.tree_id())?,
      ref_name,
      // A catalog fact, not a repository fact. Filled in by the caller that owns
      // the ref-event log.
      ref_version: 0,
      // The *raw* committer time. Sanitizing it into `snapshot_time` needs the
      // authoritative first-seen time, which only the catalog has (ADR 0006), so
      // this layer reports what Git says and refuses to invent the other value.
      snapshot_time: Timestamp::from_secs(commit.time().seconds()),
    })
  }

  fn walk_ancestry(&self, commit: &ObjectId, steps: &[AncestryStep]) -> Result<ObjectId, GfsError> {
    let pooled = self.checkout()?;
    let walked = {
      let repo: &git2::Repository = &pooled;
      let mut current = self.find_commit(repo, self.git_oid(commit)?)?;
      for (index, step) in steps.iter().enumerate() {
        // `^0` and `~0` are the commit itself, which is what Git means by them.
        let (nth, repeats) = match *step {
          AncestryStep::Parent(0) | AncestryStep::Ancestor(0) => continue,
          AncestryStep::Parent(n) => (n - 1, 1),
          // `~n` is n hops along *first* parents, which is not the same as `^n`
          // and is the difference that matters at a merge.
          AncestryStep::Ancestor(n) => (0, n),
        };
        for hop in 0..repeats {
          current = current.parent(nth as usize).map_err(|e| match e.code() {
            git2::ErrorCode::NotFound => GfsError::not_found(format!(
              "{} has no parent {}; the walk stopped {} step(s) in",
              current
                .id()
                .to_string()
                .chars()
                .take(12)
                .collect::<String>(),
              nth + 1,
              index + hop as usize
            )),
            _ => GfsError::internal(format!("reading a parent commit: {}", e.message())),
          })?;
        }
      }
      self.to_oid(current.id())?
    };
    Ok(walked)
  }

  fn read_commit(&self, commit: &ObjectId) -> Result<CommitMeta, GfsError> {
    let pooled = self.checkout()?;
    let meta = {
      let repo: &git2::Repository = &pooled;
      let c = self.find_commit(repo, self.git_oid(commit)?)?;
      // Signatures borrow the commit, so they are made owned before the struct
      // literal would extend their temporaries past this block.
      let author = Self::signature_of(&c.author());
      let committer = Self::signature_of(&c.committer());
      CommitMeta {
        commit: self.to_oid(c.id())?,
        tree: self.to_oid(c.tree_id())?,
        parents: c
          .parent_ids()
          .map(|p| self.to_oid(p))
          .collect::<Result<_, _>>()?,
        author,
        committer,
        message: c.message_bytes().to_vec(),
        snapshot_time: Timestamp::from_secs(c.time().seconds()),
      }
    };
    Ok(meta)
  }

  fn log(
    &self,
    commit: &ObjectId,
    options: &LogOptions,
  ) -> Result<(Vec<CommitMeta>, bool), GfsError> {
    let (skip, limit) = (options.skip, options.limit);
    let pooled = self.checkout()?;
    let repo: &git2::Repository = &pooled;
    let start = self.git_oid(commit)?;
    // Verified before the walk. `revwalk.push` on an object that is not a commit
    // fails somewhere inside the iteration instead, which surfaces as a mid-page
    // error after a caller has already received commits.
    self.find_commit(repo, start)?;

    let mut walk = repo
      .revwalk()
      .map_err(|e| GfsError::internal(format!("starting a revision walk: {}", e.message())))?;
    // `git log`'s default is reverse chronological, and `--topo-order` is opt-in.
    // Matching it is both the correct order and the cheap one: topological
    // sorting has to buffer the reachable graph before it can emit anything.
    // Measured on the M0.1 worst case, `git log -10` on linux.git is 0.007 s in
    // date order and 10.383 s with `--topo-order` — 1 500x, for ten commits.
    //
    // One divergence is accepted and is visible only for commits that share a
    // commit timestamp: Git and libgit2 break that tie differently, so the *set*
    // and the ordering by time agree while two same-second commits may swap.
    // django's history has such a pair. The alternative is the 10 seconds above.
    //
    // Sorting is set before pushing, which libgit2 requires: a sort mode applied
    // afterwards resets the walk.
    walk
      .set_sorting(git2::Sort::TIME)
      .map_err(|e| GfsError::internal(format!("sorting a revision walk: {}", e.message())))?;
    walk
      .push(start)
      .map_err(|e| GfsError::internal(format!("seeding a revision walk: {}", e.message())))?;
    if options.first_parent {
      // Must come after `push`: libgit2 applies the simplification to the
      // already-seeded walk, and setting it first is silently a no-op.
      walk
        .simplify_first_parent()
        .map_err(|e| GfsError::internal(format!("simplifying a revision walk: {}", e.message())))?;
    }

    let mut commits = Vec::new();
    let mut seen = 0usize;
    let mut has_more = false;
    for step in walk {
      let oid =
        step.map_err(|e| GfsError::internal(format!("walking revisions: {}", e.message())))?;
      // Path limiting is applied *before* `skip`, so `--skip` pages the filtered
      // history rather than the raw one -- otherwise page two of
      // `log -- one/file.rs` would start from an offset into commits the caller
      // never saw.
      if !options.paths.is_empty() && !self.touches(repo, oid, &options.paths)? {
        continue;
      }
      if seen < skip {
        seen += 1;
        continue;
      }
      if commits.len() == limit {
        // One past the page is what proves there is a next page. The walk stops
        // here rather than counting the whole history, which on the M0.1 worst
        // case is 1.4 million commits to answer `log -10`.
        has_more = true;
        break;
      }
      let c = self.find_commit(repo, oid)?;
      let author = Self::signature_of(&c.author());
      let committer = Self::signature_of(&c.committer());
      commits.push(CommitMeta {
        commit: self.to_oid(c.id())?,
        tree: self.to_oid(c.tree_id())?,
        parents: c
          .parent_ids()
          .map(|p| self.to_oid(p))
          .collect::<Result<_, _>>()?,
        author,
        committer,
        message: c.message_bytes().to_vec(),
        snapshot_time: Timestamp::from_secs(c.time().seconds()),
      });
    }
    Ok((commits, has_more))
  }

  fn entry(&self, commit: &ObjectId, path: &BytePath) -> EntryLookup {
    let pooled = self.checkout()?;
    let found = {
      let repo: &git2::Repository = &pooled;
      self.entry_with(repo, commit, path)?
    };
    Ok(found)
  }

  fn batch_entries(&self, commit: &ObjectId, paths: &[BytePath]) -> Vec<EntryLookup> {
    // One handle for the whole batch. The decoded-tree cache then makes the
    // shared prefixes of these paths nearly free, which is the point of having a
    // batch call at all -- a thousand separate `GetEntry` requests would each
    // re-check out a handle and re-walk the same directories.
    let pooled = match self.checkout() {
      Ok(p) => p,
      Err(e) => return paths.iter().map(|_| Err(e.clone())).collect(),
    };
    let repo: &git2::Repository = &pooled;
    paths
      .iter()
      .map(|p| self.entry_with(repo, commit, p))
      .collect()
  }

  fn list_directory(
    &self,
    commit: &ObjectId,
    path: &BytePath,
    after: Option<&[u8]>,
    limit: usize,
  ) -> Result<DirectoryPage, GfsError> {
    let limit = limit.clamp(1, limits::MAX_DIRECTORY_PAGE_SIZE);
    let pooled = self.checkout()?;
    let page = {
      let repo: &git2::Repository = &pooled;
      let Some(tree) = self.directory_tree(repo, commit, path)? else {
        return Ok(DirectoryPage {
          entries: Vec::new(),
          next_page_token: None,
        });
      };

      let (page, next_page_token) = tree.page(after, limit);
      let mut entries = Vec::with_capacity(page.len());
      for decoded in page {
        entries.push(self.entry_info(repo, commit, path.join(&decoded.name), decoded)?);
      }
      DirectoryPage {
        entries,
        next_page_token,
      }
    };
    Ok(page)
  }

  fn list_tree(
    &self,
    commit: &ObjectId,
    root: &BytePath,
    after: Option<&[u8]>,
    max_entries: usize,
  ) -> Result<TreePage, GfsError> {
    let max_entries = max_entries.clamp(1, limits::MAX_TREE_PAGE_ENTRIES);
    let pooled = self.checkout()?;
    let repo: &git2::Repository = &pooled;

    let mut entries: Vec<TreeEntryInfo> = Vec::new();
    let mut directories: Vec<BytePath> = Vec::new();
    let mut next_page_token = None;

    // Depth-first, parents before children, siblings in Git's tree order --
    // children are pushed reversed so the stack pops them in order. The walk is
    // a pure function of the commit, so `after` can name a directory and a
    // resumed walk arrives at the same place by replaying the same traversal.
    // Replaying is cheap: the skipped prefix costs tree decodes, which the tree
    // cache mostly answers, and no object-header reads at all.
    let mut stack = vec![root.clone()];
    let mut emitting = after.is_none();
    while let Some(dir) = stack.pop() {
      if !emitting && Some(dir.as_bytes()) == after {
        emitting = true;
      }
      // The soft cap, checked before a directory is expanded rather than in the
      // middle of one: a client's cached listing is only usable if it is
      // complete. The first directory of a page is always emitted, so a
      // directory larger than the cap makes progress instead of looping.
      if emitting && !entries.is_empty() && entries.len() >= max_entries {
        next_page_token = Some(dir.as_bytes().to_vec());
        break;
      }

      let Some(tree) = self.directory_tree(repo, commit, &dir)? else {
        // A gitlink. Named as a listed directory with no children, so a client
        // records "empty" rather than "not yet fetched".
        if emitting {
          directories.push(dir);
        }
        continue;
      };

      let mut children = Vec::new();
      for decoded in tree.entries() {
        let path = dir.join(&decoded.name);
        if matches!(decoded.mode, mode::DIRECTORY | mode::GITLINK) {
          children.push(path.clone());
        }
        if emitting {
          entries.push(self.entry_info(repo, commit, path, decoded)?);
        }
      }
      if emitting {
        directories.push(dir);
      }
      for child in children.into_iter().rev() {
        stack.push(child);
      }
    }

    Ok(TreePage {
      entries,
      directories,
      next_page_token,
    })
  }

  fn walk_tree(
    &self,
    commit: &ObjectId,
    root: &BytePath,
    visit: &mut dyn FnMut(WalkEntry) -> Result<(), GfsError>,
  ) -> Result<(), GfsError> {
    let pooled = self.checkout()?;
    let repo: &git2::Repository = &pooled;
    let commit_oid = self.git_oid(commit)?;

    let start = if root.is_empty() {
      self.find_commit(repo, commit_oid)?.tree_id()
    } else {
      let Some((parent, name)) = self.walk_to_parent(repo, commit, root)? else {
        return Err(GfsError::not_found("no such directory"));
      };
      let Some(entry) = parent.get(&name) else {
        return Err(GfsError::not_found("no such directory"));
      };
      if entry.mode != mode::DIRECTORY {
        return Err(GfsError::new(ErrorCode::InvalidArgument, "not a directory"));
      }
      self.git_oid(&entry.oid)?
    };

    // An explicit stack rather than libgit2's `Tree::walk`. Two reasons, and both
    // bit the M0 spike: `walk`'s callback can only signal "skip" or "abort" and
    // cannot carry an error out, so a budget or a cancellation becomes an opaque
    // abort; and the callback receives a `&str`-ish directory prefix, which is
    // the wrong type for a path that need not be UTF-8.
    let mut stack: Vec<(git2::Oid, BytePath)> = vec![(start, root.clone())];
    let odb = repo.odb().map_err(|e| not_found(&e, "object database"))?;

    while let Some((tree_oid, prefix)) = stack.pop() {
      let tree = self.decoded_tree(repo, tree_oid)?;
      // Push subdirectories in reverse so the pop order is Git's tree order.
      // A caller sorting the result would not need this; a caller streaming into
      // a front-coded path table does.
      let mut children = Vec::new();
      for decoded in tree.entries() {
        let path = prefix.join(&decoded.name);
        match decoded.mode {
          mode::DIRECTORY => children.push((self.git_oid(&decoded.oid)?, path)),
          mode::REGULAR | mode::EXECUTABLE => {
            let oid = self.git_oid(&decoded.oid)?;
            let (size, _) = odb.read_header(oid).map_err(|e| not_found(&e, "blob"))?;
            visit(WalkEntry {
              path,
              mode: decoded.mode,
              oid: decoded.oid.clone(),
              size: size as u64,
            })?;
          }
          // Symlinks, gitlinks, and unsupported modes are not searchable
          // content. See `WalkEntry`: the corpus is chosen to agree with `rg`.
          _ => {}
        }
      }
      for child in children.into_iter().rev() {
        stack.push(child);
      }
    }
    Ok(())
  }

  fn walk_paths(
    &self,
    commit: &ObjectId,
    root: &BytePath,
    visit: &mut dyn FnMut(BytePath, u32) -> Result<(), GfsError>,
  ) -> Result<(), GfsError> {
    let pooled = self.checkout()?;
    let repo: &git2::Repository = &pooled;
    let commit_oid = self.git_oid(commit)?;

    let start = if root.is_empty() {
      self.find_commit(repo, commit_oid)?.tree_id()
    } else {
      let Some((parent, name)) = self.walk_to_parent(repo, commit, root)? else {
        return Err(GfsError::not_found("no such directory"));
      };
      let Some(entry) = parent.get(&name) else {
        return Err(GfsError::not_found("no such directory"));
      };
      if entry.mode != mode::DIRECTORY {
        return Err(GfsError::new(ErrorCode::InvalidArgument, "not a directory"));
      }
      self.git_oid(&entry.oid)?
    };

    // The same explicit stack as `walk_tree`, for the same two reasons its
    // comment gives. No object-database read here: a name query needs the tree
    // entries only, and a blob header per file would be one lookup per path.
    let mut stack: Vec<(git2::Oid, BytePath)> = vec![(start, root.clone())];
    while let Some((tree_oid, prefix)) = stack.pop() {
      let tree = self.decoded_tree(repo, tree_oid)?;
      let mut children = Vec::new();
      for decoded in tree.entries() {
        let path = prefix.join(&decoded.name);
        if decoded.mode == mode::DIRECTORY {
          children.push((self.git_oid(&decoded.oid)?, path));
        } else {
          // Every non-directory mode, including symlinks and gitlinks. A
          // filename search that dropped them would answer "no such file" about
          // a file that is right there.
          visit(path, decoded.mode)?;
        }
      }
      for child in children.into_iter().rev() {
        stack.push(child);
      }
    }
    Ok(())
  }

  fn diff_commits(&self, from: &ObjectId, to: &ObjectId) -> Result<Vec<TreeDelta>, GfsError> {
    let pooled = self.checkout()?;
    let repo: &git2::Repository = &pooled;
    let old_tree = {
      let oid = self.git_oid(from)?;
      let id = self.find_commit(repo, oid)?.tree_id();
      repo.find_tree(id).map_err(|e| not_found(&e, "tree"))?
    };
    let new_tree = {
      let oid = self.git_oid(to)?;
      let id = self.find_commit(repo, oid)?.tree_id();
      repo.find_tree(id).map_err(|e| not_found(&e, "tree"))?
    };

    let mut opts = git2::DiffOptions::new();
    // A type change is an upsert followed by nothing special, but libgit2 splits
    // it into a delete and an add unless asked not to. Either is workable; the
    // combined form is fewer deltas and no risk of the pair arriving out of order.
    opts.include_typechange(true);
    // Deliberately no rename detection. See `TreeDelta`.
    let diff = repo
      .diff_tree_to_tree(Some(&old_tree), Some(&new_tree), Some(&mut opts))
      .map_err(|e| not_found(&e, "tree diff"))?;

    let odb = repo.odb().map_err(|e| not_found(&e, "object database"))?;
    let mut out = Vec::new();
    for delta in diff.deltas() {
      let new_file = delta.new_file();
      let new_mode = git_mode(new_file.mode());
      let searchable_now =
        matches!(new_mode, mode::REGULAR | mode::EXECUTABLE) && new_file.exists();

      if searchable_now {
        let path = match new_file.path_bytes() {
          Some(p) => BytePath::new(p.to_vec()),
          None => continue,
        };
        let oid = self.to_oid(new_file.id())?;
        let (size, _) = odb
          .read_header(new_file.id())
          .map_err(|e| not_found(&e, "blob"))?;
        out.push(TreeDelta::Upserted {
          path,
          mode: new_mode,
          oid,
          size: size as u64,
        });
        continue;
      }

      // Not searchable in the new commit. If it was searchable in the old one,
      // the manifest has to drop it -- including the case where a file *became*
      // a symlink, which is a removal from the searchable corpus even though the
      // path still exists.
      let old_file = delta.old_file();
      let old_mode = git_mode(old_file.mode());
      if matches!(old_mode, mode::REGULAR | mode::EXECUTABLE) && old_file.exists() {
        if let Some(p) = old_file.path_bytes() {
          out.push(TreeDelta::Removed {
            path: BytePath::new(p.to_vec()),
          });
        }
      }
    }
    Ok(out)
  }

  fn diff(&self, request: &DiffRequest) -> Result<DiffOutput, GfsError> {
    let pooled = self.checkout()?;
    let repo: &git2::Repository = &pooled;

    let new_tree = self
      .find_commit(repo, self.git_oid(&request.to)?)?
      .tree()
      .map_err(|e| not_found(&e, "tree"))?;
    // `None` is the empty tree, which is what a root commit is diffed against.
    // libgit2 spells that as a null old side rather than as a special object.
    let old_tree = match &request.from {
      Some(from) => Some(
        self
          .find_commit(repo, self.git_oid(from)?)?
          .tree()
          .map_err(|e| not_found(&e, "tree"))?,
      ),
      None => None,
    };

    let mut opts = git2::DiffOptions::new();
    opts.include_typechange(true);
    opts.context_lines(request.context_lines);
    for path in &request.paths {
      opts.pathspec(as_path(path));
    }
    let mut diff = repo
      .diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), Some(&mut opts))
      .map_err(|e| not_found(&e, "tree diff"))?;
    // Rename detection **on**, unlike `diff_commits`. The two differ because
    // their audiences do: a manifest cannot use a rename and pays similarity
    // scoring for nothing, while a person reviewing a commit is far better
    // served by one `R` line than by a delete and an add of the same file.
    let mut find = git2::DiffFindOptions::new();
    find.renames(true);
    diff
      .find_similar(Some(&mut find))
      .map_err(|e| GfsError::internal(format!("detecting renames: {}", e.message())))?;

    let mut rendered = Vec::new();
    let mut truncated = false;
    // Keyed by the new-side path, which libgit2 sets on every delta including a
    // deletion, so one key identifies a delta across the two passes.
    let mut counts: std::collections::HashMap<Vec<u8>, (u32, u32)> =
      std::collections::HashMap::new();

    let render = matches!(request.format, DiffFormat::Patch);
    let count = matches!(request.format, DiffFormat::Patch | DiffFormat::Stat);
    if render || count {
      let result = diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
        if count {
          let key = delta
            .new_file()
            .path_bytes()
            .or_else(|| delta.old_file().path_bytes())
            .unwrap_or_default()
            .to_vec();
          let entry = counts.entry(key).or_insert((0, 0));
          match line.origin() {
            '+' => entry.0 += 1,
            '-' => entry.1 += 1,
            _ => {}
          }
        }
        if render {
          // libgit2 hands back the line's *content* without its origin marker,
          // so the `+`, `-` or space has to be written back or the patch does
          // not apply.
          if matches!(line.origin(), '+' | '-' | ' ') {
            rendered.push(line.origin() as u8);
          }
          rendered.extend_from_slice(line.content());
          if rendered.len() >= request.max_bytes {
            truncated = true;
            // Aborts the walk. libgit2 reports that as a user-cancelled error,
            // which is handled below rather than propagated: a truncated diff is
            // an answer, and an error here would discard what was already
            // rendered.
            return false;
          }
        }
        true
      });
      if let Err(e) = result {
        if !truncated {
          return Err(GfsError::internal(format!(
            "rendering a diff: {}",
            e.message()
          )));
        }
      }
    }

    match request.format {
      DiffFormat::Patch => {}
      DiffFormat::Stat => {
        let stats = diff
          .stats()
          .map_err(|e| GfsError::internal(format!("summarizing a diff: {}", e.message())))?;
        let buf = stats
          .to_buf(git2::DiffStatsFormat::FULL, STAT_WIDTH)
          .map_err(|e| GfsError::internal(format!("formatting a diff stat: {}", e.message())))?;
        rendered = buf.to_vec();
      }
      DiffFormat::NameStatus | DiffFormat::NameOnly => {
        let format = if matches!(request.format, DiffFormat::NameStatus) {
          git2::DiffFormat::NameStatus
        } else {
          git2::DiffFormat::NameOnly
        };
        let result = diff.print(format, |_delta, _hunk, line| {
          rendered.extend_from_slice(line.content());
          if rendered.len() >= request.max_bytes {
            truncated = true;
            return false;
          }
          true
        });
        if let Err(e) = result {
          if !truncated {
            return Err(GfsError::internal(format!(
              "listing changed paths: {}",
              e.message()
            )));
          }
        }
      }
    }
    if rendered.len() > request.max_bytes {
      rendered.truncate(request.max_bytes);
      truncated = true;
    }

    // Built after the print pass, because libgit2 only knows whether a file is
    // binary once it has loaded its content — before that the flag reads false
    // for everything.
    let mut files = Vec::new();
    for delta in diff.deltas() {
      let new_file = delta.new_file();
      let old_file = delta.old_file();
      let path = match new_file.path_bytes().or_else(|| old_file.path_bytes()) {
        Some(p) => BytePath::new(p.to_vec()),
        None => continue,
      };
      let status = diff_status(delta.status());
      let (additions, deletions) = counts.get(path.as_bytes()).copied().unwrap_or((0, 0));
      files.push(DiffFileChange {
        old_path: match status {
          DiffStatus::Renamed => old_file.path_bytes().map(|p| BytePath::new(p.to_vec())),
          _ => None,
        },
        path,
        status,
        additions,
        deletions,
        binary: delta.flags().is_binary(),
        old_mode: git_mode(old_file.mode()),
        new_mode: git_mode(new_file.mode()),
      });
    }

    Ok(DiffOutput {
      rendered,
      files,
      truncated,
    })
  }

  fn blame(
    &self,
    commit: &ObjectId,
    path: &BytePath,
    max_content_bytes: u64,
  ) -> Result<BlameOutput, GfsError> {
    let pooled = self.checkout()?;
    let repo: &git2::Repository = &pooled;
    let oid = self.git_oid(commit)?;
    let tree = self
      .find_commit(repo, oid)?
      .tree()
      .map_err(|e| not_found(&e, "tree"))?;

    let entry = tree
      .get_path(as_path(path))
      .map_err(|_| GfsError::not_found("no such path in this commit"))?;
    // `filemode()` is the raw Git mode already, so it is compared directly
    // rather than through `git_mode`, which maps libgit2's *enum*.
    if !matches!(entry.filemode() as u32, mode::REGULAR | mode::EXECUTABLE) {
      return Err(GfsError::invalid(
        "only a regular file can be blamed; this path is a directory, symlink, or submodule",
      ));
    }
    let blob = repo
      .find_blob(entry.id())
      .map_err(|e| not_found(&e, "blob"))?;
    if blob.is_binary() {
      return Err(GfsError::invalid(
        "this file is binary, so it has no lines to attribute",
      ));
    }
    let (content, truncated) = if blob.size() as u64 > max_content_bytes {
      (Vec::new(), true)
    } else {
      (blob.content().to_vec(), false)
    };

    let mut opts = git2::BlameOptions::new();
    // Without this the blame starts from HEAD, which in a bare mirror is the
    // default branch and not the commit the caller named.
    opts.newest_commit(oid);
    let blame = repo
      .blame_file(as_path(path), Some(&mut opts))
      .map_err(|e| not_found(&e, "blame"))?;

    let mut hunks = Vec::with_capacity(blame.len());
    for hunk in blame.iter() {
      let signature = hunk.final_signature();
      hunks.push(BlameHunk {
        final_start_line: hunk.final_start_line() as u32,
        lines: hunk.lines_in_hunk() as u32,
        commit: self.to_oid(hunk.final_commit_id())?,
        orig_path: hunk
          .path()
          .map(|p| {
            use std::os::unix::ffi::OsStrExt;
            BytePath::new(p.as_os_str().as_bytes().to_vec())
          })
          .unwrap_or_else(|| path.clone()),
        orig_start_line: hunk.orig_start_line() as u32,
        author: Self::signature_of(&signature),
        boundary: hunk.is_boundary(),
      });
    }

    Ok(BlameOutput {
      hunks,
      content,
      truncated,
    })
  }

  fn index_for_commit(
    &self,
    commit: &ObjectId,
    snapshot_time: gfs_types::Timestamp,
  ) -> Result<Vec<u8>, GfsError> {
    let pooled = self.checkout()?;
    let repo: &git2::Repository = &pooled;
    let commit_oid = self.git_oid(commit)?;
    let root = self.find_commit(repo, commit_oid)?.tree_id();
    let odb = repo
      .odb()
      .map_err(|e| GfsError::internal(format!("opening the object database: {e}")))?;

    // Recursive descent in tree-entry order, recursing into a directory at the
    // point it appears. That interleaving is what yields Git's index order --
    // byte-wise by full path -- because tree entries sort directories as
    // `name/`. The explicit-stack walks in this file visit a tree's files
    // first and its subtrees after, which is a *different* order, and an index
    // whose entries are out of order is silently misread; `write_index_v2`
    // asserts the order as a backstop.
    fn descend(
      this: &Libgit2Repository,
      repo: &git2::Repository,
      commit: &ObjectId,
      odb: &git2::Odb<'_>,
      tree_oid: git2::Oid,
      prefix: &gfs_types::BytePath,
      out: &mut Vec<crate::index::IndexEntry>,
    ) -> Result<(), GfsError> {
      let tree = this.decoded_tree(repo, tree_oid)?;
      for entry in tree.entries() {
        let path = prefix.join(&entry.name);
        if entry.mode == mode::DIRECTORY {
          descend(
            this,
            repo,
            commit,
            odb,
            this.git_oid(&entry.oid)?,
            &path,
            out,
          )?;
          continue;
        }
        // A gitlink names a commit in *another* repository; reading its header
        // here would fail on an object this database has no reason to hold.
        // Git compares a gitlink by the recorded OID, never by size.
        let size = if entry.mode == mode::GITLINK {
          0
        } else {
          let (size, _) = odb
            .read_header(this.git_oid(&entry.oid)?)
            .map_err(|e| not_found(&e, "blob"))?;
          size as u64
        };
        // An expanded LFS entry records the *expanded* size with the *pointer*
        // OID — exactly the index `git lfs pull` leaves behind (m05d). The
        // size is what stat-compares against the working tree; the OID is what
        // the clean filter's pointer answer must hash to.
        let size = match &this.lfs_check {
          Some(check) => {
            let kind = EntryKind::from_mode(entry.mode);
            match this.lfs_pointer_for(repo, commit, &path, kind, &entry.oid, size)? {
              Some(pointer) if check.contains(&pointer.oid) => pointer.size,
              _ => size,
            }
          }
          None => size,
        };
        out.push(crate::index::IndexEntry {
          path: path.as_bytes().to_vec(),
          mode: entry.mode,
          oid: entry.oid.clone(),
          size,
        });
      }
      Ok(())
    }

    let mut entries = Vec::new();
    descend(
      self,
      repo,
      commit,
      &odb,
      root,
      &gfs_types::BytePath::root(),
      &mut entries,
    )?;
    crate::index::write_index_v2(&entries, snapshot_time)
  }

  fn lfs_pointers(&self, commit: &ObjectId) -> Result<Vec<LfsEntry>, GfsError> {
    let pooled = self.checkout()?;
    let repo: &git2::Repository = &pooled;
    let commit_oid = self.git_oid(commit)?;
    let root = self.find_commit(repo, commit_oid)?.tree_id();
    let odb = repo.odb().map_err(|e| not_found(&e, "object database"))?;

    let mut out = Vec::new();
    let mut stack: Vec<(git2::Oid, BytePath)> = vec![(root, BytePath::root())];
    while let Some((tree_oid, prefix)) = stack.pop() {
      let tree = self.decoded_tree(repo, tree_oid)?;
      for decoded in tree.entries() {
        let path = prefix.join(&decoded.name);
        match decoded.mode {
          mode::DIRECTORY => stack.push((self.git_oid(&decoded.oid)?, path)),
          mode::REGULAR | mode::EXECUTABLE => {
            // The header-size gate makes this walk cheap on ordinary trees:
            // only pointer-sized blobs on `filter=lfs` paths are ever read.
            let (size, _) = odb
              .read_header(self.git_oid(&decoded.oid)?)
              .map_err(|e| not_found(&e, "blob"))?;
            let kind = EntryKind::from_mode(decoded.mode);
            if let Some(pointer) =
              self.lfs_pointer_for(repo, commit, &path, kind, &decoded.oid, size as u64)?
            {
              out.push(LfsEntry {
                path,
                blob_oid: decoded.oid.clone(),
                blob_size: size as u64,
                pointer,
              });
            }
          }
          _ => {}
        }
      }
    }
    Ok(out)
  }

  fn lfs_filtered(&self, commit: &ObjectId, path: &BytePath) -> Result<bool, GfsError> {
    let pooled = self.checkout()?;
    let repo: &git2::Repository = &pooled;
    let stack = self.attribute_stack(repo, commit, path)?;
    Ok(attributes::is_lfs(attributes::resolve_filter(
      &stack,
      path.as_bytes(),
    )))
  }

  fn read_blob(&self, blob: &ObjectId) -> Result<Vec<u8>, GfsError> {
    let pooled = self.checkout()?;
    let bytes = {
      let repo: &git2::Repository = &pooled;
      let oid = self.git_oid(blob)?;
      let found = repo.find_blob(oid).map_err(|e| not_found(&e, "blob"))?;
      let size = found.size() as u64;
      if size > limits::MAX_BLOB_BYTES {
        return Err(GfsError::new(
          ErrorCode::ResourceLimit,
          format!(
            "blob is larger than the {} byte limit",
            limits::MAX_BLOB_BYTES
          ),
        ));
      }
      let content = found.content().to_vec();

      // Verify the object ID against the content before returning it.
      //
      // DESIGN.md section 10 item 7 requires every cached object to be verified
      // cryptographically before publication, and this is the cheapest place to
      // do it: the bytes are already in memory, and verifying here means the
      // client cache, the blob endpoint, and the FUSE read path all inherit the
      // guarantee rather than each re-implementing it.
      let recomputed = git2::Oid::hash_object(git2::ObjectType::Blob, &content).map_err(|e| {
        GfsError::new(
          ErrorCode::Internal,
          format!("cannot hash blob for verification: {}", e.message()),
        )
      })?;
      if recomputed != oid {
        return Err(GfsError::new(
          ErrorCode::Internal,
          "blob content does not match its object id; the repository may be corrupt",
        ));
      }
      content
    };
    Ok(bytes)
  }

  fn blob_size(&self, blob: &ObjectId) -> Result<u64, GfsError> {
    let pooled = self.checkout()?;
    let size = {
      let repo: &git2::Repository = &pooled;
      let oid = self.git_oid(blob)?;
      let (size, kind) = repo
        .odb()
        .and_then(|odb| odb.read_header(oid))
        .map_err(|e| not_found(&e, "blob"))?;
      if kind != git2::ObjectType::Blob {
        return Err(GfsError::invalid("object is not a blob"));
      }
      size as u64
    };
    Ok(size)
  }

  fn visible_ref_targets(&self) -> Result<Vec<gfs_types::RefTarget>, GfsError> {
    let pooled = self.checkout()?;
    let mut out = Vec::new();
    {
      let repo: &git2::Repository = &pooled;
      let refs = repo
        .references()
        .map_err(|e| GfsError::new(ErrorCode::Internal, e.message().to_owned()))?;
      for r in refs {
        let r = r.map_err(|e| GfsError::new(ErrorCode::Internal, e.message().to_owned()))?;
        let Some(name) = r.name() else {
          // A ref whose name is not UTF-8 cannot be a valid Git ref name, so
          // skipping it is not data loss.
          continue;
        };
        // The internal namespace never appears in an enumeration a caller can
        // see, matching the upload-pack `hideRefs` policy. ADR 0002 records that
        // this prevents discovery, not access.
        if revision::is_reserved_ref(name) {
          continue;
        }
        // A symbolic ref such as HEAD has no direct target; resolve it.
        let target = match r.target() {
          Some(t) => t,
          None => match r.resolve() {
            Ok(resolved) => match resolved.target() {
              Some(t) => t,
              None => continue,
            },
            Err(_) => continue,
          },
        };
        // Peeled here, where the object database is local and open. A tag that
        // does not peel to a commit (a tag of a blob, which Git allows) is
        // carried unpeeled rather than dropped: the ref exists, and a caller
        // that only lists names must still see it.
        let peeled = match repo.find_tag(target) {
          Ok(tag) => match tag.peel() {
            Ok(object) if object.id() != target => Some(self.to_oid(object.id())?),
            _ => None,
          },
          Err(_) => None,
        };
        out.push(gfs_types::RefTarget {
          name: name.to_owned(),
          target: self.to_oid(target)?,
          peeled,
        });
      }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
  }

  fn reserved_refs(&self) -> Result<Vec<String>, GfsError> {
    let pooled = self.checkout()?;
    let mut out = Vec::new();
    {
      let repo: &git2::Repository = &pooled;
      let refs = repo
        .references_glob(&format!("{}**", revision::RESERVED_REF_PREFIX))
        .map_err(|e| GfsError::new(ErrorCode::Internal, e.message().to_owned()))?;
      for r in refs {
        let Ok(r) = r else { continue };
        let Some(name) = r.name() else { continue };
        // The glob is the fast path; the predicate is the correctness check, so a
        // glob that ever matches more than intended cannot widen the result.
        if revision::is_reserved_ref(name) {
          out.push(name.to_owned());
        }
      }
    }
    out.sort();
    Ok(out)
  }

  fn is_visible(&self, commit: &ObjectId) -> Result<bool, GfsError> {
    let pooled = self.checkout()?;
    let visible = {
      let repo: &git2::Repository = &pooled;
      let target = self.git_oid(commit)?;

      let mut tips = Vec::new();
      let refs = repo
        .references()
        .map_err(|e| GfsError::new(ErrorCode::Internal, e.message().to_owned()))?;
      for r in refs {
        let Ok(r) = r else { continue };
        let Some(name) = r.name() else { continue };
        if revision::is_reserved_ref(name) {
          continue;
        }
        // Peel: an annotated tag's ref target is the tag object, and reachability
        // has to be measured from the commit it points at.
        if let Ok(obj) = r.peel(git2::ObjectType::Commit) {
          tips.push(obj.id());
        }
      }

      // A direct tip match is the common case for a live branch and costs nothing.
      if tips.contains(&target) {
        true
      } else {
        // `graph_descendant_of` uses libgit2's commit-graph acceleration when a
        // commit-graph file is present. One call per visible ref is acceptable
        // because the alternative -- a revwalk over all tips -- traverses the
        // whole history, which is 1.4 million commits for the Linux corpus. The
        // server caches the answer per (commit, ref generation); see M1.5.
        let mut found = false;
        for tip in tips {
          if repo.graph_descendant_of(tip, target).unwrap_or(false) {
            found = true;
            break;
          }
        }
        found
      }
    };
    Ok(visible)
  }

  fn create_lease_anchor(&self, anchor_ref: &str, commit: &ObjectId) -> Result<(), GfsError> {
    if !revision::is_reserved_ref(anchor_ref) {
      // A lease anchor outside the reserved namespace would be advertised to
      // clients and pruned by upstream fetch. Refusing here means a bug in the
      // caller cannot produce a publicly visible ref.
      return Err(GfsError::invalid(
        "a lease anchor must be inside the reserved namespace",
      ));
    }
    let pooled = self.checkout()?;
    {
      let repo: &git2::Repository = &pooled;
      let oid = self.git_oid(commit)?;
      // The object must exist before it is anchored, or the anchor is a dangling
      // ref that keeps nothing reachable while looking like it does.
      self.find_commit(repo, oid)?;

      match repo.find_reference(anchor_ref) {
        Ok(existing) => {
          // Idempotent when it already points at the same commit, which makes a
          // retried CreateMount safe. A different commit is a mount-ID collision
          // and must not silently re-anchor another mount's lease.
          if existing.target() == Some(oid) {
            return Ok(());
          }
          return Err(GfsError::new(
            ErrorCode::Conflict,
            "a lease anchor with that mount id already exists for another commit",
          ));
        }
        Err(e) if e.code() == git2::ErrorCode::NotFound => {}
        Err(e) => {
          return Err(GfsError::new(
            ErrorCode::Internal,
            format!("cannot read lease anchor: {}", e.message()),
          ))
        }
      }

      repo
        .reference(
          anchor_ref,
          oid,
          // Never force. The existence check above already handled the only case
          // where an existing anchor is acceptable.
          false,
          "gfs mount retention lease",
        )
        .map_err(|e| {
          GfsError::new(
            ErrorCode::Internal,
            format!("cannot create lease anchor: {}", e.message()),
          )
        })?;
    }
    Ok(())
  }

  fn delete_lease_anchor(&self, anchor_ref: &str) -> Result<(), GfsError> {
    if !revision::is_reserved_ref(anchor_ref) {
      return Err(GfsError::invalid(
        "only refs inside the reserved namespace may be deleted through this path",
      ));
    }
    let pooled = self.checkout()?;
    {
      let repo: &git2::Repository = &pooled;
      match repo.find_reference(anchor_ref) {
        // Already gone. Idempotent so release and restart reconciliation can both
        // run without coordinating.
        Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(()),
        Err(e) => {
          return Err(GfsError::new(
            ErrorCode::Internal,
            format!("cannot read lease anchor: {}", e.message()),
          ))
        }
        Ok(mut r) => r.delete().map_err(|e| {
          GfsError::new(
            ErrorCode::Internal,
            format!("cannot delete lease anchor: {}", e.message()),
          )
        })?,
      }
    }
    Ok(())
  }

  fn read_lease_anchor(&self, anchor_ref: &str) -> Result<Option<ObjectId>, GfsError> {
    let pooled = self.checkout()?;
    let found = {
      let repo: &git2::Repository = &pooled;
      match repo.find_reference(anchor_ref) {
        Err(e) if e.code() == git2::ErrorCode::NotFound => None,
        Err(e) => {
          return Err(GfsError::new(
            ErrorCode::Internal,
            format!("cannot read lease anchor: {}", e.message()),
          ))
        }
        Ok(r) => match r.target() {
          Some(t) => Some(self.to_oid(t)?),
          None => None,
        },
      }
    };
    Ok(found)
  }

  fn tree_cache_stats(&self) -> TreeCacheStats {
    self.trees.stats()
  }

  fn write_tree(
    &self,
    base: Option<&ObjectId>,
    changes: &[TreeChange],
  ) -> Result<ObjectId, GfsError> {
    let pooled = self.checkout()?;
    let repo: &git2::Repository = &pooled;

    let mut builder = git2::build::TreeUpdateBuilder::new();
    // Blobs are written first and in one pass, because `TreeUpdateBuilder` needs
    // an object ID per upsert and writing a blob mid-build would interleave
    // object writes with tree construction for no benefit.
    for change in changes {
      let path = std::path::Path::new(
        std::str::from_utf8(change.path.as_bytes())
          // libgit2's tree builder takes a `Path`, which on this platform is
          // bytes -- but the API is `&str`-shaped, so a non-UTF-8 path cannot go
          // through it. Refused by name rather than lossily converted, which
          // would write the file to a *different* path than the caller asked
          // for and report success.
          .map_err(|_| {
            GfsError::invalid(format!(
              "cannot commit the non-UTF-8 path {:?}; this is a known limitation",
              String::from_utf8_lossy(change.path.as_bytes())
            ))
          })?,
      );
      match &change.kind {
        TreeChangeKind::Upsert { mode, content } => {
          let filemode = to_filemode(*mode)?;
          let blob = repo.blob(content).map_err(|e| {
            GfsError::new(
              ErrorCode::Internal,
              format!("cannot write blob: {}", e.message()),
            )
          })?;
          builder.upsert(path, blob, filemode);
        }
        TreeChangeKind::Delete => {
          builder.remove(path);
        }
      }
    }

    let baseline = match base {
      Some(commit) => {
        let oid = self.git_oid(commit)?;
        self.find_commit(repo, oid)?.tree().map_err(|e| {
          GfsError::new(
            ErrorCode::Internal,
            format!("cannot read the base tree: {}", e.message()),
          )
        })?
      }
      None => {
        let empty = repo
          .treebuilder(None)
          .and_then(|b| b.write())
          .map_err(|e| {
            GfsError::new(
              ErrorCode::Internal,
              format!("cannot write the empty tree: {}", e.message()),
            )
          })?;
        repo.find_tree(empty).map_err(|e| {
          GfsError::new(
            ErrorCode::Internal,
            format!("cannot read the empty tree: {}", e.message()),
          )
        })?
      }
    };

    let written = builder.create_updated(repo, &baseline).map_err(|e| {
      GfsError::new(
        ErrorCode::Internal,
        format!("cannot write tree: {}", e.message()),
      )
    })?;
    self.to_oid(written)
  }

  fn create_commit(
    &self,
    tree: &ObjectId,
    parents: &[ObjectId],
    author: &CommitSignature,
    committer: &CommitSignature,
    message: &str,
  ) -> Result<ObjectId, GfsError> {
    let pooled = self.checkout()?;
    let repo: &git2::Repository = &pooled;

    let tree_oid = self.git_oid(tree)?;
    let tree = repo.find_tree(tree_oid).map_err(|e| {
      GfsError::new(
        ErrorCode::Internal,
        format!("cannot read tree: {}", e.message()),
      )
    })?;

    let parent_commits = parents
      .iter()
      .map(|p| {
        let oid = self.git_oid(p)?;
        self.find_commit(repo, oid)
      })
      .collect::<Result<Vec<_>, _>>()?;
    let parent_refs: Vec<&git2::Commit<'_>> = parent_commits.iter().collect();

    let author = to_signature(author)?;
    let committer = to_signature(committer)?;

    // `None` for the ref: the commit is created detached, and moving a branch to
    // it is a separate compare-and-swap. Doing both here would make the update
    // unconditional, which is the lost-commit race `update_work_ref` exists to
    // prevent.
    let oid = repo
      .commit(None, &author, &committer, message, &tree, &parent_refs)
      .map_err(|e| {
        GfsError::new(
          ErrorCode::Internal,
          format!("cannot write commit: {}", e.message()),
        )
      })?;
    self.to_oid(oid)
  }

  fn update_work_ref(
    &self,
    name: &str,
    new: &ObjectId,
    expected: Option<&ObjectId>,
  ) -> Result<(), GfsError> {
    if !revision::is_reserved_ref(name) {
      // The rule that keeps a work branch alive. `refs/heads/*` is a fetch
      // destination and the mirror fetch prunes it, so a branch written there
      // that upstream does not have is deleted by the next sync -- taking the
      // reachability of every commit on it.
      return Err(GfsError::invalid(format!(
        "{name} is outside the reserved namespace; unpushed work must live \
         there or the next upstream fetch prunes it"
      )));
    }
    let pooled = self.checkout()?;
    let repo: &git2::Repository = &pooled;

    let new_oid = self.git_oid(new)?;
    // The object must exist before a ref names it, for the same reason a lease
    // anchor checks: a dangling ref keeps nothing reachable while looking as
    // though it does.
    self.find_commit(repo, new_oid)?;

    match expected {
      Some(old) => {
        let old_oid = self.git_oid(old)?;
        repo
          .reference_matching(name, new_oid, true, old_oid, "gfs: commit")
          .map_err(|e| match e.code() {
            // libgit2 reports a failed match as a generic error; the caller
            // needs to tell "someone else committed" from "the repository is
            // broken", because the first is retryable and the second is not.
            git2::ErrorCode::NotFound | git2::ErrorCode::Modified => GfsError::new(
              ErrorCode::Conflict,
              format!("{name} moved while this commit was being made; retry"),
            ),
            _ => GfsError::new(
              ErrorCode::Internal,
              format!("cannot update {name}: {}", e.message()),
            ),
          })?;
      }
      None => {
        if repo.find_reference(name).is_ok() {
          return Err(GfsError::new(
            ErrorCode::Conflict,
            format!("{name} already exists"),
          ));
        }
        repo
          .reference(name, new_oid, false, "gfs: create")
          .map_err(|e| {
            GfsError::new(
              ErrorCode::Internal,
              format!("cannot create {name}: {}", e.message()),
            )
          })?;
      }
    }
    Ok(())
  }

  fn read_ref(&self, name: &str) -> Result<Option<ObjectId>, GfsError> {
    let pooled = self.checkout()?;
    // The inner scope this file's header describes: every libgit2 borrow ends
    // before the pooled handle's `Drop` returns it, and only owned values leave.
    let found = {
      let repo: &git2::Repository = &pooled;
      match repo.find_reference(name) {
        Ok(reference) => {
          let object = reference.peel(git2::ObjectType::Commit).map_err(|e| {
            GfsError::new(
              ErrorCode::Internal,
              format!("cannot peel {name}: {}", e.message()),
            )
          })?;
          Some(self.to_oid(object.id())?)
        }
        Err(e) if e.code() == git2::ErrorCode::NotFound => None,
        Err(e) => {
          return Err(GfsError::new(
            ErrorCode::Internal,
            format!("cannot read {name}: {}", e.message()),
          ))
        }
      }
    };
    Ok(found)
  }
}

/// A Git file mode as libgit2's enum, refusing what a commit cannot carry.
fn to_filemode(mode: u32) -> Result<git2::FileMode, GfsError> {
  match mode {
    gfs_types::mode::REGULAR => Ok(git2::FileMode::Blob),
    gfs_types::mode::EXECUTABLE => Ok(git2::FileMode::BlobExecutable),
    gfs_types::mode::SYMLINK => Ok(git2::FileMode::Link),
    gfs_types::mode::GITLINK => Ok(git2::FileMode::Commit),
    // Not `Tree`: a directory is implied by the paths under it, and accepting
    // one here would let a caller replace a subtree with an empty entry.
    other => Err(GfsError::invalid(format!(
      "{other:o} is not a file mode a commit can carry"
    ))),
  }
}

fn to_signature(sig: &CommitSignature) -> Result<git2::Signature<'static>, GfsError> {
  let when = git2::Time::new(sig.when_secs, sig.offset_minutes);
  git2::Signature::new(&sig.name, &sig.email, &when).map_err(|e| {
    // Git forbids `<`, `>` and newlines in these fields, and a name that carried
    // one would produce a commit object that stock Git refuses to parse.
    GfsError::invalid(format!("not a usable Git identity: {}", e.message()))
  })
}

/// Peel an object to a commit, refusing anything that does not reach one.
///
/// M0.3 found the concrete case this matters for: the Linux kernel's `v2.6.11`
/// tag peels to a **tree**, not a commit. ADR 0006 records the decision to reject
/// it with a typed error rather than resolve it, because returning a tree OID
/// where every downstream layer expects a commit produces a snapshot nobody can
/// read -- and the failure would surface far from its cause.
fn peel_to_commit(object: git2::Object<'_>) -> Result<git2::Commit<'_>, GfsError> {
  let kind = object.kind();
  object.peel_to_commit().map_err(|_| {
    GfsError::invalid(format!(
      "revision resolves to a {} rather than a commit",
      kind.map(|k| k.str()).unwrap_or("object of unknown type")
    ))
  })
}
