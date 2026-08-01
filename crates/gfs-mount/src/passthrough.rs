//! The real `.git`, served through the workspace mount (ADR 0011).
//!
//! One mount, everything inside the folder. The workspace directory contains a
//! real `.git/` on disk; the workspace filesystem is mounted **over** the
//! directory, shadowing it; and this module is how the shadowed state stays
//! reachable — the daemon opens the real `.git` *before* mounting and keeps
//! the descriptor, and every operation on the `.git` subtree is forwarded to
//! that retained handle. To every tool the workspace presents a perfectly
//! ordinary git directory: no gitfile, no sibling state directory, no second
//! mount.
//!
//! Two subtree behaviours, per the ADR, and nothing merged:
//!
//! * **`.git/**` is pure passthrough** to the retained handle — reads, writes,
//!   lockfile creates (`O_CREAT|O_EXCL`), renames, loose-object writes, hard
//!   links, the lot. Git's own directory does exactly what Git expects a local
//!   filesystem to do, because it is one.
//! * **`.git/gfs/objects/**` is pure projection**: ADR 0009's object store,
//!   served straight from the per-repository [`BlockStore`] this workspace
//!   borrows. `objects/info/alternates` points here with the *relative* path
//!   `../gfs/objects`, so the pointer travels with the folder.
//!
//! # Negative dentries are a requirement, not an optimization
//!
//! `spikes/reports/m05c-gitdir-through-fuse.md` measured Git issuing 6,524
//! ENOENT lookups against its primary `objects/??/` directories for one
//! `read-tree` on linux — each a FUSE round trip, together a 6× slowdown.
//! Replying to an absent name with a node-id-0 entry and a TTL plants a
//! kernel negative dentry, and repeated probes never leave the kernel again.
//! With it every measured command lands within a few ms of local disk.
//!
//! The long TTL is confined to the **object namespace** — `.git/objects/**`
//! and the `.git/gfs/objects/**` projection — where it is coherent by
//! construction: the kernel drops a negative dentry itself when a name is
//! created through the mount, nothing can mutate the shadowed state behind
//! the daemon's back, and the daemon's own writes (`HEAD`, refs, the index on
//! a repin) never create loose objects. Everywhere else under `.git` the
//! short workspace TTL applies, because a repin *does* create names there
//! (`refs/heads/...`, the overlay journal's sidecars) from behind the mount.
//!
//! # Why `/proc/self/fd`
//!
//! The retained descriptor pins the real directory, but `std::fs` speaks
//! paths. `/proc/self/fd/<n>/<rel>` resolves relative to what the descriptor
//! names — the shadowed dentry, not the mount over it — which gives every
//! `std::fs` call `openat` semantics without a tree of unsafe wrappers. The
//! platform is Linux by construction (FUSE, ADR 0003).

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use gfs_types::error::GfsError;
use gfs_types::BytePath;

use crate::odb::{BlockStore, OdbViewStats, ViewCounters};

/// The one reserved name in the presented namespace — one Git itself already
/// forbids as a tracked path.
pub const GIT_DIR_NAME: &str = ".git";
/// Where all GFS state lives, under the real git dir: `.git/gfs`.
pub const STATE_SUBDIR: &str = "gfs";
/// The projection's place inside the git dir, relative to `.git`.
pub const ODB_REL: &str = "gfs/objects";
/// What `objects/info/alternates` holds. Relative, resolved by Git against
/// the `objects` directory — `objects/../gfs/objects` — so a copied folder
/// keeps working on another machine (ADR 0011's constraint).
pub const ALTERNATES_POINTER: &str = "../gfs/objects";

/// The retained handle to the real `.git`, opened before the workspace mount
/// shadows it.
///
/// Everything the daemon does to its own state after mounting — reseeding on
/// a repin, opening a new overlay epoch, rewriting `mount.json` — goes through
/// [`GitDirHandle::root`], because the on-disk path resolves into the mount
/// (and from inside the serving daemon, into a deadlock).
pub struct GitDirHandle {
  /// Held for its descriptor: dropping it kills `root`. Never read again —
  /// the `/proc` path below is the descriptor's one point of use.
  _file: std::fs::File,
  /// `/proc/self/fd/<n>`, valid for the life of `file` in this process.
  root: PathBuf,
  /// The on-disk path, for messages and for `mount.json`.
  real: PathBuf,
}

