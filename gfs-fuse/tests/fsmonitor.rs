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

/// Put the hook binary Cargo built where the seed looks for it.
///
/// The daemon looks next to its own executable, then on `PATH`. In a test the
/// "daemon" is this test binary, so `PATH` is the route. Once per process:
/// every test here needs it and they share the environment.
fn install_hook_on_path() {
  static ONCE: std::sync::Once = std::sync::Once::new();
  ONCE.call_once(|| {
    let hook_dir = std::path::Path::new(env!("CARGO_BIN_EXE_gfs-fsmonitor"))
      .parent()
      .unwrap()
      .to_path_buf();
    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![hook_dir];
    paths.extend(std::env::split_paths(&old_path));
    std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
  });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_hook_is_installed_and_status_still_tells_the_truth() {
  install_hook_on_path();

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_created_then_deleted_file_does_not_haunt_status() {
  // The reported bug, in its minimal form. The intervening `status` is
  // load-bearing: it is what makes Git cache the directory's untracked extent,
  // and with fsmonitor configured Git stops `lstat`ing the directory
  // altogether — it invalidates that extent only when the hook names a path
  // inside it. A file created and then deleted leaves no journal row, so
  // without `Overlay::vanished` the hook has nothing to name and `?? f.txt`
  // survives the file.
  //
  // Both depths matter: the root had a second failure of its own, since its
  // timestamps had nowhere to live.
  install_hook_on_path();
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  let ws = job.workspace.clone();
  let (created, after_delete, in_subdir, after_subdir_delete) = on_fs(move || {
    let (ok, _) = git_in(&ws, &["status", "--porcelain"]);
    assert!(ok);

    std::fs::write(ws.join("f.txt"), b"x\n").unwrap();
    let (ok, created) = git_in(&ws, &["status", "--porcelain"]);
    assert!(ok, "{created}");
    std::fs::remove_file(ws.join("f.txt")).unwrap();
    let (ok, after_delete) = git_in(&ws, &["status", "--porcelain"]);
    assert!(ok, "{after_delete}");

    std::fs::write(ws.join("src/g.txt"), b"x\n").unwrap();
    let (ok, in_subdir) = git_in(&ws, &["status", "--porcelain"]);
    assert!(ok, "{in_subdir}");
    std::fs::remove_file(ws.join("src/g.txt")).unwrap();
    let (ok, after_subdir_delete) = git_in(&ws, &["status", "--porcelain"]);
    assert!(ok, "{after_subdir_delete}");

    (created, after_delete, in_subdir, after_subdir_delete)
  })
  .await;

  assert_eq!(created.trim(), "?? f.txt", "the create is seen");
  assert_eq!(
    after_delete.trim(),
    "",
    "and so is the delete — a file that is gone must not stay listed"
  );
  assert_eq!(in_subdir.trim(), "?? src/g.txt", "the same one level down");
  assert_eq!(after_subdir_delete.trim(), "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_staged_file_that_is_then_deleted_is_reported_as_deleted() {
  // The severe half of the same cause. Once a path is in the index, a hook that
  // does not name it leaves `CE_FSMONITOR_VALID` set and Git skips the `lstat`
  // that would notice the file is gone — so `git status` reports a staged file
  // as present when it is not, which is the "status lies" failure ADR 0005 was
  // written to avoid.
  install_hook_on_path();
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  let ws = job.workspace.clone();
  let (staged, after_delete) = on_fs(move || {
    std::fs::write(ws.join("s.txt"), b"y\n").unwrap();
    let (ok, out) = git_in(&ws, &["add", "s.txt"]);
    assert!(ok, "{out}");
    let (ok, staged) = git_in(&ws, &["status", "--porcelain"]);
    assert!(ok, "{staged}");
    std::fs::remove_file(ws.join("s.txt")).unwrap();
    let (ok, after_delete) = git_in(&ws, &["status", "--porcelain"]);
    assert!(ok, "{after_delete}");
    (staged, after_delete)
  })
  .await;

  assert_eq!(staged.trim(), "A  s.txt");
  assert_eq!(
    after_delete.trim(),
    "AD s.txt",
    "staged, then deleted from the worktree — what stock Git reports"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_token_advances_when_the_workspace_changes() {
  // The v2 protocol asks the token to move when the filesystem does. It used to
  // be a bare `gfs:<generation>`, constant for the life of the pin. The answer
  // is still cumulative for the generation — a superset is what makes it safe —
  // so only the generation decides a full rescan.
  install_hook_on_path();
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  let hook = job.workspace.join(".git/hooks/gfs-fsmonitor");
  let ask = |token: &str| {
    let out = std::process::Command::new(&hook)
      .current_dir(&job.workspace)
      .args(["2", token])
      .output()
      .unwrap();
    let mut fields = out.stdout.split(|b| *b == 0);
    let token = String::from_utf8_lossy(fields.next().unwrap_or_default()).into_owned();
    let paths: Vec<String> = fields
      .filter(|f| !f.is_empty())
      .map(|f| String::from_utf8_lossy(f).into_owned())
      .collect();
    (token, paths)
  };

  let (first, _) = ask("");
  assert!(
    first.starts_with("gfs:"),
    "the token names the generation: {first}"
  );
  // A token the daemon never issued is the one case that must answer "rescan".
  let (_, paths) = ask("0");
  assert_eq!(paths, vec!["/".to_owned()], "an alien token forces a rescan");

  let ws = job.workspace.clone();
  on_fs(move || {
    std::fs::write(ws.join("src/main.rs"), b"fn main() { edited() }\n").unwrap();
  })
  .await;

  let (second, paths) = ask(&first);
  assert_ne!(first, second, "the token advances with the change");
  assert!(
    paths.contains(&"src/main.rs".to_owned()),
    "and the change is named rather than rescanned: {paths:?}"
  );
}
