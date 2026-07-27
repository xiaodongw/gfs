//! The crash-safe copy-on-write overlay.
//!
//! DESIGN.md section 6.4 fixes the model: the mounted base is immutable, and
//! every local mutation is recorded in a per-job overlay — a journal of path
//! state, content files for created and copied-up data, and whiteouts for deleted
//! base paths. This crate is that overlay. It is deliberately network-free and
//! deliberately synchronous: it knows nothing about gRPC, blobs, or FUSE, and its
//! caller reaches it from async code through `spawn_blocking`, the same way
//! `xvfs-server` reaches its catalog.
//!
//! # The base is a parameter, not a dependency
//!
//! Every mutation that needs to know what the pinned commit holds at a path is
//! *told*, through [`BaseFacts`]. That is what keeps the crate synchronous while
//! still letting it own the whole state machine: the FUSE layer has already
//! resolved the path (it had to, to answer `lookup`), so passing the answer down
//! costs nothing and buys a library that a property test can drive against an
//! in-memory base tree.
//!
//! The one thing the overlay does *not* validate is whether an ancestor exists in
//! the base. The kernel resolves a parent inode before it ever issues `create`,
//! and re-deriving that would require the network the crate is arranged to avoid.
//! Ancestors the overlay itself knows about — whiteouts, non-directories, opaque
//! directories — are checked here.
//!
//! # Memory is the index; SQLite is the truth
//!
//! Every row is held in memory and mirrored to `overlay.sqlite`. Reads never
//! touch the database. A mutation writes the transaction, commits it, and only
//! then updates memory, so a crash mid-transaction leaves neither. The entry
//! count is bounded by what one job edits rather than by the size of the
//! repository, which is what makes holding all of it affordable — a monorepo has
//! millions of paths and a job changes thousands.
//!
//! # Ordering
//!
//! Content is published before the journal row that names it, and released after
//! the journal row that named it is gone. See [`store`] for the sequence and the
//! invariant it maintains.

pub mod diff;
pub mod error;
pub mod export;
pub mod fault;
pub mod hash;
pub mod journal;
pub mod merge;
pub mod model;
pub mod state;
pub mod status;
pub mod store;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use xvfs_types::{BytePath, Timestamp};

pub use error::{Condition, OverlayError, Result};

pub use export::{ExportReport, Exporter};
pub use journal::{Binding, Change, Journal, OVERLAY_FORMAT_VERSION};
pub use merge::{BaseChild, Resolution};
pub use state::{
  ancestors_of, is_within, parent_of, BaseFacts, Content, OverlayEntry, OverlayKind,
};
pub use status::{ChangeKind, Status};
pub use store::{ContentStore, SweepReport};

/// Validate a path for a syscall caller, splitting "too long" from "malformed".
///
/// [`BytePath::validate`] answers in the *service* vocabulary, where both are
/// `InvalidArgument`. A syscall caller needs `ENAMETOOLONG` for one and `EINVAL`
/// for the other, so the length question is asked first and separately.
///
/// Every overlay entry point that takes a caller-supplied path goes through
/// here. Calling `validate` directly is what produced the bug this replaces: a
/// 4 097-byte path reached `open` as `EIO`.
pub fn path_condition(path: &BytePath) -> Result<()> {
  if path.exceeds_length_limits() {
    return Err(OverlayError::name_too_long(format!(
      "path is longer than this filesystem can represent: {}",
      path.escaped()
    )));
  }
  path.validate()?;
  Ok(())
}


/// Where overlay inode numbers start.
///
/// Base inodes are handed out from 2 upward by the client's inode table, and a
/// mount that walks a monorepo will use millions of them. Overlay numbers come
/// from `2^48` so the two ranges cannot meet, which means a created file's inode
/// number is stable without the two allocators having to coordinate.
///
/// A **copied-up** path keeps the number it already had. An editor that stats a
/// file, writes it, and stats it again must see one identity; changing the number
/// under it is how "the file was replaced" gets reported for an ordinary write.
pub const OVERLAY_INO_BASE: u64 = 1 << 48;

#[derive(Clone, Debug)]
pub struct OverlayConfig {
  /// The per-job overlay quota, in bytes of local content.
  pub quota_bytes: u64,
  /// The most journal rows one `rename` of a base directory may materialize.
  ///
  /// A directory rename is metadata-only — the moved entries still point at base
  /// blobs — but it is proportional to the subtree, and renaming the root of a
  /// monorepo would write millions of rows before the caller could interrupt it.
  pub max_rename_entries: usize,
}

impl Default for OverlayConfig {
  fn default() -> Self {
    OverlayConfig {
      quota_bytes: 4 << 30,
      max_rename_entries: 1_000_000,
    }
  }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OverlayStats {
  /// Rows in the journal, whiteouts included.
  pub entries: u64,
  /// Rows that hide a base path.
  pub whiteouts: u64,
  /// Bytes of local content.
  pub local_bytes: u64,
  pub quota_bytes: u64,
}

/// Bytes for a copy-up, or the absence of them.
pub enum Source<'a> {
  /// `O_TRUNC` and `create`: nothing to copy, so nothing is fetched.
  Empty,
  /// The base blob, streamed rather than buffered.
  Reader(&'a mut dyn std::io::Read),
}

impl std::fmt::Debug for Source<'_> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Source::Empty => f.write_str("Empty"),
      Source::Reader(_) => f.write_str("Reader(..)"),
    }
  }
}

/// A base directory's descendants, supplied by the caller for a directory rename.
#[derive(Clone, Debug)]
pub struct BaseDescendant {
  /// Relative to the directory being renamed.
  pub relative: BytePath,
  pub facts: BaseFacts,
  pub symlink_target: Option<Vec<u8>>,
}

/// Drop a content id's owner only if it is still the path being removed.
///
/// A rename is one transaction of `Put(new)` then `Delete(old)`, and both mention
/// the same content id. Removing the index entry unconditionally on the delete
/// would erase the mapping the put had just established, and every subsequent
/// write through the still-open descriptor would land in a content file no row
/// could be found for -- so the size would stop advancing and a read through a
/// fresh descriptor would stop short.
fn release_content(index: &mut HashMap<u64, Vec<u8>>, id: u64, path: &[u8]) {
  if index.get(&id).is_some_and(|owner| owner == path) {
    index.remove(&id);
  }
}

