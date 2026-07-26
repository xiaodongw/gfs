//! The libgit2-backed [`GitRepository`] implementation.
//!
//! Every method follows the same shape, dictated by the FFI lifetimes: check out a
//! pooled handle, do all libgit2 work inside an inner scope so every borrow ends
//! before the handle is returned by its `Drop`, and hand back owned values. The
//! compiler enforces that discipline, which is why the inner scopes are written
//! out rather than relying on care.

use std::sync::Arc;

use xvfs_types::error::{ErrorCode, XvfsError};
use xvfs_types::revision::{self, RevisionSelector};
use xvfs_types::{
  limits, mode, BytePath, CommitMeta, EntryKind, HashAlgorithm, ObjectId, ResolvedRevision,
  Signature, Timestamp, TreeEntryInfo,
};

use crate::format::{self, RepositoryFormat};
use crate::pool::{PooledRepo, RepoPool};
use crate::repository::{DirectoryPage, EntryLookup, GitRepository};
use crate::tree::{DecodedEntry, DecodedTree, TreeCache, TreeCacheStats};

/// The tree containing a path, plus that path's final component.
///
/// `None` when any intermediate component is missing or is not a directory,
/// which is an ordinary negative lookup rather than a failure.
type ParentTree = Option<(Arc<DecodedTree>, Vec<u8>)>;

