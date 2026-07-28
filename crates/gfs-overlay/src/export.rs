//! Export: a workspace's changes as an artifact something else can apply.
//!
//! DESIGN.md section 8.5 fixes what an export must be: atomic, checksummed, and
//! carrying the base commit "so a downstream applier can reject or three-way
//! merge stale work". M3's second exit criterion is stronger still — an export
//! applied to the pinned commit must produce the same tree as the mounted
//! workspace — so this is not a report, it is a transfer format.
//!
//! # The bundle
//!
//! ```text
//! <bundle>/
//!   manifest.json     base commit, format version, one record per change
//!   changes.patch     a `git apply`-compatible patch
//!   content/<n>       the byte-exact new content of each changed path
//!   CHECKSUMS         SHA-256 of every file above
//! ```
//!
//! Both representations are present because neither is sufficient alone. The
//! patch is what a human reviews and what an existing review pipeline consumes.
//! The content files are what makes the export exact for binary files, for
//! non-UTF-8 paths, and for content a patch cannot represent — and DESIGN.md
//! section 8.5 asks for both.
//!
//! # Atomicity
//!
//! Everything is written into a sibling temporary directory, fsynced, and then
//! renamed into place. A consumer that finds `<bundle>` finds all of it; a crash
//! leaves `<bundle>.tmp-*`, which is visibly incomplete rather than plausibly
//! complete. An export half-read by a job-cleanup step is a lost patch, and a
//! lost patch is a lost task.

use std::io::Write;
use std::path::{Path, PathBuf};

use gfs_types::{BytePath, HashAlgorithm, ObjectId};
use sha2::{Digest, Sha256};

use crate::diff::{git_patch_section, Sides};
use crate::error::{OverlayError, Result};
use crate::state::Content;
use crate::status::{Change, ChangeKind, Status};
use crate::Overlay;

/// The export format version, so an applier can refuse what it does not know.
pub const EXPORT_FORMAT_VERSION: u32 = 1;

/// Base blob bytes, which the overlay cannot fetch for itself.
///
/// Implemented by the client, which has the blob cache and the snapshot API. The
/// path is passed alongside the object ID because the blob endpoint mints its
/// authorization per path, and after a rename that path is not the one the change
/// is *about* — so the caller is told which one to use rather than guessing.
pub trait BaseContent {
  fn read(&self, oid: &ObjectId, path: &BytePath) -> Result<Vec<u8>>;
}

/// A base that has nothing, for exports of a workspace with no modifications of
/// base files. Also what the tests use when every change is an addition.
#[derive(Debug, Default)]
pub struct NoBaseContent;

impl BaseContent for NoBaseContent {
  fn read(&self, oid: &ObjectId, path: &BytePath) -> Result<Vec<u8>> {
    Err(OverlayError::io(format!(
      "no base content is available for {} ({oid})",
      path.escaped()
    )))
  }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
  pub export_format_version: u32,
  pub repository_id: String,
  /// The commit the workspace was pinned to. An applier that finds its branch
  /// has moved past this can reject or three-way merge instead of clobbering.
  pub base_commit: String,
  pub changes: Vec<Record>,
  /// Directories deleted without a per-file record; see [`Status`]. Present in
  /// the manifest so a consumer knows the patch is not the whole story rather
  /// than discovering it by diffing trees.
  pub unexpanded_directory_deletions: Vec<BytePath>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Record {
  #[serde(flatten)]
  pub change: Change,
  /// `content/<n>`, for a change that has new bytes.
  pub content: Option<String>,
  pub binary: bool,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ExportReport {
  pub bundle: PathBuf,
  pub changes: usize,
  pub patch_bytes: u64,
  pub content_bytes: u64,
  /// SHA-256 of `manifest.json`, so a caller can record what it shipped without
  /// re-reading the bundle.
  pub manifest_sha256: String,
}

/// Builds one bundle.
pub struct Exporter<'a> {
  overlay: &'a Overlay,
  base: &'a dyn BaseContent,
  algorithm: HashAlgorithm,
  repository_id: String,
  base_commit: String,
}

impl std::fmt::Debug for Exporter<'_> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Exporter")
      .field("base_commit", &self.base_commit)
      .finish_non_exhaustive()
  }
}

