//! M2.4: compatibility.
//!
//! Three groups, each answering a different question.
//!
//! **Does the mount show the right tree?** Compared against the raw-tree
//! materializer from PLAN.md section 12 — `git ls-tree` plus `git cat-file`,
//! which apply no checkout-time conversion. Comparing against a real `git
//! checkout` instead would report `.gitattributes` conversion as a failure, and
//! that conversion is correct behaviour on Git's side and correct *absence* on
//! XVFS's.
//!
//! **Does it behave like a filesystem?** A POSIX subset chosen for what a build
//! actually depends on: the errno for the wrong kind of object, path limits,
//! offsets, and the read-only boundary.
//!
//! **Does the `git` surface answer the tools that probe it?** Both halves: stock
//! Git against the raw surface, and the shim against the frozen grammar.
//!
//! # What is not here
//!
//! `pjdfstest` and `xfstests` are **not run**. Neither is installed in this
//! environment and neither is packaged as a Rust dependency, so the cases below
//! are a hand-written subset of the same ground rather than the suites
//! themselves. That is a real gap in PLAN.md M2.4's first bullet and it is
//! recorded as such in the M2 report — not quietly satisfied by tests that
//! resemble them.

mod harness;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use harness::{on_fs, Backend, Mount};

/// The shim binary, built by the same `cargo test` invocation that runs this.
fn shim() -> &'static str {
  env!("CARGO_BIN_EXE_xvfs-git-shim")
}

/// Run a command in the workspace with a hermetic environment.
fn run_in(directory: &Path, program: &str, args: &[&str]) -> (bool, String, String) {
  let out = Command::new(program)
    .current_dir(directory)
    .args(args)
    // The developer's real configuration must not decide what a compatibility
    // test sees. `safe.directory` in particular would mask exactly the ownership
    // question ADR 0005 asks.
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .output()
    .unwrap_or_else(|e| panic!("running {program}: {e}"));
  (
    out.status.success(),
    String::from_utf8_lossy(&out.stdout).into_owned(),
    String::from_utf8_lossy(&out.stderr).into_owned(),
  )
}

// ---------------------------------------------------------------------------
// The tree oracle
// ---------------------------------------------------------------------------