impl std::fmt::Debug for GitDirHandle {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("GitDirHandle")
      .field("real", &self.real)
      .finish_non_exhaustive()
  }
}

impl GitDirHandle {
  /// Open the real git dir. Must run before the workspace is mounted over —
  /// afterwards the on-disk path resolves into the mount, not to the state.
  pub fn open(real_git: &Path) -> Result<Self, GfsError> {
    let file = std::fs::File::open(real_git).map_err(|e| {
      GfsError::internal(format!(
        "opening the workspace git dir {}: {e}",
        real_git.display()
      ))
    })?;
    let root = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    Ok(GitDirHandle {
      _file: file,
      root,
      real: real_git.to_path_buf(),
    })
  }

  /// The live path of the real git dir, valid while this handle exists.
  pub fn root(&self) -> &Path {
    &self.root
  }

  /// Where the mount's state lives: `.git/gfs`, through the handle.
  pub fn state_dir(&self) -> PathBuf {
    self.root.join(STATE_SUBDIR)
  }

  /// The on-disk location, for reports. Do not resolve through this after
  /// mounting.
  pub fn real_path(&self) -> &Path {
    &self.real
  }
}

/// A stat snapshot of one passthrough entry, carried in its inode record.
///
/// A snapshot, not the truth: `lookup` and `getattr` re-stat the real file,
/// so the kernel's view is at most one attribute TTL old, and the TTL for
/// this subtree is short ([`crate::fs::FsConfig::git_ttl`]) because the
/// daemon mutates the shadowed state from behind the mount on a repin.
#[derive(Clone, Copy, Debug)]
pub struct GitMeta {
  pub kind: fuser::FileType,
  pub size: u64,
  pub perm: u16,
  pub nlink: u32,
  pub mtime: SystemTime,
  pub ctime: SystemTime,
}

impl GitMeta {
  pub fn of(md: &std::fs::Metadata) -> GitMeta {
    use std::os::unix::fs::MetadataExt;
    let kind = if md.is_dir() {
      fuser::FileType::Directory
    } else if md.file_type().is_symlink() {
      fuser::FileType::Symlink
    } else {
      fuser::FileType::RegularFile
    };
    GitMeta {
      kind,
      size: md.size(),
      // The real mode passes through: Git checks access(W_OK) on files it
      // intends to rewrite, and a projection that reported 0444 would refuse
      // every index update.
      perm: (md.mode() & 0o7777) as u16,
      nlink: md.nlink() as u32,
      mtime: md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
      ctime: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(md.ctime().max(0) as u64),
    }
  }

  pub fn is_dir(&self) -> bool {
    self.kind == fuser::FileType::Directory
  }
}

/// One entry of the projected object namespace.
#[derive(Clone, Debug)]
pub enum OdbNode {
  Dir,
  File { path: String, size: u64 },
}

impl OdbNode {
  pub fn is_dir(&self) -> bool {
    matches!(self, OdbNode::Dir)
  }

  pub fn size(&self) -> u64 {
    match self {
      OdbNode::Dir => 0,
      OdbNode::File { size, .. } => *size,
    }
  }
}

/// The manifest's tree, interned once at mount time.
///
/// Immutable for the life of the store, exactly as the store itself is: a
/// file can only *stop existing* (a gateway repack), which surfaces as
/// `ESTALE` on read, never as a changed listing.
#[derive(Debug, Default)]
struct OdbTree {
  /// Keyed by path relative to the projection root; `""` is the root.
  nodes: HashMap<Vec<u8>, OdbNode>,
  /// Immediate children per directory, sorted by name.
  children: HashMap<Vec<u8>, Vec<Vec<u8>>>,
}

