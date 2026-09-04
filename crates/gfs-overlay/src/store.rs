//! `files/`: overlay content, and the ordering that makes it recoverable.
//!
//! # The invariant is one-directional
//!
//! **A committed journal row's content always exists. An unreferenced content
//! file is garbage.** Never the reverse. Everything here exists to keep that
//! true, and recovery is a sweep for garbage rather than a repair of half-written
//! state — which is the difference between a recovery routine that can be
//! reasoned about and one that guesses.
//!
//! The publication sequence:
//!
//! ```text
//!   write  files/tmp/<n>          content, not yet reachable
//!   rename files/tmp/<n> -> files/<shard>/<id>
//!   commit the journal transaction that references <id>
//! ```
//!
//! A crash of the *daemon* anywhere before the last step leaves either a
//! temporary file or an unreferenced content file. Both are collected on the
//! next open, and neither was ever visible through the mount.
//!
//! # What is not fsynced, and why
//!
//! Nothing here calls `fsync` on its own. The journal runs at
//! `synchronous = NORMAL` ([`crate::journal`]), which promises that every
//! acknowledged mutation survives the daemon dying and makes no promise about
//! the host dying; fsyncing every content file made the bytes stricter than the
//! row that names them, at about 1.5 ms of waiting per created file. So the
//! store remembers what it published and [`ContentStore::sync_published`] —
//! reached through [`crate::Overlay::sync`], which is what `fsync(2)` on the
//! mount calls — forces the files and then their shard directories out. After
//! a power loss a row can name a file that is short or missing; the sweep
//! reports a missing file rather than inventing an empty one, and a short file
//! is what POSIX gives any file that was never fsynced.
//!
//! # Sharding
//!
//! `files/<id & 0xff>/<id>`, two hex digits. A job that rewrites a build tree can
//! produce hundreds of thousands of content files, and putting them in one
//! directory makes every open a linear scan on filesystems without directory
//! indexing.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::error::{OverlayError, Result};

pub const FILES_DIR: &str = "files";
const TEMP_DIR: &str = "tmp";

/// A content file being built, not yet reachable from the journal.
#[derive(Debug)]
pub struct Staged {
  file: std::fs::File,
  path: PathBuf,
  written: u64,
  /// Cleared by [`ContentStore::publish`]. While set, dropping the value removes
  /// the temporary file.
  armed: bool,
}

impl Staged {
  pub fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
    self
      .file
      .write_all(bytes)
      .map_err(|e| OverlayError::io(format!("writing staged overlay content: {e}")))?;
    self.written += bytes.len() as u64;
    crate::fault::trip(crate::fault::point::CONTENT_STAGED);
    Ok(())
  }

  /// Stream a reader in, which is how a copy-up avoids holding a whole blob in
  /// memory. A 100 MiB source file is an ordinary case in the corpus.
  pub fn copy_from(&mut self, reader: &mut dyn std::io::Read) -> Result<u64> {
    let copied = std::io::copy(reader, &mut self.file)
      .map_err(|e| OverlayError::io(format!("copying into staged overlay content: {e}")))?;
    self.written += copied;
    crate::fault::trip(crate::fault::point::CONTENT_STAGED);
    Ok(copied)
  }

  pub fn written(&self) -> u64 {
    self.written
  }
}

impl Drop for Staged {
  /// A staged file abandoned by an error path is removed immediately rather than
  /// left for the next recovery sweep. Recovery is the backstop for a crash, not
  /// the routine cleanup path.
  fn drop(&mut self) {
    if self.armed {
      let _ = std::fs::remove_file(&self.path);
    }
  }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SweepReport {
  pub temporary_files_removed: u64,
  pub orphan_files_removed: u64,
  pub orphan_bytes_removed: u64,
  /// Content ids a journal row references that are not on disk. Non-empty means
  /// the invariant above was violated, which is a bug or a damaged filesystem —
  /// reported rather than silently repaired, because "repairing" it means
  /// serving an empty file where the job wrote bytes.
  pub missing_content: Vec<u64>,
}

impl SweepReport {
  pub fn is_clean(&self) -> bool {
    *self == SweepReport::default()
  }
}

#[derive(Clone, Debug)]
pub struct ContentStore {
  root: PathBuf,
  /// Which shard directories this process has already made. One `mkdir` per
  /// shard per process instead of a `create_dir_all` probe per publish.
  shards: Arc<[AtomicBool; 256]>,
}

impl ContentStore {
  pub fn open(state_dir: &Path) -> Result<Self> {
    let root = state_dir.join(FILES_DIR);
    std::fs::create_dir_all(root.join(TEMP_DIR))
      .map_err(|e| OverlayError::io(format!("creating the overlay content store: {e}")))?;
    Ok(ContentStore {
      root,
      shards: Arc::new([const { AtomicBool::new(false) }; 256]),
    })
  }

