//! The projected object database, driven by real Git (ADR 0009), through the
//! single workspace mount (ADR 0011).
//!
//! These are the M9.2 exit questions, asked at the new surface: can stock
//! `git`, resolving `objects/info/alternates` = `../gfs/objects` inside the
//! workspace's own `.git`, answer history questions — and does it cost blocks
//! proportional to the question rather than the repository.

use gfs_mount::odb::{OdbClient, OdbProjection};
use gfs_test::mount::{on_fs, Backend, Job, TOKEN};

fn git_in(workspace: &std::path::Path, args: &[&str]) -> (bool, String) {
  let out = std::process::Command::new("git")
    .env_clear()
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env("PATH", "/usr/bin:/bin")
    .env("GIT_AUTHOR_NAME", "a")
    .env("GIT_AUTHOR_EMAIL", "a@example.com")
    .env("GIT_COMMITTER_NAME", "a")
    .env("GIT_COMMITTER_EMAIL", "a@example.com")
    .current_dir(workspace)
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

fn head_of(backend: &Backend) -> String {
  let out = std::process::Command::new("git")
    .env_clear()
    .env("PATH", "/usr/bin:/bin")
    .arg("-C")
    .arg(&backend.repo_path)
    .args(["rev-parse", "HEAD"])
    .output()
    .unwrap();
  String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stock_git_walks_history_through_the_projection() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let head = head_of(&backend);
  let ws = job.workspace.clone();

  // History, entirely through borrowed objects reached at `.git/gfs/objects`:
  // the log walk, object type inspection, and a commit's own diff.
  let expect = head.clone();
  let ((ok_log, log), (ok_kind, kind), (ok_show, show)) = on_fs(move || {
    (
      git_in(&ws, &["log", "--oneline"]),
      git_in(&ws, &["cat-file", "-t", &expect]),
      git_in(&ws, &["show", "--stat", "--oneline", "HEAD"]),
    )
  })
  .await;
  assert!(ok_log, "{log}");
  assert!(!log.trim().is_empty());
  assert!(ok_kind, "{kind}");
  assert_eq!(kind.trim(), "commit");
  assert!(ok_show, "{show}");

  let report = job.daemon.inspect();
  assert!(
    report.odb.blocks_fetched > 0,
    "history must have read something"
  );
  // The projection served blocks, not files: a whole-store fetch of even the
  // tiny fixture would be far more than this bound.
  assert!(
    report.odb.bytes_fetched < 1 << 20,
    "a bounded question fetched {} bytes",
    report.odb.bytes_fetched
  );
  // And the traffic is attributed to this workspace's view (ADR 0009's
  // per-job attribution, now counted inside the one mount).
  assert!(
    report.odb_job.bytes_fetched > 0,
    "the job's own view must have counted the reads: {:?}",
    report.odb_job
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_read_is_served_from_cached_blocks() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  let ws = job.workspace.clone();
  let (ok, out) = on_fs(move || git_in(&ws, &["log", "--oneline"])).await;
  assert!(ok, "{out}");
  let first = job.daemon.inspect().odb;

  let ws = job.workspace.clone();
  let (ok, out) = on_fs(move || git_in(&ws, &["log", "--oneline"])).await;
  assert!(ok, "{out}");
  let second = job.daemon.inspect().odb;

  assert_eq!(
    second.blocks_fetched, first.blocks_fetched,
    "the same question must not fetch again"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_residency_limit_evicts_and_refetches_instead_of_refusing() {
  // The store level, where the accounting is observable below the kernel's
  // page cache. One byte: every block admitted is over the limit, so every
  // unpinned block is evicted as soon as its read releases it — the most
  // adversarial setting, and the only one guaranteed to trip on a fixture of
  // any size.
  let backend = Backend::start("basic").await;
  let root = tempfile::tempdir().unwrap();
  let client = OdbClient::new(&backend.http, TOKEN, backend.repo_id.clone());
  let projection = OdbProjection::open(client, root.path(), 1)
    .await
    .expect("open the odb projection");

  let listing = projection.store.listing();
  let (a, _) = listing.first().expect("the fixture has objects").clone();
  let (b, _) = listing.last().expect("the fixture has objects").clone();
  assert_ne!(a, b, "the fixture has at least two files");

  let (first_read, _) = projection.store.read(&a, 0, 64).await.expect("read a");
  projection
    .store
    .read(&b, 0, 64)
    .await
    .expect("read b evicts a");
  let (second_read, _) = projection
    .store
    .read(&a, 0, 64)
    .await
    .expect("read a again");
  assert_eq!(
    first_read, second_read,
    "eviction must not change any answer"
  );

  let stats = projection.store.stats();
  assert!(
    stats.evicted_blocks > 0,
    "reads over the limit must evict; stats {stats:?}"
  );
  assert!(
    stats.refetched_blocks > 0,
    "evicted blocks read again must count as refetches; stats {stats:?}"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_commits_write_locally_and_read_through_the_alternate() {
  // The write path ADR 0009 claims, on ADR 0011's surface: an agent's commit
  // lands in the workspace's own `.git/objects` as loose objects — through
  // the passthrough — while the parent commit and tree resolve through the
  // projection at `.git/gfs/objects`.
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let head = head_of(&backend);
  let ws = job.workspace.clone();

  let parent = head.clone();
  let (new_commit, log) = on_fs(move || {
    // A commit whose tree reuses the parent's, so building it reads borrowed
    // trees. `commit-tree` avoids touching the worktree here.
    let (ok, tree) = git_in(&ws, &["rev-parse", "HEAD^{tree}"]);
    assert!(ok, "{tree}");
    let (ok, out) = git_in(
      &ws,
      &[
        "commit-tree",
        tree.trim(),
        "-p",
        &parent,
        "-m",
        "local work",
      ],
    );
    assert!(ok, "{out}");
    let new_commit = out.trim().to_owned();
    let (ok, log) = git_in(&ws, &["log", "--oneline", &new_commit]);
    assert!(ok, "{log}");
    (new_commit, log)
  })
  .await;

  // The new object is local; the projection saw no write. Both checked
  // through the mount, which is the only namespace a tool has.
  // A commit hex is ASCII, so byte splitting is exact.
  let (fan, rest) = new_commit.split_at(2);
  let loose = job.workspace.join(".git/objects").join(fan).join(rest);
  let projected = job.workspace.join(".git/gfs/objects").join(fan).join(rest);
  let (loose_exists, projected_exists) = on_fs(move || (loose.exists(), projected.exists())).await;
  assert!(loose_exists, "the commit must be a local loose object");
  assert!(
    !projected_exists,
    "nothing may appear in the read-only projection"
  );

  // And history through the new commit crosses from local into borrowed.
  assert!(log.contains("local work"), "{log}");
}