struct Inner {
  journal: Journal,
  entries: BTreeMap<Vec<u8>, OverlayEntry>,
  children: HashMap<Vec<u8>, BTreeSet<Vec<u8>>>,
  /// Content id to the path that owns it.
  ///
  /// A write arrives with a descriptor, not a path — that is what makes an open
  /// file survive a rename, and what makes an unlinked one still writable — so
  /// the journal has to be reachable from the content id alone.
  by_content: HashMap<u64, Vec<u8>>,
  next_ino: u64,
  next_content_id: u64,
  local_bytes: u64,
  /// The overlay logical clock: the last timestamp issued.
  clock: Timestamp,
}

pub struct Overlay {
  inner: Mutex<Inner>,
  store: ContentStore,
  config: OverlayConfig,
  snapshot_time: Timestamp,
  state_dir: PathBuf,
  recovery: SweepReport,
}

impl std::fmt::Debug for Overlay {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Overlay")
      .field("state_dir", &self.state_dir)
      .field("snapshot_time", &self.snapshot_time)
      .finish_non_exhaustive()
  }
}

impl Overlay {
  /// Open or create the overlay for one mount, recovering whatever a previous
  /// process left behind.
  pub fn open(
    state_dir: &Path,
    binding: &Binding,
    snapshot_time: Timestamp,
    config: OverlayConfig,
  ) -> Result<Self> {
    std::fs::create_dir_all(state_dir)
      .map_err(|e| OverlayError::io(format!("creating the overlay directory: {e}")))?;
    let journal = Journal::open(&state_dir.join(journal::OVERLAY_DB_FILE), binding)?;
    let store = ContentStore::open(state_dir)?;

    let loaded = journal.load()?;
    let referenced: HashSet<u64> = loaded.iter().filter_map(|e| e.content.local_id()).collect();
    let recovery = store.sweep(&referenced)?;
    let local_bytes = store.total_bytes(&referenced);

    let mut entries = BTreeMap::new();
    let mut children: HashMap<Vec<u8>, BTreeSet<Vec<u8>>> = HashMap::new();
    let mut by_content: HashMap<u64, Vec<u8>> = HashMap::new();
    // The clock never runs backwards across a restart. Seeded from the highest
    // time any surviving entry carries, so a mutation after recovery is still
    // newer than every mutation before it even if the host clock moved back.
    let mut clock = snapshot_time;
    for entry in loaded {
      clock = clock.max(entry.mtime).max(entry.ctime);
      children
        .entry(entry.parent().into_bytes())
        .or_default()
        .insert(entry.name());
      if let Some(id) = entry.content.local_id() {
        by_content.insert(id, entry.path.as_bytes().to_vec());
      }
      entries.insert(entry.path.as_bytes().to_vec(), entry);
    }

    let next_ino = journal.next_ino()?.max(OVERLAY_INO_BASE);
    let next_content_id = journal.next_content_id()?.max(1);

    Ok(Overlay {
      inner: Mutex::new(Inner {
        journal,
        entries,
        children,
        by_content,
        next_ino,
        next_content_id,
        local_bytes,
        clock,
      }),
      store,
      config,
      snapshot_time,
      state_dir: state_dir.to_path_buf(),
      recovery,
    })
  }

  /// What the open sweep found. Empty when the previous process shut down
  /// cleanly, which is the only time it is empty.
  pub fn recovery(&self) -> &SweepReport {
    &self.recovery
  }

  pub fn config(&self) -> &OverlayConfig {
    &self.config
  }

  pub fn state_dir(&self) -> &Path {
    &self.state_dir
  }

  pub fn content_store(&self) -> &ContentStore {
    &self.store
  }

  pub fn snapshot_time(&self) -> Timestamp {
    self.snapshot_time
  }

  fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
    // Poisoning is recovered from rather than propagated, for the reason the
    // catalog gives: a panic in one request must not turn into a workspace that
    // refuses every subsequent write. SQLite rolls back any open transaction
    // when the guard drops, so the durable state is consistent either way.
    self.inner.lock().unwrap_or_else(|e| e.into_inner())
  }

  pub fn is_empty(&self) -> bool {
    self.lock().entries.is_empty()
  }

  pub fn stats(&self) -> OverlayStats {
    let inner = self.lock();
    OverlayStats {
      entries: inner.entries.len() as u64,
      whiteouts: inner.entries.values().filter(|e| !e.present).count() as u64,
      local_bytes: inner.local_bytes,
      quota_bytes: self.config.quota_bytes,
    }
  }

  /// Force every committed mutation onto stable storage.
  ///
  /// What `fsync(2)` against the mount reaches. Content files are synced by
  /// their handles; this is the journal's half.
  pub fn sync(&self) -> Result<()> {
    self.lock().journal.sync()
  }

  // -------------------------------------------------------------------------
  // Reading
  // -------------------------------------------------------------------------

  pub fn get(&self, path: &BytePath) -> Option<OverlayEntry> {
    self.lock().entries.get(path.as_bytes()).cloned()
  }

  /// Resolve one path against the overlay, saying what the caller should do.
  ///
  /// See [`merge::resolve`] for the rule; this is the stateful wrapper.
  pub fn resolve(&self, path: &BytePath) -> Resolution {
    let inner = self.lock();
    merge::resolve(&inner.entries, path)
  }

  /// Whether a directory's base children are hidden.
  pub fn masks_base(&self, dir: &BytePath) -> bool {
    let inner = self.lock();
    merge::masks_base(&inner.entries, dir)
  }

  /// The overlay's own children of a directory, in name order.
  pub fn children(&self, dir: &BytePath) -> Vec<OverlayEntry> {
    let inner = self.lock();
    inner
      .children
      .get(dir.as_bytes())
      .into_iter()
      .flatten()
      .filter_map(|name| {
        let path = dir.join(name);
        inner.entries.get(path.as_bytes()).cloned()
      })
      .collect()
  }

  /// Overlay children the base listing did not produce, so `readdir` appends
  /// them once it has paged the base to the end.
  ///
  /// Keyed on the names the caller actually saw rather than on whether the row
  /// records base facts. Those are not the same question after a rename — a row
  /// moved to a new path still remembers the base of the path it came from — and
  /// getting it wrong makes a moved file either invisible or listed twice.
  pub fn extra_children(&self, dir: &BytePath, base_names: &HashSet<Vec<u8>>) -> Vec<OverlayEntry> {
    self
      .children(dir)
      .into_iter()
      .filter(|entry| entry.present && !base_names.contains(&entry.name()))
      .collect()
  }

  /// Whether a directory is empty in the merged view.
  ///
  /// `base_children` is one listing of the base directory's names; the caller
  /// supplies it because only the caller can page the base.
  pub fn merged_dir_is_empty(&self, dir: &BytePath, base_children: &[Vec<u8>]) -> bool {
    let inner = self.lock();
    // Any present overlay child makes it non-empty, whether or not it shadows a
    // base name: a shadowing child is still a child.
    let has_overlay_child = inner
      .children
      .get(dir.as_bytes())
      .into_iter()
      .flatten()
      .any(|name| {
        let path = dir.join(name);
        inner
          .entries
          .get(path.as_bytes())
          .is_some_and(|entry| entry.present)
      });
    if has_overlay_child {
      return false;
    }
    if merge::masks_base(&inner.entries, dir) {
      return true;
    }
    base_children.iter().all(|name| {
      let path = dir.join(name);
      matches!(inner.entries.get(path.as_bytes()), Some(entry) if !entry.present)
    })
  }

  /// Every row, sorted by path. The input to status, diff, and export.
  pub fn entries(&self) -> Vec<OverlayEntry> {
    self.lock().entries.values().cloned().collect()
  }

  pub fn open_content(&self, entry: &OverlayEntry) -> Result<std::fs::File> {
    match entry.content.local_id() {
      Some(id) => self.store.open_read(id),
      None => Err(OverlayError::io(format!(
        "{} has no local content",
        entry.path.escaped()
      ))),
    }
  }

  // -------------------------------------------------------------------------
  // Mutating
  // -------------------------------------------------------------------------

  /// Commit a change set: journal first, memory second, released content last.
  fn commit(&self, inner: &mut Inner, changes: Vec<Change>, release: Vec<u64>) -> Result<()> {
    inner
      .journal
      .apply(&changes, inner.next_ino, inner.next_content_id)?;
    for change in &changes {
      match change {
        Change::Put(entry) => {
          if let Some(previous) = inner.entries.get(entry.path.as_bytes()) {
            if let Some(id) = previous.content.local_id() {
              inner.local_bytes = inner.local_bytes.saturating_sub(previous.size);
              if Some(id) != entry.content.local_id() {
                release_content(&mut inner.by_content, id, entry.path.as_bytes());
              }
            }
          }
          if let Some(id) = entry.content.local_id() {
            inner.local_bytes = inner.local_bytes.saturating_add(entry.size);
            inner.by_content.insert(id, entry.path.as_bytes().to_vec());
          }
          inner
            .children
            .entry(entry.parent().into_bytes())
            .or_default()
            .insert(entry.name());
          inner
            .entries
            .insert(entry.path.as_bytes().to_vec(), entry.clone());
        }
        Change::Delete(path) => {
          if let Some(previous) = inner.entries.remove(path.as_bytes()) {
            if let Some(id) = previous.content.local_id() {
              inner.local_bytes = inner.local_bytes.saturating_sub(previous.size);
              release_content(&mut inner.by_content, id, path.as_bytes());
            }
            if let Some(set) = inner.children.get_mut(previous.parent().as_bytes()) {
              set.remove(&previous.name());
            }
          }
        }
      }
    }
    // After the commit, never before: a content file removed first and then a
    // failed transaction would leave a live row pointing at nothing, which is
    // the one direction the store's invariant does not allow.
    for id in release {
      let _ = self.store.remove(id);
    }
    Ok(())
  }

  fn next_time(&self, inner: &mut Inner) -> Timestamp {
    let t = xvfs_types::time::overlay_time(Timestamp::now(), self.snapshot_time, Some(inner.clock));
    inner.clock = t;
    t
  }

  /// Allocate an inode number for a row the overlay creates on its own.
  ///
  /// Only a directory rename does that, for the base descendants it materializes.
  /// Everything else is told a number by the caller, because the caller owns the
  /// live inode table and the kernel's dentries are keyed on it.
  fn allocate_ino(inner: &mut Inner) -> u64 {
    let ino = inner.next_ino;
    inner.next_ino += 1;
    ino
  }

  /// Take the caller's number, or allocate one if it did not supply one.
  fn adopt_ino(inner: &mut Inner, ino: u64) -> u64 {
    if ino == 0 {
      return Self::allocate_ino(inner);
    }
    ino
  }

  fn allocate_content_id(inner: &mut Inner) -> u64 {
    let id = inner.next_content_id;
    inner.next_content_id += 1;
    id
  }

  fn check_quota(&self, inner: &Inner, added: u64) -> Result<()> {
    if inner.local_bytes.saturating_add(added) > self.config.quota_bytes {
      return Err(OverlayError::quota(format!(
        "the overlay quota of {} bytes is exhausted",
        self.config.quota_bytes
      )));
    }
    Ok(())
  }

  /// Refuse a path whose parent is not a usable directory.
  ///
  /// `parent_base` is what the pinned commit holds at the parent, which only the
  /// caller can know. Combined with [`merge::resolve`]'s own ancestor walk it is a
  /// complete check: if a deeper ancestor is broken, resolution of the parent
  /// already says so, and if the parent is base-only the caller has just told us
  /// what it is.
  fn check_parent(inner: &Inner, path: &BytePath, parent_base: Option<&BaseFacts>) -> Result<()> {
    let parent = parent_of(path);
    if parent.is_empty() {
      return Ok(());
    }
    match Self::existing(inner, &parent, parent_base) {
      None => Err(OverlayError::no_entry(format!(
        "{} does not exist",
        parent.escaped()
      ))),
      Some(kind) if !kind.is_dir_like() => Err(OverlayError::not_directory(format!(
        "{} is not a directory",
        parent.escaped()
      ))),
      Some(_) => Ok(()),
    }
  }

  /// What exists at a path in the merged view, given what the base has there.
  fn existing(
    inner: &Inner,
    path: &BytePath,
    base: Option<&BaseFacts>,
  ) -> Option<xvfs_types::EntryKind> {
    match merge::resolve(&inner.entries, path) {
      Resolution::Overlay(entry) => Some(entry.kind.to_entry_kind()),
      Resolution::Absent => None,
      Resolution::Base => base.map(|b| b.kind),
    }
  }

  /// Create an empty regular file.
  pub fn create_file(
    &self,
    path: &BytePath,
    base: Option<BaseFacts>,
    parent_base: Option<BaseFacts>,
    ino: u64,
    executable: bool,
  ) -> Result<OverlayEntry> {
    path_condition(path)?;
    let mut inner = self.lock();
    Self::check_parent(&inner, path, parent_base.as_ref())?;
    if Self::existing(&inner, path, base.as_ref()).is_some() {
      return Err(OverlayError::exists(path.escaped()));
    }
    self.check_quota(&inner, 0)?;

    let ino = Self::adopt_ino(&mut inner, ino);
    let content_id = Self::allocate_content_id(&mut inner);
    self.store.create_empty(content_id)?;
    let now = self.next_time(&mut inner);
    let entry = OverlayEntry {
      path: path.clone(),
      present: true,
      kind: if executable {
        OverlayKind::Executable
      } else {
        OverlayKind::Regular
      },
      opaque: false,
      ino,
      content: Content::Local(content_id),
      symlink_target: None,
      size: 0,
      mtime: now,
      ctime: now,
      renamed_from: None,
      base,
    };
    self.commit(&mut inner, vec![Change::Put(entry.clone())], Vec::new())?;
    Ok(entry)
  }

  pub fn mkdir(
    &self,
    path: &BytePath,
    base: Option<BaseFacts>,
    parent_base: Option<BaseFacts>,
    ino: u64,
  ) -> Result<OverlayEntry> {
    path_condition(path)?;
    let mut inner = self.lock();
    Self::check_parent(&inner, path, parent_base.as_ref())?;
    if Self::existing(&inner, path, base.as_ref()).is_some() {
      return Err(OverlayError::exists(path.escaped()));
    }
    let ino = Self::adopt_ino(&mut inner, ino);
    let now = self.next_time(&mut inner);
    let entry = OverlayEntry {
      path: path.clone(),
      present: true,
      kind: OverlayKind::Directory,
      // A created directory hides whatever the base has at the same path. See
      // the module docs on `state`: `rm -rf build && mkdir build` must not leave
      // the base's children showing through.
      opaque: true,
      ino,
      content: Content::None,
      symlink_target: None,
      size: 0,
      mtime: now,
      ctime: now,
      renamed_from: None,
      base,
    };
    self.commit(&mut inner, vec![Change::Put(entry.clone())], Vec::new())?;
    Ok(entry)
  }

  pub fn symlink(
    &self,
    path: &BytePath,
    target: &[u8],
    base: Option<BaseFacts>,
    parent_base: Option<BaseFacts>,
    ino: u64,
  ) -> Result<OverlayEntry> {
    path_condition(path)?;
    if target.is_empty() || target.contains(&0) {
      return Err(OverlayError::invalid("a symlink target must be non-empty"));
    }
    let mut inner = self.lock();
    Self::check_parent(&inner, path, parent_base.as_ref())?;
    if Self::existing(&inner, path, base.as_ref()).is_some() {
      return Err(OverlayError::exists(path.escaped()));
    }
    let ino = Self::adopt_ino(&mut inner, ino);
    let now = self.next_time(&mut inner);
    let entry = OverlayEntry {
      path: path.clone(),
      present: true,
      kind: OverlayKind::Symlink,
      opaque: false,
      ino,
      // A symlink's target is its content, and it is small: kept in the row
      // rather than in a content file, so `readlink` never opens anything.
      content: Content::None,
      symlink_target: Some(target.to_vec()),
      size: target.len() as u64,
      mtime: now,
      ctime: now,
      renamed_from: None,
      base,
    };
    self.commit(&mut inner, vec![Change::Put(entry.clone())], Vec::new())?;
    Ok(entry)
  }

  /// Give a path local content: the copy-up.
  ///
  /// Idempotent for an entry that already has local content, so a racing second
  /// writer does not copy the blob twice or lose the first writer's bytes.
  ///
  /// `ino` is used only when the overlay has no row for the path yet, and it must
  /// be the number the caller already reported for the base entry — see
  /// [`OVERLAY_INO_BASE`] for why copy-up must not change identity.
  pub fn materialize(
    &self,
    path: &BytePath,
    base: Option<BaseFacts>,
    ino: u64,
    source: Source<'_>,
  ) -> Result<OverlayEntry> {
    path_condition(path)?;
    let mut inner = self.lock();
    // Resolved rather than looked up: a whiteout, or a path under one, is
    // *absent*, and treating it as "no row yet, use the base facts" would copy
    // up a file the workspace has already deleted.
    let existing = match merge::resolve(&inner.entries, path) {
      Resolution::Absent => return Err(OverlayError::no_entry(path.escaped())),
      Resolution::Overlay(entry) => Some(*entry),
      Resolution::Base => None,
    };

    let (kind, symlink_target, base_facts, ino, renamed_from) = match &existing {
      Some(entry) => {
        if entry.content.local_id().is_some() {
          return Ok(entry.clone());
        }
        if entry.kind.is_dir() {
          return Err(OverlayError::is_directory(path.escaped()));
        }
        if entry.kind == OverlayKind::Symlink {
          return Err(OverlayError::symlink_content(path));
        }
        (
          entry.kind,
          entry.symlink_target.clone(),
          entry.base.clone(),
          entry.ino,
          entry.renamed_from.clone(),
        )
      }
      _ => {
        let facts = base.clone().ok_or_else(|| {
          OverlayError::no_entry(format!("{} has no base entry to copy up", path.escaped()))
        })?;
        let kind = OverlayKind::from_entry_kind(facts.kind)
          .ok_or_else(|| OverlayError::not_permitted_kind(facts.kind))?;
        if kind.is_dir() {
          return Err(OverlayError::is_directory(path.escaped()));
        }
        if kind == OverlayKind::Symlink {
          return Err(OverlayError::symlink_content(path));
        }
        (kind, None, Some(facts), ino, None)
      }
    };

    let content_id = Self::allocate_content_id(&mut inner);
    let mut staged = self.store.stage(content_id)?;
    let size = match source {
      Source::Empty => 0,
      Source::Reader(reader) => {
        // The quota is checked before the copy and again with the real size:
        // the caller's declared size can be wrong, and an over-quota copy that
        // already wrote the bytes has already spent the disk.
        self.check_quota(&inner, base_facts.as_ref().map_or(0, |b| b.size))?;
        staged.copy_from(reader)?
      }
    };
    self.check_quota(&inner, size)?;
    self.store.publish(staged, content_id)?;

    let now = self.next_time(&mut inner);
    let entry = OverlayEntry {
      path: path.clone(),
      present: true,
      kind,
      opaque: false,
      ino,
      content: Content::Local(content_id),
      symlink_target,
      size,
      mtime: now,
      ctime: now,
      renamed_from,
      base: base_facts,
    };
    let release = existing
      .as_ref()
      .and_then(|e| e.content.local_id())
      .into_iter()
      .collect();
    self.commit(&mut inner, vec![Change::Put(entry.clone())], release)?;
    Ok(entry)
  }

  /// Write into an already-materialized file, returning how many bytes landed.
  ///
  /// A short write is a real answer, not a failure: it is what a POSIX write
  /// against a filesystem approaching its limit returns, and it is what keeps a
  /// quota from endangering the edits already in the overlay.
  pub fn write_at(&self, path: &BytePath, offset: u64, data: &[u8]) -> Result<usize> {
    use std::os::unix::fs::FileExt;

    let mut inner = self.lock();
    let entry = inner
      .entries
      .get(path.as_bytes())
      .cloned()
      .filter(|e| e.present)
      .ok_or_else(|| OverlayError::no_entry(path.escaped()))?;
    let Some(id) = entry.content.local_id() else {
      return Err(OverlayError::io(format!(
        "{} has not been copied up",
        path.escaped()
      )));
    };

    // `local_bytes` already counts this file at its current size, so the only
    // thing the quota has to admit is the growth.
    let end = offset.saturating_add(data.len() as u64);
    let growth = end.saturating_sub(entry.size);
    let headroom = self.config.quota_bytes.saturating_sub(inner.local_bytes);
    let data = if growth > headroom {
      let allowed = (data.len() as u64).saturating_sub(growth - headroom) as usize;
      if allowed == 0 {
        return Err(OverlayError::quota(format!(
          "the overlay quota of {} bytes is exhausted",
          self.config.quota_bytes
        )));
      }
      &data[..allowed]
    } else {
      data
    };

    let file = self.store.open_write(id)?;
    let written = file
      .write_at(data, offset)
      .map_err(|e| OverlayError::io(format!("writing {}: {e}", path.escaped())))?;

    let size = entry.size.max(offset.saturating_add(written as u64));
    let now = self.next_time(&mut inner);
    let updated = OverlayEntry {
      size,
      mtime: now,
      ctime: now,
      ..entry
    };
    self.commit(&mut inner, vec![Change::Put(updated)], Vec::new())?;
    Ok(written)
  }

  /// Write through an open descriptor rather than through a path.
  ///
  /// The descriptor is what POSIX says a write goes to, and it is why an open
  /// file keeps working after its name is renamed or removed. A row that no
  /// longer exists — the file was unlinked while open — still accepts the write:
  /// the bytes land in a content file the kernel keeps alive until the last
  /// descriptor closes, and there is simply no journal row left to update.
  pub fn write_content(
    &self,
    content_id: u64,
    file: &std::fs::File,
    offset: u64,
    data: &[u8],
  ) -> Result<usize> {
    use std::os::unix::fs::FileExt;

    let mut inner = self.lock();
    let entry = inner
      .by_content
      .get(&content_id)
      .and_then(|path| inner.entries.get(path.as_slice()))
      .cloned();

    // `local_bytes` already counts this file at its current size, so the only
    // thing the quota has to admit is the growth. An unlinked file is not
    // counted at all, so it is bounded by the quota's remaining headroom.
    let current = entry.as_ref().map_or(0, |e| e.size);
    let end = offset.saturating_add(data.len() as u64);
    let growth = end.saturating_sub(current);
    let headroom = self.config.quota_bytes.saturating_sub(inner.local_bytes);
    let data = if growth > headroom {
      let allowed = (data.len() as u64).saturating_sub(growth - headroom) as usize;
      if allowed == 0 {
        return Err(OverlayError::quota(format!(
          "the overlay quota of {} bytes is exhausted",
          self.config.quota_bytes
        )));
      }
      &data[..allowed]
    } else {
      data
    };

    let written = file
      .write_at(data, offset)
      .map_err(|e| OverlayError::io(format!("writing overlay content {content_id}: {e}")))?;

    if let Some(entry) = entry {
      let size = entry.size.max(offset.saturating_add(written as u64));
      let now = self.next_time(&mut inner);
      let updated = OverlayEntry {
        size,
        mtime: now,
        ctime: now,
        ..entry
      };
      self.commit(&mut inner, vec![Change::Put(updated)], Vec::new())?;
    }
    Ok(written)
  }

  /// Set a materialized file's length.
  pub fn truncate(&self, path: &BytePath, size: u64) -> Result<OverlayEntry> {
    let mut inner = self.lock();
    let entry = inner
      .entries
      .get(path.as_bytes())
      .cloned()
      .filter(|e| e.present)
      .ok_or_else(|| OverlayError::no_entry(path.escaped()))?;
    if entry.kind.is_dir() {
      return Err(OverlayError::is_directory(path.escaped()));
    }
    let Some(id) = entry.content.local_id() else {
      return Err(OverlayError::io(format!(
        "{} has not been copied up",
        path.escaped()
      )));
    };
    if size > entry.size {
      self.check_quota(&inner, size - entry.size)?;
    }
    self
      .store
      .open_write(id)?
      .set_len(size)
      .map_err(|e| OverlayError::io(format!("truncating {}: {e}", path.escaped())))?;
    let now = self.next_time(&mut inner);
    let updated = OverlayEntry {
      size,
      mtime: now,
      ctime: now,
      ..entry
    };
    self.commit(&mut inner, vec![Change::Put(updated.clone())], Vec::new())?;
    Ok(updated)
  }

  /// Record a metadata-only divergence: a mode change, or a timestamp.
  ///
  /// The resulting row keeps [`Content::Base`], so nothing is downloaded. This is
  /// the whole reason that variant exists.
  pub fn adopt(&self, path: &BytePath, base: Option<BaseFacts>, ino: u64) -> Result<OverlayEntry> {
    path_condition(path)?;
    let mut inner = self.lock();
    // Resolved, not looked up: the path may be masked by a whiteout or an opaque
    // directory several levels above it, in which case there is nothing here to
    // adopt however much the base still holds at the same path.
    match merge::resolve(&inner.entries, path) {
      Resolution::Overlay(entry) => return Ok(*entry),
      Resolution::Absent => return Err(OverlayError::no_entry(path.escaped())),
      Resolution::Base => {}
    }
    let facts =
      base.ok_or_else(|| OverlayError::no_entry(format!("{} does not exist", path.escaped())))?;
    let kind = OverlayKind::from_entry_kind(facts.kind)
      .ok_or_else(|| OverlayError::not_permitted_kind(facts.kind))?;
    let now = self.next_time(&mut inner);
    let entry = OverlayEntry {
      path: path.clone(),
      present: true,
      kind,
      // Adopting an existing base directory must not hide its base children;
      // only a *created* directory is opaque.
      opaque: false,
      ino,
      content: if kind.is_dir() {
        Content::None
      } else {
        Content::Base(facts.oid.clone())
      },
      symlink_target: None,
      size: facts.size,
      mtime: now,
      ctime: now,
      renamed_from: None,
      base: Some(facts),
    };
    self.commit(&mut inner, vec![Change::Put(entry.clone())], Vec::new())?;
    Ok(entry)
  }

  /// Set the executable bit. Every other permission bit is out of scope: Git
  /// records exactly one, and pretending to store the rest would report a mode
  /// that no export could reproduce.
  pub fn set_executable(
    &self,
    path: &BytePath,
    base: Option<BaseFacts>,
    ino: u64,
    executable: bool,
  ) -> Result<OverlayEntry> {
    let entry = self.adopt(path, base, ino)?;
    if entry.kind.is_dir() || entry.kind == OverlayKind::Symlink {
      // A directory's mode is not exported and a symlink's is fixed at 0777.
      return Ok(entry);
    }
    let wanted = if executable {
      OverlayKind::Executable
    } else {
      OverlayKind::Regular
    };
    if entry.kind == wanted {
      return Ok(entry);
    }
    let mut inner = self.lock();
    let now = self.next_time(&mut inner);
    let updated = OverlayEntry {
      kind: wanted,
      ctime: now,
      ..entry
    };
    self.commit(&mut inner, vec![Change::Put(updated.clone())], Vec::new())?;
    Ok(updated)
  }

  /// Set an explicit modification time, clamped to the overlay floor.
  ///
  /// ADR 0006: an `mtime` below `snapshot_time + one_tick` is raised rather than
  /// honoured, and exact restoration of an older time is a documented MVP
  /// incompatibility. `ctime` still advances on the logical clock, because a
  /// metadata change happened whatever the caller asked for `mtime` to say.
  pub fn set_times(
    &self,
    path: &BytePath,
    base: Option<BaseFacts>,
    ino: u64,
    mtime: Option<Timestamp>,
  ) -> Result<OverlayEntry> {
    let entry = self.adopt(path, base, ino)?;
    let mut inner = self.lock();
    let now = self.next_time(&mut inner);
    let mtime = mtime
      .map(|t| xvfs_types::time::clamp_requested_overlay_time(t, self.snapshot_time))
      .unwrap_or(now);
    let updated = OverlayEntry {
      mtime,
      ctime: now,
      ..entry
    };
    self.commit(&mut inner, vec![Change::Put(updated.clone())], Vec::new())?;
    Ok(updated)
  }

  /// Record that a directory's contents changed.
  ///
  /// POSIX requires a directory's `mtime` and `ctime` to advance when an entry is
  /// created or removed in it, and pjdfstest's `rmdir/00` and `symlink/00` check
  /// exactly that. Before this, a mount reported the pinned commit's sanitized
  /// snapshot time for a directory forever, however much a job changed inside it
  /// — so a build system or watcher that keys on directory mtime, which is the
  /// ordinary way to notice "something appeared in here", saw nothing.
  ///
  /// This adopts the directory into the overlay if it was base-only. That costs
  /// one journal row per directory a job writes into, bounded by the job's edit
  /// set rather than the repository, and it is invisible downstream:
  /// [`Status`] skips directory rows outright because Git records no
  /// directories, so an adopted parent produces no change, no diff hunk, and
  /// nothing in an export.
  ///
  /// The caller supplies `ino` because a base directory's inode number belongs to
  /// the client's inode table; allocating a fresh one here would change the
  /// directory's identity underneath anything holding it open.
  pub fn touch_directory(
    &self,
    path: &BytePath,
    base: Option<BaseFacts>,
    ino: u64,
  ) -> Result<OverlayEntry> {
    self.set_times(path, base, ino, None)
  }

  /// Remove a path: a whiteout when the base has one there, a plain delete when
  /// it does not.
  ///
  /// `merged_is_empty` is only consulted for a directory, and only the caller can
  /// compute it, because only the caller can page the base listing.
  pub fn remove(
    &self,
    path: &BytePath,
    base: Option<BaseFacts>,
    expect_dir: bool,
    merged_is_empty: bool,
  ) -> Result<()> {
    if path.is_empty() {
      return Err(OverlayError::invalid("the mount root cannot be removed"));
    }
    let mut inner = self.lock();
    let existing = inner.entries.get(path.as_bytes()).cloned();
    let kind = match Self::existing(&inner, path, base.as_ref()) {
      Some(kind) => kind,
      None => return Err(OverlayError::no_entry(path.escaped())),
    };
    let is_dir = kind.is_dir_like();
    if expect_dir && !is_dir {
      return Err(OverlayError::not_directory(path.escaped()));
    }
    if !expect_dir && is_dir {
      return Err(OverlayError::is_directory(path.escaped()));
    }
    if is_dir && !merged_is_empty {
      return Err(OverlayError::not_empty(path.escaped()));
    }

    // What the base holds here: from the row if there is one, otherwise from
    // what the caller resolved. A row's own `base` is authoritative, because it
    // was recorded when the path first diverged and the base cannot move.
    let base_facts = existing
      .as_ref()
      .and_then(|e| e.base.clone())
      .or(base.clone());

    let mut changes = Vec::new();
    let mut release = Vec::new();
    if let Some(entry) = &existing {
      if let Some(id) = entry.content.local_id() {
        release.push(id);
      }
    }
    // Removing a directory drops the rows underneath it, but **not** the
    // whiteouts. A directory can only be removed once it is empty, so the only
    // rows under it are whiteouts -- and those are the record of which base files
    // the job deleted. Dropping them would make `rm -rf src/` indistinguishable
    // from "the directory was never there", and an export would then have to walk
    // the base subtree to find out what to delete.
    if is_dir {
      let doomed: Vec<BytePath> = inner
        .entries
        .values()
        .filter(|e| e.present && e.path != *path && is_within(&e.path, path))
        .map(|e| e.path.clone())
        .collect();
      for victim in doomed {
        if let Some(entry) = inner.entries.get(victim.as_bytes()) {
          if let Some(id) = entry.content.local_id() {
            release.push(id);
          }
        }
        changes.push(Change::Delete(victim));
      }
    }

    match base_facts {
      Some(facts) => {
        let now = self.next_time(&mut inner);
        let ino = existing.as_ref().map(|e| e.ino).unwrap_or(0);
        changes.push(Change::Put(OverlayEntry {
          path: path.clone(),
          present: false,
          kind: OverlayKind::from_entry_kind(facts.kind).unwrap_or(OverlayKind::Regular),
          opaque: false,
          ino,
          content: Content::None,
          symlink_target: None,
          size: 0,
          mtime: now,
          ctime: now,
          renamed_from: None,
          base: Some(facts),
        }));
      }
      // Nothing in the base to hide, so the row goes away entirely rather than
      // becoming a whiteout of nothing. This is what keeps `present == false`
      // meaning exactly "a base entry is deleted".
      None => changes.push(Change::Delete(path.clone())),
    }
    self.commit(&mut inner, changes, release)
  }

  /// Rename within the mount.
  ///
  /// A file rename is metadata only. A directory rename materializes the base
  /// subtree as rows that still point at base blobs — see the crate docs on why
  /// that is preferred to an overlayfs-style redirect.
  #[allow(clippy::too_many_arguments)]
  pub fn rename(
    &self,
    from: &BytePath,
    from_ino: u64,
    from_base: Option<BaseFacts>,
    to: &BytePath,
    to_base: Option<BaseFacts>,
    to_parent_base: Option<BaseFacts>,
    from_descendants: &[BaseDescendant],
    to_merged_is_empty: bool,
    no_replace: bool,
  ) -> Result<()> {
    path_condition(from)?;
    path_condition(to)?;
    if from.is_empty() || to.is_empty() {
      return Err(OverlayError::invalid("the mount root cannot be renamed"));
    }
    if from == to {
      return Ok(());
    }
    if is_within(to, from) {
      return Err(OverlayError::invalid(
        "a directory cannot be renamed into itself",
      ));
    }

    let mut inner = self.lock();
    Self::check_parent(&inner, to, to_parent_base.as_ref())?;
    let source_row = inner.entries.get(from.as_bytes()).cloned();
    let source_kind = Self::existing(&inner, from, from_base.as_ref())
      .ok_or_else(|| OverlayError::no_entry(from.escaped()))?;
    let target_kind = Self::existing(&inner, to, to_base.as_ref());

    if let Some(target_kind) = target_kind {
      if no_replace {
        return Err(OverlayError::exists(to.escaped()));
      }
      match (source_kind.is_dir_like(), target_kind.is_dir_like()) {
        (true, false) => return Err(OverlayError::not_directory(to.escaped())),
        (false, true) => return Err(OverlayError::is_directory(to.escaped())),
        (true, true) if !to_merged_is_empty => return Err(OverlayError::not_empty(to.escaped())),
        _ => {}
      }
    }

    let now = self.next_time(&mut inner);
    let mut changes = Vec::new();
    let mut release = Vec::new();

    // The destination's existing content and rows go, whatever they were.
    if let Some(target_row) = inner.entries.get(to.as_bytes()) {
      if let Some(id) = target_row.content.local_id() {
        release.push(id);
      }
    }
    let doomed: Vec<BytePath> = inner
      .entries
      .values()
      .filter(|e| e.path != *to && is_within(&e.path, to))
      .map(|e| e.path.clone())
      .collect();
    for victim in doomed {
      if let Some(entry) = inner.entries.get(victim.as_bytes()) {
        if let Some(id) = entry.content.local_id() {
          release.push(id);
        }
      }
      changes.push(Change::Delete(victim));
    }

    // The moved entry itself.
    let moved = match &source_row {
      Some(row) if row.present => OverlayEntry {
        path: to.clone(),
        renamed_from: Some(row.renamed_from.clone().unwrap_or_else(|| from.clone())),
        base: to_base.clone(),
        ctime: now,
        // A directory that arrives by rename brings its children as explicit
        // rows, so whatever the base holds at the destination must not show
        // through it -- even when the row being moved was an adopted base
        // directory, which was not opaque where it came from.
        opaque: row.kind.is_dir(),
        ..row.clone()
      },
      _ => {
        let facts = from_base
          .clone()
          .ok_or_else(|| OverlayError::no_entry(format!("{} has no base entry", from.escaped())))?;
        let kind = OverlayKind::from_entry_kind(facts.kind)
          .ok_or_else(|| OverlayError::not_permitted_kind(facts.kind))?;
        OverlayEntry {
          path: to.clone(),
          present: true,
          kind,
          // A base directory arriving at a new path must hide whatever the base
          // has at *that* path; its own children arrive as explicit rows below.
          opaque: kind.is_dir(),
          // The number the caller's inode table already has for the source. A
          // rename moves a name, not a file, and the kernel keeps the dentry it
          // has rather than looking the destination up again.
          ino: Self::adopt_ino(&mut inner, from_ino),
          content: if kind.is_dir() {
            Content::None
          } else {
            Content::Base(facts.oid.clone())
          },
          symlink_target: None,
          size: facts.size,
          mtime: now,
          ctime: now,
          renamed_from: Some(from.clone()),
          base: to_base.clone(),
        }
      }
    };
    let moved_is_dir = moved.kind.is_dir();
    changes.push(Change::Put(moved));

    if moved_is_dir {
      // Overlay rows already under `from` move with it.
      let movers: Vec<OverlayEntry> = inner
        .entries
        .values()
        .filter(|e| e.path != *from && is_within(&e.path, from))
        .cloned()
        .collect();
      if movers.len() + from_descendants.len() > self.config.max_rename_entries {
        return Err(OverlayError::quota(format!(
          "renaming {} would materialize more than {} overlay entries",
          from.escaped(),
          self.config.max_rename_entries
        )));
      }
      for row in movers {
        // Whiteouts stay where they are. They record base files the job deleted
        // *before* the rename, and those deletions are still deletions -- the
        // files were never moved. Carrying them across would instead punch holes
        // in the moved subtree.
        if !row.present {
          continue;
        }
        let suffix = &row.path.as_bytes()[from.as_bytes().len()..];
        let mut target = to.as_bytes().to_vec();
        target.extend_from_slice(suffix);
        changes.push(Change::Delete(row.path.clone()));
        {
          changes.push(Change::Put(OverlayEntry {
            path: BytePath::new(target),
            ctime: now,
            opaque: row.kind.is_dir(),
            // The destination directory is opaque, so no base entry is reachable
            // at this path and there is nothing for the row to shadow. Carrying
            // the *source* path's base facts across would make status report a
            // moved file as a modification of an unrelated one.
            base: None,
            ..row
          }));
        }
      }
      // And the base's own descendants, as metadata that still points at base
      // blobs. No content is fetched.
      //
      // Filtered through the real resolution rule rather than by comparing path
      // sets: a base descendant that was deleted, or that an overlay row already
      // covers, must not be resurrected at the destination, and "deleted" can
      // mean a whiteout several levels above it.
      for descendant in from_descendants {
        let mut target = to.as_bytes().to_vec();
        target.push(b'/');
        target.extend_from_slice(descendant.relative.as_bytes());
        let mut source = from.as_bytes().to_vec();
        source.push(b'/');
        source.extend_from_slice(descendant.relative.as_bytes());
        if merge::resolve(&inner.entries, &BytePath::new(source.clone())) != Resolution::Base {
          continue;
        }
        let Some(kind) = OverlayKind::from_entry_kind(descendant.facts.kind) else {
          continue;
        };
        changes.push(Change::Put(OverlayEntry {
          path: BytePath::new(target),
          present: true,
          kind,
          opaque: kind.is_dir(),
          ino: Self::allocate_ino(&mut inner),
          content: if kind.is_dir() {
            Content::None
          } else {
            Content::Base(descendant.facts.oid.clone())
          },
          symlink_target: descendant.symlink_target.clone(),
          size: descendant.facts.size,
          mtime: now,
          ctime: now,
          // Where the blob still lives in the pinned commit. A row whose content
          // is `Content::Base` is served by fetching that blob, and the fetch
          // needs a path the base actually has -- which is no longer this row's
          // own path once it has moved.
          renamed_from: Some(BytePath::new(source)),
          base: None,
        }));
      }
    }

    // Finally the source: a whiteout when the base has something there, and a
    // plain removal when it does not.
    let source_base = source_row
      .as_ref()
      .and_then(|e| e.base.clone())
      .or(from_base);
    match source_base {
      Some(facts) => changes.push(Change::Put(OverlayEntry {
        path: from.clone(),
        present: false,
        kind: OverlayKind::from_entry_kind(facts.kind).unwrap_or(OverlayKind::Regular),
        opaque: false,
        ino: source_row.as_ref().map(|e| e.ino).unwrap_or(0),
        content: Content::None,
        symlink_target: None,
        size: 0,
        mtime: now,
        ctime: now,
        renamed_from: None,
        base: Some(facts),
      })),
      None => changes.push(Change::Delete(from.clone())),
    }

    self.commit(&mut inner, changes, release)
  }
}

impl OverlayError {
  /// A symlink's target is its content, and it is kept in the row. Copying one
  /// into a content file would make `readlink` and `read` disagree about what
  /// the link is, so it is refused rather than approximated.
  fn symlink_content(path: &BytePath) -> OverlayError {
    OverlayError::invalid(format!("{} is a symlink", path.escaped()))
  }

  fn not_permitted_kind(kind: xvfs_types::EntryKind) -> OverlayError {
    OverlayError::new(
      Condition::NotPermitted,
      format!("{kind:?} entries cannot be modified through the overlay"),
    )
  }
}