  fn shard_of(&self, id: u64) -> PathBuf {
    self.root.join(format!("{:02x}", id & 0xff))
  }

  /// The shard directory for `id`, created on first use.
  fn ensure_shard(&self, id: u64) -> Result<PathBuf> {
    let shard = self.shard_of(id);
    let flag = &self.shards[(id & 0xff) as usize];
    if !flag.load(Ordering::Relaxed) {
      std::fs::create_dir_all(&shard)
        .map_err(|e| OverlayError::io(format!("creating an overlay content shard: {e}")))?;
      flag.store(true, Ordering::Relaxed);
    }
    Ok(shard)
  }

  pub fn root(&self) -> &Path {
    &self.root
  }

  pub fn path_of(&self, id: u64) -> PathBuf {
    self.shard_of(id).join(id.to_string())
  }

  pub fn stage(&self, token: u64) -> Result<Staged> {
    let path = self.root.join(TEMP_DIR).join(format!("stage-{token}"));
    let file = std::fs::File::create(&path)
      .map_err(|e| OverlayError::io(format!("creating staged overlay content: {e}")))?;
    Ok(Staged {
      file,
      path,
      written: 0,
      armed: true,
    })
  }

  /// Rename the staged file into place. No fsync: see the module docs.
  pub fn publish(&self, mut staged: Staged, id: u64) -> Result<u64> {
    let size = staged.written;
    crate::fault::trip(crate::fault::point::CONTENT_SYNCED);
    self.ensure_shard(id)?;
    std::fs::rename(&staged.path, self.path_of(id))
      .map_err(|e| OverlayError::io(format!("publishing overlay content: {e}")))?;
    // Disarmed rather than forgotten: `mem::forget` would leak the descriptor,
    // and the descriptor has to close. Dropping `staged` at the end of this
    // function now closes the file without removing the name it no longer owns.
    staged.armed = false;
    crate::fault::trip(crate::fault::point::CONTENT_PUBLISHED);
    Ok(size)
  }

  /// An empty content file, created in place and returned open for writing.
  ///
  /// The `O_TRUNC` path and every `create`: PLAN.md M3.2 requires that
  /// replacing a whole file does not fetch the old one, and an empty file has
  /// nothing to stage. `create_new` rather than `create`: an id is never
  /// reused, so a name already there is a bug worth failing on.
  pub fn create_empty(&self, id: u64) -> Result<std::fs::File> {
    self.ensure_shard(id)?;
    let file = std::fs::OpenOptions::new()
      .read(true)
      .write(true)
      .create_new(true)
      .open(self.path_of(id))
      .map_err(|e| OverlayError::io(format!("creating overlay content {id}: {e}")))?;
    crate::fault::trip(crate::fault::point::CONTENT_PUBLISHED);
    Ok(file)
  }

  /// Force published content onto the device: each file, then each shard
  /// directory a file was renamed into, in that order so a name never
  /// outlives its bytes.
  ///
  /// An id whose file is gone is skipped: it was removed after it was
  /// published, and there is nothing left to make durable.
  pub fn sync_published(&self, ids: impl IntoIterator<Item = u64>) -> Result<()> {
    let mut shards = HashSet::new();
    for id in ids {
      match std::fs::File::open(self.path_of(id)) {
        Ok(file) => {
          file
            .sync_all()
            .map_err(|e| OverlayError::io(format!("syncing overlay content {id}: {e}")))?;
          shards.insert(id & 0xff);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
          return Err(OverlayError::io(format!(
            "opening overlay content {id} to sync it: {e}"
          )))
        }
      }
    }
    for shard in shards {
      sync_dir(&self.shard_of(shard))?;
    }
    Ok(())
  }

  pub fn open_read(&self, id: u64) -> Result<std::fs::File> {
    std::fs::File::open(self.path_of(id))
      .map_err(|e| OverlayError::io(format!("opening overlay content {id}: {e}")))
  }

  pub fn open_write(&self, id: u64) -> Result<std::fs::File> {
    std::fs::OpenOptions::new()
      .read(true)
      .write(true)
      .open(self.path_of(id))
      .map_err(|e| OverlayError::io(format!("opening overlay content {id} for writing: {e}")))
  }

  pub fn size(&self, id: u64) -> Result<u64> {
    Ok(
      std::fs::metadata(self.path_of(id))
        .map_err(|e| OverlayError::io(format!("stat of overlay content {id}: {e}")))?
        .len(),
    )
  }

  pub fn remove(&self, id: u64) -> Result<()> {
    match std::fs::remove_file(self.path_of(id)) {
      Ok(()) => Ok(()),
      // Already gone is the outcome the caller wanted. A delete that ran before
      // a crash and is replayed after it must not fail the replay.
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
      Err(e) => Err(OverlayError::io(format!(
        "removing overlay content {id}: {e}"
      ))),
    }
  }