pub struct Libgit2Repository {
  pool: Arc<RepoPool>,
  format: RepositoryFormat,
  trees: TreeCache,
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
  ) -> Result<Self, XvfsError> {
    let path = path.as_ref();
    // Format first: a `reftable` repository must produce the format verdict, not
    // a generic libgit2 open failure, and `format::check` reads config directly
    // so it works even when libgit2 cannot open the repository at all.
    let format = format::check(path)?;
    Ok(Libgit2Repository {
      pool: RepoPool::open(path, max_handles)?,
      format,
      trees: TreeCache::new(tree_cache_bytes),
    })
  }

  pub fn path(&self) -> &std::path::Path {
    self.pool.path()
  }

  fn to_oid(&self, oid: git2::Oid) -> Result<ObjectId, XvfsError> {
    // The algorithm comes from the validated repository format, not from the
    // length of whatever OID libgit2 happened to produce. The M0 spike derived it
    // from HEAD's OID length, which reports SHA-1 for an empty repository whose
    // config says otherwise -- harmless there, but it makes the algorithm a
    // property of the data rather than of the repository.
    ObjectId::from_raw(self.format.algorithm, oid.as_bytes()).map_err(XvfsError::from)
  }

  fn git_oid(&self, id: &ObjectId) -> Result<git2::Oid, XvfsError> {
    if id.algorithm() != self.format.algorithm {
      return Err(XvfsError::invalid(format!(
        "object id is {} but this repository is {}",
        id.algorithm(),
        self.format.algorithm
      )));
    }
    git2::Oid::from_bytes(id.as_bytes()).map_err(|e| XvfsError::invalid(e.message().to_owned()))
  }

  fn checkout(&self) -> Result<PooledRepo, XvfsError> {
    self.pool.checkout()
  }

  /// Decode a tree, using the cache when possible.
  fn decoded_tree(
    &self,
    repo: &git2::Repository,
    tree_oid: git2::Oid,
  ) -> Result<Arc<DecodedTree>, XvfsError> {
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
  ) -> Result<ParentTree, XvfsError> {
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

  /// Build the API-level entry for one decoded tree entry.
  fn entry_info(
    &self,
    repo: &git2::Repository,
    path: BytePath,
    decoded: &DecodedEntry,
  ) -> Result<TreeEntryInfo, XvfsError> {
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
    Ok(TreeEntryInfo {
      path,
      kind,
      mode: decoded.mode,
      oid: decoded.oid.clone(),
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
  ) -> Result<Option<TreeEntryInfo>, XvfsError> {
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
    Ok(Some(self.entry_info(repo, path.clone(), decoded)?))
  }

  fn find_commit<'r>(
    &self,
    repo: &'r git2::Repository,
    oid: git2::Oid,
  ) -> Result<git2::Commit<'r>, XvfsError> {
    repo.find_commit(oid).map_err(|e| not_found(&e, "commit"))
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

/// Map a libgit2 error, distinguishing a genuine absence from a real failure.
///
/// libgit2 reports both "no such object" and "the pack is corrupt" through
/// `git2::Error`, and collapsing them would make a corrupted repository look like
/// an empty one -- which is how a data-loss incident gets reported as a
/// not-found.
fn not_found(e: &git2::Error, what: &str) -> XvfsError {
  match e.code() {
    git2::ErrorCode::NotFound => XvfsError::not_found(format!("no such {what}")),
    _ => XvfsError::new(
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

  fn resolve(&self, selector: &RevisionSelector) -> Result<ResolvedRevision, XvfsError> {
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
          git2::ErrorCode::NotFound => XvfsError::not_found("no object matches that abbreviation"),
          // libgit2 reports an ambiguous prefix distinctly. Reporting it rather
          // than picking a candidate is the whole reason abbreviations have a
          // minimum length.
          git2::ErrorCode::Ambiguous => {
            XvfsError::invalid("abbreviated object id is ambiguous; supply more characters")
          }
          _ => XvfsError::invalid(format!("cannot resolve abbreviation: {}", e.message())),
        })?;
        let commit = peel_to_commit(object)?;
        (commit, None)
      }
      RevisionSelector::FullRef(name) | RevisionSelector::ShortName(name) => {
        // Belt and braces. The selector grammar already rejected the reserved
        // namespace in both spellings, and this is the layer that would actually
        // hand out a lease anchor if that ever regressed.
        if revision::is_reserved_ref(name) {
          return Err(XvfsError::new(
            ErrorCode::ReservedNamespace,
            "that ref is in the reserved internal namespace",
          ));
        }
        let reference =
          repo
            .resolve_reference_from_short_name(name)
            .map_err(|e| match e.code() {
              git2::ErrorCode::NotFound => XvfsError::not_found("no such ref"),
              _ => XvfsError::invalid(format!("cannot resolve ref: {}", e.message())),
            })?;
        let resolved_name = reference.name().map(str::to_owned);
        if resolved_name
          .as_deref()
          .is_some_and(revision::is_reserved_ref)
        {
          // The short-name case: `xvfs/mounts/x` resolves to
          // `refs/xvfs/mounts/x`. Caught at the *resolved* name, so it holds even
          // for a spelling the parser did not anticipate.
          return Err(XvfsError::new(
            ErrorCode::ReservedNamespace,
            "that ref is in the reserved internal namespace",
          ));
        }
        let object = reference
          .peel(git2::ObjectType::Any)
          .map_err(|e| XvfsError::invalid(format!("cannot peel ref: {}", e.message())))?;
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

  fn read_commit(&self, commit: &ObjectId) -> Result<CommitMeta, XvfsError> {
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
  ) -> Result<DirectoryPage, XvfsError> {
    let limit = limit.clamp(1, limits::MAX_DIRECTORY_PAGE_SIZE);
    let pooled = self.checkout()?;
    let page = {
      let repo: &git2::Repository = &pooled;
      let commit_oid = self.git_oid(commit)?;

      let tree = if path.is_empty() {
        let root = self.find_commit(repo, commit_oid)?.tree_id();
        self.decoded_tree(repo, root)?
      } else {
        let Some((parent, name)) = self.walk_to_parent(repo, commit, path)? else {
          return Err(XvfsError::not_found("no such directory"));
        };
        let Some(entry) = parent.get(&name) else {
          return Err(XvfsError::not_found("no such directory"));
        };
        match entry.mode {
          mode::DIRECTORY => {
            let child = self.git_oid(&entry.oid)?;
            self.decoded_tree(repo, child)?
          }
          // DESIGN.md section 8.2 presents a gitlink as an empty read-only
          // directory. Returning an empty page keeps that rule in one place
          // instead of making every caller special-case it, and XVFS genuinely
          // has nothing to list: a submodule's contents live in another
          // repository.
          mode::GITLINK => {
            return Ok(DirectoryPage {
              entries: Vec::new(),
              next_page_token: None,
            })
          }
          _ => {
            return Err(XvfsError::new(
              ErrorCode::InvalidArgument,
              "not a directory",
            ))
          }
        }
      };

      let (page, next_page_token) = tree.page(after, limit);
      let mut entries = Vec::with_capacity(page.len());
      for decoded in page {
        entries.push(self.entry_info(repo, path.join(&decoded.name), decoded)?);
      }
      DirectoryPage {
        entries,
        next_page_token,
      }
    };
    Ok(page)
  }

  fn read_blob(&self, blob: &ObjectId) -> Result<Vec<u8>, XvfsError> {
    let pooled = self.checkout()?;
    let bytes = {
      let repo: &git2::Repository = &pooled;
      let oid = self.git_oid(blob)?;
      let found = repo.find_blob(oid).map_err(|e| not_found(&e, "blob"))?;
      let size = found.size() as u64;
      if size > limits::MAX_BLOB_BYTES {
        return Err(XvfsError::new(
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
        XvfsError::new(
          ErrorCode::Internal,
          format!("cannot hash blob for verification: {}", e.message()),
        )
      })?;
      if recomputed != oid {
        return Err(XvfsError::new(
          ErrorCode::Internal,
          "blob content does not match its object id; the repository may be corrupt",
        ));
      }
      content
    };
    Ok(bytes)
  }

  fn blob_size(&self, blob: &ObjectId) -> Result<u64, XvfsError> {
    let pooled = self.checkout()?;
    let size = {
      let repo: &git2::Repository = &pooled;
      let oid = self.git_oid(blob)?;
      let (size, kind) = repo
        .odb()
        .and_then(|odb| odb.read_header(oid))
        .map_err(|e| not_found(&e, "blob"))?;
      if kind != git2::ObjectType::Blob {
        return Err(XvfsError::invalid("object is not a blob"));
      }
      size as u64
    };
    Ok(size)
  }

  fn visible_refs(&self) -> Result<Vec<(String, ObjectId)>, XvfsError> {
    let pooled = self.checkout()?;
    let mut out = Vec::new();
    {
      let repo: &git2::Repository = &pooled;
      let refs = repo
        .references()
        .map_err(|e| XvfsError::new(ErrorCode::Internal, e.message().to_owned()))?;
      for r in refs {
        let r = r.map_err(|e| XvfsError::new(ErrorCode::Internal, e.message().to_owned()))?;
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
        out.push((name.to_owned(), self.to_oid(target)?));
      }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
  }

  fn reserved_refs(&self) -> Result<Vec<String>, XvfsError> {
    let pooled = self.checkout()?;
    let mut out = Vec::new();
    {
      let repo: &git2::Repository = &pooled;
      let refs = repo
        .references_glob(&format!("{}**", revision::RESERVED_REF_PREFIX))
        .map_err(|e| XvfsError::new(ErrorCode::Internal, e.message().to_owned()))?;
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

  fn is_visible(&self, commit: &ObjectId) -> Result<bool, XvfsError> {
    let pooled = self.checkout()?;
    let visible = {
      let repo: &git2::Repository = &pooled;
      let target = self.git_oid(commit)?;

      let mut tips = Vec::new();
      let refs = repo
        .references()
        .map_err(|e| XvfsError::new(ErrorCode::Internal, e.message().to_owned()))?;
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

  fn create_lease_anchor(&self, anchor_ref: &str, commit: &ObjectId) -> Result<(), XvfsError> {
    if !revision::is_reserved_ref(anchor_ref) {
      // A lease anchor outside the reserved namespace would be advertised to
      // clients and pruned by upstream fetch. Refusing here means a bug in the
      // caller cannot produce a publicly visible ref.
      return Err(XvfsError::invalid(
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
          return Err(XvfsError::new(
            ErrorCode::Conflict,
            "a lease anchor with that mount id already exists for another commit",
          ));
        }
        Err(e) if e.code() == git2::ErrorCode::NotFound => {}
        Err(e) => {
          return Err(XvfsError::new(
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
          "xvfs mount retention lease",
        )
        .map_err(|e| {
          XvfsError::new(
            ErrorCode::Internal,
            format!("cannot create lease anchor: {}", e.message()),
          )
        })?;
    }
    Ok(())
  }

  fn delete_lease_anchor(&self, anchor_ref: &str) -> Result<(), XvfsError> {
    if !revision::is_reserved_ref(anchor_ref) {
      return Err(XvfsError::invalid(
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
          return Err(XvfsError::new(
            ErrorCode::Internal,
            format!("cannot read lease anchor: {}", e.message()),
          ))
        }
        Ok(mut r) => r.delete().map_err(|e| {
          XvfsError::new(
            ErrorCode::Internal,
            format!("cannot delete lease anchor: {}", e.message()),
          )
        })?,
      }
    }
    Ok(())
  }

  fn read_lease_anchor(&self, anchor_ref: &str) -> Result<Option<ObjectId>, XvfsError> {
    let pooled = self.checkout()?;
    let found = {
      let repo: &git2::Repository = &pooled;
      match repo.find_reference(anchor_ref) {
        Err(e) if e.code() == git2::ErrorCode::NotFound => None,
        Err(e) => {
          return Err(XvfsError::new(
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
}

/// Peel an object to a commit, refusing anything that does not reach one.
///
/// M0.3 found the concrete case this matters for: the Linux kernel's `v2.6.11`
/// tag peels to a **tree**, not a commit. ADR 0006 records the decision to reject
/// it with a typed error rather than resolve it, because returning a tree OID
/// where every downstream layer expects a commit produces a snapshot nobody can
/// read -- and the failure would surface far from its cause.
fn peel_to_commit(object: git2::Object<'_>) -> Result<git2::Commit<'_>, XvfsError> {
  let kind = object.kind();
  object.peel_to_commit().map_err(|_| {
    XvfsError::invalid(format!(
      "revision resolves to a {} rather than a commit",
      kind.map(|k| k.str()).unwrap_or("object of unknown type")
    ))
  })
}
