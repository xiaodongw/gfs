//! M3.4's second half: the failures a writable mount meets while it is running.
//!
//! The crash matrix in `gfs-test/tests/overlay_crash.rs` covers what a dead
//! process leaves behind. These cover what a live one does when the world stops
//! cooperating — the quota fills, the server goes away mid-copy-up, two writers
//! race, a rename cycles, and the mount is torn down with descriptors still open.
//!
//! Every case asserts the same shape of thing: the failure is *reported*, and the
//! edits that were already acknowledged are still there afterwards. A filesystem
//! that answered a full quota by dropping a previous write would pass a test that
//! only checked the errno.

use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;

use gfs_test::mount::{on_fs, read_dir_names, Backend, Job, Mount};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_exhausted_quota_fails_the_new_write_and_keeps_the_old_ones() {
  // PLAN.md M3.2: "enforce per-job overlay disk quota without endangering
  // existing edits". The distinction that matters is between a write that is
  // refused and a workspace that is damaged.
  let backend = Backend::start("basic").await;
  let mount = Mount::with_configs(
    &backend,
    "main",
    gfs_mount::FsConfig::default(),
    gfs_overlay::OverlayConfig {
      quota_bytes: 32 * 1024,
      ..gfs_overlay::OverlayConfig::default()
    },
  )
  .await;
  let root = mount.path.clone();

  let (kept, refused) = on_fs(move || {
    std::fs::write(root.join("kept.txt"), b"this must survive\n").unwrap();

    // Fill the quota, then try once more.
    let mut filler = std::fs::File::create(root.join("filler.bin")).unwrap();
    while filler.write_all(&[b'z'; 8192]).is_ok() {}
    let refused = std::fs::write(root.join("late.txt"), vec![b'q'; 8192]);

    (std::fs::read(root.join("kept.txt")).unwrap(), refused)
  })
  .await;

  assert_eq!(kept, b"this must survive\n", "the earlier edit is intact");
  let error = refused.expect_err("a write past the quota must fail");
  assert_eq!(
    error.raw_os_error(),
    Some(libc::EDQUOT),
    "EDQUOT, not ENOSPC: the host disk is fine, the job's budget is not"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn losing_the_server_fails_a_copy_up_without_damaging_the_overlay() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  // One acknowledged edit before the server goes away.
  let established = root.clone();
  on_fs(move || std::fs::write(established.join("local.txt"), b"already written\n").unwrap()).await;
  backend.stop();
  // The listener stops accepting; give it a moment to actually close.
  tokio::time::sleep(std::time::Duration::from_millis(100)).await;

  let (copy_up, local) = on_fs(move || {
    // `README.md` has never been read, so copying it up needs the network.
    let copy_up = std::fs::OpenOptions::new()
      .read(true)
      .write(true)
      .open(root.join("README.md"));
    // And the overlay keeps serving what it already holds.
    (copy_up.map(|_| ()), std::fs::read(root.join("local.txt")))
  })
  .await;

  assert!(
    copy_up.is_err(),
    "a copy-up that cannot reach the server must fail rather than invent content"
  );
  assert_eq!(
    local.unwrap(),
    b"already written\n",
    "an overlay file needs no server at all"
  );
  assert!(mount.overlay.recovery().is_clean());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writers_to_different_files_all_land() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let mut writers = Vec::new();
  for index in 0..16 {
    let dir = root.clone();
    writers.push(on_fs(move || {
      let path = dir.join(format!("concurrent-{index:02}.txt"));
      std::fs::write(&path, format!("writer {index}\n")).unwrap();
      std::fs::read(&path).unwrap()
    }));
  }
  for (index, writer) in writers.into_iter().enumerate() {
    assert_eq!(writer.await, format!("writer {index}\n").into_bytes());
  }

  let names = on_fs(move || read_dir_names(&root)).await;
  let written = names
    .iter()
    .filter(|n| n.starts_with(b"concurrent-"))
    .count();
  assert_eq!(
    written, 16,
    "every concurrent create is listed exactly once"
  );
  assert_eq!(mount.overlay.stats().entries, 16);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writers_to_one_file_leave_it_consistent() {
  // Not a test of who wins -- POSIX does not say, and neither does GFS. What it
  // asserts is that the *size accounting* survives the race: the journal's idea
  // of the file must match what is on disk, or every later read is truncated.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let setup = root.clone();
  on_fs(move || std::fs::write(setup.join("contested.txt"), vec![b'.'; 4096]).unwrap()).await;

  let mut writers = Vec::new();
  for index in 0..8u8 {
    let dir = root.clone();
    writers.push(on_fs(move || {
      let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join("contested.txt"))
        .unwrap();
      file.seek(SeekFrom::Start(u64::from(index) * 512)).unwrap();
      file.write_all(&[b'a' + index; 512]).unwrap();
    }));
  }
  for writer in writers {
    writer.await;
  }

  let (size, bytes) = on_fs(move || {
    let path = root.join("contested.txt");
    (
      std::fs::metadata(&path).unwrap().len(),
      std::fs::read(&path).unwrap(),
    )
  })
  .await;
  assert_eq!(size, 4096);
  assert_eq!(bytes.len(), 4096, "the journal's size matches the content");
  assert_eq!(
    mount
      .overlay
      .get(&gfs_types::BytePath::new(b"contested.txt".to_vec()))
      .unwrap()
      .size,
    4096
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rename_cycle_leaves_every_file_at_exactly_one_path() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let names = on_fs(move || {
    std::fs::write(root.join("a.txt"), b"A\n").unwrap();
    std::fs::write(root.join("b.txt"), b"B\n").unwrap();
    // The three-step swap every editor and every build system performs.
    std::fs::rename(root.join("a.txt"), root.join("tmp.txt")).unwrap();
    std::fs::rename(root.join("b.txt"), root.join("a.txt")).unwrap();
    std::fs::rename(root.join("tmp.txt"), root.join("b.txt")).unwrap();
    (
      std::fs::read(root.join("a.txt")).unwrap(),
      std::fs::read(root.join("b.txt")).unwrap(),
      read_dir_names(&root),
    )
  })
  .await;

  assert_eq!(names.0, b"B\n", "the files swapped");
  assert_eq!(names.1, b"A\n");
  assert!(
    !names.2.contains(&b"tmp.txt".to_vec()),
    "the intermediate name is gone: {:?}",
    names.2
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deleted_base_directory_recreated_and_refilled_stays_consistent() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let names = on_fs(move || {
    for _ in 0..3 {
      std::fs::remove_dir_all(root.join("src")).unwrap();
      std::fs::create_dir(root.join("src")).unwrap();
      std::fs::write(root.join("src/only.rs"), b"fn only() {}\n").unwrap();
    }
    read_dir_names(&root.join("src"))
  })
  .await;

  assert_eq!(names, vec![b"only.rs".to_vec()], "{names:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unmounting_with_open_descriptors_does_not_lose_a_committed_write() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let ws = job.workspace.clone();

  // A descriptor left open across the teardown, with its write already
  // acknowledged.
  let handle = on_fs({
    let ws = ws.clone();
    move || {
      let mut file = std::fs::File::create(ws.join("open-across-unmount.txt")).unwrap();
      file.write_all(b"acknowledged before teardown\n").unwrap();
      file
    }
  })
  .await;

  job.daemon.shutdown().await;
  drop(handle);

  // The overlay outlives the mount, so a restarted daemon resumes the job.
  let resumed = Job::start(&backend, "main").await;
  let _ = resumed;

  // Reopened directly, because the point is that the *state directory* holds it.
  let overlay = gfs_overlay::Overlay::open(
    &job.state_dir.join("overlay"),
    &gfs_overlay::Binding {
      repository_id: backend.repo_id.as_str().to_owned(),
      base_commit: job.daemon.inspect().commit,
    },
    job.daemon.inspect().snapshot_time,
    gfs_overlay::OverlayConfig::default(),
  )
  .expect("the overlay reopens after an unmount with open handles");

  let entry = overlay
    .get(&gfs_types::BytePath::new(
      b"open-across-unmount.txt".to_vec(),
    ))
    .expect("the acknowledged write survived the teardown");
  let mut bytes = Vec::new();
  overlay
    .open_content(&entry)
    .unwrap()
    .read_to_end(&mut bytes)
    .unwrap();
  assert_eq!(bytes, b"acknowledged before teardown\n");
  assert!(overlay.recovery().is_clean(), "{:?}", overlay.recovery());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_restart_resumes_the_workspace_it_left() {
  // The whole reason the overlay is durable rather than in memory: an OOM kill
  // or a node drain in the middle of a job must not throw the job's work away.
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let ws = job.workspace.clone();

  on_fs({
    let ws = ws.clone();
    move || {
      std::fs::write(ws.join("survivor.txt"), b"written before the restart\n").unwrap();
      std::fs::remove_file(ws.join("README.md")).unwrap();
    }
  })
  .await;
  job.daemon.shutdown().await;
  // The `Job` value stays alive on purpose: dropping it would take its temporary
  // directory -- and the workspace folder inside it -- with it, which is not
  // what a supervisor restart looks like.

  // A new daemon over the same workspace folder: exactly what a supervisor
  // does, and all it has to name — the state travels inside (ADR 0011).
  let resumed = Job::with_workspace(&backend, "main", &ws).await;
  let workspace = resumed.workspace.clone();
  let (content, missing, names) = on_fs(move || {
    (
      std::fs::read(workspace.join("survivor.txt")).unwrap(),
      std::fs::metadata(workspace.join("README.md")).is_err(),
      read_dir_names(&workspace),
    )
  })
  .await;

  assert_eq!(content, b"written before the restart\n");
  assert!(missing, "the deletion survived too: {names:?}");
  assert_eq!(resumed.daemon.inspect().overlay.entries, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_eviction_does_not_disturb_overlay_content() {
  // The blob cache is bounded and the overlay is not: evicting a base blob must
  // not touch a copied-up file, and a copied-up file must not need the cache.
  let backend = Backend::start("content").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let bytes = on_fs(move || {
    std::fs::write(root.join("crlf.txt"), b"copied up\n").unwrap();
    // Read enough other blobs to churn the cache.
    let _ = std::fs::read(root.join("huge-line.txt")).unwrap();
    let _ = std::fs::read(root.join("large-blob.bin")).unwrap();
    std::fs::read(root.join("crlf.txt")).unwrap()
  })
  .await;
  assert_eq!(bytes, b"copied up\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_only_state_directory_fails_the_write_rather_than_the_mount() {
  // A permission failure under the overlay is an I/O error on the write, not a
  // panic and not a corrupt journal. `EIO` is what the caller can act on.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();
  let overlay = Arc::clone(&mount.overlay);

  let files = overlay.content_store().root().to_path_buf();
  let restore = files.clone();
  on_fs(move || {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&files, std::fs::Permissions::from_mode(0o500)).unwrap();
  })
  .await;

  let failed = on_fs(move || std::fs::write(root.join("denied.txt"), b"x")).await;

  on_fs(move || {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&restore, std::fs::Permissions::from_mode(0o700)).unwrap();
  })
  .await;

  let error = failed.expect_err("a write into an unwritable overlay must fail");
  assert_eq!(error.raw_os_error(), Some(libc::EIO), "{error}");
  assert!(mount.overlay.recovery().is_clean());
}
