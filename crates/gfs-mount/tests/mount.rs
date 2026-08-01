//! The read-only mount, driven through real syscalls.
//!
//! These are the M2.2 and M2.3 cases: the inode and metadata model, the blob
//! cache, and the synthesized `.git` surface. The oracle comparisons and the
//! wider compatibility matrix are in `compat.rs`.

use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use gfs_mount::FsConfig;
use gfs_test::mount::{on_fs, read_dir_names, Backend, Mount};

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
async fn the_git_dir_is_the_real_seeded_one_served_back_through_the_mount() {
  // ADR 0011: `.git` is a real directory, shadowed by the mount and passed
  // through — what a tool reads is exactly what the seed wrote, plus the
  // projection injected at `gfs/objects`.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let git = mount.join(".git");

  let (names, head, config, json, alternates) = on_fs(move || {
    let names = read_dir_names(&git);
    let head = std::fs::read(git.join("HEAD")).unwrap();
    let config = std::fs::read_to_string(git.join("config")).unwrap();
    let json = std::fs::read(git.join("gfs.json")).unwrap();
    let alternates = std::fs::read_to_string(git.join("objects/info/alternates")).unwrap();
    // Without these Git does not recognize the directory as a repository.
    assert!(git.join("objects").is_dir());
    assert!(git.join("refs").is_dir());
    // The projection, presented inside the git dir under the one name the
    // alternates points at.
    assert!(git.join("gfs/objects").is_dir());
    (names, head, config, json, alternates)
  })
  .await;

  let names: Vec<String> = names
    .into_iter()
    .map(|n| String::from_utf8(n).unwrap())
    .collect();
  for required in ["HEAD", "config", "gfs", "gfs.json", "objects", "refs"] {
    assert!(names.contains(&required.to_owned()), "{names:?}");
  }
  assert_eq!(head, b"ref: refs/heads/main\n");
  assert!(config.contains("repositoryformatversion = 0"));
  // Relative, so the folder can travel (ADR 0011).
  assert_eq!(alternates, "../gfs/objects\n");

  let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
  assert_eq!(value["commit"], mount.commit.to_qualified());
  assert_eq!(value["mount_id"], mount.mount_id.as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_git_surface_costs_no_server_traffic() {
  // DESIGN.md section 8.6: whatever occupies `.git` is outside change tracking
  // and hydration accounting. It is the real local directory passed through,
  // so reading its metadata must not fetch a single blob.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let git = mount.join(".git");

  on_fs(move || {
    for name in ["HEAD", "config", "gfs.json"] {
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
async fn the_git_dir_is_writable_and_only_the_projection_is_not() {
  // ADR 0011 inverted ADR 0005's rule: `.git` is the real directory passed
  // through, and Git's own write protocol (lockfile create, rename, unlink)
  // must work against it. What stays read-only is exactly the projection at
  // `.git/gfs/objects` — a write there could only corrupt the shared store.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  on_fs(move || {
    // The lockfile protocol, verbatim: exclusive create, write, rename over.
    let lock = root.join(".git/config.lock");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options.open(&lock).unwrap();
    use std::io::Write as _;
    file.write_all(b"[gfs]\n\ttest = true\n").unwrap();
    drop(file);
    let second = options.open(&lock).unwrap_err();
    assert_eq!(
      second.raw_os_error(),
      Some(libc::EEXIST),
      "O_EXCL is forwarded, or every lock race is invisible"
    );
    std::fs::rename(&lock, root.join(".git/config.test")).unwrap();
    assert!(root.join(".git/config.test").is_file());
    std::fs::remove_file(root.join(".git/config.test")).unwrap();

    // The projection refuses writes: `EACCES` from `DefaultPermissions` over
    // its 0444/0555 modes, or `EROFS` from the handler as the second line of
    // the same defence.
    for (label, error) in [
      (
        "create in the projection",
        std::fs::write(root.join(".git/gfs/objects/intruder"), b"x").unwrap_err(),
      ),
      (
        "mkdir in the projection",
        std::fs::create_dir(root.join(".git/gfs/objects/newdir")).unwrap_err(),
      ),
    ] {
      assert!(
        matches!(error.raw_os_error(), Some(libc::EACCES) | Some(libc::EROFS)),
        "{label}: {error}"
      );
    }

    // And the boundary Git itself never gets to cross in the *tree*: a hard
    // link outside `.git` is `EPERM` for the life of the MVP, not a
    // consequence of read-only.
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
    gfs_overlay::OverlayConfig {
      quota_bytes: quota,
      ..gfs_overlay::OverlayConfig::default()
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

  let expected = gfs_test::git(
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
  let repo = gfs_types::RepositoryId::parse("r-verify").unwrap();
  let cache =
    gfs_mount::BlobCache::open(tmp.path(), &repo, gfs_types::HashAlgorithm::Sha1, 1 << 20).unwrap();

  let claimed =
    gfs_mount::cache::hash_blob(gfs_types::HashAlgorithm::Sha1, b"the real content").unwrap();
  let actual =
    gfs_mount::cache::hash_blob(gfs_types::HashAlgorithm::Sha1, b"tampered content").unwrap();
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
async fn an_unmounted_path_holds_only_its_git_rather_than_stale_content() {
  let backend = Backend::start("basic").await;
  let mut mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();
  let r = root.clone();
  on_fs(move || assert!(r.join("README.md").exists())).await;

  mount.unmount();

  // ADR 0011: what unmounting leaves is a plain folder with a fat `.git` —
  // the projected tree is gone, the state travels with the folder.
  let names = on_fs(move || read_dir_names(&root)).await;
  assert_eq!(names, vec![b".git".to_vec()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_spent_hydration_budget_refuses_the_open_with_edquot() {
  // ADR 0009's enforcement property, end to end through real syscalls: the
  // configuration that keeps a workspace cheap is overridable per invocation, so
  // the budget at the filesystem is the only limit a sweep cannot bypass. The
  // refusal must land on `open` -- a refusal per `read` makes `grep -r` print one
  // error per file and keep walking.
  let backend = Backend::start("basic").await;
  let mount = Mount::with_config(
    &backend,
    "main",
    FsConfig {
      // Fits README.md ("# basic\n", 8 bytes) and nothing after it.
      hydration_budget_bytes: 10,
      ..FsConfig::default()
    },
  )
  .await;

  let readme = mount.join("README.md");
  let main_rs = mount.join("src/main.rs");
  let (first, denied) = on_fs(move || {
    let first = std::fs::read(&readme).expect("the first small file fits the budget");
    let denied = std::fs::File::open(&main_rs).expect_err("the budget is spent");
    (first, denied)
  })
  .await;

  assert_eq!(first, b"# basic\n");
  assert_eq!(
    denied.raw_os_error(),
    Some(libc::EDQUOT),
    "EDQUOT, chosen for its strerror -- EIO would read as a corrupt filesystem: {denied:?}"
  );

  let report = mount.fs.budget_report();
  assert_eq!(report.refusals, 1, "{report:?}");
  assert_eq!(report.charged_bytes, 8, "{report:?}");
  assert_eq!(
    mount.fs.cache_stats().fetches,
    1,
    "the refused blob was never downloaded: an EDQUOT issued after the fetch \
     has already spent what it was meant to protect"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_budget_charges_a_blob_once_and_cached_reads_stay_free() {
  // Section 8.4 limits *new remote hydration* while preserving cached access. A
  // budget that counted every read would refuse a job for the cache's eviction
  // behaviour rather than its own appetite.
  let backend = Backend::start("basic").await;
  let mount = Mount::with_config(
    &backend,
    "main",
    FsConfig {
      hydration_budget_bytes: 10,
      ..FsConfig::default()
    },
  )
  .await;

  let path = mount.join("README.md");
  let p = path.clone();
  on_fs(move || std::fs::read(&p).unwrap()).await;
  // Well past the limit if re-reads were charged: 8 bytes x 5.
  for _ in 0..4 {
    let p = path.clone();
    on_fs(move || std::fs::read(&p).expect("a cached read is free")).await;
  }

  let report = mount.fs.budget_report();
  assert_eq!(report.charged_bytes, 8, "{report:?}");
  assert_eq!(report.refusals, 0, "{report:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fsync_of_a_git_directory_syncs_the_real_directory_not_the_overlay() {
  // The routing this asserts is load-bearing (ADR 0011 implementation notes):
  // SQLite fsyncs the overlay journal's own directory by its canonicalized
  // on-disk path — through this very mount — on some commits, while the
  // committing thread holds the overlay lock. An `fsyncdir` that reached
  // `Overlay::sync` from a `.git` subtree handle would therefore deadlock the
  // mount against its own journal, which is exactly how the first
  // `echo >> README.md` into a real workspace hung.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  on_fs(move || {
    for dir in [".git", ".git/gfs", ".git/objects"] {
      let handle = std::fs::File::open(root.join(dir)).unwrap();
      handle
        .sync_all()
        .unwrap_or_else(|e| panic!("fsync of {dir} through the mount: {e}"));
    }
    // The projection has nothing local to make durable, and must say so
    // rather than stall.
    let projected = std::fs::File::open(root.join(".git/gfs/objects")).unwrap();
    projected.sync_all().unwrap();
  })
  .await;
}
