//! The reference model, and the harness that drives the real overlay beside it.
//!
//! PLAN.md M3.1 asks for "a pure model for property-based state-machine tests"
//! and M3's first exit criterion is that random mutation sequences match it. This
//! module is both halves of that: a filesystem model with no journal, no content
//! files, and no crash safety — just a map from path to node, where every
//! operation is obviously correct — and an applier that performs the same
//! operation against a real [`Overlay`] over an in-memory base tree.
//!
//! # Why the model is a whole filesystem, not a whole overlay
//!
//! The model holds the *merged* result directly. If it modelled the overlay's own
//! representation — whiteouts, opacity, copy-up — it would be a second
//! implementation of the thing under test, and the two would agree on their
//! shared misunderstandings. A `BTreeMap<path, node>` that `mkdir` inserts into
//! cannot be wrong about what `mkdir` means.
//!
//! # Determinism
//!
//! Sequences come from a seeded xorshift generator rather than a property-testing
//! framework. A failing seed is printed and reproduces exactly, which is the part
//! of shrinking that matters here: the operation log is short enough to read.

use std::collections::BTreeMap;

use gfs_types::{BytePath, EntryKind, HashAlgorithm, ObjectId, Timestamp};

use crate::error::Condition;
use crate::state::{BaseFacts, Content, OverlayKind};
use crate::{BaseDescendant, Overlay, Source};

// ---------------------------------------------------------------------------
// The base: an immutable in-memory tree standing in for a pinned commit
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseNode {
  pub kind: EntryKind,
  /// File bytes, or a symlink target. Empty for a directory.
  pub content: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct BaseTree {
  nodes: BTreeMap<Vec<u8>, BaseNode>,
}

impl BaseTree {
  /// Build a tree from `path -> (kind, content)`, creating parent directories.
  pub fn new(entries: &[(&str, EntryKind, &[u8])]) -> Self {
    let mut tree = BaseTree::default();
    for (path, kind, content) in entries {
      let path = BytePath::new(path.as_bytes().to_vec());
      for ancestor in crate::ancestors_of(&path) {
        if ancestor.is_empty() {
          continue;
        }
        tree.nodes.entry(ancestor.into_bytes()).or_insert(BaseNode {
          kind: EntryKind::Directory,
          content: Vec::new(),
        });
      }
      tree.nodes.insert(
        path.into_bytes(),
        BaseNode {
          kind: *kind,
          content: content.to_vec(),
        },
      );
    }
    tree
  }

  pub fn get(&self, path: &BytePath) -> Option<&BaseNode> {
    if path.is_empty() {
      return None;
    }
    self.nodes.get(path.as_bytes())
  }

  pub fn paths(&self) -> impl Iterator<Item = BytePath> + '_ {
    self.nodes.keys().map(|p| BytePath::new(p.clone()))
  }

  pub fn oid_of(&self, path: &BytePath) -> Option<ObjectId> {
    self
      .get(path)
      .map(|node| crate::hash::blob_oid(HashAlgorithm::Sha1, &node.content).expect("sha1"))
  }

  /// What the caller of an overlay mutation must pass for this path.
  pub fn facts(&self, path: &BytePath) -> Option<BaseFacts> {
    let node = self.get(path)?;
    Some(BaseFacts {
      oid: crate::hash::blob_oid(HashAlgorithm::Sha1, &node.content).expect("sha1"),
      kind: node.kind,
      size: node.content.len() as u64,
    })
  }

  pub fn content_of_oid(&self, oid: &ObjectId) -> Option<Vec<u8>> {
    self
      .nodes
      .values()
      .find(|node| crate::hash::blob_oid(HashAlgorithm::Sha1, &node.content).expect("sha1") == *oid)
      .map(|node| node.content.clone())
  }

  pub fn child_names(&self, dir: &BytePath) -> Vec<Vec<u8>> {
    self
      .nodes
      .keys()
      .filter_map(|path| child_name(dir.as_bytes(), path))
      .collect()
  }

  /// Every descendant of a directory, relative to it.
  pub fn descendants(&self, dir: &BytePath) -> Vec<BaseDescendant> {
    let prefix = {
      let mut p = dir.as_bytes().to_vec();
      p.push(b'/');
      p
    };
    self
      .nodes
      .iter()
      .filter(|(path, _)| path.starts_with(&prefix))
      .map(|(path, node)| BaseDescendant {
        relative: BytePath::new(path[prefix.len()..].to_vec()),
        facts: BaseFacts {
          oid: crate::hash::blob_oid(HashAlgorithm::Sha1, &node.content).expect("sha1"),
          kind: node.kind,
          size: node.content.len() as u64,
        },
        symlink_target: (node.kind == EntryKind::Symlink).then(|| node.content.clone()),
      })
      .collect()
  }
}