impl OdbTree {
  fn build(listing: &[(String, u64)]) -> OdbTree {
    let mut tree = OdbTree::default();
    tree.nodes.insert(Vec::new(), OdbNode::Dir);
    tree.children.insert(Vec::new(), Vec::new());
    let mut sorted: Vec<&(String, u64)> = listing.iter().collect();
    sorted.sort();
    for (path, size) in sorted {
      let bytes = path.as_bytes();
      let mut walked: Vec<u8> = Vec::new();
      let components: Vec<&[u8]> = bytes.split(|b| *b == b'/').collect();
      for (i, component) in components.iter().enumerate() {
        let parent = walked.clone();
        if !walked.is_empty() {
          walked.push(b'/');
        }
        walked.extend_from_slice(component);
        let leaf = i == components.len() - 1;
        if !tree.nodes.contains_key(&walked) {
          tree.nodes.insert(
            walked.clone(),
            if leaf {
              OdbNode::File {
                path: path.clone(),
                size: *size,
              }
            } else {
              OdbNode::Dir
            },
          );
          tree.children.entry(walked.clone()).or_default();
          tree
            .children
            .entry(parent)
            .or_default()
            .push(component.to_vec());
        }
      }
    }
    for names in tree.children.values_mut() {
      names.sort();
    }
    tree
  }

  fn get(&self, rel: &[u8]) -> Option<&OdbNode> {
    self.nodes.get(rel)
  }

  fn child_names(&self, rel: &[u8]) -> &[Vec<u8>] {
    self
      .children
      .get(rel)
      .map(Vec::as_slice)
      .unwrap_or_default()
  }
}

/// The `.git` subtree's serving state: the retained handle plus this
/// workspace's window onto the shared object store.
///
/// Owned by [`crate::fs::Gfs`] and constant across repins — a repin changes
/// which commit the tree shows, not where the git dir lives.
pub struct GitPassthrough {
  handle: Arc<GitDirHandle>,
  store: Arc<BlockStore>,
  odb: OdbTree,
  /// This workspace's share of the store's traffic (per-job attribution,
  /// ADR 0009 — the view survives, only its mountpoint moved inside).
  counters: Arc<ViewCounters>,
}

impl std::fmt::Debug for GitPassthrough {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("GitPassthrough")
      .field("git_dir", &self.handle.real)
      .finish_non_exhaustive()
  }
}

/// Split a workspace path into its position inside the git subtree.
///
/// `None`: not a `.git` path. `Some("")` is `.git` itself; components after
/// that are relative to the real git dir.
pub fn git_rel(path: &BytePath) -> Option<&[u8]> {
  let bytes = path.as_bytes();
  if bytes == GIT_DIR_NAME.as_bytes() {
    return Some(b"");
  }
  bytes.strip_prefix(b".git/")
}

/// Whether a git-relative path is inside the projected object namespace.
///
/// `Some("")` is `.git/gfs/objects` itself.
pub fn odb_rel(git_relative: &[u8]) -> Option<&[u8]> {
  if git_relative == ODB_REL.as_bytes() {
    return Some(b"");
  }
  git_relative.strip_prefix(b"gfs/objects/")
}

/// Whether an absent name under this git-relative directory may be cached
/// long: true exactly on the object namespace, where m05c measured the probe
/// storm and where nothing is ever created from behind the mount.
pub fn in_object_namespace(git_relative_dir: &[u8]) -> bool {
  git_relative_dir == b"objects"
    || git_relative_dir.starts_with(b"objects/")
    || odb_rel(git_relative_dir).is_some()
}

impl GitPassthrough {
  pub fn new(handle: Arc<GitDirHandle>, store: Arc<BlockStore>) -> GitPassthrough {
    let odb = OdbTree::build(&store.listing());
    GitPassthrough {
      handle,
      store,
      odb,
      counters: Arc::new(ViewCounters::default()),
    }
  }

  pub fn handle(&self) -> &Arc<GitDirHandle> {
    &self.handle
  }

  pub fn store(&self) -> &Arc<BlockStore> {
    &self.store
  }

