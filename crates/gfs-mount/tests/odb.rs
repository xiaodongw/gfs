//! The projected object database, driven by real Git (ADR 0009).
//!
//! These are the M9.2 exit questions: can stock `git`, pointed at the
//! projection through `objects/info/alternates`, answer history questions —
//! and does it cost blocks proportional to the question rather than the
//! repository.

use gfs_mount::odb::{OdbClient, OdbProjection};
use gfs_test::mount::{Backend, TOKEN};

async fn projected(backend: &Backend, root: &tempfile::TempDir) -> std::sync::Arc<OdbProjection> {
  projected_with_limit(backend, root, 0).await
}

async fn projected_with_limit(
  backend: &Backend,
  root: &tempfile::TempDir,
  residency_limit: u64,
) -> std::sync::Arc<OdbProjection> {
  let client = OdbClient::new(&backend.http, TOKEN, backend.repo_id.clone());
  OdbProjection::mount(client, root.path(), residency_limit)
    .await
    .expect("mount the odb projection")
}

/// A local `.git` whose object database is the projection, the way a workspace's
/// is: plain local files, alternates into the mount, nothing copied.
fn agent_git(dir: &std::path::Path, odb_mount: &std::path::Path, head: &str) -> std::path::PathBuf {
  let gitdir = dir.join("agent-git");
  std::fs::create_dir_all(gitdir.join("objects/info")).unwrap();
  std::fs::create_dir_all(gitdir.join("refs/heads")).unwrap();
  std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
  std::fs::write(gitdir.join("refs/heads/main"), format!("{head}\n")).unwrap();
  std::fs::write(
    gitdir.join("config"),
    "[core]\n\trepositoryformatversion = 0\n\tbare = true\n",
  )
  .unwrap();
  std::fs::write(
    gitdir.join("objects/info/alternates"),
    format!("{}\n", odb_mount.display()),
  )
  .unwrap();
  gitdir
}

