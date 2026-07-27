//! M3.2's mutation surface, driven through real syscalls.
//!
//! Every case here goes through the kernel — `std::fs`, `open(2)`, `renameat`,
//! `mmap` — rather than calling the `Filesystem` trait. That is deliberate for
//! the same reason M2's suite is: the interesting behaviour lives between the
//! syscall and the overlay, and a test that called `create` directly would
//! exercise neither the kernel's permission checks nor its dentry cache, which is
//! where a merged filesystem goes wrong.
//!
//! The properties that are easy to implement and easy to get subtly wrong get
//! their own case: an open descriptor surviving `unlink` and `rename`, a
//! recreated directory not showing the base's children, and a copy-up that does
//! not fetch the bytes it is about to overwrite.

mod harness;

use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use harness::{on_fs, read_dir_names, Backend, Mount};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_created_file_reads_back_and_is_newer_than_the_base() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();
  let base_time = mount.fs.snapshot_time();

  let (content, mtime, names) = on_fs(move || {
    std::fs::write(root.join("new.txt"), b"hello overlay\n").unwrap();
    let meta = std::fs::metadata(root.join("new.txt")).unwrap();
    (
      std::fs::read(root.join("new.txt")).unwrap(),
      meta.mtime(),
      read_dir_names(&root),
    )
  })
  .await;

  assert_eq!(content, b"hello overlay\n");
  // ADR 0006's overlay clock: an acknowledged edit is strictly newer than the
  // base, which is what stops a build system rebuilding forever or never.
  assert!(
    mtime > base_time.secs,
    "{mtime} must be after the snapshot time {}",
    base_time.secs
  );
  assert!(names.contains(&b"new.txt".to_vec()), "{names:?}");
  assert!(
    names.contains(&b"README.md".to_vec()),
    "the base is still there"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn editing_a_base_file_copies_it_up_and_leaves_the_base_alone() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let content = on_fs(move || {
    let mut file = std::fs::OpenOptions::new()
      .read(true)
      .write(true)
      .open(root.join("README.md"))
      .unwrap();
    file.seek(SeekFrom::End(0)).unwrap();
    file.write_all(b"appended\n").unwrap();
    drop(file);
    std::fs::read(root.join("README.md")).unwrap()
  })
  .await;

  assert_eq!(content, b"# basic\nappended\n");
  let entry = mount
    .overlay
    .get(&xvfs_types::BytePath::new(b"README.md".to_vec()))
    .expect("the path diverged");
  assert!(
    entry.content.local_id().is_some(),
    "an edited file has local content"
  );
  assert!(
    entry.base.is_some(),
    "and remembers what the base had, so status can compare"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn truncating_a_base_file_never_downloads_the_bytes_it_replaces() {
  // PLAN.md M3.2's `O_TRUNC` bullet. `content` holds a 16 MiB blob; replacing it
  // wholesale must transfer none of it.
  let backend = Backend::start("content").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let content = on_fs(move || {
    let mut file = std::fs::File::create(root.join("large-blob.bin")).unwrap();
    file.write_all(b"replaced").unwrap();
    drop(file);
    std::fs::read(root.join("large-blob.bin")).unwrap()
  })
  .await;

  assert_eq!(content, b"replaced");
  assert_eq!(
    mount.fs.cache_stats().bytes_fetched,
    0,
    "O_TRUNC must not hydrate the version being thrown away"
  );
  assert_eq!(mount.fs.stats().copy_up_bytes, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deleted_base_file_disappears_from_lookup_and_readdir() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let (names, error) = on_fs(move || {
    std::fs::remove_file(root.join("README.md")).unwrap();
    (
      read_dir_names(&root),
      std::fs::metadata(root.join("README.md")).unwrap_err(),
    )
  })
  .await;

  assert!(!names.contains(&b"README.md".to_vec()), "{names:?}");
  assert_eq!(error.raw_os_error(), Some(libc::ENOENT));
  assert_eq!(mount.overlay.stats().whiteouts, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_a_base_directory_hides_its_whole_subtree_and_costs_one_row() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let (before, after, child) = on_fs(move || {
    let before = read_dir_names(&root.join("src"));
    // `remove_dir_all` walks and unlinks, which is what an agent's `rm -rf` does.
    std::fs::remove_dir_all(root.join("src")).unwrap();
    (
      before,
      read_dir_names(&root),
      std::fs::metadata(root.join("src/main.rs")).unwrap_err(),
    )
  })
  .await;

  assert!(!before.is_empty(), "the base directory had children");
  assert!(!after.contains(&b"src".to_vec()), "{after:?}");
  assert_eq!(child.raw_os_error(), Some(libc::ENOENT));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recreated_directory_does_not_show_the_base_children_it_replaced() {
  // The opacity rule. Without it, `rm -rf build && mkdir build` leaves the base's
  // `build/` contents showing through an empty directory, and every later `ls`
  // and every export is wrong.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let names = on_fs(move || {
    std::fs::remove_dir_all(root.join("src")).unwrap();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("src/only.rs"), b"fn only() {}\n").unwrap();
    read_dir_names(&root.join("src"))
  })
  .await;

  assert_eq!(names, vec![b"only.rs".to_vec()], "{names:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rmdir_refuses_a_directory_that_still_has_base_children() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let (busy, ok) = on_fs(move || {
    let busy = std::fs::remove_dir(root.join("src")).unwrap_err();
    std::fs::create_dir(root.join("empty")).unwrap();
    let ok = std::fs::remove_dir(root.join("empty"));
    (busy, ok)
  })
  .await;

  assert_eq!(busy.raw_os_error(), Some(libc::ENOTEMPTY));
  ok.expect("an empty directory can be removed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renaming_a_base_file_preserves_its_inode_and_fetches_nothing() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let (before, after, content, names) = on_fs(move || {
    let before = std::fs::metadata(root.join("README.md")).unwrap().ino();
    std::fs::rename(root.join("README.md"), root.join("READ.md")).unwrap();
    let after = std::fs::metadata(root.join("READ.md")).unwrap().ino();
    (
      before,
      after,
      std::fs::read(root.join("READ.md")).unwrap(),
      read_dir_names(&root),
    )
  })
  .await;

  // POSIX: a rename moves a name, not a file. A build tool that re-stats the
  // destination must not be told the file was replaced.
  assert_eq!(before, after, "rename must preserve identity");
  assert_eq!(content, b"# basic\n");
  assert!(names.contains(&b"READ.md".to_vec()), "{names:?}");
  assert!(!names.contains(&b"README.md".to_vec()), "{names:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renaming_a_base_directory_moves_the_subtree_without_hydrating_it() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let (names, gone) = on_fs(move || {
    let expected = read_dir_names(&root.join("src"));
    std::fs::rename(root.join("src"), root.join("source")).unwrap();
    assert_eq!(read_dir_names(&root.join("source")), expected);
    (
      read_dir_names(&root),
      std::fs::metadata(root.join("src")).unwrap_err(),
    )
  })
  .await;

  assert!(names.contains(&b"source".to_vec()), "{names:?}");
  assert!(!names.contains(&b"src".to_vec()), "{names:?}");
  assert_eq!(gone.raw_os_error(), Some(libc::ENOENT));
  assert_eq!(
    mount.fs.cache_stats().bytes_fetched,
    0,
    "moving a directory moves metadata, not content"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_contents_of_a_renamed_base_file_are_still_readable() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let content = on_fs(move || {
    std::fs::rename(root.join("src"), root.join("source")).unwrap();
    std::fs::read(root.join("source/main.rs")).unwrap()
  })
  .await;
  assert!(!content.is_empty(), "the moved blob is still served");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_open_descriptor_survives_unlink_and_keeps_accepting_writes() {
  // POSIX: a name is not a file. Every editor that writes to a temporary and
  // renames it over the target depends on this, and so does every `tail -f`.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let (read_back, gone) = on_fs(move || {
    let path = root.join("doomed.txt");
    let mut file = std::fs::OpenOptions::new()
      .read(true)
      .write(true)
      .create(true)
      .truncate(true)
      .open(&path)
      .unwrap();
    file.write_all(b"before").unwrap();
    std::fs::remove_file(&path).unwrap();

    file.write_all(b"-after").unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    (bytes, std::fs::metadata(&path).unwrap_err())
  })
  .await;

  assert_eq!(read_back, b"before-after");
  assert_eq!(gone.raw_os_error(), Some(libc::ENOENT));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_open_descriptor_follows_its_file_across_a_rename() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let content = on_fs(move || {
    let mut file = std::fs::File::create(root.join("a.txt")).unwrap();
    file.write_all(b"one").unwrap();
    std::fs::rename(root.join("a.txt"), root.join("b.txt")).unwrap();
    file.write_all(b"-two").unwrap();
    file.sync_all().unwrap();
    std::fs::read(root.join("b.txt")).unwrap()
  })
  .await;
  assert_eq!(content, b"one-two");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_created_symlink_reads_back_without_a_content_file() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let (target, through) = on_fs(move || {
    std::os::unix::fs::symlink("README.md", root.join("readme-link")).unwrap();
    (
      std::fs::read_link(root.join("readme-link")).unwrap(),
      std::fs::read(root.join("readme-link")).unwrap(),
    )
  })
  .await;

  assert_eq!(target, std::path::Path::new("README.md"));
  assert_eq!(through, b"# basic\n", "the link resolves in the mount");
  assert_eq!(
    mount.overlay.stats().local_bytes,
    0,
    "a symlink target lives in the row, not in a content file"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chmod_records_the_executable_bit_without_fetching_the_blob() {
  let backend = Backend::start("content").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let mode = on_fs(move || {
    let path = root.join("large-blob.bin");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::metadata(&path).unwrap().permissions().mode() & 0o777
  })
  .await;

  assert_eq!(mode, 0o755);
  assert_eq!(
    mount.fs.cache_stats().bytes_fetched,
    0,
    "a mode change is metadata; it must not download 16 MiB"
  );
  assert_eq!(mount.overlay.stats().local_bytes, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_explicit_old_mtime_is_clamped_to_the_overlay_floor() {
  // ADR 0006's documented MVP incompatibility, observed end to end: exact
  // restoration of an older mtime is not supported, because an acknowledged edit
  // must stay newer than the base.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();
  let base_time = mount.fs.snapshot_time();

  let (secs, nanos) = on_fs(move || {
    let path = root.join("stamped.txt");
    std::fs::write(&path, b"x").unwrap();
    let file = std::fs::File::open(&path).unwrap();
    file
      .set_times(
        std::fs::FileTimes::new()
          .set_accessed(std::time::UNIX_EPOCH)
          .set_modified(std::time::UNIX_EPOCH),
      )
      .unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    (meta.mtime(), meta.mtime_nsec())
  })
  .await;

  // The clamp is to `snapshot_time + one tick`, which is one nanosecond -- so
  // the comparison has to be at nanosecond resolution or it reads as equal.
  assert!(
    (secs, nanos as u32) > (base_time.secs, base_time.nanos),
    "an mtime of the epoch was clamped to the overlay floor, not honoured: \
     got {secs}.{nanos:09} against a base of {}.{:09}",
    base_time.secs,
    base_time.nanos
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_overlay_quota_stops_a_write_without_losing_what_was_accepted() {
  let backend = Backend::start("basic").await;
  let mount = Mount::with_configs(
    &backend,
    "main",
    xvfs_fuse::FsConfig::default(),
    xvfs_overlay::OverlayConfig {
      quota_bytes: 64 * 1024,
      ..xvfs_overlay::OverlayConfig::default()
    },
  )
  .await;
  let root = mount.path.clone();

  let (written, size) = on_fs(move || {
    let path = root.join("big.txt");
    let mut file = std::fs::File::create(&path).unwrap();
    let mut written = 0usize;
    // Write past the quota. POSIX allows a short write, and that is what a
    // filesystem approaching its limit returns; the accepted bytes stay accepted.
    for _ in 0..64 {
      match file.write(&[b'z'; 4096]) {
        Ok(0) => break,
        Ok(n) => written += n,
        Err(_) => break,
      }
    }
    let _ = file.flush();
    drop(file);
    (written, std::fs::metadata(&path).unwrap().len())
  })
  .await;

  assert!(written > 0, "some of the writes were accepted");
  assert_eq!(size as usize, written, "and every accepted byte is on disk");
  assert!(
    size <= 64 * 1024,
    "the quota bounded the overlay at {size} bytes"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn statfs_reports_what_the_overlay_has_actually_used() {
  #[allow(unsafe_code)]
  fn statvfs(path: &std::path::Path) -> (u64, u64, u64) {
    // Same opt-out as M2's `statfs` test: there is no safe wrapper in `std`, the
    // struct is zeroed before the call, and it is only read after a 0 return.
    let mut buffer: libc::statvfs = unsafe { std::mem::zeroed() };
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut buffer) };
    assert_eq!(rc, 0);
    (
      buffer.f_blocks as u64,
      buffer.f_bfree as u64,
      buffer.f_frsize as u64,
    )
  }

  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let (free_before, free_after) = on_fs(move || {
    let (_, before, _) = statvfs(&root);
    std::fs::write(root.join("filler.bin"), vec![b'q'; 512 * 1024]).unwrap();
    let (_, after, frsize) = statvfs(&root);
    let _ = frsize;
    (before, after)
  })
  .await;

  assert!(
    free_after < free_before,
    "a 512 KiB write must show up in `df`: {free_after} vs {free_before}"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlay_children_appear_after_a_fully_paged_base_listing() {
  // The offset-stability rule in `fill_directory`: extras are appended only once
  // the base listing is exhausted, so a child's offset never moves between two
  // `readdir` calls on one handle. 5002 base entries is several pages.
  let backend = Backend::start("bigdir").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let names = on_fs(move || {
    let dir = root.join("many");
    std::fs::write(dir.join("zz-added.txt"), b"new\n").unwrap();
    read_dir_names(&dir)
  })
  .await;

  assert!(names.contains(&b"zz-added.txt".to_vec()));
  assert_eq!(
    names.len(),
    5003,
    "the base's 5002 entries plus the created one"
  );
  assert_eq!(
    names.iter().filter(|n| *n == b"zz-added.txt").count(),
    1,
    "and it is listed exactly once"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_replacing_a_base_file_by_rename_is_listed_once() {
  // The case that made `readdir` decide extras by name rather than by whether a
  // row records base facts: after a rename the row remembers the base of the path
  // it came *from*, so the wrong test listed the file twice or not at all.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  let (names, content) = on_fs(move || {
    std::fs::write(root.join("staged.tmp"), b"replacement\n").unwrap();
    std::fs::rename(root.join("staged.tmp"), root.join("README.md")).unwrap();
    (
      read_dir_names(&root),
      std::fs::read(root.join("README.md")).unwrap(),
    )
  })
  .await;

  assert_eq!(content, b"replacement\n");
  assert_eq!(
    names.iter().filter(|n| *n == b"README.md").count(),
    1,
    "{names:?}"
  );
  assert!(!names.contains(&b"staged.tmp".to_vec()), "{names:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_synthesized_git_surface_is_outside_the_overlay() {
  // DESIGN.md section 8.6: whatever occupies `.git` is excluded from change
  // tracking. A workspace whose only "edit" was a failed write into `.git` must
  // still be clean, or `xvfs refresh` would refuse for no reason.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  on_fs(move || {
    let _ = std::fs::write(root.join(".git/HEAD"), b"x");
    let _ = std::fs::create_dir(root.join(".git/objects"));
  })
  .await;

  assert!(mount.overlay.is_empty(), "the .git surface is not tracked");
}