/// The immediate child name of `dir` on the way to `path`, if `path` is one.
fn child_name(dir: &[u8], path: &[u8]) -> Option<Vec<u8>> {
  let rest = if dir.is_empty() {
    path
  } else {
    if !path.starts_with(dir) || path.len() <= dir.len() || path[dir.len()] != b'/' {
      return None;
    }
    &path[dir.len() + 1..]
  };
  if rest.is_empty() || rest.contains(&b'/') {
    return None;
  }
  Some(rest.to_vec())
}

// ---------------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
  pub kind: OverlayKind,
  /// File bytes, or a symlink target. Empty for a directory.
  pub content: Vec<u8>,
}

/// The merged filesystem, modelled directly.
#[derive(Clone, Debug)]
pub struct Model {
  pub live: BTreeMap<Vec<u8>, Node>,
}

type Outcome = std::result::Result<(), Condition>;

impl Model {
  pub fn new(base: &BaseTree) -> Self {
    let mut live = BTreeMap::new();
    for (path, node) in &base.nodes {
      let Some(kind) = OverlayKind::from_entry_kind(node.kind) else {
        continue;
      };
      live.insert(
        path.clone(),
        Node {
          kind,
          content: node.content.clone(),
        },
      );
    }
    Model { live }
  }

  fn get(&self, path: &BytePath) -> Option<&Node> {
    self.live.get(path.as_bytes())
  }

  /// The same rule the overlay applies: a name can only be created under a
  /// directory that exists.
  fn check_parent(&self, path: &BytePath) -> Outcome {
    let parent = crate::parent_of(path);
    if parent.is_empty() {
      return Ok(());
    }
    match self.get(&parent) {
      None => Err(Condition::NoEntry),
      Some(node) if !node.kind.is_dir() => Err(Condition::NotDirectory),
      Some(_) => Ok(()),
    }
  }

  fn is_empty_dir(&self, path: &BytePath) -> bool {
    !self
      .live
      .keys()
      .any(|other| child_name(path.as_bytes(), other).is_some())
  }

  pub fn create_file(&mut self, path: &BytePath) -> Outcome {
    self.check_parent(path)?;
    if self.get(path).is_some() {
      return Err(Condition::Exists);
    }
    self.live.insert(
      path.as_bytes().to_vec(),
      Node {
        kind: OverlayKind::Regular,
        content: Vec::new(),
      },
    );
    Ok(())
  }

  pub fn mkdir(&mut self, path: &BytePath) -> Outcome {
    self.check_parent(path)?;
    if self.get(path).is_some() {
      return Err(Condition::Exists);
    }
    self.live.insert(
      path.as_bytes().to_vec(),
      Node {
        kind: OverlayKind::Directory,
        content: Vec::new(),
      },
    );
    Ok(())
  }

  pub fn symlink(&mut self, path: &BytePath, target: &[u8]) -> Outcome {
    self.check_parent(path)?;
    if self.get(path).is_some() {
      return Err(Condition::Exists);
    }
    self.live.insert(
      path.as_bytes().to_vec(),
      Node {
        kind: OverlayKind::Symlink,
        content: target.to_vec(),
      },
    );
    Ok(())
  }

  pub fn write(&mut self, path: &BytePath, offset: u64, data: &[u8]) -> Outcome {
    let Some(node) = self.live.get_mut(path.as_bytes()) else {
      return Err(Condition::NoEntry);
    };
    if node.kind.is_dir() {
      return Err(Condition::IsDirectory);
    }
    if node.kind == OverlayKind::Symlink {
      return Err(Condition::Invalid);
    }
    let end = offset as usize + data.len();
    if node.content.len() < end {
      node.content.resize(end, 0);
    }
    node.content[offset as usize..end].copy_from_slice(data);
    Ok(())
  }

  pub fn truncate(&mut self, path: &BytePath, size: u64) -> Outcome {
    let Some(node) = self.live.get_mut(path.as_bytes()) else {
      return Err(Condition::NoEntry);
    };
    if node.kind.is_dir() {
      return Err(Condition::IsDirectory);
    }
    if node.kind == OverlayKind::Symlink {
      return Err(Condition::Invalid);
    }
    node.content.resize(size as usize, 0);
    Ok(())
  }