async fn assert_matches_raw_tree(fixture: &str) {
  let backend = Backend::start(fixture).await;
  let mount = Mount::new(&backend, "main").await;

  let repo = backend.repo_path.clone();
  let root = mount.path.clone();
  let differences = on_fs(move || {
    let materialized = tempfile::tempdir().unwrap();
    xvfs_test::materialize_raw(&repo, "main", materialized.path()).unwrap();
    let expected = xvfs_test::snapshot_tree(materialized.path()).unwrap();
    let actual = xvfs_test::snapshot_tree(&root).unwrap();
    xvfs_test::diff_trees(&expected, &actual)
  })
  .await;

  assert!(
    differences.is_empty(),
    "{fixture} diverges from the raw-tree oracle:\n  {}",
    differences.join("\n  ")
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_mount_matches_the_raw_tree_for_basic_shapes() {
  assert_matches_raw_tree("basic").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_mount_matches_the_raw_tree_for_modes_symlinks_and_gitlinks() {
  assert_matches_raw_tree("modes").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_mount_matches_the_raw_tree_for_non_utf8_paths() {
  assert_matches_raw_tree("bytes").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_mount_matches_the_raw_tree_for_awkward_content() {
  // Empty, CRLF, no final newline, NUL bytes, a 4 MiB line, and a 12 MiB blob.
  assert_matches_raw_tree("content").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_mount_matches_the_raw_tree_for_deep_nesting() {
  assert_matches_raw_tree("deep").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_mount_matches_the_raw_tree_for_a_fully_packed_repository() {
  assert_matches_raw_tree("packed").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_mount_matches_the_raw_tree_even_where_gitattributes_would_convert() {
  assert_matches_raw_tree("attrs").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_filtered_checkout_diverges_and_that_is_the_documented_behaviour() {
  // PLAN.md M2.4's second half: run the same comparison with filters enabled and
  // record the divergence as expected. DESIGN.md section 12 and ADR 0006 both
  // fix "raw blob bytes, no content filters" as an MVP compatibility boundary,
  // so this test *asserts the difference exists*. If it ever stops diverging,
  // either the fixture stopped exercising conversion or XVFS started applying
  // filters -- and both are things a reviewer needs told.
  let backend = Backend::start("attrs").await;
  let mount = Mount::new(&backend, "main").await;

  let repo = backend.repo_path.clone();
  let root = mount.path.clone();
  let (differences, raw_bytes, filtered_bytes) = on_fs(move || {
    let checkout = tempfile::tempdir().unwrap();
    xvfs_test::materialize_checkout(&repo, "main", checkout.path()).unwrap();
    let expected = xvfs_test::snapshot_tree(checkout.path()).unwrap();
    let actual = xvfs_test::snapshot_tree(&root).unwrap();
    (
      xvfs_test::diff_trees(&expected, &actual),
      std::fs::read(root.join("converted.txt")).unwrap(),
      std::fs::read(checkout.path().join("converted.txt")).unwrap(),
    )
  })
  .await;

  assert!(
    !differences.is_empty(),
    "the attrs fixture must diverge from a filtered checkout"
  );
  assert!(
    differences
      .iter()
      .any(|d| d.contains("converted.txt") && d.starts_with("differs:")),
    "the divergence must be the converted file: {differences:?}"
  );
  assert!(
    !raw_bytes.windows(2).any(|w| w == b"\r\n"),
    "the mount serves the stored LF bytes"
  );
  assert!(
    filtered_bytes.windows(2).any(|w| w == b"\r\n"),
    "a real checkout applies eol=crlf"
  );
}

// ---------------------------------------------------------------------------
// POSIX behaviour
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_errno_for_the_wrong_kind_of_object_is_the_posix_one() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  on_fs(move || {
    // A path component that is a file.
    let e = std::fs::metadata(root.join("README.md/inner")).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::ENOTDIR), "file as a directory");

    // Reading a directory as a file.
    let e = std::fs::read(root.join("src")).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::EISDIR), "directory as a file");

    // Listing a file as a directory.
    let e = std::fs::read_dir(root.join("README.md")).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::ENOTDIR), "readdir on a file");

    // A component that does not exist, below one that does.
    let e = std::fs::metadata(root.join("src/nope/deeper")).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_over_long_name_is_refused_by_the_kernel_before_it_reaches_the_daemon() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();
  let before = mount.fs.stats().metadata_requests;

  on_fs(move || {
    let long = "x".repeat(5000);
    let e = std::fs::metadata(root.join(long)).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::ENAMETOOLONG));
  })
  .await;

  assert_eq!(
    mount.fs.stats().metadata_requests,
    before,
    "the kernel enforces NAME_MAX, so no round trip happens"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reads_past_the_end_return_nothing_rather_than_zeros() {
  use std::io::{Read, Seek, SeekFrom};

  let backend = Backend::start("content").await;
  let mount = Mount::new(&backend, "main").await;
  let path = mount.join("no-final-newline.txt");

  on_fs(move || {
    let mut file = std::fs::File::open(&path).unwrap();
    let size = file.metadata().unwrap().len();
    file.seek(SeekFrom::Start(size)).unwrap();
    let mut tail = Vec::new();
    assert_eq!(file.read_to_end(&mut tail).unwrap(), 0);

    // And a read starting well past the end is also empty, not an error.
    file.seek(SeekFrom::Start(size + 4096)).unwrap();
    let mut beyond = [0u8; 16];
    assert_eq!(file.read(&mut beyond).unwrap(), 0);
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_huge_line_and_a_large_blob_read_back_byte_for_byte() {
  let backend = Backend::start("content").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  on_fs(move || {
    let huge = std::fs::read(root.join("huge-line.txt")).unwrap();
    assert_eq!(huge.len(), 4 * 1024 * 1024);
    assert!(huge.iter().all(|b| *b == b'x'));

    let large = std::fs::read(root.join("large-blob.bin")).unwrap();
    assert_eq!(large.len(), 12 * 1024 * 1024);
    assert!(large.iter().all(|b| *b == 0xAB));
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permissions_are_enforced_by_the_kernel_not_only_reported() {
  // `default_permissions` is set at mount time so the kernel checks the mode
  // bits `getattr` reports. Without it, a mode-0 entry would still be openable.
  let backend = Backend::start("modes").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  on_fs(move || {
    let script = std::fs::metadata(root.join("script.sh")).unwrap();
    assert_eq!(script.permissions().mode() & 0o111, 0o111);
    let plain = std::fs::metadata(root.join("plain.txt")).unwrap();
    assert_eq!(plain.permissions().mode() & 0o111, 0);
  })
  .await;
}

// ---------------------------------------------------------------------------
// The `.git` surface and the shim
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stock_git_finds_the_repository_root_through_the_synthesized_surface() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();
  let commit = mount.commit.to_hex();

  on_fs(move || {
    let (ok, out, err) = run_in(&root, "git", &["rev-parse", "--show-toplevel"]);
    assert!(ok, "rev-parse --show-toplevel failed: {err}");
    assert_eq!(out.trim(), root.to_string_lossy());

    let (ok, out, _) = run_in(&root, "git", &["rev-parse", "HEAD"]);
    assert!(ok);
    assert_eq!(out.trim(), commit);

    let (ok, out, _) = run_in(&root, "git", &["rev-parse", "--abbrev-ref", "HEAD"]);
    assert!(ok);
    assert_eq!(out.trim(), "main");

    let (ok, out, _) = run_in(&root, "git", &["symbolic-ref", "--short", "HEAD"]);
    assert!(ok);
    assert_eq!(out.trim(), "main");

    // Ownership: the mount reports the daemon's UID, which is this process's UID
    // here, so Git operates without `safe.directory`. ADR 0005 verified the
    // same-UID case clean; the cross-UID case is M6.1's, and needs
    // `user_allow_other` to test at all.
    let (ok, _, err) = run_in(&root, "git", &["rev-parse", "--git-dir"]);
    assert!(ok, "unexpected ownership refusal: {err}");
    assert!(!err.contains("dubious ownership"));
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stock_git_ls_files_and_diff_are_silently_empty_which_is_why_the_shim_exists() {
  // ADR 0005's central measurement, asserted rather than trusted. If a future Git
  // starts failing loudly here instead, the shim's justification changes and a
  // reviewer should be told by a test rather than by an incident.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  on_fs(move || {
    let (ok, out, _) = run_in(&root, "git", &["ls-files"]);
    assert!(ok, "stock `git ls-files` still exits 0 against the surface");
    assert!(
      out.trim().is_empty(),
      "and still reports no tracked files at all: {out:?}"
    );

    let (ok, out, _) = run_in(&root, "git", &["diff", "--stat"]);
    assert!(ok);
    assert!(out.trim().is_empty());

    // The commands that fail do so visibly, which is the half the design got right.
    let (ok, _, _) = run_in(&root, "git", &["status", "--porcelain"]);
    assert!(!ok, "stock `git status` fails against the surface");
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_shim_answers_the_frozen_grammar() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();
  let commit = mount.commit.to_hex();

  on_fs(move || {
    let git = shim();

    // The two commands stock Git answers wrongly.
    let (ok, out, err) = run_in(&root, git, &["ls-files"]);
    assert!(ok, "{err}");
    let listed: Vec<&str> = out.lines().collect();
    assert!(listed.contains(&"README.md"), "{listed:?}");
    assert!(listed.contains(&"src/main.rs"), "{listed:?}");
    assert!(listed.contains(&"src/lib/util.rs"), "{listed:?}");
    assert!(
      !listed.iter().any(|p| p.starts_with(".git")),
      "the synthesized surface is not tracked content: {listed:?}"
    );

    // `status` and `diff` are covered in `tests/export.rs` instead: from M3 they
    // read the overlay journal through the daemon's control socket, and this
    // harness mounts a filesystem without running a daemon.

    // rev-parse and symbolic-ref, which stock Git also answers -- checked so the
    // shim cannot silently disagree with the thing it shadows.
    let (ok, out, _) = run_in(&root, git, &["rev-parse", "HEAD"]);
    assert!(ok);
    assert_eq!(out.trim(), commit);
    let (ok, out, _) = run_in(&root, git, &["rev-parse", "--show-toplevel"]);
    assert!(ok);
    assert_eq!(out.trim(), root.to_string_lossy());
    let (ok, out, _) = run_in(&root, git, &["rev-parse", "--is-inside-work-tree"]);
    assert!(ok);
    assert_eq!(out.trim(), "true");
    let (ok, out, _) = run_in(&root, git, &["symbolic-ref", "--short", "HEAD"]);
    assert!(ok);
    assert_eq!(out.trim(), "main");

    // show HEAD:<path>
    let (ok, out, err) = run_in(&root, git, &["show", "HEAD:README.md"]);
    assert!(ok, "{err}");
    assert_eq!(out, "# basic\n");

    // log -1
    let (ok, out, err) = run_in(&root, git, &["log", "-1", "--format=%H"]);
    assert!(ok, "{err}");
    assert_eq!(out.trim(), commit);
    let (ok, out, _) = run_in(&root, git, &["log", "-1", "--format=%s"]);
    assert!(ok);
    assert_eq!(out.trim(), "second");
    let (ok, out, _) = run_in(&root, git, &["log", "-1"]);
    assert!(ok);
    assert!(out.contains(&format!("commit {commit}")), "{out}");
    assert!(out.contains("XVFS Fixture"), "{out}");
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_shim_scopes_ls_files_by_pathspec() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  on_fs(move || {
    let git = shim();
    let (ok, out, _) = run_in(&root, git, &["ls-files", "--", "src"]);
    assert!(ok);
    let listed: Vec<&str> = out.lines().collect();
    assert!(listed.iter().all(|p| p.starts_with("src/")), "{listed:?}");
    assert!(listed.contains(&"src/lib/util.rs"));

    let (ok, out, _) = run_in(&root, git, &["ls-files", "--", "*.md"]);
    assert!(ok);
    assert_eq!(out.lines().collect::<Vec<_>>(), vec!["README.md"]);
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_shim_refuses_everything_outside_the_grammar_with_an_actionable_message() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  on_fs(move || {
    let git = shim();
    for args in [
      vec!["checkout", "main"],
      vec!["commit", "-m", "x"],
      vec!["add", "."],
      vec!["log"],
      vec!["log", "-5"],
      vec!["diff", "HEAD~1"],
      vec!["show", "main:README.md"],
      vec!["rev-parse", "HEAD~1"],
      vec!["ls-files", "--", ":(exclude)src"],
      vec!["-C", "/tmp", "status"],
      vec!["status", "--long"],
      vec!["log", "-1", "--format=%zz"],
    ] {
      let (ok, out, err) = run_in(&root, git, &args);
      assert!(!ok, "`git {}` must be refused", args.join(" "));
      assert!(
        out.is_empty(),
        "a refusal must print nothing on stdout, or a caller reads it as an answer: {out:?}"
      );
      assert!(
        err.contains("unsupported") && err.contains("the supported grammar"),
        "the message must name what is supported: {err}"
      );
    }
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_shim_refuses_to_answer_for_an_ordinary_git_repository() {
  // Installed early in `PATH`, the shim is invoked everywhere -- including
  // outside a workspace. Answering for a real repository would replace a working
  // `git` with a crippled one.
  let outside = tempfile::tempdir().unwrap();
  let path = outside.path().to_path_buf();
  on_fs(move || {
    let (ok, _, err) = run_in(&path, shim(), &["status"]);
    assert!(!ok);
    assert!(err.contains("not an XVFS workspace"), "{err}");
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_shim_works_from_a_subdirectory() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();
  let subdirectory = mount.join("src/lib");

  on_fs(move || {
    let (ok, out, err) = run_in(&subdirectory, shim(), &["rev-parse", "--show-toplevel"]);
    assert!(ok, "{err}");
    assert_eq!(out.trim(), root.to_string_lossy());
  })
  .await;
}