impl<'a> Exporter<'a> {
  pub fn new(
    overlay: &'a Overlay,
    base: &'a dyn BaseContent,
    algorithm: HashAlgorithm,
    repository_id: impl Into<String>,
    base_commit: impl Into<String>,
  ) -> Self {
    Exporter {
      overlay,
      base,
      algorithm,
      repository_id: repository_id.into(),
      base_commit: base_commit.into(),
    }
  }

  /// The patch alone, for `gfs diff` and the `git` shim.
  pub fn patch(&self, status: &Status) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for change in &status.changes {
      let sides = self.sides(change)?;
      out.extend_from_slice(&git_patch_section(change, &sides));
    }
    Ok(out)
  }

  /// Write the bundle atomically and return what was written.
  pub fn write_bundle(&self, status: &Status, bundle: &Path) -> Result<ExportReport> {
    let staging = staging_dir(bundle);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(staging.join("content"))
      .map_err(|e| OverlayError::io(format!("creating the export bundle: {e}")))?;

    let mut records = Vec::new();
    let mut patch = Vec::new();
    let mut content_bytes = 0u64;
    for (index, change) in status.changes.iter().enumerate() {
      let sides = self.sides(change)?;
      patch.extend_from_slice(&git_patch_section(change, &sides));

      let has_content = change.kind != ChangeKind::Deleted && change.new_oid.is_some();
      let content = if has_content {
        let name = format!("content/{index:06}");
        content_bytes += sides.new.len() as u64;
        write_file(&staging.join(&name), &sides.new)?;
        Some(name)
      } else {
        None
      };
      records.push(Record {
        binary: crate::diff::is_binary(&sides.old) || crate::diff::is_binary(&sides.new),
        change: change.clone(),
        content,
      });
    }

    let manifest = Manifest {
      export_format_version: EXPORT_FORMAT_VERSION,
      repository_id: self.repository_id.clone(),
      base_commit: self.base_commit.clone(),
      changes: records,
      unexpanded_directory_deletions: status.directory_deletions.clone(),
    };
    // Pretty-printed and key-ordered by the struct definition, so two exports of
    // the same workspace are byte-identical and a review tool can diff them.
    let mut encoded = serde_json::to_vec_pretty(&manifest)
      .map_err(|e| OverlayError::io(format!("encoding the export manifest: {e}")))?;
    encoded.push(b'\n');
    write_file(&staging.join("manifest.json"), &encoded)?;
    write_file(&staging.join("changes.patch"), &patch)?;
    write_checksums(&staging)?;
    crate::store::sync_dir(&staging)?;

    let _ = std::fs::remove_dir_all(bundle);
    std::fs::rename(&staging, bundle)
      .map_err(|e| OverlayError::io(format!("publishing the export bundle: {e}")))?;
    if let Some(parent) = bundle.parent() {
      crate::store::sync_dir(parent)?;
    }

    Ok(ExportReport {
      bundle: bundle.to_path_buf(),
      changes: status.changes.len(),
      patch_bytes: patch.len() as u64,
      content_bytes,
      manifest_sha256: hex(&Sha256::digest(&encoded)),
    })
  }

  /// The old and new bytes for one change.
  fn sides(&self, change: &Change) -> Result<Sides> {
    let old = match (&change.old_oid, change.kind) {
      // A rename with no content change: both sides are the same bytes, and the
      // patch section is a rename header with no hunk. Fetching them would be a
      // download for a diff that is empty by construction.
      (_, ChangeKind::Renamed) | (_, ChangeKind::ModeChanged) => Vec::new(),
      (Some(oid), _) => {
        let oid = ObjectId::parse_qualified(oid)
          .map_err(|e| OverlayError::io(format!("unreadable base object id: {e}")))?;
        let path = change.from.clone().unwrap_or_else(|| change.path.clone());
        self.base.read(&oid, &path)?
      }
      (None, _) => Vec::new(),
    };
    let new = match change.kind {
      // A deletion's new side is nothing at all. Echoing the old side back
      // produces a `deleted file mode` header with no hunk, and `git apply`
      // rejects it with "removal patch leaves file contents".
      ChangeKind::Deleted => Vec::new(),
      // A rename with no content change and a pure mode change both have
      // identical sides, so the section is a header with no hunk -- which is
      // exactly what Git emits for them.
      ChangeKind::Renamed | ChangeKind::ModeChanged => old.clone(),
      _ => self.workspace_bytes(&change.path)?,
    };
    Ok(Sides { old, new })
  }

  fn workspace_bytes(&self, path: &BytePath) -> Result<Vec<u8>> {
    let Some(entry) = self.overlay.get(path) else {
      return Ok(Vec::new());
    };
    match &entry.content {
      Content::Local(_) => {
        let mut file = self.overlay.open_content(&entry)?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes)
          .map_err(|e| OverlayError::io(format!("reading {}: {e}", path.escaped())))?;
        Ok(bytes)
      }
      Content::Base(oid) => {
        let source = entry.renamed_from.clone().unwrap_or_else(|| path.clone());
        self.base.read(oid, &source)
      }
      Content::None => Ok(entry.symlink_target.clone().unwrap_or_default()),
    }
  }

  pub fn algorithm(&self) -> HashAlgorithm {
    self.algorithm
  }
}

