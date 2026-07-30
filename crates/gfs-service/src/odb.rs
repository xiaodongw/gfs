//! The projected object database: what ADR 0009 lets a workspace's Git borrow.
//!
//! A workspace's `.git` reaches this repository's object database through
//! `objects/info/alternates` into a read-only projection, and these functions
//! are the server half of that projection: what files exist (the manifest) and
//! their bytes (range reads). The mount fetches in 64 KiB blocks, so the range
//! path is the hot one and must never load a pack into memory — linux's is
//! 8.1 GiB.
//!
//! # Authorization is repository-level, and that is a decision, not an oversight
//!
//! ADR 0002 measured that repository read access already implies object-database
//! read access — protocol v2 `upload-pack` serves any object a caller can name,
//! reachable or not — and concluded "one bare repository is one authorization
//! domain". Projecting `objects/` therefore discloses nothing the Git gateway
//! does not. The blob endpoint's per-object tickets still matter *there* because
//! the snapshot API promises more than Git does; this surface promises exactly
//! what Git does.
//!
//! What is **not** served: refs. `packed-refs` would disclose every subject's
//! `refs/gfs/work/*` branches, which is information the object database does not
//! carry (ADR 0009 requires the per-mount ref view to be filtered). The manifest
//! is files under `objects/` only, and the path grammar cannot name anything
//! else.

use std::path::{Path, PathBuf};

use gfs_types::error::{ErrorCode, GfsError};

/// One file a workspace's Git may read, relative to `objects/`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OdbFile {
  /// Always one of the shapes [`validate_odb_path`] accepts.
  pub path: String,
  pub size: u64,
}

/// The files the projection presents, in one deterministic listing.
///
/// Taken at a moment in time: a pack written after this call is invisible until
/// the mount refreshes, which is correct because the mount's pinned commit
/// predates it. A pack *deleted* after this call is the retention hazard the
/// mirror's `gc.pruneExpire` policy exists to close.
pub fn manifest(repo_path: &Path) -> Result<Vec<OdbFile>, GfsError> {
  let objects = repo_path.join("objects");
  let mut files = Vec::new();

  // pack/: every pack and its lookup structures.
  collect(&objects.join("pack"), "pack", &mut files)?;
  // info/commit-graph, and the split-graph directory when it exists.
  let info = objects.join("info");
  if let Ok(md) = std::fs::metadata(info.join("commit-graph")) {
    files.push(OdbFile {
      path: "info/commit-graph".to_owned(),
      size: md.len(),
    });
  }
  collect(
    &info.join("commit-graphs"),
    "info/commit-graphs",
    &mut files,
  )?;
  // Loose fan-out directories: exactly two lowercase hex characters.
  if let Ok(entries) = std::fs::read_dir(&objects) {
    for entry in entries.flatten() {
      let name = entry.file_name();
      let Some(name) = name.to_str() else { continue };
      if name.len() == 2
        && name
          .bytes()
          .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
      {
        collect(&objects.join(name), name, &mut files)?;
      }
    }
  }

  // Deterministic order, so two calls against an unchanged store byte-compare
  // equal and a client can diff manifests cheaply.
  files.sort_by(|a, b| a.path.cmp(&b.path));

  // Every listed path must satisfy the read path's own grammar; a manifest
  // entry the file endpoint would refuse is a bug here, not there.
  for f in &files {
    validate_odb_path(&f.path)?;
  }
  Ok(files)
}

fn collect(dir: &Path, prefix: &str, out: &mut Vec<OdbFile>) -> Result<(), GfsError> {
  let entries = match std::fs::read_dir(dir) {
    Ok(e) => e,
    // A mirror with no loose objects has no fan-out directories; absence is a
    // shape, not an error.
    Err(_) => return Ok(()),
  };
  for entry in entries.flatten() {
    let Ok(md) = entry.metadata() else { continue };
    if !md.is_file() {
      continue;
    }
    let name = entry.file_name();
    let Some(name) = name.to_str() else {
      continue;
    };
    // Skip locks and temporary files mid-write; they are not part of the store.
    if name.ends_with(".lock") || name.starts_with("tmp_") || name.starts_with('.') {
      continue;
    }
    let path = format!("{prefix}/{name}");
    if validate_odb_path(&path).is_ok() {
      out.push(OdbFile {
        path,
        size: md.len(),
      });
    }
  }
  Ok(())
}