  pub fn counters(&self) -> &Arc<ViewCounters> {
    &self.counters
  }

  pub fn view_stats(&self) -> OdbViewStats {
    self.counters.snapshot()
  }

  /// The real location of a git-relative path, through the retained handle.
  pub fn real(&self, git_relative: &[u8]) -> PathBuf {
    if git_relative.is_empty() {
      return self.handle.root.clone();
    }
    self
      .handle
      .root
      .join(std::ffi::OsStr::from_bytes(git_relative))
  }

  /// Stat one git-relative path. `symlink_metadata`, never `metadata`: a
  /// symlink must present as itself, not escape the tree. The one exception
  /// is the git dir itself — its "path" is the `/proc/self/fd` magic link,
  /// which must be followed to reach the directory it pins, and which no
  /// symlink inside the tree can ever be confused with.
  pub fn stat(&self, git_relative: &[u8]) -> std::io::Result<GitMeta> {
    if git_relative.is_empty() {
      return std::fs::metadata(&self.handle.root).map(|md| GitMeta::of(&md));
    }
    std::fs::symlink_metadata(self.real(git_relative)).map(|md| GitMeta::of(&md))
  }

  /// The projection entry at an odb-relative path, if the manifest has one.
  pub fn odb_node(&self, rel: &[u8]) -> Option<OdbNode> {
    self.odb.get(rel).cloned()
  }

  /// The names in a projected directory, sorted.
  pub fn odb_children(&self, rel: &[u8]) -> Vec<(Vec<u8>, OdbNode)> {
    self
      .odb
      .child_names(rel)
      .iter()
      .filter_map(|name| {
        let mut child = rel.to_vec();
        if !child.is_empty() {
          child.push(b'/');
        }
        child.extend_from_slice(name);
        self
          .odb
          .get(&child)
          .map(|node| (name.clone(), node.clone()))
      })
      .collect()
  }