fn staging_dir(bundle: &Path) -> PathBuf {
  let mut name = bundle.file_name().unwrap_or_default().to_os_string();
  name.push(".tmp");
  bundle.with_file_name(name)
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
  let mut file = std::fs::File::create(path)
    .map_err(|e| OverlayError::io(format!("creating {}: {e}", path.display())))?;
  file
    .write_all(bytes)
    .map_err(|e| OverlayError::io(format!("writing {}: {e}", path.display())))?;
  file
    .sync_all()
    .map_err(|e| OverlayError::io(format!("syncing {}: {e}", path.display())))?;
  Ok(())
}

/// `<sha256>  <relative path>` per line, sorted, which is what `sha256sum -c`
/// reads.
fn write_checksums(root: &Path) -> Result<()> {
  let mut lines: Vec<String> = Vec::new();
  collect(root, root, &mut lines)?;
  lines.sort();
  let body = lines.join("\n") + "\n";
  write_file(&root.join("CHECKSUMS"), body.as_bytes())
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
  let entries = std::fs::read_dir(dir)
    .map_err(|e| OverlayError::io(format!("reading {}: {e}", dir.display())))?;
  for entry in entries.flatten() {
    let path = entry.path();
    if path.is_dir() {
      collect(root, &path, out)?;
      continue;
    }
    let bytes = std::fs::read(&path)
      .map_err(|e| OverlayError::io(format!("reading {}: {e}", path.display())))?;
    let relative = path.strip_prefix(root).unwrap_or(&path);
    out.push(format!(
      "{}  {}",
      hex(&Sha256::digest(&bytes)),
      relative.display()
    ));
  }
  Ok(())
}

fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_staging_directory_is_a_sibling_so_the_rename_stays_on_one_filesystem() {
    // A `rename(2)` across filesystems fails with `EXDEV`, and an export that
    // published by copying would not be atomic at all.
    let bundle = Path::new("/jobs/42/export");
    assert_eq!(staging_dir(bundle), Path::new("/jobs/42/export.tmp"));
    assert_eq!(staging_dir(bundle).parent(), bundle.parent());
  }
}
