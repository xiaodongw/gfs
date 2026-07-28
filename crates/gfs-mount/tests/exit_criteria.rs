//! M2's six exit criteria, each measured rather than asserted from intuition.
//!
//! Run with `--nocapture` to see the numbers; the M2 report quotes them.
//!
//! Two of the six are covered elsewhere and referenced here rather than
//! duplicated: refresh generation isolation is
//! `lifecycle.rs::refresh_swaps_generations_and_keeps_open_handles_on_the_old_one`,
//! and selective transfer is measured both here and in
//! `mount.rs::reading_one_file_does_not_hydrate_its_siblings`.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use gfs_mount::daemon::{Daemon, DaemonConfig};
use gfs_mount::{FsConfig, MountConfig};
use gfs_test::mount::{on_fs, Backend, Mount};
use gfs_types::LeasePolicy;

/// ADR 0006's performance gate: cold mount to a usable root.
const COLD_MOUNT_TARGET: Duration = Duration::from_secs(2);
const COLD_MOUNT_BYTES: u64 = 10 * 1024 * 1024;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn criterion_1_cold_mount_meets_the_startup_and_download_target() {
  // The whole path an orchestrator pays for: CreateMount, the FUSE session,
  // publication, and the first directory listing that makes the workspace usable.
  let backend = Backend::start("bigdir").await;
  let tmp = tempfile::tempdir().unwrap();
  let cache = tempfile::tempdir().unwrap();
  let workspace = tmp.path().join("ws");

  let started = Instant::now();
  let daemon = Daemon::start(DaemonConfig {
    state_dir: tmp.path().join("ws.gfs"),
    workspace: workspace.clone(),
    cache_dir: cache.path().to_path_buf(),
    grpc_endpoint: backend.grpc.clone(),
    http_endpoint: backend.http.clone(),
    token: gfs_test::mount::TOKEN.to_owned(),
    repository_id: backend.repo_id.clone(),
    revision_selector: "main".to_owned(),
    cache_quota_bytes: 1 << 30,
    fs: FsConfig::default(),
    overlay: gfs_overlay::OverlayConfig::default(),
    mount: MountConfig::default(),
    lease_policy: LeasePolicy::adr_0006(),
    retire_timeout: Duration::from_secs(5),
  })
  .await
  .unwrap();

  let w = workspace.clone();
  let root_entries = on_fs(move || std::fs::read_dir(&w).unwrap().count()).await;
  let elapsed = started.elapsed();
  let report = daemon.inspect();

  println!(
    "cold mount to a usable root: {:?}, {} blob bytes, {} root entries",
    elapsed, report.cache.bytes_fetched, root_entries
  );

  assert!(
    elapsed < COLD_MOUNT_TARGET,
    "cold mount took {elapsed:?}, target is {COLD_MOUNT_TARGET:?}"
  );
  assert!(
    report.cache.bytes_fetched < COLD_MOUNT_BYTES,
    "cold mount downloaded {} bytes",
    report.cache.bytes_fetched
  );
  // The stronger claim, and the one the design rests on: reaching a usable root
  // downloads no file content at all. The 10 MiB target is a ceiling, not a
  // budget to spend.
  assert_eq!(report.cache.bytes_fetched, 0);

  daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn criterion_2_reading_selected_files_transfers_only_what_is_needed() {
  // The `content` fixture holds 16 MiB across seven files. Reading two of them
  // must cost the bytes of those two and nothing else.
  let backend = Backend::start("content").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let read = on_fs(move || {
    std::fs::read(root.join("crlf.txt")).unwrap().len()
      + std::fs::read(root.join("binary.bin")).unwrap().len()
  })
  .await;

  let cache = mount.fs.cache_stats();
  let stats = mount.fs.stats();
  println!(
    "selective read: {} bytes wanted, {} fetched, {} blobs, {} metadata requests",
    read, cache.bytes_fetched, cache.fetches, stats.metadata_requests
  );

  assert_eq!(cache.fetches, 2, "one blob per file read");
  assert_eq!(
    cache.bytes_fetched, read as u64,
    "not one byte more than the files themselves"
  );
  // The 12 MiB and 4 MiB blobs were never touched.
  assert!(cache.bytes_fetched < 1024);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn criterion_3_a_representative_read_only_build_and_analysis_task_succeeds() {
  // What "representative" means here is bounded by what M0 could settle. ADR
  // 0006 records question 2 -- which monorepos and workloads define success --
  // as **unresolved and needing product input**, so this cannot be the pilot's
  // real task corpus. What it can be, and is, is the two shapes every such task
  // reduces to: a compiler reading sources through the mount, and an analysis
  // tool sweeping it. The `.git` probing half is covered in `compat.rs`.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();
  let out = tempfile::tempdir().unwrap();
  let binary = out.path().join("hello");

  let (compiled, matched) = on_fs(move || {
    // A real compiler, reading its input through FUSE and writing outside it.
    let status = std::process::Command::new("rustc")
      .arg("--edition")
      .arg("2021")
      .arg("-o")
      .arg(&binary)
      .arg(root.join("src/main.rs"))
      .status()
      .expect("rustc must be available in a Rust project's test environment");

    // An analysis sweep, which is what an agent does before it edits anything.
    let grep = std::process::Command::new("grep")
      .args(["-rl", "fn main"])
      .arg(&root)
      .output()
      .expect("grep");

    (
      status.success() && binary.is_file(),
      String::from_utf8_lossy(&grep.stdout).lines().count(),
    )
  })
  .await;

  assert!(
    compiled,
    "rustc must build a source file read from the mount"
  );
  assert_eq!(matched, 1, "grep found exactly src/main.rs");

  let cache = mount.fs.cache_stats();
  println!(
    "build + analysis task: {} blobs, {} bytes hydrated",
    cache.fetches, cache.bytes_fetched
  );
  // `grep -r` reads every file, so this is *not* a claim about hydration -- it
  // is the measurement DESIGN.md section 8.4 warns about: a program that opens
  // every file hydrates every file unless a budget stops it.
  assert!(cache.fetches >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn criterion_4_a_future_dated_commit_reports_a_sane_base_timestamp() {
  // ADR 0006 acceptance case 1, end to end against a real 2050-dated commit
  // rather than a hand-built `Timestamp`. A build system that sees a source file
  // dated in the future rebuilds forever or never.
  use std::os::unix::fs::MetadataExt;

  let backend = Backend::start("future").await;
  let mount = Mount::new(&backend, "main").await;
  let path = mount.join("future.txt");

  let mtime = on_fs(move || std::fs::metadata(&path).unwrap().mtime()).await;
  let now = SystemTime::now()
    .duration_since(SystemTime::UNIX_EPOCH)
    .unwrap()
    .as_secs() as i64;

  // 2050-01-01T00:00:00Z.
  const COMMITTER_TIME: i64 = 2_524_608_000;

  println!("future-dated commit: committer {COMMITTER_TIME} (2050), reported mtime {mtime}");
  assert!(
    mtime < COMMITTER_TIME,
    "the raw committer timestamp reached the filesystem unclamped"
  );
  // `<=` rather than `<`: the clamp lands one *nanosecond* below the catalog's
  // first-seen time, so at second granularity the two are usually equal. What
  // matters is that it is not in the future.
  assert!(
    mtime <= now,
    "a clamped timestamp must not be in the future, got {mtime} against {now}"
  );
  assert!(
    mtime >= gfs_types::time::MIN_SUPPORTED_UNIX_SECS,
    "and must stay above the 1990 floor"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn criterion_4_base_timestamps_are_identical_across_remounts() {
  use std::os::unix::fs::MetadataExt;

  let backend = Backend::start("basic").await;
  let mut observed = Vec::new();
  for _ in 0..3 {
    let mount = Mount::new(&backend, "main").await;
    let path = mount.join("src/main.rs");
    let meta = on_fs(move || std::fs::metadata(&path).unwrap()).await;
    observed.push((meta.mtime(), meta.mtime_nsec(), meta.ctime()));
  }
  println!("base timestamp across three mounts: {:?}", observed[0]);
  assert!(
    observed.windows(2).all(|w| w[0] == w[1]),
    "base timestamps drifted across remounts: {observed:?}"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn criterion_6_daemon_failure_does_not_corrupt_the_shared_cache() {
  // "Daemon or server failure does not corrupt the shared cache." The cache is
  // shared between mounts of one repository on a host, so a daemon that dies
  // mid-fetch must leave nothing a later daemon can mistake for a complete
  // object.
  let backend = Backend::start("basic").await;
  let cache_dir = tempfile::tempdir().unwrap();

  // A first daemon warms the cache, then dies without a clean shutdown.
  let expected = {
    let tmp = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(DaemonConfig {
      state_dir: tmp.path().join("ws.gfs"),
      workspace: tmp.path().join("ws"),
      cache_dir: cache_dir.path().to_path_buf(),
      grpc_endpoint: backend.grpc.clone(),
      http_endpoint: backend.http.clone(),
      token: gfs_test::mount::TOKEN.to_owned(),
      repository_id: backend.repo_id.clone(),
      revision_selector: "main".to_owned(),
      cache_quota_bytes: 1 << 30,
      fs: FsConfig::default(),
      overlay: gfs_overlay::OverlayConfig::default(),
      mount: MountConfig::default(),
      lease_policy: LeasePolicy::adr_0006(),
      retire_timeout: Duration::from_secs(5),
    })
    .await
    .unwrap();

    let workspace = tmp.path().join("ws");
    let content = on_fs(move || std::fs::read(workspace.join("README.md")).unwrap()).await;

    // Simulate the crash: a half-written temporary in the cache, of the kind an
    // interrupted download leaves behind.
    let partial = cache_dir
      .path()
      .join("blobs")
      .join(backend.repo_id.as_str())
      .join("sha1");
    let shard = std::fs::read_dir(&partial)
      .unwrap()
      .flatten()
      .next()
      .expect("the cache has at least one shard")
      .path();
    std::fs::write(shard.join(".deadbeef.99999"), b"half a blob").unwrap();

    // No `shutdown()`: the session is dropped, which is what a killed daemon
    // leaves behind.
    drop(daemon);
    content
  };

  // A second daemon over the same cache.
  let tmp = tempfile::tempdir().unwrap();
  let cache = gfs_mount::BlobCache::open(
    cache_dir.path(),
    &backend.repo_id,
    gfs_types::HashAlgorithm::Sha1,
    1 << 30,
  )
  .unwrap();
  let adopted = cache.bytes_on_disk();

  let daemon = Daemon::start(DaemonConfig {
    state_dir: tmp.path().join("ws.gfs"),
    workspace: tmp.path().join("ws"),
    cache_dir: cache_dir.path().to_path_buf(),
    grpc_endpoint: backend.grpc.clone(),
    http_endpoint: backend.http.clone(),
    token: gfs_test::mount::TOKEN.to_owned(),
    repository_id: backend.repo_id.clone(),
    revision_selector: "main".to_owned(),
    cache_quota_bytes: 1 << 30,
    fs: FsConfig::default(),
    overlay: gfs_overlay::OverlayConfig::default(),
    mount: MountConfig::default(),
    lease_policy: LeasePolicy::adr_0006(),
    retire_timeout: Duration::from_secs(5),
  })
  .await
  .unwrap();

  let workspace = tmp.path().join("ws");
  let again = on_fs(move || std::fs::read(workspace.join("README.md")).unwrap()).await;
  let report = daemon.inspect();

  println!(
    "after an unclean daemon exit: {adopted} bytes adopted, {} re-fetched, {} cache hits",
    report.cache.bytes_fetched, report.cache.hits
  );

  assert_eq!(again, expected, "the content survived unchanged");
  assert_eq!(
    report.cache.verification_failures, 0,
    "nothing in the cache failed verification"
  );
  assert_eq!(
    report.cache.bytes_fetched, 0,
    "the warm entry was reused rather than re-downloaded"
  );

  daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn criterion_6_server_failure_leaves_cached_content_readable() {
  let backend = Backend::start("basic").await;
  let mount = Mount::with_config(
    &backend,
    "main",
    FsConfig {
      attempts: 1,
      ..FsConfig::default()
    },
  )
  .await;

  let warm = mount.join("README.md");
  let w = warm.clone();
  let before = on_fs(move || std::fs::read(&w).unwrap()).await;

  backend.stop();
  tokio::time::sleep(Duration::from_millis(200)).await;

  let after = on_fs(move || std::fs::read(&warm).unwrap()).await;
  assert_eq!(before, after);
  assert_eq!(mount.fs.cache_stats().verification_failures, 0);
}

/// Referenced from the report: the refresh criterion lives in `lifecycle.rs`,
/// and this asserts the two files stay in step rather than one silently losing
/// the case.
#[test]
fn criterion_5_is_covered_in_the_lifecycle_suite() {
  let source = include_str!("lifecycle.rs");
  assert!(
    source.contains("fn refresh_swaps_generations_and_keeps_open_handles_on_the_old_one"),
    "the refresh generation-isolation criterion must stay covered"
  );
  // Silence the unused-import warning that would otherwise appear if this file
  // ever loses its async tests.
  let _ = Arc::new(0);
}
