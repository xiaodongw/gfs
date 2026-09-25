//! Local mode (ADR 0013): a workspace over a clone on this machine, no server.
//!
//! Smoke tests for the major entry points, against a real FUSE mount and a
//! real `git clone` of a fixture. The host these run through is pointed at a
//! closed port, so anything that reached for a server would fail loudly.

use gfs_mount::control::{Request, Response};
use gfs_mount::search::SearchRequest;
use gfs_search::SearchOutcome;
use gfs_test::mount::{on_fs, Job};
use gfs_test::{diff_trees, materialize_raw, snapshot_tree};

fn git_in(dir: &std::path::Path, args: &[&str]) -> (bool, String) {
  let out = std::process::Command::new("git")
    .env_clear()
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env("PATH", "/usr/bin:/bin")
    .env("GIT_AUTHOR_NAME", "agent")
    .env("GIT_AUTHOR_EMAIL", "agent@example.com")
    .env("GIT_COMMITTER_NAME", "agent")
    .env("GIT_COMMITTER_EMAIL", "agent@example.com")
    .current_dir(dir)
    .args(args)
    .output()
    .unwrap();
  (
    out.status.success(),
    format!(
      "{}{}",
      String::from_utf8_lossy(&out.stdout),
      String::from_utf8_lossy(&out.stderr)
    ),
  )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_mount_presents_the_clone_commit_without_a_server() {
  // The clone outlives the job, so the post-unmount check can read it.
  let clone_dir = tempfile::tempdir().unwrap();
  let clone = clone_dir.path().join("clone");
  Job::clone_fixture("basic", &clone);
  let job = Job::local_from(&clone, "main", tempfile::tempdir().unwrap()).await;
  let ws = job.workspace.clone();

  // The tree is the clone's HEAD, byte for byte, by the real-Git oracle.
  let oracle = tempfile::tempdir().unwrap();
  materialize_raw(&clone, "HEAD", oracle.path()).unwrap();
  let expected = snapshot_tree(oracle.path()).unwrap();
  let actual = on_fs({
    let ws = ws.clone();
    move || snapshot_tree(&ws).unwrap()
  })
  .await;
  let differences = diff_trees(&expected, &actual);
  assert!(differences.is_empty(), "{differences:?}");

  // The report says where the bytes come from, and the lease is not a thing.
  let report = job.daemon.inspect();
  assert_eq!(
    report.local_clone.as_deref(),
    Some(clone.to_str().unwrap()),
    "{report:?}"
  );
  assert_eq!(report.health.state, gfs_mount::HealthState::Healthy);
  assert!(report.repository_id.starts_with("local-"), "{report:?}");

  // The object store is borrowed, not projected: the alternates file names the
  // clone's objects directory and there is no `.git/gfs/objects`.
  let (alternates, projection) = on_fs({
    let ws = ws.clone();
    move || {
      (
        std::fs::read_to_string(ws.join(".git/objects/info/alternates")).unwrap(),
        ws.join(".git/gfs/objects").exists(),
      )
    }
  })
  .await;
  assert_eq!(
    alternates.trim(),
    clone.join(".git/objects").to_str().unwrap(),
    "alternates borrows the clone"
  );
  assert!(!projection, "local mode presents no projection");

  // Stock Git over the workspace: clean, with history, with the clone as
  // `origin`, and the pinned commit anchored in the clone.
  let head = git_in(&clone, &["rev-parse", "HEAD"]).1.trim().to_owned();
  let (status, log, origin, anchors) = on_fs({
    let ws = ws.clone();
    let clone = clone.clone();
    move || {
      (
        git_in(&ws, &["status", "--porcelain"]),
        git_in(&ws, &["log", "-1", "--format=%H"]),
        git_in(&ws, &["remote", "get-url", "origin"]),
        git_in(&clone, &["for-each-ref", "refs/gfs/mounts/"]),
      )
    }
  })
  .await;
  assert!(status.0 && status.1.trim().is_empty(), "{}", status.1);
  assert_eq!(log.1.trim(), head, "{}", log.1);
  assert_eq!(origin.1.trim(), clone.to_str().unwrap(), "{}", origin.1);
  assert!(
    anchors.1.contains(&head) && anchors.1.contains(&report.mount_id),
    "the pin is anchored in the clone: {}",
    anchors.1
  );

  // Search scans the pack: a line from the fixture, found with no index.
  let search = job
    .daemon
    .search(&SearchRequest {
      pattern: "println".to_owned(),
      literal: true,
      case_insensitive: false,
      scope: Vec::new(),
      include_globs: Vec::new(),
      exclude_globs: Vec::new(),
      context_before: 0,
      context_after: 0,
      max_results: 0,
      max_line_bytes: 0,
      search_ignored: false,
    })
    .await
    .unwrap();
  let SearchOutcome::Completed(result) = search.outcome else {
    panic!("the search did not complete: {:?}", search.outcome);
  };
  let paths: Vec<Vec<u8>> = result.matches.iter().map(|m| m.path.clone()).collect();
  assert_eq!(paths, vec![b"src/main.rs".to_vec()], "{result:?}");
  assert_eq!(
    result.completion.execution_status,
    gfs_search::ExecutionStatus::Complete
  );

  // Edit, commit with stock Git, push back into the clone over the filesystem.
  let (edit, add, commit, push, landed) = on_fs({
    let ws = ws.clone();
    let clone = clone.clone();
    move || {
      std::fs::write(
        ws.join("src/main.rs"),
        b"fn main() { println!(\"local\"); }\n",
      )
      .unwrap();
      std::fs::write(ws.join("NOTES.md"), b"from a local-mode workspace\n").unwrap();
      let edit = git_in(&ws, &["status", "--porcelain"]);
      let add = git_in(&ws, &["add", "-A"]);
      let commit = git_in(&ws, &["commit", "-q", "-m", "local edit"]);
      let push = git_in(
        &ws,
        &["push", "-q", "origin", "HEAD:refs/heads/from-workspace"],
      );
      let landed = git_in(&clone, &["show", "from-workspace:NOTES.md"]);
      (edit, add, commit, push, landed)
    }
  })
  .await;
  assert!(
    edit.1.contains(" M src/main.rs") && edit.1.contains("?? NOTES.md"),
    "{}",
    edit.1
  );
  assert!(add.0, "{}", add.1);
  assert!(commit.0, "{}", commit.1);
  assert!(push.0, "{}", push.1);
  assert_eq!(
    landed.1.trim(),
    "from a local-mode workspace",
    "{}",
    landed.1
  );

  // Unmounting releases the anchor: nothing of the workspace stays in the clone
  // but the branch it pushed.
  drop(job);
  let anchors = git_in(&clone, &["for-each-ref", "refs/gfs/"]).1;
  assert!(anchors.trim().is_empty(), "anchors linger: {anchors}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_mount_reads_history_and_blame_from_the_clone() {
  let job = Job::local("basic", "main").await;
  let ws = job.workspace.clone();

  // Stock `git log` and `git blame` answer from the borrowed object store, and
  // the daemon's own history surface answers from the same libgit2 handle.
  let (log, blame) = on_fs(move || {
    (
      git_in(&ws, &["log", "--oneline"]),
      git_in(&ws, &["blame", "--porcelain", "src/main.rs"]),
    )
  })
  .await;
  assert!(log.0 && log.1.lines().count() == 2, "{}", log.1);
  assert!(blame.0 && blame.1.contains("second"), "{}", blame.1);

  let report = job.daemon.inspect();
  assert_eq!(
    report.cache.fetches, 0,
    "nothing is copied into the blob cache in local mode: {:?}",
    report.cache
  );
  assert_eq!(
    report.budget.limit_bytes, 0,
    "no hydration budget in local mode"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writes_are_visible_by_size_before_close_and_to_git_after() {
  // The write path has two shapes -- every write through the daemon, or the
  // kernel writing a backing file directly when passthrough is on -- and the
  // contract is the same for both: `stat` on an open, half-written file tells
  // the truth, the bytes read back, and Git sees the modification.
  let clone_dir = tempfile::tempdir().unwrap();
  let clone = clone_dir.path().join("clone");
  Job::clone_fixture("basic", &clone);
  let job = Job::local_from(&clone, "main", tempfile::tempdir().unwrap()).await;
  let ws = job.workspace.clone();

  let (base_len, mid_size, after_size, appended, chunked, status) = on_fs({
    let ws = ws.clone();
    move || {
      use std::io::{Read, Write};
      let readme = ws.join("README.md");
      let base_len = std::fs::metadata(&readme).unwrap().len();
      // Append to a base file: copy-up, then bytes the row does not know yet.
      let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&readme)
        .unwrap();
      file.write_all(b"more\n").unwrap();
      let mid_size = std::fs::metadata(&readme).unwrap().len();
      drop(file);
      let after_size = std::fs::metadata(&readme).unwrap().len();
      let mut appended = String::new();
      std::fs::File::open(&readme)
        .unwrap()
        .read_to_string(&mut appended)
        .unwrap();
      // A new file written in small pieces, read back whole.
      let fresh = ws.join("chunks.bin");
      let mut file = std::fs::File::create(&fresh).unwrap();
      let chunk = vec![7u8; 4096];
      for _ in 0..64 {
        file.write_all(&chunk).unwrap();
      }
      drop(file);
      let chunked = std::fs::read(&fresh).unwrap();
      let status = git_in(&ws, &["status", "--porcelain"]).1;
      (base_len, mid_size, after_size, appended, chunked, status)
    }
  })
  .await;

  assert_eq!(
    mid_size,
    base_len + 5,
    "size is live while the file is open"
  );
  assert_eq!(after_size, base_len + 5);
  assert!(appended.ends_with("more\n"), "{appended:?}");
  assert_eq!(chunked.len(), 64 * 4096);
  assert!(chunked.iter().all(|b| *b == 7));
  assert!(status.contains(" M README.md"), "{status}");
  assert!(status.contains("?? chunks.bin"), "{status}");

  let stats = job.daemon.inspect().stats;
  if job.daemon.passthrough_active() {
    assert!(stats.passthrough_opens > 0, "{stats:?}");
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prewarmed_local_mount_inflates_the_tree_in_the_background() {
  let clone_dir = tempfile::tempdir().unwrap();
  let clone = clone_dir.path().join("clone");
  Job::clone_fixture("basic", &clone);
  let job = Job::local_from_with(&clone, "main", tempfile::tempdir().unwrap(), true).await;

  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
  let report = loop {
    let report = job
      .daemon
      .inspect()
      .prewarm
      .expect("a prewarm was asked for");
    if report.done || std::time::Instant::now() > deadline {
      break report;
    }
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
  };
  assert!(report.done, "{report:?}");
  assert!(report.blobs > 0 && report.bytes > 0, "{report:?}");

  // The mount still reads correctly after the walk.
  let readme = job.workspace.join("README.md");
  let content = on_fs(move || std::fs::read(&readme).unwrap()).await;
  assert_eq!(content, b"# basic\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_checkout_back_to_the_pinned_commit_leaves_no_copies_behind() {
  // A stock `git checkout` writes every differing file through the mount.
  // Going back to the pinned commit writes the base's own bytes again, and
  // once each file's last writer closes the copy gives way to a reference to
  // the base blob: no quota held, same stat, same bytes.
  let clone_dir = tempfile::tempdir().unwrap();
  let clone = clone_dir.path().join("clone");
  Job::clone_fixture("basic", &clone);
  let job = Job::local_from(&clone, "main", tempfile::tempdir().unwrap()).await;
  let ws = job.workspace.clone();

  let settle = |job: &Job| {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
      let report = job.daemon.inspect();
      if report.overlay.local_bytes == 0 || std::time::Instant::now() > deadline {
        return report;
      }
      std::thread::sleep(std::time::Duration::from_millis(20));
    }
  };

  // The same bytes over a base file.
  let before = on_fs({
    let ws = ws.clone();
    move || {
      use std::os::unix::fs::MetadataExt;
      std::fs::write(ws.join("README.md"), b"# basic\n").unwrap();
      let m = std::fs::metadata(ws.join("README.md")).unwrap();
      (m.ino(), m.len(), m.mtime(), m.mtime_nsec())
    }
  })
  .await;
  let report = tokio::task::block_in_place(|| settle(&job));
  assert_eq!(report.overlay.local_bytes, 0, "{:?}", report.overlay);
  assert!(report.stats.base_references >= 1, "{:?}", report.stats);
  let (after, bytes) = on_fs({
    let ws = ws.clone();
    move || {
      use std::os::unix::fs::MetadataExt;
      let m = std::fs::metadata(ws.join("README.md")).unwrap();
      (
        (m.ino(), m.len(), m.mtime(), m.mtime_nsec()),
        std::fs::read(ws.join("README.md")).unwrap(),
      )
    }
  })
  .await;
  assert_eq!(after, before, "the reference keeps the stat Git recorded");
  assert_eq!(bytes, b"# basic\n");

  // A write after that copies up again, from the blob.
  let appended = on_fs({
    let ws = ws.clone();
    move || {
      use std::io::Write;
      let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(ws.join("README.md"))
        .unwrap();
      f.write_all(b"more\n").unwrap();
      drop(f);
      std::fs::read(ws.join("README.md")).unwrap()
    }
  })
  .await;
  assert_eq!(appended, b"# basic\nmore\n");
  assert!(job.daemon.inspect().overlay.local_bytes > 0);

  // A checkout away and back.
  let plain = |ws: &std::path::Path| {
    git_in(
      ws,
      &[
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.untrackedCache=false",
        "status",
        "--porcelain",
      ],
    )
    .1
  };
  let status = on_fs({
    let ws = ws.clone();
    move || {
      assert!(git_in(&ws, &["checkout", "-q", "--", "README.md"]).0);
      let (ok, out) = git_in(&ws, &["checkout", "-q", "v1.0"]);
      assert!(ok, "{out}");
      assert_eq!(
        std::fs::read(ws.join("src/main.rs")).unwrap(),
        b"fn main() { println!(\"hi\"); }\n"
      );
      let (ok, out) = git_in(&ws, &["checkout", "-q", "main"]);
      assert!(ok, "{out}");
      git_in(&ws, &["status", "--porcelain"]).1
    }
  })
  .await;
  assert_eq!(status, "", "back on the pinned commit, nothing to report");
  let report = tokio::task::block_in_place(|| settle(&job));
  assert_eq!(report.overlay.local_bytes, 0, "{:?}", report.overlay);
  let (status, oracle, main_rs) = on_fs({
    let ws = ws.clone();
    move || {
      (
        git_in(&ws, &["status", "--porcelain"]).1,
        plain(&ws),
        std::fs::read(ws.join("src/main.rs")).unwrap(),
      )
    }
  })
  .await;
  assert_eq!(status, oracle);
  assert_eq!(main_rs, b"fn main() { println!(\"bye\"); }\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gfs_switch_resolves_branches_in_local_mode() {
  // In local mode, `gfs switch` should resolve branches against the workspace
  // and the clone without gateway contact.
  let clone_dir = tempfile::tempdir().unwrap();
  let clone = clone_dir.path().join("clone");
  Job::clone_fixture("basic", &clone);

  // Create a local branch in the clone to test switching to it
  git_in(&clone, &["branch", "old", "v1.0"]);

  let job = Job::local_from(&clone, "main", tempfile::tempdir().unwrap()).await;
  let ws = job.workspace.clone();

  // Initial state: on main
  let initial_main_rs = on_fs({
    let ws = ws.clone();
    move || std::fs::read(ws.join("src/main.rs")).unwrap()
  })
  .await;
  assert_eq!(initial_main_rs, b"fn main() { println!(\"bye\"); }\n");

  // Switch to the clone-only branch "old" (which doesn't exist in workspace yet)
  use gfs_mount::control::Request;
  let Response::Refresh(refresh) = job
    .call(Request::Switch {
      selector: "old".to_owned(),
      branch: None,
      create: None,
      start_point: None,
    })
    .await
  else {
    panic!("expected a refresh");
  };

  // After switching, verify the workspace shows the v1.0 content
  let after_switch_main_rs = on_fs({
    let ws = ws.clone();
    move || std::fs::read(ws.join("src/main.rs")).unwrap()
  })
  .await;
  assert_eq!(after_switch_main_rs, b"fn main() { println!(\"hi\"); }\n");
  assert!(!refresh.unchanged, "switching to a different commit should change the pin");

  // Verify the overlay is clean (no writes through FUSE)
  assert_eq!(job.daemon.inspect().overlay.entries, 0, "clean switch leaves no overlay rows");
}