fn git(gitdir: &std::path::Path, args: &[&str]) -> (bool, String) {
  let out = std::process::Command::new("git")
    .env_clear()
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env("PATH", "/usr/bin:/bin")
    .env("GIT_DIR", gitdir)
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
  let root = tempfile::tempdir().unwrap();
  let projection = projected(&backend, &root).await;
  let head = head_of(&backend);
  let gitdir = agent_git(root.path(), &projection.mountpoint, &head);

  // History, entirely through borrowed objects: the log walk, object type
  // inspection, and a commit's own diff.
  let (ok, log) = git(&gitdir, &["log", "--oneline"]);
  assert!(ok, "{log}");
  assert!(!log.trim().is_empty());

  let (ok, kind) = git(&gitdir, &["cat-file", "-t", &head]);
  assert!(ok, "{kind}");
  assert_eq!(kind.trim(), "commit");

  let (ok, show) = git(&gitdir, &["show", "--stat", "--oneline", &head]);
  assert!(ok, "{show}");

  let stats = projection.store.stats();
  assert!(stats.blocks_fetched > 0, "history must have read something");
  // The projection served blocks, not files: a whole-store fetch of even the
  // tiny fixture would be far more than this bound.
  assert!(
    stats.bytes_fetched < 1 << 20,
    "a bounded question fetched {} bytes",
    stats.bytes_fetched
  );

  projection.unmount();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_read_is_served_from_cached_blocks() {
  let backend = Backend::start("basic").await;
  let root = tempfile::tempdir().unwrap();
  let projection = projected(&backend, &root).await;
  let head = head_of(&backend);
  let gitdir = agent_git(root.path(), &projection.mountpoint, &head);

  let (ok, out) = git(&gitdir, &["log", "--oneline"]);
  assert!(ok, "{out}");
  let first = projection.store.stats();

  let (ok, out) = git(&gitdir, &["log", "--oneline"]);
  assert!(ok, "{out}");
  let second = projection.store.stats();

  assert_eq!(
    second.blocks_fetched, first.blocks_fetched,
    "the same question must not fetch again"
  );
  projection.unmount();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_residency_limit_evicts_and_refetches_instead_of_refusing() {
  let backend = Backend::start("basic").await;
  let root = tempfile::tempdir().unwrap();
  // One byte: every block admitted is over the limit, so every unpinned block
  // is evicted as soon as its read releases it. The most adversarial setting,
  // and the only one guaranteed to trip on a fixture of any size.
  let projection = projected_with_limit(&backend, &root, 1).await;
  let head = head_of(&backend);
  let gitdir = agent_git(root.path(), &projection.mountpoint, &head);

  let (ok, first_log) = git(&gitdir, &["log", "--oneline"]);
  assert!(ok, "{first_log}");
  let first = projection.store.stats();
  assert!(
    first.evicted_blocks > 0,
    "a walk over the limit must evict; stats {first:?}"
  );

  // The same question again: answered correctly, never refused. (The kernel's
  // page cache may serve it without reaching the store at all — KEEP_CACHE
  // holds true bytes, so an eviction underneath it is invisible, which is
  // exactly why eviction is safe to run while a mount is live.)
  let (ok, second_log) = git(&gitdir, &["log", "--oneline"]);
  assert!(ok, "{second_log}");
  assert_eq!(first_log, second_log, "eviction must not change any answer");

  // Refetch accounting, asserted below the page cache: reading two files
  // through the store alternately, with a limit of one byte, means the second
  // read of the first file finds its blocks evicted and fetches them again.
  let listing = projection.store.listing();
  let (a, _) = listing.first().expect("the fixture has objects");
  let (b, _) = listing.last().expect("the fixture has objects");
  assert_ne!(a, b, "the fixture has at least two files");
  projection.store.read(a, 0, 64).await.expect("read a");
  projection
    .store
    .read(b, 0, 64)
    .await
    .expect("read b evicts a");
  projection.store.read(a, 0, 64).await.expect("read a again");
  let second = projection.store.stats();
  assert!(
    second.refetched_blocks > 0,
    "evicted blocks read again must count as refetches; stats {second:?}"
  );
  projection.unmount();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_commits_write_locally_and_read_through_the_alternate() {
  // The write path ADR 0009 claims: an agent's commit lands in its own git dir
  // as loose objects, while the parent commit and tree resolve through the
  // read-only projection. This is what replaces the export/apply pipeline.
  let backend = Backend::start("basic").await;
  let root = tempfile::tempdir().unwrap();
  let projection = projected(&backend, &root).await;
  let head = head_of(&backend);
  let gitdir = agent_git(root.path(), &projection.mountpoint, &head);

  // A commit whose tree reuses the parent's, so building it reads borrowed
  // trees. `commit-tree` avoids needing a worktree here.
  let (ok, tree) = git(&gitdir, &["rev-parse", "HEAD^{tree}"]);
  assert!(ok, "{tree}");
  let out = std::process::Command::new("git")
    .env_clear()
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env("PATH", "/usr/bin:/bin")
    .env("GIT_DIR", &gitdir)
    .env("GIT_AUTHOR_NAME", "a")
    .env("GIT_AUTHOR_EMAIL", "a@example.com")
    .env("GIT_COMMITTER_NAME", "a")
    .env("GIT_COMMITTER_EMAIL", "a@example.com")
    .args(["commit-tree", tree.trim(), "-p", &head, "-m", "local work"])
    .output()
    .unwrap();
  assert!(
    out.status.success(),
    "{}",
    String::from_utf8_lossy(&out.stderr)
  );
  let new_commit = String::from_utf8_lossy(&out.stdout).trim().to_owned();

  // The new object is local; the projection saw no write.
  // A commit hex is ASCII, so byte splitting is exact.
  let (fan, rest) = new_commit.split_at(2);
  assert!(
    gitdir.join("objects").join(fan).join(rest).exists(),
    "the commit must be a local loose object"
  );
  assert!(
    !projection.mountpoint.join(fan).join(rest).exists(),
    "nothing may appear in the read-only projection"
  );

  // And history through the new commit crosses from local into borrowed.
  let (ok, log) = git(&gitdir, &["log", "--oneline", &new_commit]);
  assert!(ok, "{log}");
  assert!(log.contains("local work"), "{log}");
  projection.unmount();
}