  /// List a real git directory's own entries.
  ///
  /// At `.git/gfs` any real `objects` is skipped: that name belongs to the
  /// projection, which the caller injects as an [`OdbNode`] — never as a
  /// passthrough entry, or the next `getattr` would re-stat a directory the
  /// disk does not have — so the namespace is never merged.
  pub fn list(&self, git_relative: &[u8]) -> std::io::Result<Vec<(Vec<u8>, GitMeta)>> {
    let mut out = Vec::new();
    let at_state_dir = git_relative == STATE_SUBDIR.as_bytes();
    for entry in std::fs::read_dir(self.real(git_relative))? {
      let entry = entry?;
      let name = entry.file_name().as_bytes().to_vec();
      if at_state_dir && name == b"objects" {
        continue;
      }
      let Ok(md) = entry.metadata() else {
        continue;
      };
      out.push((name, GitMeta::of(&md)));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
  }
}

/// Map an I/O failure from the real filesystem onto its errno, faithfully:
/// the passthrough's whole contract is that `.git` behaves like the local
/// directory it is.
pub fn errno_io(e: &std::io::Error) -> fuser::Errno {
  match e.raw_os_error() {
    Some(code) if code > 0 => fuser::Errno::from_i32(code),
    _ => fuser::Errno::EIO,
  }
}

/// Set both file times on a git-relative path.
///
/// `utimensat` has no safe wrapper in `std` (`set_times` needs an open
/// `File`, and opening a symlink's target is exactly what must not happen
/// here). The call reads no memory beyond its two-entry array. This is the
/// workspace-level deny-not-forbid opt-out, on the same reasoning as
/// `fallocate` in [`crate::odb`].
#[allow(unsafe_code)]
pub fn set_times(
  real: &Path,
  atime: Option<SystemTime>,
  mtime: Option<SystemTime>,
) -> std::io::Result<()> {
  fn spec(t: Option<SystemTime>) -> libc::timespec {
    match t {
      None => libc::timespec {
        tv_sec: 0,
        tv_nsec: libc::UTIME_OMIT,
      },
      Some(t) => {
        let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
        libc::timespec {
          tv_sec: d.as_secs() as libc::time_t,
          tv_nsec: i64::from(d.subsec_nanos()) as libc::c_long,
        }
      }
    }
  }
  let times = [spec(atime), spec(mtime)];
  let path = std::ffi::CString::new(real.as_os_str().as_bytes())
    .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
  let rc = unsafe {
    libc::utimensat(
      libc::AT_FDCWD,
      path.as_ptr(),
      times.as_ptr(),
      libc::AT_SYMLINK_NOFOLLOW,
    )
  };
  if rc != 0 {
    return Err(std::io::Error::last_os_error());
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_git_subtree_is_recognized_exactly() {
    assert_eq!(git_rel(&BytePath::new(b".git".to_vec())), Some(&b""[..]));
    assert_eq!(
      git_rel(&BytePath::new(b".git/config".to_vec())),
      Some(&b"config"[..])
    );
    // A tracked file whose name merely shares the prefix.
    assert_eq!(git_rel(&BytePath::new(b".gitignore".to_vec())), None);
    assert_eq!(git_rel(&BytePath::new(b"src/.git".to_vec())), None);
  }

  #[test]
  fn the_projection_boundary_sits_at_gfs_objects() {
    assert_eq!(odb_rel(b"gfs/objects"), Some(&b""[..]));
    assert_eq!(odb_rel(b"gfs/objects/pack/p.idx"), Some(&b"pack/p.idx"[..]));
    assert_eq!(odb_rel(b"gfs"), None);
    assert_eq!(odb_rel(b"objects/info/alternates"), None);
  }

  #[test]
  fn long_negative_ttls_cover_exactly_the_object_namespace() {
    // The m05c requirement: probes of absent loose objects and of the
    // projection may be cached long; names a repin creates from behind the
    // mount may not.
    assert!(in_object_namespace(b"objects"));
    assert!(in_object_namespace(b"objects/ab"));
    assert!(in_object_namespace(b"gfs/objects"));
    assert!(in_object_namespace(b"gfs/objects/pack"));
    assert!(!in_object_namespace(b""));
    assert!(!in_object_namespace(b"refs/heads"));
    assert!(!in_object_namespace(b"gfs"));
    assert!(!in_object_namespace(b"gfs/overlay"));
  }

  #[test]
  fn the_odb_tree_interns_the_manifest_with_directories() {
    let tree = OdbTree::build(&[
      ("pack/a.pack".to_owned(), 10),
      ("pack/a.idx".to_owned(), 2),
      ("info/commit-graph".to_owned(), 5),
    ]);
    assert!(tree.get(b"").is_some_and(OdbNode::is_dir));
    assert!(tree.get(b"pack").is_some_and(OdbNode::is_dir));
    let OdbNode::File { size, .. } = tree.get(b"pack/a.pack").unwrap() else {
      panic!("a manifest file is a file");
    };
    assert_eq!(*size, 10);
    let names: Vec<&[u8]> = tree.child_names(b"").iter().map(Vec::as_slice).collect();
    assert_eq!(names, vec![&b"info"[..], &b"pack"[..]]);
    assert!(tree.get(b"pack/missing.idx").is_none());
  }

  #[test]
  fn a_retained_handle_survives_the_disappearance_of_its_path() {
    // The property the whole layout rests on: the handle reaches the real
    // directory even when nothing else can. Renaming the directory away
    // models "shadowed by a mount" without needing a mount.
    let tmp = tempfile::tempdir().unwrap();
    let git = tmp.path().join(".git");
    std::fs::create_dir_all(git.join("gfs")).unwrap();
    std::fs::write(git.join("HEAD"), b"ref: refs/heads/main\n").unwrap();

    let handle = GitDirHandle::open(&git).unwrap();
    std::fs::rename(&git, tmp.path().join("elsewhere")).unwrap();

    let head = std::fs::read_to_string(handle.root().join("HEAD")).unwrap();
    assert_eq!(head, "ref: refs/heads/main\n");
    assert!(handle.state_dir().is_dir());
  }
}
