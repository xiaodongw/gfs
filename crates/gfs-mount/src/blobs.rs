//! Pre-fetching the base blobs a diff or an export needs.
//!
//! The exporter in `gfs-overlay` is synchronous and network-free by design, so
//! something has to hand it the pinned commit's bytes. This is that something,
//! and putting it in one place makes the cost model checkable: **only the paths
//! the journal says changed are fetched**, which is the property DESIGN.md
//! section 8.5 states as "diff reads only changed overlay files and their base
//! blobs".
//!
//! It is a `HashMap` rather than a lazy reader because the alternative is
//! blocking on a future from inside a synchronous trait method, which is how a
//! tokio worker deadlocks against the runtime it is running on.
//!
//! A blob that cannot be fetched is recorded as an error rather than as empty
//! content. An export whose "old side" silently became empty would apply as a
//! whole-file rewrite and pass every check that compares the *result*.

use std::collections::HashMap;
use std::sync::Arc;

use gfs_overlay::export::BaseContent;
use gfs_overlay::{Content, Overlay, OverlayError, Status};
use gfs_types::error::GfsError;
use gfs_types::{BytePath, ObjectId};

use crate::cache::BlobCache;
use crate::source::SnapshotSource;

#[derive(Debug, Default)]
pub struct PreloadedBase {
  blobs: HashMap<String, Vec<u8>>,
}

impl PreloadedBase {
  /// Fetch every base blob the change set refers to.
  pub async fn fetch(
    client: &Arc<dyn SnapshotSource>,
    cache: &Arc<BlobCache>,
    status: &Status,
    overlay: &Overlay,
  ) -> Result<Self, GfsError> {
    let mut wanted: Vec<(ObjectId, BytePath)> = Vec::new();

    for change in &status.changes {
      // The old side of a modification or a deletion.
      if let Some(oid) = &change.old_oid {
        // A rename and a mode change have identical sides, so the exporter never
        // asks for their bytes -- fetching them would be a download for a hunk
        // that is empty by construction.
        if change.is_content_change() && change.kind != gfs_overlay::ChangeKind::Renamed {
          let oid = ObjectId::parse_qualified(oid)
            .map_err(|e| GfsError::internal(format!("unreadable base object id: {e}")))?;
          let path = change.from.clone().unwrap_or_else(|| change.path.clone());
          wanted.push((oid, path));
        }
      }
      // And the new side, when the workspace's bytes are still the base's --
      // a file moved but not edited.
      if let Some(entry) = overlay.get(&change.path) {
        if let Content::Base(oid) = &entry.content {
          let path = entry
            .renamed_from
            .clone()
            .unwrap_or_else(|| change.path.clone());
          wanted.push((oid.clone(), path));
        }
      }
    }

    let mut blobs = HashMap::new();
    for (oid, path) in wanted {
      let key = oid.to_qualified();
      if blobs.contains_key(&key) {
        continue;
      }
      blobs.insert(key, read_blob(client, cache, &oid, &path).await?);
    }
    Ok(PreloadedBase { blobs })
  }

  pub fn len(&self) -> usize {
    self.blobs.len()
  }

  pub fn is_empty(&self) -> bool {
    self.blobs.is_empty()
  }
}

impl BaseContent for PreloadedBase {
  fn read(&self, oid: &ObjectId, path: &BytePath) -> gfs_overlay::Result<Vec<u8>> {
    self.blobs.get(&oid.to_qualified()).cloned().ok_or_else(|| {
      OverlayError::io(format!(
        "the base blob for {} ({oid}) was not pre-fetched",
        path.escaped()
      ))
    })
  }
}

/// One blob, through the shared cache so a diff of a file the job already read
/// costs nothing.
async fn read_blob(
  client: &Arc<dyn SnapshotSource>,
  cache: &Arc<BlobCache>,
  oid: &ObjectId,
  path: &BytePath,
) -> Result<Vec<u8>, GfsError> {
  let ticket = if cache.contains(oid) {
    String::new()
  } else {
    client
      .get_entry(path, true)
      .await?
      .and_then(|entry| entry.blob_ticket)
      .unwrap_or_default()
  };
  let (cached, _) = cache.open_blob(client, oid, &ticket).await?;
  let bytes = std::fs::read(&cached).map_err(|e| {
    GfsError::internal(format!("reading a cached blob for {}: {e}", path.escaped()))
  })?;
  cache.release_blob(oid);
  Ok(bytes)
}
