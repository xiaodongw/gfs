//! The caches a workspace's index is seeded with (`UNTR`, `FSMN`) are ones
//! stock Git accepts and trusts: the first `git status` reads no directory it
//! has no reason to, and answers exactly what Git answers without them.
//!
//! Run against a real clone of a fixture with a stand-in fsmonitor hook, so
//! what is under test is the bytes and Git's reading of them; the daemon's own
//! hook answer is exercised by every mounted workspace.

use gfs_git::repository::GitRepository;
use gfs_git::Libgit2Repository;
use gfs_test::mount::Job;
use gfs_types::{HashAlgorithm, RevisionExpression};

fn git(
  dir: &std::path::Path,
  xdg: &std::path::Path,
  trace: Option<&std::path::Path>,
  args: &[&str],
) -> String {
  let mut command = std::process::Command::new("git");
  command
    .env_clear()
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env("XDG_CONFIG_HOME", xdg)
    .env("HOME", xdg)
    .env("PATH", "/usr/bin:/bin")
    .current_dir(dir)
    .args(args);
  if let Some(trace) = trace {
    command.env("GIT_TRACE2_PERF", trace);
  }
  let out = command.output().unwrap();
  assert!(
    out.status.success(),
    "git {args:?}: {}",
    String::from_utf8_lossy(&out.stderr)
  );
  String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Seed `ws`'s index from the pinned commit, with the caches, and point
/// `core.fsmonitor` at a hook that reports `changed` since the seeded token.
fn seed(ws: &std::path::Path, xdg: &std::path::Path, changed: &[&str]) {
  let repo = Libgit2Repository::open(ws, 2, 1 << 20).unwrap();
  let head = RevisionExpression::parse("HEAD", HashAlgorithm::Sha1).unwrap();
  let resolved = repo.resolve(&head.base).unwrap();
  let index = repo
    .index_for_commit(&resolved.commit, resolved.snapshot_time)
    .unwrap();
  let worktree = ws.canonicalize().unwrap();
  let ident = format!("Location {}, system Linux", worktree.display());
  let seeded = gfs_git::index::with_workspace_caches(
    &index,
    &gfs_git::index::WorkspaceCaches {
      ident: &ident,
      dir_flags: (1 << 1) | (1 << 2),
      info_exclude: std::fs::read(ws.join(".git/info/exclude"))
        .ok()
        .map(|mut c| {
          if !c.is_empty() {
            c.push(b'\n');
          }
          gfs_git::index::blob_id(&c)
        }),
      excludes_file: None,
      fsmonitor_token: "gfs:1:0",
    },
  )
  .expect("an index gfs wrote can carry the caches");
  std::fs::write(ws.join(".git/index"), seeded).unwrap();

  let mut answer = String::from("printf 'gfs:1:1\\0");
  for path in changed {
    answer.push_str(&format!("{path}\\0"));
  }
  answer.push_str("'\n");
  let hook = ws.join(".git/fsmonitor-hook");
  std::fs::write(&hook, format!("#!/bin/sh\n{answer}")).unwrap();
  use std::os::unix::fs::PermissionsExt;
  std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
  for (key, value) in [
    ("core.fsmonitor", ".git/fsmonitor-hook"),
    ("core.untrackedCache", "true"),
    ("core.checkStat", "minimal"),
    ("core.trustctime", "false"),
  ] {
    git(ws, xdg, None, &["config", key, value]);
  }
}

/// The seeded status, the directories it opened, and Git's uncached answer.
fn statuses(ws: &std::path::Path, xdg: &std::path::Path) -> (String, u64, String) {
  let trace = ws.join(".git/trace2");
  let seeded = git(ws, xdg, Some(&trace), &["status", "--porcelain=v2"]);
  let opened = std::fs::read_to_string(&trace)
    .unwrap()
    .split("opendir:")
    .nth(1)
    .and_then(|rest| {
      rest
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
    })
    .expect("trace2 reports the untracked walk's opendir count");
  let plain = git(
    ws,
    xdg,
    None,
    &[
      "-c",
      "core.fsmonitor=false",
      "-c",
      "core.untrackedCache=false",
      "status",
      "--porcelain=v2",
    ],
  );
  (seeded, opened, plain)
}

#[test]
fn git_trusts_a_seeded_pristine_workspace_and_agrees_with_itself() {
  let tmp = tempfile::tempdir().unwrap();
  let ws = tmp.path().join("ws");
  let xdg = tmp.path().join("xdg");
  std::fs::create_dir_all(&xdg).unwrap();
  Job::clone_fixture("basic", &ws);
  seed(&ws, &xdg, &[]);

  let (seeded, opened, plain) = statuses(&ws, &xdg);
  assert_eq!(seeded, plain);
  assert_eq!(seeded, "", "a pristine workspace has nothing to report");
  // Without the cache Git opens every directory; with it, none.
  assert_eq!(opened, 0, "the seeded untracked cache was not trusted");
}

#[test]
fn a_change_the_hook_reports_is_seen_through_the_seeded_caches() {
  let tmp = tempfile::tempdir().unwrap();
  let ws = tmp.path().join("ws");
  let xdg = tmp.path().join("xdg");
  std::fs::create_dir_all(&xdg).unwrap();
  Job::clone_fixture("basic", &ws);

  // A new file in a new nested directory, and a modified tracked file: the
  // two things the pristine caches claim cannot exist.
  let tracked = git(&ws, &xdg, None, &["ls-files"]);
  let tracked = tracked
    .lines()
    .next()
    .expect("the fixture has files")
    .to_owned();
  std::fs::create_dir_all(ws.join("fresh/nested")).unwrap();
  std::fs::write(ws.join("fresh/nested/new.txt"), "new\n").unwrap();
  std::fs::write(ws.join(&tracked), "changed\n").unwrap();
  seed(&ws, &xdg, &["fresh/nested/new.txt", &tracked]);

  let (seeded, _, plain) = statuses(&ws, &xdg);
  assert_eq!(seeded, plain);
  assert!(seeded.contains("? fresh/"), "{seeded}");
  assert!(seeded.contains(&tracked), "{seeded}");
}