  pub fn set_executable(&mut self, path: &BytePath, executable: bool) -> Outcome {
    let Some(node) = self.live.get_mut(path.as_bytes()) else {
      return Err(Condition::NoEntry);
    };
    if node.kind.is_file() {
      node.kind = if executable {
        OverlayKind::Executable
      } else {
        OverlayKind::Regular
      };
    }
    Ok(())
  }

  pub fn unlink(&mut self, path: &BytePath) -> Outcome {
    match self.get(path) {
      None => Err(Condition::NoEntry),
      Some(node) if node.kind.is_dir() => Err(Condition::IsDirectory),
      Some(_) => {
        self.live.remove(path.as_bytes());
        Ok(())
      }
    }
  }

  pub fn rmdir(&mut self, path: &BytePath) -> Outcome {
    match self.get(path) {
      None => Err(Condition::NoEntry),
      Some(node) if !node.kind.is_dir() => Err(Condition::NotDirectory),
      Some(_) if !self.is_empty_dir(path) => Err(Condition::NotEmpty),
      Some(_) => {
        self.live.remove(path.as_bytes());
        Ok(())
      }
    }
  }

  pub fn rename(&mut self, from: &BytePath, to: &BytePath) -> Outcome {
    if from == to {
      return Ok(());
    }
    if crate::is_within(to, from) {
      return Err(Condition::Invalid);
    }
    let Some(source) = self.get(from).cloned() else {
      return Err(Condition::NoEntry);
    };
    self.check_parent(to)?;
    if let Some(target) = self.get(to).cloned() {
      match (source.kind.is_dir(), target.kind.is_dir()) {
        (true, false) => return Err(Condition::NotDirectory),
        (false, true) => return Err(Condition::IsDirectory),
        (true, true) if !self.is_empty_dir(to) => return Err(Condition::NotEmpty),
        _ => {}
      }
    }

    // Everything at or under the destination goes, then everything at or under
    // the source moves across.
    let doomed: Vec<Vec<u8>> = self
      .live
      .keys()
      .filter(|path| crate::is_within(&BytePath::new((*path).clone()), to))
      .cloned()
      .collect();
    for path in doomed {
      self.live.remove(&path);
    }
    let movers: Vec<(Vec<u8>, Node)> = self
      .live
      .iter()
      .filter(|(path, _)| crate::is_within(&BytePath::new((*path).clone()), from))
      .map(|(path, node)| (path.clone(), node.clone()))
      .collect();
    for (path, node) in movers {
      self.live.remove(&path);
      let mut target = to.as_bytes().to_vec();
      target.extend_from_slice(&path[from.as_bytes().len()..]);
      self.live.insert(target, node);
    }
    Ok(())
  }
}

// ---------------------------------------------------------------------------
// Reading the real overlay back as a filesystem
// ---------------------------------------------------------------------------

/// Materialize the overlay's merged view over a base tree, using the real
/// resolution and merge rules rather than a test-local re-derivation of them.
pub fn merged_view(overlay: &Overlay, base: &BaseTree) -> BTreeMap<Vec<u8>, Node> {
  let mut out = BTreeMap::new();
  walk(overlay, base, &BytePath::root(), &mut out);
  out
}

fn walk(overlay: &Overlay, base: &BaseTree, dir: &BytePath, out: &mut BTreeMap<Vec<u8>, Node>) {
  let mut children: Vec<(Vec<u8>, Node)> = Vec::new();
  let mut base_names: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();

  if !overlay.masks_base(dir) {
    for name in base.child_names(dir) {
      base_names.insert(name.clone());
      let path = dir.join(&name);
      match overlay.resolve(&path) {
        crate::Resolution::Absent => continue,
        crate::Resolution::Overlay(entry) => children.push((name, node_of(overlay, base, &entry))),
        crate::Resolution::Base => {
          let node = base.get(&path).expect("a listed base child exists");
          if let Some(kind) = OverlayKind::from_entry_kind(node.kind) {
            children.push((
              name,
              Node {
                kind,
                content: node.content.clone(),
              },
            ));
          }
        }
      }
    }
  }
  for entry in overlay.extra_children(dir, &base_names) {
    children.push((entry.name(), node_of(overlay, base, &entry)));
  }

  for (name, node) in children {
    let path = dir.join(&name);
    let is_dir = node.kind.is_dir();
    out.insert(path.as_bytes().to_vec(), node);
    if is_dir {
      walk(overlay, base, &path, out);
    }
  }
}

