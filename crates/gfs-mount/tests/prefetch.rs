//! Prefetching, observed through real syscalls.
//!
//! Two claims are worth pinning, and they are different in kind:
//!
//! * a recognized walk must answer from **one** recursive fetch instead of one
//!   round trip per directory — the 5 328-listing, 555-second first
//!   `git status` this exists to remove;
//! * what the walk sees must be identical either way. A prefetched listing that
//!   is subtly different from the one `ListDirectory` would have returned is not
//!   an optimisation, it is a second, wrong filesystem.

use gfs_mount::FsConfig;
use gfs_test::mount::{on_fs, Backend, Mount};

/// Every path under `dir`, relative, sorted. `.git` is passthrough disk rather
/// than the base tree, so it is skipped.
fn walk(dir: &std::path::Path, root: &std::path::Path) -> Vec<String> {
  let mut out = Vec::new();
  for entry in std::fs::read_dir(dir).unwrap() {
    let entry = entry.unwrap();
    if entry.file_name() == ".git" {
      continue;
    }
    let path = entry.path();
    out.push(
      path
        .strip_prefix(root)
        .unwrap()
        .to_string_lossy()
        .into_owned(),
    );
    if entry.metadata().unwrap().is_dir() {
      out.extend(walk(&path, root));
    }
  }
  out.sort();
  out
}

/// Wait for a background prefetch to do something, or give up.
async fn until(mount: &Mount, done: impl Fn(gfs_mount::FsStats) -> bool) -> bool {
  for _ in 0..100 {
    if done(mount.fs.stats()) {
      return true;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
  }
  false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recognized_walk_is_answered_by_one_recursive_fetch() {
  let backend = Backend::start("deep").await;

  // The same walk twice: once with the detector off, which is one
  // `ListDirectory` per directory, and once with it on.
  let off = Mount::with_config(
    &backend,
    "main",
    FsConfig {
      walk_prefetch_threshold: 0,
      ..FsConfig::default()
    },
  )
  .await;
  let root = off.path.clone();
  let without = on_fs(move || walk(&root, &root)).await;
  let plain = off.fs.stats();
  assert_eq!(plain.tree_prefetches, 0);
  assert!(
    plain.directory_pages > 8,
    "the fixture must have enough directories to be worth prefetching, got {}",
    plain.directory_pages
  );

  let on = Mount::new(&backend, "main").await;
  let root = on.path.clone();
  let with = on_fs(move || walk(&root, &root)).await;
  let prefetched = on.fs.stats();

  assert_eq!(
    with, without,
    "prefetched listings disagree with fetched ones"
  );
  assert_eq!(prefetched.tree_prefetches, 1);
  assert!(
    prefetched.prefetched_listings >= plain.directory_pages - 4,
    "the recursive fetch should have filled the directories the walk needed: \
     {} filled against {} fetched one at a time",
    prefetched.prefetched_listings,
    plain.directory_pages
  );
  assert!(
    prefetched.directory_pages < plain.directory_pages,
    "the walk still paid per directory: {} against {}",
    prefetched.directory_pages,
    plain.directory_pages
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reading_a_directory_through_fetches_the_rest_of_it() {
  let backend = Backend::start("content").await;
  let mount = Mount::with_config(
    &backend,
    "main",
    FsConfig {
      read_prefetch_threshold: 3,
      // Below the fixture's 4 MiB and 12 MiB files, which is the bound that
      // keeps a wrong guess cheap: a big file is what it costs most for.
      read_prefetch_max_file_bytes: 1 << 20,
      ..FsConfig::default()
    },
  )
  .await;

  let root = mount.path.clone();
  on_fs(move || {
    for name in ["crlf.txt", "no-final-newline.txt", "binary.bin"] {
      std::fs::read(root.join(name)).unwrap();
    }
  })
  .await;

  assert!(
    until(&mount, |s| s.prefetched_blobs > 0).await,
    "reading three files out of one directory should have fetched the rest"
  );
  let stats = mount.fs.stats();
  assert_eq!(stats.content_prefetches, 1);
  assert!(
    stats.prefetched_bytes < 1 << 20,
    "the oversized files must have been left alone, got {} bytes",
    stats.prefetched_bytes
  );

  // The file nobody asked for is now local: reading it moves no bytes.
  let before = mount.fs.cache_stats();
  let root = mount.path.clone();
  let content = on_fs(move || std::fs::read(root.join("utf16.txt")).unwrap()).await;
  assert_eq!(content, b"\xff\xfeh\0e\0l\0l\0o\0");
  let after = mount.fs.cache_stats();
  assert_eq!(
    after.fetches, before.fetches,
    "a prefetched file was fetched again when it was read"
  );
}
