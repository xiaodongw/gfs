//! The daemon-side listing cache, observed through real syscalls.
//!
//! The live finding this pins (2026-08-02, `~/.gfs-lab/flask`): every *warm*
//! `git status` cost ~15 server round trips — readdir is never kernel-cached,
//! and each absent-name probe repeated once its 1 s negative dentry expired.
//! Against an immutable pin the daemon can answer all of it from one complete
//! listing per directory, so a warm metadata walk must reach the server
//! exactly zero times.

use gfs_test::mount::{on_fs, Backend, Mount};

/// Recursive readdir + a stat of everything found + a probe of an absent name
/// in every directory — the shape of git's untracked scan. `.git` is skipped:
/// it is passthrough disk, not the base tree.
fn walk(dir: &std::path::Path, probe: &str) {
  assert!(
    std::fs::symlink_metadata(dir.join(probe)).is_err(),
    "the probe name must not exist"
  );
  for entry in std::fs::read_dir(dir).unwrap() {
    let entry = entry.unwrap();
    if entry.file_name() == ".git" {
      continue;
    }
    if entry.metadata().unwrap().is_dir() {
      walk(&entry.path(), probe);
    }
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_warm_metadata_walk_never_reaches_the_server() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;

  // Cold: one listing fetch per directory is the deal.
  let root = mount.path.clone();
  on_fs(move || walk(&root, ".probe-one")).await;
  let cold = mount.fs.stats();
  assert!(cold.directory_pages > 0, "the cold walk fetched listings");

  // Warm: the same walk with a *fresh* absent name, fresh so the kernel's
  // negative dentry from the first walk cannot be what answers it — the
  // daemon must answer from the cached listings, not the server.
  let root = mount.path.clone();
  on_fs(move || walk(&root, ".probe-two")).await;
  let warm = mount.fs.stats();
  assert_eq!(
    warm.directory_pages, cold.directory_pages,
    "a warm readdir walk fetched directory pages from the server"
  );
  assert_eq!(
    warm.metadata_requests, cold.metadata_requests,
    "a warm lookup reached the server for an entry the listings already decide"
  );
  assert!(
    warm.listing_hits > cold.listing_hits,
    "the warm walk was served by the listing cache"
  );

  // A negative served from the cache is not a curse on the name: the overlay
  // is consulted before the base, so creating it makes it exist immediately.
  let root = mount.path.clone();
  let visible = on_fs(move || {
    std::fs::write(root.join(".probe-two"), b"now real\n").unwrap();
    std::fs::symlink_metadata(root.join(".probe-two")).is_ok()
  })
  .await;
  assert!(
    visible,
    "a created file must be visible despite the cached absence"
  );
}
