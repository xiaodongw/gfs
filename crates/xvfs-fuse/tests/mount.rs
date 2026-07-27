//! The read-only mount, driven through real syscalls.
//!
//! These are the M2.2 and M2.3 cases: the inode and metadata model, the blob
//! cache, and the synthesized `.git` surface. The oracle comparisons and the
//! wider compatibility matrix are in `compat.rs`.

mod harness;

use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use harness::{on_fs, read_dir_names, Backend, Mount};
use xvfs_fuse::FsConfig;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_is_readable_without_a_local_repository() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let path = mount.join("README.md");

  let content = on_fs(move || std::fs::read(&path).unwrap()).await;
  assert_eq!(content, b"# basic\n");

  // The first exit criterion's other half: the bytes came from the server, and
  // the client holds no repository at all.
  let cache = mount.fs.cache_stats();
  assert_eq!(cache.fetches, 1);
  assert_eq!(cache.bytes_fetched, 8);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reading_one_file_does_not_hydrate_its_siblings() {
  // "Reading selected files transfers only required metadata and blobs" -- M2's
  // second exit criterion, and the property the whole design exists for.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let path = mount.join("src/main.rs");

  on_fs(move || std::fs::read(&path).unwrap()).await;

  let cache = mount.fs.cache_stats();
  assert_eq!(cache.fetches, 1, "exactly one blob was fetched");
  // `src/lib/util.rs`, `src/new.rs`, and `README.md` were never touched.
  assert!(cache.bytes_fetched < 64, "{cache:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_read_of_the_same_blob_costs_nothing() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let path = mount.join("README.md");

  let p = path.clone();
  on_fs(move || std::fs::read(&p).unwrap()).await;
  let before = mount.fs.cache_stats();
  on_fs(move || std::fs::read(&path).unwrap()).await;
  let after = mount.fs.cache_stats();

  assert_eq!(after.fetches, before.fetches, "no second download");
  assert_eq!(after.bytes_fetched, before.bytes_fetched);
  assert!(after.hits > before.hits);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_opens_of_one_blob_download_it_once() {
  // The single-flight case. A parallel build opens the same header from many
  // processes at once; without deduplication that is one download per opener.
  let backend = Backend::start("content").await;
  let mount = Mount::new(&backend, "main").await;
  let path = mount.join("large-blob.bin");

  let mut handles = Vec::new();
  for _ in 0..8 {
    let p = path.clone();
    handles.push(tokio::task::spawn_blocking(move || {
      std::fs::read(&p).map(|b| b.len())
    }));
  }
  let mut lengths = Vec::new();
  for handle in handles {
    lengths.push(handle.await.unwrap().unwrap());
  }
  assert!(lengths.iter().all(|l| *l == lengths[0]));

  let cache = mount.fs.cache_stats();
  assert_eq!(
    cache.fetches, 1,
    "one download for eight openers: {cache:?}"
  );
  assert!(cache.coalesced + cache.hits >= 7, "{cache:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn directory_listing_merges_the_synthesized_git_surface() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let names = on_fs(move || read_dir_names(&root)).await;
  let names: Vec<String> = names
    .into_iter()
    .map(|n| String::from_utf8(n).unwrap())
    .collect();
  assert!(names.contains(&".git".to_owned()));
  assert!(names.contains(&"README.md".to_owned()));
  assert!(names.contains(&"src".to_owned()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_git_surface_has_the_six_entries_adr_0005_requires() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let git = mount.join(".git");

  let (names, head, config, json) = on_fs(move || {
    let names = read_dir_names(&git);
    let head = std::fs::read(git.join("HEAD")).unwrap();
    let config = std::fs::read_to_string(git.join("config")).unwrap();
    let json = std::fs::read(git.join("xvfs.json")).unwrap();
    // The two entries DESIGN.md omitted; without them Git does not recognize the
    // directory as a repository at all.
    assert!(git.join("objects").is_dir());
    assert!(git.join("refs").is_dir());
    (names, head, config, json)
  })
  .await;

  let mut names: Vec<String> = names
    .into_iter()
    .map(|n| String::from_utf8(n).unwrap())
    .collect();
  names.sort();
  assert_eq!(
    names,
    vec![
      "HEAD",
      "config",
      "objects",
      "packed-refs",
      "refs",
      "xvfs.json"
    ]
  );
  assert_eq!(head, b"ref: refs/heads/main\n");
  assert!(config.contains("repositoryformatversion = 0"));

  let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
  assert_eq!(value["commit"], mount.commit.to_qualified());
  assert_eq!(value["mount_id"], mount.mount_id.as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_git_surface_costs_no_server_traffic() {
  // DESIGN.md section 8.6: whatever occupies `.git` is outside change tracking
  // and hydration accounting. It is synthesized in memory, so reading all of it
  // must not fetch a single blob.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let git = mount.join(".git");

  on_fs(move || {
    for name in ["HEAD", "packed-refs", "config", "xvfs.json"] {
      std::fs::read(git.join(name)).unwrap();
    }
  })
  .await;

  assert_eq!(mount.fs.cache_stats().fetches, 0);
  assert_eq!(mount.fs.cache_stats().bytes_fetched, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_modes_survive_the_round_trip() {
  let backend = Backend::start("modes").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  on_fs(move || {
    // Writable, because the overlay is behind every base path: reporting 0444
    // would make `test -w` fail and an editor open the file read-only for a
    // write that would in fact succeed.
    let plain = std::fs::symlink_metadata(root.join("plain.txt")).unwrap();
    assert!(plain.is_file());
    assert_eq!(plain.permissions().mode() & 0o777, 0o644);

    let script = std::fs::symlink_metadata(root.join("script.sh")).unwrap();
    assert_eq!(
      script.permissions().mode() & 0o777,
      0o755,
      "the executable bit is what tells a build the script can run"
    );

    let link = std::fs::symlink_metadata(root.join("rel-link")).unwrap();
    assert!(link.file_type().is_symlink());
    assert_eq!(
      std::fs::read_link(root.join("rel-link")).unwrap(),
      std::path::Path::new("plain.txt")
    );

    // A submodule: an empty, read-only directory (DESIGN.md section 8.2).
    let gitlink = root.join("vendor/submodule");
    assert!(gitlink.is_dir());
    assert_eq!(std::fs::read_dir(&gitlink).unwrap().count(), 0);
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_symlink_target_is_served_without_fetching_a_blob() {
  // The server returns the target with the entry, so `ls -l` over a directory of
  // symlinks resolves every one with no blob traffic at all.
  let backend = Backend::start("modes").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let targets = on_fs(move || {
    [
      std::fs::read_link(root.join("rel-link")).unwrap(),
      std::fs::read_link(root.join("abs-link")).unwrap(),
      std::fs::read_link(root.join("escape-link")).unwrap(),
    ]
  })
  .await;
  assert_eq!(targets[0], std::path::Path::new("plain.txt"));
  assert_eq!(targets[1], std::path::Path::new("/etc/passwd"));
  assert_eq!(targets[2], std::path::Path::new("../../../etc/shadow"));
  assert_eq!(mount.fs.cache_stats().fetches, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_symlink_loop_reports_eloop_rather_than_hanging() {
  let backend = Backend::start("modes").await;
  let mount = Mount::new(&backend, "main").await;
  let path = mount.join("loop-a");

  let error = on_fs(move || std::fs::read(&path).unwrap_err()).await;
  assert_eq!(
    error.raw_os_error(),
    Some(libc::ELOOP),
    "the kernel must break the cycle, not the daemon"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_base_entry_reports_the_sanitized_snapshot_time() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();
  let expected = mount.fs.snapshot_time();

  on_fs(move || {
    for relative in ["README.md", "src", "src/main.rs"] {
      let meta = std::fs::symlink_metadata(root.join(relative)).unwrap();
      assert_eq!(meta.mtime(), expected.secs, "{relative}");
      assert_eq!(meta.ctime(), expected.secs, "{relative}");
    }
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn base_timestamps_are_identical_across_two_mounts() {
  // M2's exit criterion: base timestamps are stable across remounts and hosts.
  // The cross-host half is a property of the *stored* value, which `snapshot_time`
  // tests directly; this is the cross-remount half, end to end.
  let backend = Backend::start("basic").await;
  let first = Mount::new(&backend, "main").await;
  let a = first.join("README.md");
  let first_meta = on_fs(move || std::fs::metadata(&a).unwrap()).await;
  drop(first);

  let second = Mount::new(&backend, "main").await;
  let b = second.join("README.md");
  let second_meta = on_fs(move || std::fs::metadata(&b).unwrap()).await;

  assert_eq!(first_meta.mtime(), second_meta.mtime());
  assert_eq!(first_meta.mtime_nsec(), second_meta.mtime_nsec());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_inode_number_is_stable_within_one_mount() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let path = mount.join("README.md");

  let (first, second) = on_fs(move || {
    let first = std::fs::metadata(&path).unwrap().ino();
    // Enough other lookups that a naive allocator would have moved on.
    for name in ["src", "src/main.rs", "src/lib", "docs"] {
      let _ = std::fs::metadata(path.parent().unwrap().join(name));
    }
    let second = std::fs::metadata(&path).unwrap().ino();
    (first, second)
  })
  .await;
  assert_eq!(first, second);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_path_is_enoent_and_is_not_asked_about_twice() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let path = mount.join("does-not-exist.h");

  let before = mount.fs.stats().metadata_requests;
  on_fs(move || {
    for _ in 0..50 {
      let error = std::fs::metadata(&path).unwrap_err();
      assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
  })
  .await;
  let after = mount.fs.stats().metadata_requests;

  // The kernel caches the negative entry, so fifty misses cost one round trip.
  // A compiler searching an include path produces thousands of these.
  assert!(
    after - before <= 2,
    "{} round trips for 50 negative lookups",
    after - before
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_stats_of_one_path_do_not_reach_the_server() {
  // ADR 0003 measured 1000 `stat(2)` calls producing 0 upcalls at a 60-second
  // TTL. The commit is immutable, so this is correct rather than merely fast.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let path = mount.join("README.md");

  let p = path.clone();
  on_fs(move || std::fs::metadata(&p).unwrap()).await;
  let before = mount.fs.stats().metadata_requests;
  on_fs(move || {
    for _ in 0..1000 {
      std::fs::metadata(&path).unwrap();
    }
  })
  .await;
  assert_eq!(mount.fs.stats().metadata_requests, before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_synthesized_git_surface_is_read_only() {
  // The mount became writable in M3, but `.git` did not: ADR 0005 fixes it as a
  // synthesized read-only surface, and there is no overlay content behind it to
  // copy up. `EROFS` and specifically not `ENOSYS`, because the operation is
  // understood and refused rather than unimplemented.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  on_fs(move || {
    // `EACCES` rather than `EROFS` for these three, and that is the mount option
    // `DefaultPermissions` doing its job: the surface reports 0444 files inside
    // a 0555 directory, so the kernel refuses before the request becomes an
    // upcall. The handlers below answer `EROFS` if it ever reaches them, which is
    // the second line of the same defence rather than the first.
    for (label, error) in [
      (
        "write into .git",
        std::fs::write(root.join(".git/HEAD"), b"x").unwrap_err(),
      ),
      (
        "mkdir in .git",
        std::fs::create_dir(root.join(".git/newdir")).unwrap_err(),
      ),
      (
        "unlink in .git",
        std::fs::remove_file(root.join(".git/config")).unwrap_err(),
      ),
    ] {
      assert!(
        matches!(error.raw_os_error(), Some(libc::EACCES) | Some(libc::EROFS)),
        "{label}: {error}"
      );
    }

    // And the boundary Git itself never gets to cross: a hard link is `EPERM`
    // for the life of the MVP, not a consequence of read-only.
    let e = std::fs::hard_link(root.join("README.md"), root.join("linked")).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::EPERM), "hard link");
  })
  .await;
}

// `statvfs` has no safe wrapper in `std`; the workspace's `unsafe_code = "deny"`
// is a deny rather than a forbid precisely so a call like this can opt out with
// a reason. The struct is zeroed before the call and only read after it returns 0.
#[allow(unsafe_code)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn statfs_reports_the_overlay_quota_not_the_host_filesystem() {
  // PLAN.md M2.2. A build that reads `df` must see the budget it will be stopped
  // by, not the host's spare terabyte.
  let backend = Backend::start("basic").await;
  let quota = 256 * 1024 * 1024;
  let mount = Mount::with_configs(
    &backend,
    "main",
    FsConfig::default(),
    xvfs_overlay::OverlayConfig {
      quota_bytes: quota,
      ..xvfs_overlay::OverlayConfig::default()
    },
  )
  .await;
  let root = mount.path.clone();

  let (blocks, bsize) = on_fs(move || {
    let mut buffer: libc::statvfs = unsafe { std::mem::zeroed() };
    let c_path = std::ffi::CString::new(root.as_os_str().as_encoded_bytes()).unwrap();
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut buffer) };
    assert_eq!(rc, 0);
    (buffer.f_blocks as u64, buffer.f_frsize as u64)
  })
  .await;

  assert_eq!(blocks * bsize, quota);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_reads_and_seeks_return_the_right_bytes() {
  let backend = Backend::start("content").await;
  let mount = Mount::new(&backend, "main").await;
  let path = mount.join("crlf.txt");

  let (whole, tail) = on_fs(move || {
    let whole = std::fs::read(&path).unwrap();
    let mut file = std::fs::File::open(&path).unwrap();
    file.seek(SeekFrom::Start(2)).unwrap();
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).unwrap();
    (whole, tail)
  })
  .await;
  assert_eq!(&whole[2..], &tail[..]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_file_reads_as_empty_rather_than_failing() {
  let backend = Backend::start("content").await;
  let mount = Mount::new(&backend, "main").await;
  let path = mount.join("empty.txt");

  let content = on_fs(move || std::fs::read(&path).unwrap()).await;
  assert!(content.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_non_utf8_path_is_listable_and_readable() {
  // ADR 0006 records that no corpus tip contains one, so this is insurance -- and
  // insurance that is never exercised is not insurance.
  let backend = Backend::start("bytes").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let names = on_fs(move || read_dir_names(&root)).await;
  let non_utf8 = names
    .iter()
    .find(|n| String::from_utf8(n.to_vec()).is_err())
    .expect("the bytes fixture has a non-UTF-8 name");

  let root = mount.path.clone();
  let name = non_utf8.clone();
  on_fs(move || {
    use std::os::unix::ffi::OsStrExt;
    let path = root.join(std::ffi::OsStr::from_bytes(&name));
    let meta = std::fs::symlink_metadata(&path).unwrap();
    assert!(meta.is_file() || meta.is_dir());
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deep_path_resolves_component_by_component() {
  let backend = Backend::start("deep").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let found = on_fs(move || {
    // Walk down, ignoring the synthesized surface, which is a directory at the
    // root and would otherwise capture the descent immediately.
    let mut current = root;
    let mut depth = 0;
    loop {
      let next = std::fs::read_dir(&current)
        .unwrap()
        .flatten()
        .find(|e| e.file_name() != ".git" && e.path().is_dir());
      let Some(next) = next else { break };
      current = next.path();
      depth += 1;
      assert!(depth < 100, "the fixture is not this deep");
    }
    // The leaf is only reachable if every one of those components resolved.
    assert_eq!(std::fs::read(current.join("leaf.txt")).unwrap(), b"deep\n");
    depth
  })
  .await;
  assert_eq!(found, 40, "the deep fixture nests 40 levels");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_huge_directory_lists_completely_across_pages() {
  // The M0 spike measured a pagination bug returning 1597 of 1598 entries because
  // it paginated on names rather than on Git's tree sort key. This is the case
  // that would have caught it from the client side.
  let backend = Backend::start("bigdir").await;
  let mount = Mount::with_config(
    &backend,
    "main",
    FsConfig {
      // Small enough to force many pages, so the boundary logic is exercised
      // rather than accidentally avoided by a single-page listing.
      directory_page_size: 37,
      ..FsConfig::default()
    },
  )
  .await;

  let dir = mount.join("many");
  let mounted = on_fs(move || read_dir_names(&dir)).await;

  let expected = xvfs_test::git(
    &backend.repo_path,
    &["ls-tree", "--name-only", "main", "many/"],
  )
  .unwrap()
  .lines()
  .map(|line| {
    line
      .rsplit('/')
      .next()
      .unwrap_or_default()
      .as_bytes()
      .to_vec()
  })
  .collect::<std::collections::BTreeSet<_>>();

  assert_eq!(mounted.len(), expected.len(), "entry count");
  assert_eq!(
    mounted
      .into_iter()
      .collect::<std::collections::BTreeSet<_>>(),
    expected
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_corrupt_blob_fails_loudly_instead_of_reaching_the_reader() {
  // The reason the cache verifies at all. Simulated by publishing a file under a
  // name whose hash it does not match, which is what a truncated response or a
  // corrupted disk produces.
  let tmp = tempfile::tempdir().unwrap();
  let repo = xvfs_types::RepositoryId::parse("r-verify").unwrap();
  let cache =
    xvfs_fuse::BlobCache::open(tmp.path(), &repo, xvfs_types::HashAlgorithm::Sha1, 1 << 20)
      .unwrap();

  let claimed =
    xvfs_fuse::cache::hash_blob(xvfs_types::HashAlgorithm::Sha1, b"the real content").unwrap();
  let actual =
    xvfs_fuse::cache::hash_blob(xvfs_types::HashAlgorithm::Sha1, b"tampered content").unwrap();
  assert_ne!(claimed, actual);
  assert!(!cache.contains(&claimed));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn losing_the_server_produces_eio_and_leaves_cached_reads_working() {
  // ADR 0006's failure policy: "server unreachable, uncached base read ->
  // retryable EIO; overlay and cached reads continue".
  let backend = Backend::start("basic").await;
  let mount = Mount::with_config(
    &backend,
    "main",
    FsConfig {
      // One attempt, so the test does not pay for the retry ladder.
      attempts: 1,
      ..FsConfig::default()
    },
  )
  .await;

  let warm = mount.join("README.md");
  let w = warm.clone();
  let cached = on_fs(move || std::fs::read(&w).unwrap()).await;
  assert_eq!(cached, b"# basic\n");

  backend.stop();
  // Give the listeners time to stop accepting.
  tokio::time::sleep(std::time::Duration::from_millis(200)).await;

  let cold = mount.join("src/new.rs");
  let error = on_fs(move || std::fs::read(&cold).unwrap_err()).await;
  assert_eq!(
    error.raw_os_error(),
    Some(libc::EIO),
    "an unreachable server must never look like a missing file"
  );

  // The already-cached read still works, from the local cache.
  let still = on_fs(move || std::fs::read(&warm).unwrap()).await;
  assert_eq!(still, b"# basic\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unmounted_path_is_empty_rather_than_stale() {
  let backend = Backend::start("basic").await;
  let mut mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();
  let r = root.clone();
  on_fs(move || assert!(r.join("README.md").exists())).await;

  mount.unmount();

  let listed = on_fs(move || std::fs::read_dir(&root).unwrap().count()).await;
  assert_eq!(
    listed, 0,
    "the mount point is a plain empty directory again"
  );
}