/// The complete grammar of servable paths. Anything else is refused.
///
/// An allowlist of shapes rather than a traversal check: there is no `..`
/// rejection here because there is no rule by which `..` could ever match. The
/// grammar *is* the security boundary, which is why the manifest builder runs
/// its own output through it.
pub fn validate_odb_path(path: &str) -> Result<(), GfsError> {
  let refuse = || {
    GfsError::new(
      ErrorCode::InvalidArgument,
      format!("not a servable object-store path: {path:?}"),
    )
  };
  let hex = |s: &str| {
    !s.is_empty()
      && s
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
  };

  if let Some(name) = path.strip_prefix("pack/") {
    // pack-<hex>.<known suffix>, exactly one path component.
    let Some(rest) = name.strip_prefix("pack-") else {
      return Err(refuse());
    };
    let Some((digest, suffix)) = rest.rsplit_once('.') else {
      return Err(refuse());
    };
    if !hex(digest) || !matches!(suffix, "pack" | "idx" | "rev" | "bitmap" | "mtimes") {
      return Err(refuse());
    }
    return Ok(());
  }
  if path == "info/commit-graph" {
    return Ok(());
  }
  if let Some(name) = path.strip_prefix("info/commit-graphs/") {
    if name == "commit-graph-chain" {
      return Ok(());
    }
    let Some(rest) = name.strip_prefix("graph-") else {
      return Err(refuse());
    };
    let Some(digest) = rest.strip_suffix(".graph") else {
      return Err(refuse());
    };
    if !hex(digest) {
      return Err(refuse());
    }
    return Ok(());
  }
  // A loose object: two hex fan-out characters, then the rest of the digest.
  if let Some((fan, rest)) = path.split_once('/') {
    if fan.len() == 2 && hex(fan) && rest.len() >= 38 && hex(rest) {
      return Ok(());
    }
  }
  Err(refuse())
}

/// The absolute path a validated odb path names, under this repository.
pub fn resolve(repo_path: &Path, odb_path: &str) -> Result<PathBuf, GfsError> {
  validate_odb_path(odb_path)?;
  Ok(repo_path.join("objects").join(odb_path))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_grammar_accepts_exactly_the_store_shapes() {
    for good in [
      "pack/pack-dde2022bbb11e2e7ece0d1f7753ee2b758ed5c13.pack",
      "pack/pack-dde2022bbb11e2e7ece0d1f7753ee2b758ed5c13.idx",
      "pack/pack-dde2022bbb11e2e7ece0d1f7753ee2b758ed5c13.rev",
      "pack/pack-dde2022bbb11e2e7ece0d1f7753ee2b758ed5c13.bitmap",
      "pack/pack-dde2022bbb11e2e7ece0d1f7753ee2b758ed5c13.mtimes",
      "info/commit-graph",
      "info/commit-graphs/commit-graph-chain",
      "info/commit-graphs/graph-abc123.graph",
      "ab/cdef0123456789abcdef0123456789abcdef01",
    ] {
      validate_odb_path(good).unwrap_or_else(|e| panic!("{good}: {e}"));
    }
  }

  #[test]
  fn the_grammar_refuses_everything_that_is_not_the_store() {
    for bad in [
      "../config",
      "pack/../../HEAD",
      "pack/pack-XYZ.pack",
      "pack/pack-abc.exe",
      "pack/nested/pack-abc.pack",
      "info/alternates",
      "info/packs",
      "AB/cdef0123456789abcdef0123456789abcdef01",
      "ab/short",
      "abc/def",
      "config",
      "",
    ] {
      assert!(validate_odb_path(bad).is_err(), "accepted {bad:?}");
    }
  }

  #[test]
  fn a_manifest_lists_packs_and_loose_objects_with_sizes() {
    let (_tmp, path) = gfs_test::scratch_clone("basic").unwrap();
    let files = manifest(&path).unwrap();
    assert!(!files.is_empty());
    for f in &files {
      validate_odb_path(&f.path).unwrap();
      assert!(f.size > 0 || f.path.starts_with("info/"), "{f:?}");
      let full = resolve(&path, &f.path).unwrap();
      assert_eq!(std::fs::metadata(full).unwrap().len(), f.size);
    }
  }
}