fn node_of(overlay: &Overlay, base: &BaseTree, entry: &crate::OverlayEntry) -> Node {
  let content = match &entry.content {
    Content::None => entry.symlink_target.clone().unwrap_or_default(),
    Content::Local(_) => {
      let mut file = overlay.open_content(entry).expect("local content");
      let mut bytes = Vec::new();
      std::io::Read::read_to_end(&mut file, &mut bytes).expect("reading local content");
      bytes
    }
    Content::Base(oid) => base.content_of_oid(oid).unwrap_or_default(),
  };
  Node {
    kind: entry.kind,
    content,
  }
}

// ---------------------------------------------------------------------------
// The operation alphabet and its two appliers
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum Op {
  CreateFile(BytePath),
  Mkdir(BytePath),
  Symlink(BytePath, Vec<u8>),
  Write(BytePath, u64, Vec<u8>),
  Truncate(BytePath, u64),
  SetExecutable(BytePath, bool),
  Unlink(BytePath),
  Rmdir(BytePath),
  Rename(BytePath, BytePath),
}

impl Op {
  pub fn apply_to_model(&self, model: &mut Model) -> Outcome {
    match self {
      Op::CreateFile(path) => model.create_file(path),
      Op::Mkdir(path) => model.mkdir(path),
      Op::Symlink(path, target) => model.symlink(path, target),
      Op::Write(path, offset, data) => model.write(path, *offset, data),
      Op::Truncate(path, size) => model.truncate(path, *size),
      Op::SetExecutable(path, executable) => model.set_executable(path, *executable),
      Op::Unlink(path) => model.unlink(path),
      Op::Rmdir(path) => model.rmdir(path),
      Op::Rename(from, to) => model.rename(from, to),
    }
  }

  /// Apply the same operation to a real overlay, supplying the base facts the
  /// FUSE layer would have resolved.
  pub fn apply_to_overlay(&self, overlay: &Overlay, base: &BaseTree) -> Outcome {
    let ino_for = |path: &BytePath| -> u64 {
      // Stands in for the client's inode table. Any stable number works; the
      // overlay only has to keep it, not choose it.
      let mut hash = 1469598103934665603u64;
      for byte in path.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(1099511628211);
      }
      hash % (crate::OVERLAY_INO_BASE - 2) + 2
    };

    let parent_for = |path: &BytePath| {
      let parent = crate::parent_of(path);
      crate::Parent::new(ino_for(&parent), base.facts(&parent))
    };

    let result = match self {
      Op::CreateFile(path) => overlay
        .create_file(
          path,
          base.facts(path),
          parent_for(path),
          ino_for(path),
          false,
        )
        .map(|_| ()),
      Op::Mkdir(path) => overlay
        .mkdir(path, base.facts(path), parent_for(path), ino_for(path))
        .map(|_| ()),
      Op::Symlink(path, target) => overlay
        .symlink(
          path,
          target,
          base.facts(path),
          parent_for(path),
          ino_for(path),
        )
        .map(|_| ()),
      Op::Write(path, offset, data) => materialize(overlay, base, path, ino_for(path))
        .and_then(|_| overlay.write_at(path, *offset, data))
        .map(|_| ()),
      Op::Truncate(path, size) => materialize(overlay, base, path, ino_for(path))
        .and_then(|_| overlay.truncate(path, *size))
        .map(|_| ()),
      Op::SetExecutable(path, executable) => overlay
        .set_executable(path, base.facts(path), ino_for(path), *executable)
        .map(|_| ()),
      Op::Unlink(path) => overlay.remove(path, base.facts(path), parent_for(path), false, true),
      Op::Rmdir(path) => {
        let empty = overlay.merged_dir_is_empty(path, &base.child_names(path));
        overlay.remove(path, base.facts(path), parent_for(path), true, empty)
      }
      Op::Rename(from, to) => {
        let empty = overlay.merged_dir_is_empty(to, &base.child_names(to));
        overlay.rename(
          from,
          ino_for(from),
          base.facts(from),
          parent_for(from),
          to,
          base.facts(to),
          parent_for(to),
          &base.descendants(from),
          empty,
          false,
        )
      }
    };
    result.map_err(|e| e.condition)
  }
}

