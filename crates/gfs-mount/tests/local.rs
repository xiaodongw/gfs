//! Local mode (ADR 0013): a workspace over a clone on this machine, no server.
//!
//! Smoke tests for the major entry points, against a real FUSE mount and a
//! real `git clone` of a fixture. The host these run through is pointed at a
//! closed port, so anything that reached for a server would fail loudly.

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
