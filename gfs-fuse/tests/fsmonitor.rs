//! The fsmonitor hook, end to end: daemon, workspace, real `git status`.
//!
//! The one property that must hold is the dangerous direction: a hook that
//! *hides* a change makes `git status` lie, which is ADR 0005's original sin
//! reappearing through a side door. So the test that matters is edit → status
//! → the edit is reported, with the hook demonstrably installed and answering.

use gfs_test::mount::{on_fs, Backend, Job};

fn git_in(workspace: &std::path::Path, args: &[&str]) -> (bool, String) {
  let out = std::process::Command::new("git")
    .env_clear()
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    // The hook script execs an absolute binary path, so this PATH only needs
    // git's own helpers and a shell.
    .env("PATH", "/usr/bin:/bin")
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
async fn the_hook_is_installed_and_status_still_tells_the_truth() {
  // The daemon looks for `gfs-fsmonitor` next to its own executable, then on
  // PATH. In a test the "daemon" is this test binary, so PATH is how the seed
  // finds the real hook binary Cargo built.
  let hook_dir = std::path::Path::new(env!("CARGO_BIN_EXE_gfs-fsmonitor"))
    .parent()
    .unwrap()
    .to_path_buf();
  let old_path = std::env::var_os("PATH").unwrap_or_default();
  let mut paths = vec![hook_dir];
  paths.extend(std::env::split_paths(&old_path));
  std::env::set_var("PATH", std::env::join_paths(paths).unwrap());

  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  // The seed must have found the binary and wired the config.
  let config = std::fs::read_to_string(job.workspace.join(".git/config")).unwrap();
  assert!(
    config.contains("fsmonitor = "),
    "the hook must be installed when the binary is findable:\n{config}"
  );
  let hook = job.workspace.join(".git/hooks/gfs-fsmonitor");
  assert!(hook.is_file(), "the hook script must exist");

  let ws = job.workspace.clone();
  let (prime, edited, second) = on_fs(move || {
    // Priming run: the hook's token is new to Git, so this one is a full
    // rescan by design.
    let (ok, prime) = git_in(&ws, &["status", "--porcelain"]);
    assert!(ok, "{prime}");

    // The dangerous direction: a change made *after* priming must be reported
    // on the next run, which now trusts the hook.
    std::fs::write(ws.join("src/main.rs"), b"fn main() { edited() }\n").unwrap();
    let (ok, edited) = git_in(&ws, &["status", "--porcelain"]);
    assert!(ok, "{edited}");

    let (ok, second) = git_in(&ws, &["status", "--porcelain"]);
    assert!(ok, "{second}");
    (prime, edited, second)
  })
  .await;

  assert_eq!(prime.trim(), "", "a fresh workspace is clean");
  assert_eq!(
    edited.trim(),
    "M src/main.rs",
    "the hook must not hide the edit"
  );
  assert_eq!(
    second.trim(),
    "M src/main.rs",
    "and it must keep reporting it"
  );
}
