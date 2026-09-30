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
  // gfs's settings live in the file `.git/config` includes.
  let config = std::fs::read_to_string(job.workspace.join(".git/gfs/config")).unwrap();
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
  // be a bare `gfs:<generation>`, constant for the life of the pin. Only the
  // generation decides a full rescan.
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
  assert_eq!(
    paths,
    vec!["/".to_owned()],
    "an alien token forces a rescan"
  );

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_answer_is_a_delta_and_a_quiet_status_rewrites_nothing() {
  // A cumulative answer names every overlay path on every call; Git then
  // invalidates each one's untracked-cache directory and rewrites the whole
  // index -- 236 MB on universe, every `git status`. The answer is only what
  // changed since the caller's token, and nothing at all when nothing did.
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
    let mut paths: Vec<String> = fields
      .filter(|f| !f.is_empty())
      .map(|f| String::from_utf8_lossy(f).into_owned())
      .collect();
    paths.sort();
    (token, paths)
  };

  let ws = job.workspace.clone();
  on_fs({
    let ws = ws.clone();
    move || std::fs::write(ws.join("src/main.rs"), b"fn main() { edited() }\n").unwrap()
  })
  .await;
  let (t0, _) = ask("");
  on_fs({
    let ws = ws.clone();
    move || std::fs::write(ws.join("new.txt"), b"new\n").unwrap()
  })
  .await;
  let (t1, paths) = ask(&t0);
  assert_eq!(paths, vec!["new.txt".to_owned()], "only what changed since t0");
  let (t2, paths) = ask(&t1);
  assert_eq!(t2, t1, "nothing changed: the same token, byte for byte");
  assert!(paths.is_empty(), "{paths:?}");

  // A write through a descriptor still open is reported before the close.
  let (held, t3, paths) = on_fs({
    let ws = ws.clone();
    let hook = hook.clone();
    let t2 = t2.clone();
    move || {
      use std::io::Write;
      let mut held = std::fs::OpenOptions::new()
        .append(true)
        .open(ws.join("new.txt"))
        .unwrap();
      held.write_all(b"more\n").unwrap();
      let out = std::process::Command::new(&hook)
        .current_dir(&ws)
        .args(["2", &t2])
        .output()
        .unwrap();
      let mut fields = out.stdout.split(|b| *b == 0);
      let token = String::from_utf8_lossy(fields.next().unwrap_or_default()).into_owned();
      let paths: Vec<String> = fields
        .filter(|f| !f.is_empty())
        .map(|f| String::from_utf8_lossy(f).into_owned())
        .collect();
      (held, token, paths)
    }
  })
  .await;
  drop(held);
  assert_ne!(t3, t2);
  assert_eq!(paths, vec!["new.txt".to_owned()], "the unsettled write");

  // A rename names both sides; a delete names what is gone.
  on_fs({
    let ws = ws.clone();
    move || {
      std::fs::rename(ws.join("src/main.rs"), ws.join("src/moved.rs")).unwrap();
      std::fs::remove_file(ws.join("new.txt")).unwrap();
    }
  })
  .await;
  let (_, paths) = ask(&t3);
  for expected in ["new.txt", "src/main.rs", "src/moved.rs"] {
    assert!(paths.contains(&expected.to_owned()), "{expected} in {paths:?}");
  }

  // And through Git: a status after the changes writes the index, the next
  // one -- nothing changed -- leaves it alone, and both tell the truth.
  let (first, quiet, rewritten, plain) = on_fs(move || {
    let identity = |ws: &std::path::Path| {
      use std::os::unix::fs::MetadataExt;
      let m = std::fs::metadata(ws.join(".git/index")).unwrap();
      (m.ino(), m.mtime(), m.mtime_nsec())
    };
    let (ok, first) = git_in(&ws, &["status", "--porcelain"]);
    assert!(ok, "{first}");
    let before = identity(&ws);
    let (ok, quiet) = git_in(&ws, &["status", "--porcelain"]);
    assert!(ok, "{quiet}");
    let rewritten = identity(&ws) != before;
    let (ok, plain) = git_in(
      &ws,
      &[
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.untrackedCache=false",
        "status",
        "--porcelain",
      ],
    );
    assert!(ok, "{plain}");
    (first, quiet, rewritten, plain)
  })
  .await;
  assert_eq!(first, plain);
  assert_eq!(quiet, plain);
  assert!(!rewritten, "a status with nothing new must not rewrite the index");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gfs_switch_carries_edits_the_two_commits_agree_on() {
  // Git's rule: a local change travels when its path is the same in both
  // commits; otherwise the switch is refused and nothing moves. `main` and
  // `old` (v1.0) agree on README.md and src/lib/; they differ on
  // src/main.rs, src/new.rs (main only), and docs/guide.md (old only).
  install_hook_on_path();
  use gfs_mount::control::{Request, Response};
  let clone_dir = tempfile::tempdir().unwrap();
  let clone = clone_dir.path().join("clone");
  Job::clone_fixture("basic", &clone);
  assert!(git_in(&clone, &["branch", "old", "v1.0"]).0);
  // `deep` adds a/b/c.txt: a two-level directory main does not have.
  std::fs::create_dir_all(clone.join("a/b")).unwrap();
  std::fs::write(clone.join("a/b/c.txt"), b"c\n").unwrap();
  for args in [
    &["switch", "-q", "-c", "deep"][..],
    &["add", "a"],
    &["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-qm", "deep"],
    &["switch", "-q", "main"],
  ] {
    let (ok, out) = git_in(&clone, args);
    assert!(ok, "{out}");
  }
  let job = Job::local_from(&clone, "main", tempfile::tempdir().unwrap()).await;
  let ws = job.workspace.clone();
  let config = std::fs::read_to_string(ws.join(".git/gfs/config")).unwrap();
  assert!(config.contains("fsmonitor"), "the hook must be answering:\n{config}");

  let refusal = |target: &str| {
    let socket = job.socket();
    let request = Request::Switch {
      selector: target.to_owned(),
      branch: None,
      create: false,
      start_point: None,
      detach: false,
    };
    async move {
      let response = on_fs(move || gfs_mount::control::call(&socket, &request).unwrap()).await;
      match response {
        Response::Error { message, .. } => Some(message),
        _ => None,
      }
    }
  };
  let run = |args: &'static [&'static str]| {
    let ws = ws.clone();
    on_fs(move || git_in(&ws, args))
  };
  let fs = |f: fn(&std::path::Path)| {
    let ws = ws.clone();
    on_fs(move || f(&ws))
  };
  let statuses = || {
    let ws = ws.clone();
    on_fs(move || {
      let cached = git_in(&ws, &["status", "--porcelain"]).1;
      let plain = git_in(
        &ws,
        &[
          "-c",
          "core.fsmonitor=false",
          "-c",
          "core.untrackedCache=false",
          "status",
          "--porcelain",
        ],
      )
      .1;
      (cached, plain)
    })
  };
  // Seed the untracked cache and the fsmonitor token as a user's shell would.
  run(&["status", "--porcelain"]).await;

  // An untracked file where `old` tracks one: refused, nothing moves.
  fs(|ws| {
    std::fs::write(ws.join("README.md"), b"edited\n").unwrap();
    std::fs::write(ws.join("docs/guide.md"), b"mine\n").unwrap_or_else(|_| {
      std::fs::create_dir(ws.join("docs")).unwrap();
      std::fs::write(ws.join("docs/guide.md"), b"mine\n").unwrap();
    });
  })
  .await;
  let message = refusal("old").await.expect("an untracked conflict is refused");
  assert!(message.contains("docs/guide.md"), "{message}");
  assert!(!message.contains("README.md"), "{message}");
  assert_eq!(
    run(&["symbolic-ref", "HEAD"]).await.1.trim(),
    "refs/heads/main"
  );

  // A change to a path `old` does not have: refused too.
  fs(|ws| {
    std::fs::remove_file(ws.join("docs/guide.md")).unwrap();
    std::fs::write(ws.join("src/new.rs"), b"changed\n").unwrap();
  })
  .await;
  let message = refusal("old").await.expect("a changed path is refused");
  assert!(message.contains("src/new.rs"), "{message}");

  // Staged changes would be lost with the index: refused until unstaged.
  fs(|ws| {
    std::fs::remove_file(ws.join("src/new.rs")).unwrap();
    std::fs::write(ws.join("notes.txt"), b"todo\n").unwrap();
    std::fs::write(ws.join("src/lib/extra.rs"), b"pub fn extra() {}\n").unwrap();
    std::fs::remove_file(ws.join("src/lib/util.rs")).unwrap();
  })
  .await;
  assert!(run(&["add", "notes.txt"]).await.0);
  let message = refusal("old").await.expect("staged changes are refused");
  assert!(message.contains("staged"), "{message}");
  assert!(run(&["restore", "--staged", "notes.txt"]).await.0);

  // Now it goes: the edit, the deletion, and the new files come along; the
  // deletion of src/new.rs, which `old` does not have either, is gone.
  let (before, _) = statuses().await;
  assert!(before.contains(" D src/new.rs"), "{before}");
  assert_eq!(refusal("old").await, None);
  let (cached, plain) = statuses().await;
  assert_eq!(cached, plain);
  for line in [" M README.md", " D src/lib/util.rs", "?? notes.txt", "?? src/lib/extra.rs"] {
    assert!(cached.contains(line), "{line} missing from:\n{cached}");
  }
  assert!(!cached.contains("src/new.rs"), "{cached}");
  fs(|ws| {
    assert_eq!(std::fs::read(ws.join("README.md")).unwrap(), b"edited\n");
    assert_eq!(
      std::fs::read(ws.join("src/main.rs")).unwrap(),
      b"fn main() { println!(\"hi\"); }\n"
    );
    assert_eq!(std::fs::read(ws.join("docs/guide.md")).unwrap(), b"guide\n");
    assert!(!ws.join("src/new.rs").exists());
    assert!(!ws.join("src/lib/util.rs").exists());
  })
  .await;
  // A quiet status afterwards agrees too: the carried paths were reported once.
  let (cached, plain) = statuses().await;
  assert_eq!(cached, plain);

  // And back: the same edits, over main's files.
  assert_eq!(refusal("main").await, None);
  let (cached, plain) = statuses().await;
  assert_eq!(cached, plain);
  for line in [" M README.md", " D src/lib/util.rs", "?? notes.txt", "?? src/lib/extra.rs"] {
    assert!(cached.contains(line), "{line} missing from:\n{cached}");
  }
  fs(|ws| {
    assert_eq!(std::fs::read(ws.join("src/new.rs")).unwrap(), b"pub fn added() {}\n");
    assert!(!ws.join("docs/guide.md").exists());
  })
  .await;

  // An untracked file under directories only `deep` has: switching away
  // keeps it, and the directories it needs, as Git does.
  assert_eq!(refusal("deep").await, None);
  fs(|ws| std::fs::write(ws.join("a/b/x.txt"), b"x\n").unwrap()).await;
  assert_eq!(refusal("main").await, None);
  let (cached, plain) = statuses().await;
  assert_eq!(cached, plain);
  assert!(cached.contains("?? a/"), "{cached}");
  fs(|ws| {
    assert_eq!(std::fs::read(ws.join("a/b/x.txt")).unwrap(), b"x\n");
    assert!(!ws.join("a/b/c.txt").exists());
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gfs_switch_leaves_local_commits_on_their_branch() {
  // A stock `git commit` moves HEAD past the pin. `gfs switch` judges edits
  // against HEAD, as Git does: what was committed is clean and takes the
  // target's version; what was not is carried. The commits stay on their
  // branch, and switching back to it pins them.
  install_hook_on_path();
  use gfs_mount::control::{Request, Response};
  let clone_dir = tempfile::tempdir().unwrap();
  let clone = clone_dir.path().join("clone");
  Job::clone_fixture("basic", &clone);
  let job = Job::local_from(&clone, "main", tempfile::tempdir().unwrap()).await;
  let ws = job.workspace.clone();

  let switch = |target: &str, create: bool, detach: bool| {
    let socket = job.socket();
    let request = Request::Switch {
      selector: target.to_owned(),
      branch: None,
      create,
      start_point: None,
      detach,
    };
    async move {
      let response = on_fs(move || gfs_mount::control::call(&socket, &request).unwrap()).await;
      match response {
        Response::Error { message, .. } => Some(message),
        _ => None,
      }
    }
  };
  let run = |args: &'static [&'static str]| {
    let ws = ws.clone();
    on_fs(move || {
      let (ok, out) = git_in(&ws, args);
      assert!(ok, "git {args:?}: {out}");
      out
    })
  };
  let fs = |f: fn(&std::path::Path)| {
    let ws = ws.clone();
    on_fs(move || f(&ws))
  };
  let statuses = || {
    let ws = ws.clone();
    on_fs(move || {
      let cached = git_in(&ws, &["status", "--porcelain"]).1;
      let plain = git_in(
        &ws,
        &[
          "-c",
          "core.fsmonitor=false",
          "-c",
          "core.untrackedCache=false",
          "status",
          "--porcelain",
        ],
      )
      .1;
      (cached, plain)
    })
  };
  run(&["config", "user.name", "someone"]).await;
  run(&["config", "user.email", "someone@example.com"]).await;
  run(&["status", "--porcelain"]).await;

  // Commit on a branch of our own: an edit and a new directory.
  assert_eq!(switch("feature-x", true, false).await, None);
  fs(|ws| {
    std::fs::write(ws.join("src/main.rs"), b"fn main() { feature(); }\n").unwrap();
    std::fs::create_dir(ws.join("feat")).unwrap();
    std::fs::write(ws.join("feat/one.rs"), b"pub fn one() {}\n").unwrap();
  })
  .await;
  run(&["add", "-A"]).await;
  run(&["commit", "-qm", "feature one"]).await;
  let feature = run(&["rev-parse", "HEAD"]).await.trim().to_owned();
  // Uncommitted on top: an edit main agrees on, and an untracked file.
  fs(|ws| {
    std::fs::write(ws.join("README.md"), b"edited\n").unwrap();
    std::fs::write(ws.join("notes.txt"), b"todo\n").unwrap();
  })
  .await;

  // Away to main: the committed edit and directory give way to main's tree;
  // the uncommitted ones come along.
  assert_eq!(switch("main", false, false).await, None);
  let (cached, plain) = statuses().await;
  assert_eq!(cached, plain);
  assert_eq!(cached, " M README.md\n?? notes.txt\n");
  fs(|ws| {
    assert_eq!(
      std::fs::read(ws.join("src/main.rs")).unwrap(),
      b"fn main() { println!(\"bye\"); }\n"
    );
    assert!(!ws.join("feat").exists());
    assert_eq!(std::fs::read(ws.join("README.md")).unwrap(), b"edited\n");
  })
  .await;
  assert_eq!(run(&["rev-parse", "feature-x"]).await.trim(), feature);
  let (ok, _) = git_in(&clone, &["cat-file", "-e", &feature]);
  assert!(ok, "the commit was copied into the clone");

  // And back: the branch's own commit is pinned, edits still carried.
  assert_eq!(switch("feature-x", false, false).await, None);
  let (cached, plain) = statuses().await;
  assert_eq!(cached, plain);
  assert_eq!(cached, " M README.md\n?? notes.txt\n");
  fs(|ws| {
    assert_eq!(
      std::fs::read(ws.join("src/main.rs")).unwrap(),
      b"fn main() { feature(); }\n"
    );
    assert_eq!(std::fs::read(ws.join("feat/one.rs")).unwrap(), b"pub fn one() {}\n");
  })
  .await;

  // A second commit, then an uncommitted edit to a path main changes:
  // refused, nothing moves.
  fs(|ws| std::fs::write(ws.join("feat/two.rs"), b"pub fn two() {}\n").unwrap()).await;
  run(&["add", "feat/two.rs"]).await;
  run(&["commit", "-qm", "feature two"]).await;
  fs(|ws| std::fs::write(ws.join("src/main.rs"), b"fn main() { dirty(); }\n").unwrap()).await;
  let message = switch("main", false, false).await.expect("a conflict is refused");
  assert!(message.contains("src/main.rs"), "{message}");
  assert!(!message.contains("feat/"), "{message}");
  assert_eq!(
    run(&["symbolic-ref", "HEAD"]).await.trim(),
    "refs/heads/feature-x"
  );

  // Staged changes after a commit are refused too.
  run(&["checkout", "--", "src/main.rs"]).await;
  run(&["add", "README.md"]).await;
  let message = switch("main", false, false).await.expect("staged changes are refused");
  assert!(message.contains("staged"), "{message}");
  run(&["restore", "--staged", "README.md"]).await;

  // A local commit by hash, from the other branch.
  assert_eq!(switch("main", false, false).await, None);
  let feature_ref = feature.clone();
  assert_eq!(switch(&feature_ref, false, true).await, None);
  let (cached, plain) = statuses().await;
  assert_eq!(cached, plain);
  assert_eq!(cached, " M README.md\n?? notes.txt\n");
  fs(|ws| {
    assert!(ws.join("feat/one.rs").exists());
    assert!(!ws.join("feat/two.rs").exists());
  })
  .await;
  assert_eq!(run(&["rev-parse", "HEAD"]).await.trim(), feature);
}