  /// Collect what a crash left behind and report what the journal expects but
  /// cannot find.
  pub fn sweep(&self, referenced: &HashSet<u64>) -> Result<SweepReport> {
    let mut report = SweepReport::default();

    let temp = self.root.join(TEMP_DIR);
    if let Ok(entries) = std::fs::read_dir(&temp) {
      for entry in entries.flatten() {
        if std::fs::remove_file(entry.path()).is_ok() {
          report.temporary_files_removed += 1;
        }
      }
    }

    let shards = std::fs::read_dir(&self.root)
      .map_err(|e| OverlayError::io(format!("reading the overlay content store: {e}")))?;
    for shard in shards.flatten() {
      if shard.file_name() == std::ffi::OsStr::new(TEMP_DIR) {
        continue;
      }
      let Ok(files) = std::fs::read_dir(shard.path()) else {
        continue;
      };
      for file in files.flatten() {
        let Some(id) = file
          .file_name()
          .to_str()
          .and_then(|name| name.parse::<u64>().ok())
        else {
          continue;
        };
        if referenced.contains(&id) {
          continue;
        }
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        if std::fs::remove_file(file.path()).is_ok() {
          report.orphan_files_removed += 1;
          report.orphan_bytes_removed += size;
        }
      }
    }

    for id in referenced {
      if !self.path_of(*id).exists() {
        report.missing_content.push(*id);
      }
    }
    report.missing_content.sort_unstable();
    Ok(report)
  }

  /// Total bytes held, for the quota accounting.
  pub fn total_bytes(&self, referenced: &HashSet<u64>) -> u64 {
    referenced
      .iter()
      .filter_map(|id| std::fs::metadata(self.path_of(*id)).ok())
      .map(|m| m.len())
      .sum()
  }
}

/// `fsync` a directory, so a rename into it survives a power loss.
pub fn sync_dir(path: &Path) -> Result<()> {
  std::fs::File::open(path)
    .and_then(|d| d.sync_all())
    .map_err(|e| OverlayError::io(format!("syncing {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn store() -> (tempfile::TempDir, ContentStore) {
    let tmp = tempfile::tempdir().unwrap();
    let store = ContentStore::open(tmp.path()).unwrap();
    (tmp, store)
  }

  #[test]
  fn published_content_is_readable_and_staged_content_is_not() {
    let (_tmp, store) = store();
    let mut staged = store.stage(1).unwrap();
    staged.write_all(b"hello").unwrap();
    assert!(!store.path_of(7).exists(), "not reachable before publish");
    assert_eq!(store.publish(staged, 7).unwrap(), 5);
    assert_eq!(std::fs::read(store.path_of(7)).unwrap(), b"hello");
  }

  #[test]
  fn an_abandoned_stage_removes_itself_without_waiting_for_recovery() {
    let (_tmp, store) = store();
    let path = {
      let mut staged = store.stage(3).unwrap();
      staged.write_all(b"x").unwrap();
      staged.path.clone()
    };
    assert!(!path.exists(), "the error path cleans up after itself");
  }

  #[test]
  fn a_sweep_collects_orphans_and_temporaries_but_keeps_referenced_content() {
    let (_tmp, store) = store();
    let mut kept = store.stage(1).unwrap();
    kept.write_all(b"keep").unwrap();
    store.publish(kept, 1).unwrap();
    let mut orphan = store.stage(2).unwrap();
    orphan.write_all(b"orphaned").unwrap();
    store.publish(orphan, 2).unwrap();
    // A temporary file a crash left behind mid-write.
    std::fs::write(store.root().join(TEMP_DIR).join("stage-99"), b"torn").unwrap();

    let referenced: HashSet<u64> = [1].into_iter().collect();
    let report = store.sweep(&referenced).unwrap();
    assert_eq!(report.orphan_files_removed, 1);
    assert_eq!(report.orphan_bytes_removed, 8);
    assert_eq!(report.temporary_files_removed, 1);
    assert!(report.missing_content.is_empty());
    assert!(store.path_of(1).exists());
    assert!(!store.path_of(2).exists());
  }

  #[test]
  fn a_sweep_reports_referenced_content_that_is_gone_rather_than_hiding_it() {
    // The invariant violated. Serving an empty file here would silently discard
    // an acknowledged write, which is the one thing M3.4 must never do.
    let (_tmp, store) = store();
    let referenced: HashSet<u64> = [42].into_iter().collect();
    let report = store.sweep(&referenced).unwrap();
    assert_eq!(report.missing_content, vec![42]);
    assert!(!report.is_clean());
  }
}
