//! Stock Git inside a mounted workspace (ADR 0009).
//!
//! The M9.4 exit questions, asked through real syscalls and a real `git`
//! binary: does the seeded `.git` make the workspace a repository stock Git
//! accepts, does `status` read clean without sweeping, does an overlay edit
//! show up as a modification, and do local commits stay local.

use std::sync::Arc;

use gfs_mount::MountHost;
use gfs_test::mount::{host_config, mount_spec, on_fs, Backend, Job};

fn git_in(workspace: &std::path::Path, args: &[&str]) -> (bool, String) {
  let out = std::process::Command::new("git")
    .env_clear()
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env("PATH", "/usr/bin:/bin")
    .env("GIT_AUTHOR_NAME", "agent")
    .env("GIT_AUTHOR_EMAIL", "agent@example.com")
    .env("GIT_COMMITTER_NAME", "agent")
    .env("GIT_COMMITTER_EMAIL", "agent@example.com")
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fresh_workspace_reads_clean_through_stock_git() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let ws = job.workspace.clone();

  let ((ok_root, root), (ok_status, status), (ok_log, log), (ok_files, files)) = on_fs(move || {
    (
      git_in(&ws, &["rev-parse", "--show-toplevel"]),
      git_in(&ws, &["status", "--porcelain"]),
      git_in(&ws, &["log", "--oneline", "-5"]),
      git_in(&ws, &["ls-files"]),
    )
  })
  .await;

  assert!(ok_root, "{root}");
  assert!(ok_status, "{status}");
  assert_eq!(
    status.trim(),
    "",
    "a pinned workspace has nothing to report"
  );
  assert!(ok_log, "{log}");
  assert!(
    !log.trim().is_empty(),
    "history answers through the projection"
  );
  // The question ADR 0005's shim answered wrongly and `gfs find` existed for:
  // a real index answers it exactly.
  assert!(ok_files, "{files}");
  assert!(files.contains("README.md"), "{files}");
  assert!(files.contains("src/main.rs"), "{files}");

  // The log walk above read pack data through this workspace's own odb view,
  // so the traffic is attributed to this job — the gap ADR 0009's amendment
  // recorded ("counted per repository, not per job") is closed.
  let report = job.daemon.inspect();
  assert!(
    report.odb_job.bytes_fetched > 0,
    "history reads must be attributed to the job: {:?}",
    report.odb_job
  );
  assert!(
    report.odb.bytes_fetched >= report.odb_job.bytes_fetched,
    "the shared store counts at least what this job counted"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tags_and_remote_tracking_refs_are_materialized_at_mount() {
  // Seeding only the pinned branch left `git describe` answering "No names
  // found", `origin/main` an unknown revision, and `git status -sb` with no
  // upstream — three messages that read as a corrupt repository. The whole
  // filtered ref set is now written as `packed-refs` at pin time.
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let ws = job.workspace.clone();

  let (tags, remotes, describe, upstream, branch_status, reserved) = on_fs(move || {
    (
      git_in(&ws, &["tag", "--list"]),
      git_in(&ws, &["branch", "-r"]),
      // Peeled in `packed-refs`, so this answers without reading a tag object
      // out of the projection.
      git_in(&ws, &["describe", "--tags"]),
      git_in(&ws, &["rev-parse", "origin/main"]),
      git_in(&ws, &["status", "-sb"]),
      git_in(&ws, &["show-ref"]),
    )
  })
  .await;

  assert!(tags.0, "{}", tags.1);
  assert!(
    tags.1.contains("v1.0") && tags.1.contains("v2.0"),
    "lightweight and annotated tags both materialize: {}",
    tags.1
  );
  assert!(
    tags.1.contains("tree-tag"),
    "a tag that peels to a tree is still a tag: {}",
    tags.1
  );
  assert!(remotes.0, "{}", remotes.1);
  assert!(
    remotes.1.contains("origin/main"),
    "branches arrive as remote-tracking refs: {}",
    remotes.1
  );
  assert!(describe.0, "{}", describe.1);
  assert!(upstream.0, "`origin/main` must resolve: {}", upstream.1);
  assert!(
    branch_status.1.contains("...origin/main"),
    "the pinned branch has an upstream to count against: {}",
    branch_status.1
  );
  // ADR 0002's boundary: the reserved namespace is filtered out of the listing,
  // exactly as it is out of the gateway's advertisement.
  assert!(
    !reserved.1.contains("refs/gfs/"),
    "the reserved namespace must never be materialized: {}",
    reserved.1
  );
  // The agent's own branch namespace stays the agent's: an upstream branch
  // packed into `refs/heads/` would collide with a branch it creates.
  let packed = std::fs::read_to_string(job.workspace.join(".git/packed-refs")).unwrap();
  assert!(
    !packed.contains(" refs/heads/"),
    "upstream branches must not be packed into refs/heads:\n{packed}"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_commit_pushes_to_the_real_branch_through_the_seeded_remote() {
  // The push half of ADR 0009: `git push origin <branch>` out of the box. The
  // seeded config maps `refs/heads/*` onto the gateway's real branches — the
  // gateway is a fork of its upstream — and the credential helper reads
  // GFS_TOKEN from the job's environment, so stock Git needs to be told
  // nothing.
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let ws = job.workspace.clone();

  let (pushed_ok, push_out, local_head) = on_fs(move || {
    std::fs::write(ws.join("pushed.txt"), b"leaving as a pack\n").unwrap();
    let (ok, out) = git_in(&ws, &["add", "pushed.txt"]);
    assert!(ok, "{out}");
    let (ok, out) = git_in(&ws, &["commit", "-q", "-m", "local work"]);
    assert!(ok, "{out}");
    let (_, head) = git_in(&ws, &["rev-parse", "HEAD"]);

    let out = std::process::Command::new("git")
      .env_clear()
      .env("GIT_CONFIG_GLOBAL", "/dev/null")
      .env("GIT_CONFIG_SYSTEM", "/dev/null")
      .env("PATH", "/usr/bin:/bin")
      .env("GFS_TOKEN", gfs_test::mount::TOKEN)
      .current_dir(&ws)
      .args(["push", "-q", "origin", "main"])
      .output()
      .unwrap();
    (
      out.status.success(),
      String::from_utf8_lossy(&out.stderr).into_owned(),
      head.trim().to_owned(),
    )
  })
  .await;
  assert!(pushed_ok, "push failed: {push_out}");

  // The commit is on the server, on the real branch, at exactly the local
  // HEAD: the gateway is a fork of its upstream, and a push lands where any
  // other Git host would put it.
  let out = std::process::Command::new("git")
    .env_clear()
    .env("PATH", "/usr/bin:/bin")
    .arg("-C")
    .arg(&backend.repo_path)
    .args(["rev-parse", "refs/heads/main"])
    .output()
    .unwrap();
  assert!(
    out.status.success(),
    "{}",
    String::from_utf8_lossy(&out.stderr)
  );
  assert_eq!(
    String::from_utf8_lossy(&out.stdout).trim(),
    local_head,
    "the pushed ref must be the local commit"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bare_push_never_fans_out_to_branches_the_caller_is_not_on() {
  // The seeded config used to carry `remote.origin.push =
  // refs/heads/*:refs/heads/*`, which overrides `push.default` outright — so a
  // bare `git push` offered to publish every local branch, including scratch
  // ones the caller was not standing on and had never asked to share. An agent
  // that makes throwaway branches would publish all of them with the most
  // ordinary Git command there is.
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let ws = job.workspace.clone();

  let (dry_run, config) = on_fs(move || {
    let (ok, out) = git_in(&ws, &["switch", "-q", "-c", "scratch-probe"]);
    assert!(ok, "{out}");
    let (ok, out) = git_in(&ws, &["switch", "-q", "-c", "the-work"]);
    assert!(ok, "{out}");

    // `--dry-run` still resolves the refspec, which is the whole question here,
    // and needs no credential because nothing is transmitted.
    let (_, dry_run) = git_in(&ws, &["push", "--dry-run"]);
    let config = std::fs::read_to_string(ws.join(".git/config")).unwrap();
    (dry_run, config)
  })
  .await;

  assert!(
    !dry_run.contains("scratch-probe"),
    "a bare push must never offer a branch the caller is not on:\n{dry_run}"
  );
  assert!(
    !config.contains("push = refs/heads/"),
    "an explicit remote push refspec defeats push.default:\n{config}"
  );
  assert!(
    config.contains("default = simple"),
    "the workspace names push.default rather than inheriting the host's:\n{config}"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pushed_commit_survives_deleting_the_workspace_and_cloning_again() {
  // The round trip every other Git host supports: commit, push, delete the
  // local clone, clone again — the work is on the branch, so it comes back.
  let backend = Backend::start("basic").await;
  let tmp = tempfile::tempdir().unwrap();
  let cache = tempfile::tempdir().unwrap();
  let workspace = tmp.path().join("ws");

  {
    let sockets = tempfile::tempdir().unwrap();
    let (host, listener) = MountHost::bind(host_config(&backend, sockets.path())).unwrap();
    tokio::spawn(Arc::clone(&host).serve(listener));
    let daemon = host
      .mount(mount_spec(
        &backend,
        "main",
        &workspace,
        cache.path(),
        gfs_overlay::OverlayConfig::default(),
      ))
      .await
      .unwrap();
    let ws = workspace.clone();
    on_fs(move || {
      std::fs::write(ws.join("kept.txt"), b"pushed, so it comes back\n").unwrap();
      let (ok, out) = git_in(&ws, &["add", "kept.txt"]);
      assert!(ok, "{out}");
      let (ok, out) = git_in(&ws, &["commit", "-q", "-m", "kept"]);
      assert!(ok, "{out}");
      let out = std::process::Command::new("git")
        .env_clear()
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("PATH", "/usr/bin:/bin")
        .env("GFS_TOKEN", gfs_test::mount::TOKEN)
        .current_dir(&ws)
        .args(["push", "-q", "origin", "main"])
        .output()
        .unwrap();
      assert!(
        out.status.success(),
        "push failed: {}",
        String::from_utf8_lossy(&out.stderr)
      );
    })
    .await;
    daemon.shutdown().await;
  }
  std::fs::remove_dir_all(&workspace).unwrap();

  let sockets = tempfile::tempdir().unwrap();
  let (host, listener) = MountHost::bind(host_config(&backend, sockets.path())).unwrap();
  tokio::spawn(Arc::clone(&host).serve(listener));
  let daemon = host
    .mount(mount_spec(
      &backend,
      "main",
      &workspace,
      cache.path(),
      gfs_overlay::OverlayConfig::default(),
    ))
    .await
    .expect("a fresh clone of the branch the push landed on");
  let ws = workspace.clone();
  let content = on_fs(move || std::fs::read_to_string(ws.join("kept.txt")).unwrap()).await;
  assert_eq!(content, "pushed, so it comes back\n");
  daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_overlay_edit_is_a_modification_and_a_local_commit_stays_local() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let ws = job.workspace.clone();

  let (status, commit_out, show, log) = on_fs(move || {
    // Edit through FUSE: the overlay holds the change.
    std::fs::write(ws.join("README.md"), b"edited by an agent\n").unwrap();
    let (ok, status) = git_in(&ws, &["status", "--porcelain"]);
    assert!(ok, "{status}");

    // The write path ADR 0009 claims: add + commit, no export, no RPC.
    let (ok, add) = git_in(&ws, &["add", "README.md"]);
    assert!(ok, "{add}");
    let (ok, commit_out) = git_in(&ws, &["commit", "-m", "agent work"]);
    let (_, show) = git_in(&ws, &["show", "--stat", "--oneline", "HEAD"]);
    let (_, log) = git_in(&ws, &["log", "--oneline", "-2"]);
    assert!(ok, "{commit_out}");
    (status, commit_out, show, log)
  })
  .await;

  assert_eq!(status.trim(), "M README.md", "{status}");
  assert!(commit_out.contains("agent work"), "{commit_out}");
  assert!(show.contains("README.md"), "{show}");
  assert!(log.contains("agent work"), "{log}");

  // The commit is on local disk in the workspace's own git dir — reached
  // through the mount's passthrough, which is the only namespace a tool has.
  let objects = job.workspace.join(".git/objects");
  let loose = on_fs(move || walk_count(&objects)).await;
  assert!(loose >= 3, "commit, tree, and blob must be local: {loose}");
}

fn walk_count(dir: &std::path::Path) -> usize {
  let mut n = 0;
  for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
    let path = entry.path();
    if path.is_dir() {
      n += walk_count(&path);
    } else if path.parent().is_some_and(|p| {
      p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.len() == 2)
    }) {
      n += 1;
    }
  }
  n
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_switch_over_local_commits_is_refused_rather_than_stranding_them() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let ws = job.workspace.clone();

  on_fs(move || {
    std::fs::write(ws.join("README.md"), b"work\n").unwrap();
    let (ok, out) = git_in(&ws, &["commit", "-am", "unpushed"]);
    assert!(ok, "{out}");
    // The worktree edit is now committed; restore the overlay to clean so the
    // dirty-workspace check does not fire first and mask the commit guard.
    let (_, _) = git_in(&ws, &["checkout", "--", "README.md"]);
  })
  .await;

  // The overlay still holds the checkout's copy-up, so discard it to isolate
  // the guard under test... except a checkout writes the *committed* content,
  // which differs from base. Both guards refusing is fine; what must not
  // happen is a refresh that succeeds and overwrites the branch ref.
  let err = job
    .daemon
    .refresh()
    .await
    .expect_err("a refresh over local commits must refuse");
  let text = err.message.to_string();
  assert!(
    text.contains("local commits") || text.contains("local changes"),
    "{text}"
  );

  // And the commit is still reachable in the seeded git dir, read through
  // the mount.
  let git_dir = job.workspace.join(".git");
  let head = on_fs(move || gfs_mount::gitdir::local_head(&git_dir).unwrap()).await;
  assert_eq!(head.len(), 40, "{head}");
}

/// Run git against the mirror itself, the way an upstream push would move it.
fn git_mirror(repo: &std::path::Path, args: &[&str]) -> String {
  let out = std::process::Command::new("git")
    .env_clear()
    .env("PATH", "/usr/bin:/bin")
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env("GIT_AUTHOR_NAME", "upstream")
    .env("GIT_AUTHOR_EMAIL", "upstream@example.com")
    .env("GIT_COMMITTER_NAME", "upstream")
    .env("GIT_COMMITTER_EMAIL", "upstream@example.com")
    .arg("-C")
    .arg(repo)
    .args(args)
    .output()
    .unwrap();
  assert!(
    out.status.success(),
    "git {args:?}: {}",
    String::from_utf8_lossy(&out.stderr)
  );
  String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remount_over_an_edit_free_state_dir_follows_the_moved_branch() {
  // The re-clone flow: mount, unmount cleanly, the branch moves upstream,
  // mount the same state directory again. The leftover overlay holds no edits,
  // so it adopts the new base — the binding refusal exists to protect edits,
  // and refusing here would block every re-clone for the sake of nothing.
  let backend = Backend::start("basic").await;
  let tmp = tempfile::tempdir().unwrap();
  let cache = tempfile::tempdir().unwrap();
  let workspace = tmp.path().join("ws");

  {
    let sockets = tempfile::tempdir().unwrap();
    let (host, listener) = MountHost::bind(host_config(&backend, sockets.path())).unwrap();
    tokio::spawn(Arc::clone(&host).serve(listener));
    let daemon = host
      .mount(mount_spec(
        &backend,
        "main",
        &workspace,
        cache.path(),
        gfs_overlay::OverlayConfig::default(),
      ))
      .await
      .unwrap();
    daemon.shutdown().await;
  }

  let old = git_mirror(&backend.repo_path, &["rev-parse", "refs/heads/main"]);
  let tree = git_mirror(&backend.repo_path, &["rev-parse", "refs/heads/main^{tree}"]);
  let moved = git_mirror(
    &backend.repo_path,
    &["commit-tree", &tree, "-p", &old, "-m", "moved upstream"],
  );
  git_mirror(
    &backend.repo_path,
    &["update-ref", "refs/heads/main", &moved],
  );

  let sockets = tempfile::tempdir().unwrap();
  let (host, listener) = MountHost::bind(host_config(&backend, sockets.path())).unwrap();
  tokio::spawn(Arc::clone(&host).serve(listener));
  let daemon = host
    .mount(mount_spec(
      &backend,
      "main",
      &workspace,
      cache.path(),
      gfs_overlay::OverlayConfig::default(),
    ))
    .await
    .expect("an edit-free leftover workspace must remount at the moved head");

  let ws = workspace.clone();
  let (ok, head) = on_fs(move || git_in(&ws, &["rev-parse", "HEAD"])).await;
  assert!(ok, "{head}");
  assert_eq!(head.trim(), moved, "the remount serves the moved branch");
  daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remount_over_local_commits_adopts_them_when_the_pin_matches_their_base() {
  // The first-mount twin of the switch guard above. There is no in-memory pin
  // to compare against on a fresh mount, so the mount reads what the last seed
  // recorded (`gfs.json`) — a `HEAD` past it is local commits. When the
  // selector still resolves to the commit they were made on, they are the
  // agent's work continuing: the mount seeds around the refs and the index,
  // and the overlay's binding check adopts the matching edits.
  let backend = Backend::start("basic").await;
  let tmp = tempfile::tempdir().unwrap();
  let cache = tempfile::tempdir().unwrap();
  let workspace = tmp.path().join("ws");

  {
    let sockets = tempfile::tempdir().unwrap();
    let (host, listener) = MountHost::bind(host_config(&backend, sockets.path())).unwrap();
    tokio::spawn(Arc::clone(&host).serve(listener));
    let daemon = host
      .mount(mount_spec(
        &backend,
        "main",
        &workspace,
        cache.path(),
        gfs_overlay::OverlayConfig::default(),
      ))
      .await
      .unwrap();
    let ws = workspace.clone();
    on_fs(move || {
      std::fs::write(ws.join("local.txt"), b"unpushed\n").unwrap();
      let (ok, out) = git_in(&ws, &["add", "local.txt"]);
      assert!(ok, "{out}");
      let (ok, out) = git_in(&ws, &["commit", "-q", "-m", "left behind"]);
      assert!(ok, "{out}");
    })
    .await;
    daemon.shutdown().await;
  }

  let sockets = tempfile::tempdir().unwrap();
  let (host, listener) = MountHost::bind(host_config(&backend, sockets.path())).unwrap();
  tokio::spawn(Arc::clone(&host).serve(listener));
  let daemon = host
    .mount(mount_spec(
      &backend,
      "main",
      &workspace,
      cache.path(),
      gfs_overlay::OverlayConfig::default(),
    ))
    .await
    .expect("an unmoved base must remount with the local commits adopted");

  let ws = workspace.clone();
  let (head, content, status) = on_fs(move || {
    let (ok, head) = git_in(&ws, &["rev-parse", "HEAD"]);
    assert!(ok, "{head}");
    let content = std::fs::read_to_string(ws.join("local.txt")).unwrap();
    let (ok, status) = git_in(&ws, &["status", "--porcelain"]);
    assert!(ok, "{status}");
    (head, content, status)
  })
  .await;
  let (ok, expected) = on_fs({
    let ws = workspace.clone();
    move || git_in(&ws, &["rev-parse", "refs/heads/main"])
  })
  .await;
  assert!(ok, "{expected}");
  assert_eq!(head, expected, "HEAD is still the local commit's branch");
  assert_eq!(content, "unpushed\n", "the committed edit is still served");
  assert_eq!(
    status.trim(),
    "",
    "the preserved index matches the local HEAD"
  );
  daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remount_past_local_commits_is_refused_and_their_base_still_mounts() {
  // The refusal half: the branch moved upstream while the local commits sit on
  // the old pin. Re-seeding at the moved head would strand them, so that mount
  // is refused — and the recovery the error names (mount at their base) must
  // actually work, adopting the commits for a push or an export.
  let backend = Backend::start("basic").await;
  let tmp = tempfile::tempdir().unwrap();
  let cache = tempfile::tempdir().unwrap();
  let workspace = tmp.path().join("ws");

  let base = git_mirror(&backend.repo_path, &["rev-parse", "refs/heads/main"]);
  {
    let sockets = tempfile::tempdir().unwrap();
    let (host, listener) = MountHost::bind(host_config(&backend, sockets.path())).unwrap();
    tokio::spawn(Arc::clone(&host).serve(listener));
    let daemon = host
      .mount(mount_spec(
        &backend,
        "main",
        &workspace,
        cache.path(),
        gfs_overlay::OverlayConfig::default(),
      ))
      .await
      .unwrap();
    let ws = workspace.clone();
    on_fs(move || {
      std::fs::write(ws.join("local.txt"), b"unpushed\n").unwrap();
      let (ok, out) = git_in(&ws, &["add", "local.txt"]);
      assert!(ok, "{out}");
      let (ok, out) = git_in(&ws, &["commit", "-q", "-m", "left behind"]);
      assert!(ok, "{out}");
    })
    .await;
    daemon.shutdown().await;
  }

  let tree = git_mirror(&backend.repo_path, &["rev-parse", "refs/heads/main^{tree}"]);
  let moved = git_mirror(
    &backend.repo_path,
    &["commit-tree", &tree, "-p", &base, "-m", "moved upstream"],
  );
  git_mirror(
    &backend.repo_path,
    &["update-ref", "refs/heads/main", &moved],
  );

  let sockets = tempfile::tempdir().unwrap();
  let (host, listener) = MountHost::bind(host_config(&backend, sockets.path())).unwrap();
  tokio::spawn(Arc::clone(&host).serve(listener));
  let error = host
    .mount(mount_spec(
      &backend,
      "main",
      &workspace,
      cache.path(),
      gfs_overlay::OverlayConfig::default(),
    ))
    .await
    .expect_err("a pin past the local commits' base must not re-seed over them");
  assert_eq!(error.code, gfs_types::error::ErrorCode::FailedPrecondition);
  assert!(error.message.contains("local commits"), "{}", error.message);

  let daemon = host
    .mount(mount_spec(
      &backend,
      &base,
      &workspace,
      cache.path(),
      gfs_overlay::OverlayConfig::default(),
    ))
    .await
    .expect("the recovery the error names must mount at the base");
  let ws = workspace.clone();
  let (ok, content) =
    on_fs(move || (true, std::fs::read_to_string(ws.join("local.txt")).unwrap())).await;
  assert!(ok);
  assert_eq!(content, "unpushed\n");
  daemon.shutdown().await;
}

/// A first commit must write what changed, not the whole repository.
///
/// Two mechanisms meet here, and both were broken. The shipped index carries a
/// cache tree, so Git reuses every directory the commit did not touch instead of
/// re-deriving all of them; and a `utimensat` on the projection succeeds, so
/// Git's "do I already have this object" check can say yes about a packed object
/// rather than writing a duplicate loose copy. Without the first, a five-file
/// change on vscode wrote 4 299 objects and took 12.3 s.
///
/// `siblings` is the fixture because it has a dozen directories: the object
/// count separates "wrote the changed path" from "wrote the tree".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_first_commit_writes_the_changed_path_and_not_the_whole_tree() {
  let backend = Backend::start("siblings").await;
  let job = Job::start(&backend, "main").await;
  let ws = job.workspace.clone();

  let (before, after_hash, after_commit, status, dirs) = on_fs(move || {
    let objects = ws.join(".git/objects");
    let before = walk_count(&objects);

    // Re-hashing content the projection already holds must reuse the packed
    // object. This is the call that used to fail: Git freshens the pack first,
    // and a refusal there reads as "cannot vouch for it", so it wrote a copy.
    let (ok, out) = git_in(&ws, &["hash-object", "-w", "--", "b/f.txt"]);
    assert!(ok, "{out}");
    let after_hash = walk_count(&objects);

    std::fs::write(ws.join("a/f.txt"), b"edited by an agent\n").unwrap();
    let (ok, out) = git_in(&ws, &["add", "-A"]);
    assert!(ok, "{out}");
    let (ok, out) = git_in(&ws, &["commit", "-m", "one file"]);
    assert!(ok, "{out}");
    let after_commit = walk_count(&objects);

    let (_, status) = git_in(&ws, &["status", "--porcelain"]);
    let (_, dirs) = git_in(&ws, &["ls-tree", "-r", "-d", "--name-only", "HEAD"]);
    (before, after_hash, after_commit, status, dirs)
  })
  .await;

  assert_eq!(
    after_hash, before,
    "an object the projection already holds must not be written again"
  );
  // One blob, the tree for `a/`, the new root tree, the commit.
  let written = after_commit - after_hash;
  let directories = dirs.lines().count();
  assert!(
    written <= 5,
    "a one-file commit wrote {written} objects across {directories} directories; \
     the cache tree is missing or wrong"
  );
  assert!(
    directories >= 10,
    "the fixture must have enough directories for this to mean anything: {directories}"
  );
  assert_eq!(
    status.trim(),
    "",
    "the commit must leave a clean tree: {status}"
  );
}