/// Copy up a path so it can be written, which is what the FUSE layer does on the
/// first write to a base file.
fn materialize(overlay: &Overlay, base: &BaseTree, path: &BytePath, ino: u64) -> crate::Result<()> {
  // The bytes to copy up come from whatever the entry currently *points at*, not
  // from the base entry that happens to share its path. A row moved here by a
  // rename carries the object id of the blob it was moved from, and copying the
  // destination's base bytes instead is precisely the mistake the model
  // comparison exists to catch -- it caught this one.
  let bytes = match overlay.resolve(path) {
    crate::Resolution::Absent => return Err(crate::OverlayError::no_entry(path.escaped())),
    crate::Resolution::Overlay(entry) => {
      if entry.content.local_id().is_some() {
        return Ok(());
      }
      if entry.kind.is_dir() {
        return Err(crate::OverlayError::is_directory(path.escaped()));
      }
      match &entry.content {
        Content::Base(oid) => base.content_of_oid(oid).unwrap_or_default(),
        _ => Vec::new(),
      }
    }
    crate::Resolution::Base => {
      let Some(node) = base.get(path) else {
        return Err(crate::OverlayError::no_entry(path.escaped()));
      };
      if node.kind == EntryKind::Directory {
        return Err(crate::OverlayError::is_directory(path.escaped()));
      }
      node.content.clone()
    }
  };
  let mut reader = std::io::Cursor::new(bytes);
  overlay
    .materialize(path, base.facts(path), ino, Source::Reader(&mut reader))
    .map(|_| ())
}

// ---------------------------------------------------------------------------
// Sequence generation
// ---------------------------------------------------------------------------

/// A seeded xorshift64*. Deterministic across platforms, which `rand`'s
/// thread-local generators are not, and a failing seed reproduces exactly.
#[derive(Debug)]
pub struct Rng(u64);

impl Rng {
  pub fn new(seed: u64) -> Self {
    Rng(seed | 1)
  }

  pub fn next_u64(&mut self) -> u64 {
    self.0 ^= self.0 >> 12;
    self.0 ^= self.0 << 25;
    self.0 ^= self.0 >> 27;
    self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
  }

  pub fn below(&mut self, n: usize) -> usize {
    (self.next_u64() % n as u64) as usize
  }

  pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
    &items[self.below(items.len())]
  }
}

/// Generate one operation against the model's current state.
///
/// Paths are drawn from a small pool so collisions — create over an existing
/// file, rename onto a directory, delete something already deleted — happen
/// often. A generator that produced fresh unique paths would exercise almost
/// none of the state machine.
pub fn generate(rng: &mut Rng, model: &Model, pool: &[BytePath]) -> Op {
  let path = rng.pick(pool).clone();
  let other = rng.pick(pool).clone();
  // Prefer paths that exist for the operations that need one, so the sequence
  // does not degenerate into a long run of ENOENT.
  let existing: Vec<BytePath> = model
    .live
    .keys()
    .map(|p| BytePath::new(p.clone()))
    .filter(|p| !p.is_empty())
    .collect();
  let live = if existing.is_empty() {
    path.clone()
  } else {
    rng.pick(&existing).clone()
  };

  match rng.below(12) {
    0 => Op::CreateFile(path),
    1 => Op::Mkdir(path),
    2 => Op::Symlink(path, b"../target".to_vec()),
    3 | 4 => {
      let offset = (rng.below(4) * 3) as u64;
      let len = 1 + rng.below(6);
      Op::Write(live, offset, vec![b'a' + (rng.below(26) as u8); len])
    }
    5 => Op::Truncate(live, rng.below(9) as u64),
    6 => Op::SetExecutable(live, rng.below(2) == 0),
    7 => Op::Unlink(live),
    8 => Op::Rmdir(live),
    9 | 10 => Op::Rename(live, other),
    _ => Op::CreateFile(path),
  }
}

/// The default path pool: base paths plus names that do not exist yet.
pub fn path_pool(base: &BaseTree, extra: &[&str]) -> Vec<BytePath> {
  let mut pool: Vec<BytePath> = base.paths().collect();
  for name in extra {
    pool.push(BytePath::new(name.as_bytes().to_vec()));
  }
  pool
}

/// The binding a test overlay is opened with.
pub fn test_binding() -> crate::Binding {
  crate::Binding {
    repository_id: "r-model".to_owned(),
    base_commit: ObjectId::from_raw(HashAlgorithm::Sha1, &[0x11; 20])
      .expect("sha1")
      .to_qualified(),
  }
}

pub fn test_snapshot_time() -> Timestamp {
  Timestamp::from_secs(1_600_000_000)
}
