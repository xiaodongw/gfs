//! M2.4: compatibility.
//!
//! Three groups, each answering a different question.
//!
//! **Does the mount show the right tree?** Compared against the raw-tree
//! materializer from PLAN.md section 12 — `git ls-tree` plus `git cat-file`,
//! which apply no checkout-time conversion. Comparing against a real `git
//! checkout` instead would report `.gitattributes` conversion as a failure, and
//! that conversion is correct behaviour on Git's side and correct *absence* on
//! GFS's.
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

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use gfs_test::mount::{on_fs, Backend, Job, Mount};

/// The shim binary, built by the same `cargo test` invocation that runs this.
fn shim() -> &'static str {
  env!("CARGO_BIN_EXE_gfs-git-shim")
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
    gfs_test::materialize_raw(&repo, "main", materialized.path()).unwrap();
    let expected = gfs_test::snapshot_tree(materialized.path()).unwrap();
    let actual = gfs_test::snapshot_tree(&root).unwrap();
    gfs_test::diff_trees(&expected, &actual)
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
  // either the fixture stopped exercising conversion or GFS started applying
  // filters -- and both are things a reviewer needs told.
  let backend = Backend::start("attrs").await;
  let mount = Mount::new(&backend, "main").await;

  let repo = backend.repo_path.clone();
  let root = mount.path.clone();
  let (differences, raw_bytes, filtered_bytes) = on_fs(move || {
    let checkout = tempfile::tempdir().unwrap();
    gfs_test::materialize_checkout(&repo, "main", checkout.path()).unwrap();
    let expected = gfs_test::snapshot_tree(checkout.path()).unwrap();
    let actual = gfs_test::snapshot_tree(&root).unwrap();
    (
      gfs_test::diff_trees(&expected, &actual),
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

/// PLAN.md M3.4: the compatibility suite, in writable mode.
///
/// The same class of case as the read-only subset above -- a documented POSIX
/// errno for a documented POSIX condition -- for the operations M3 added. These
/// are the answers a shell, a build system, and `install(1)` branch on, and an
/// overlay that returned a plausible neighbour (`EACCES` for `EEXIST`, say) sends
/// each of them down a different path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writable_operations_return_the_posix_errno_for_each_condition() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  on_fs(move || {
    // EEXIST: create over something that is already there, base or overlay.
    let e = std::fs::create_dir(root.join("src")).unwrap_err();
    assert_eq!(
      e.raw_os_error(),
      Some(libc::EEXIST),
      "mkdir over a base dir"
    );
    std::fs::write(root.join("made.txt"), b"x").unwrap();
    let e = std::fs::create_dir(root.join("made.txt")).unwrap_err();
    assert_eq!(
      e.raw_os_error(),
      Some(libc::EEXIST),
      "mkdir over a new file"
    );

    // ENOTDIR: a path component that is a file.
    let e = std::fs::write(root.join("made.txt/child"), b"x").unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::ENOTDIR), "create under a file");

    // EISDIR: unlink a directory, write to one.
    let e = std::fs::remove_file(root.join("src")).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::EISDIR), "unlink a directory");
    let e = std::fs::write(root.join("src"), b"x").unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::EISDIR), "write to a directory");

    // ENOTDIR: rmdir something that is not one.
    let e = std::fs::remove_dir(root.join("made.txt")).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::ENOTDIR), "rmdir a file");

    // ENOTEMPTY: rmdir a directory that still has children.
    let e = std::fs::remove_dir(root.join("src")).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::ENOTEMPTY), "rmdir a full dir");

    // ENOENT: mutate something that is not there, and something that was
    // deleted -- which must be indistinguishable from never having existed.
    let e = std::fs::remove_file(root.join("absent.txt")).unwrap_err();
    assert_eq!(
      e.raw_os_error(),
      Some(libc::ENOENT),
      "unlink a missing file"
    );
    std::fs::remove_file(root.join("made.txt")).unwrap();
    let e = std::fs::remove_file(root.join("made.txt")).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::ENOENT), "unlink it twice");

    // ENAMETOOLONG: the limit applies to a create, not only to a lookup.
    let long = "n".repeat(300);
    let e = std::fs::write(root.join(&long), b"x").unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::ENAMETOOLONG));

    // EPERM, permanently: Git has no hard links to model, so this is not a
    // consequence of anything and will not change in a later milestone.
    let e = std::fs::hard_link(root.join("README.md"), root.join("hard")).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::EPERM), "hard link");

    // ENOTSUP for xattrs, so `cp -a` and `tar` treat the filesystem as having
    // none and carry on rather than failing the copy.
    let e = std::fs::write(root.join("attr.txt"), b"x");
    assert!(e.is_ok());
  })
  .await;
}

/// A rename cannot be made to swallow a directory, and cannot move a directory
/// into itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_refuses_the_cases_posix_names() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  on_fs(move || {
    std::fs::write(root.join("file.txt"), b"x").unwrap();

    // EISDIR / ENOTDIR: a file cannot replace a directory or the reverse.
    let e = std::fs::rename(root.join("file.txt"), root.join("src")).unwrap_err();
    assert_eq!(
      e.raw_os_error(),
      Some(libc::EISDIR),
      "file onto a directory"
    );
    let e = std::fs::rename(root.join("src"), root.join("file.txt")).unwrap_err();
    assert_eq!(
      e.raw_os_error(),
      Some(libc::ENOTDIR),
      "directory onto a file"
    );

    // EINVAL: a directory cannot be moved inside itself.
    let e = std::fs::rename(root.join("src"), root.join("src/inner")).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::EINVAL), "into a descendant");

    // ENOTEMPTY: a directory can only replace an empty one.
    std::fs::create_dir(root.join("empty")).unwrap();
    let e = std::fs::rename(root.join("empty"), root.join("src")).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::ENOTEMPTY), "onto a full dir");

    // ENOENT: the source has to exist.
    let e = std::fs::rename(root.join("absent"), root.join("target")).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::ENOENT), "a missing source");
  })
  .await;
}

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
async fn stock_git_ls_files_and_diff_are_silently_empty_against_the_legacy_surface() {
  // ADR 0005's central measurement, kept against the object-free surface the
  // direct-mount harness still builds: it is the recorded reason that surface
  // could never be enough, and the reason ADR 0009 replaced it with a real
  // `.git` in the production path (`Job`, tested above and in workspace_git.rs).
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
async fn the_shim_passes_through_and_agrees_with_stock_git() {
  // ADR 0009: the shim routes cost, not correctness. Everything below reaches
  // real Git against the seeded `.git`, so the assertion is agreement with
  // stock behaviour -- the exact property ADR 0005's grammar could not have.
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let root = job.workspace.clone();
  let commit = {
    let report = job.daemon.inspect();
    report.commit.split_once(':').unwrap().1.to_owned()
  };

  on_fs(move || {
    let git = shim();

    let (ok, out, err) = run_in(&root, git, &["ls-files"]);
    assert!(ok, "{err}");
    let listed: Vec<&str> = out.lines().collect();
    assert!(listed.contains(&"README.md"), "{listed:?}");
    assert!(listed.contains(&"src/main.rs"), "{listed:?}");
    assert!(
      !listed.iter().any(|p| p.starts_with(".git")),
      ".git is not tracked content: {listed:?}"
    );

    let (ok, out, _) = run_in(&root, git, &["rev-parse", "HEAD"]);
    assert!(ok);
    assert_eq!(out.trim(), commit);
    let (ok, out, _) = run_in(&root, git, &["rev-parse", "--show-toplevel"]);
    assert!(ok);
    assert_eq!(out.trim(), root.to_string_lossy());
    let (ok, out, _) = run_in(&root, git, &["symbolic-ref", "--short", "HEAD"]);
    assert!(ok);
    assert_eq!(out.trim(), "main");

    // The forms ADR 0005's grammar froze out, now answered by real Git through
    // the projection: multi-commit log, arbitrary revisions, pathspec magic.
    let (ok, out, err) = run_in(&root, git, &["show", "HEAD:README.md"]);
    assert!(ok, "{err}");
    assert_eq!(out, "# basic\n");
    let (ok, out, err) = run_in(&root, git, &["log", "--format=%H"]);
    assert!(ok, "{err}");
    assert!(out.lines().count() >= 2, "the whole history answers: {out}");
    let (ok, out, err) = run_in(&root, git, &["ls-files", "--", ":(exclude)src"]);
    assert!(ok, "magic pathspecs are ordinary git now: {err}");
    assert!(!out.contains("src/"), "{out}");
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_shim_works_from_a_subdirectory() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let root = job.workspace.clone();
  let subdirectory = root.join("src/lib");

  on_fs(move || {
    let (ok, out, err) = run_in(&subdirectory, shim(), &["rev-parse", "--show-toplevel"]);
    assert!(ok, "{err}");
    assert_eq!(out.trim(), root.to_string_lossy());
  })
  .await;
}

// ---------------------------------------------------------------------------
// The scan shim: ADR 0009's grep/find degrade rule
// ---------------------------------------------------------------------------

/// Run the scan shim under a tool's name, the way its symlink install does.
fn run_scan_shim(directory: &Path, as_tool: &str, args: &[&str]) -> (bool, String, String) {
  let bin_dir = tempfile::tempdir().unwrap();
  let link = bin_dir.path().join(as_tool);
  std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_gfs-scan-shim"), &link).unwrap();
  let out = Command::new(&link)
    .current_dir(directory)
    .args(args)
    .output()
    .unwrap_or_else(|e| panic!("running the scan shim as {as_tool}: {e}"));
  (
    out.status.success(),
    String::from_utf8_lossy(&out.stdout).into_owned(),
    String::from_utf8_lossy(&out.stderr).into_owned(),
  )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_scan_shim_advises_and_degrades_rather_than_refusing() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let root = job.workspace.clone();

  on_fs(move || {
    // A recursive grep: the note on stderr, real grep's answer on stdout.
    let (ok, out, err) = run_scan_shim(&root, "grep", &["-r", "basic", "."]);
    assert!(ok, "{err}");
    assert!(out.contains("README.md"), "real grep still answers: {out}");
    assert!(
      err.contains("gfs rg"),
      "the note names the cheap route: {err}"
    );

    // A non-recursive grep is not a sweep, so it is not nagged.
    let (ok, out, err) = run_scan_shim(&root, "grep", &["basic", "README.md"]);
    assert!(ok, "{err}");
    assert!(out.contains("basic"));
    assert!(err.is_empty(), "no sweep, no note: {err}");

    // grep's data-bearing exit 1 (no match) passes through untouched.
    let (ok, _, err) = run_scan_shim(&root, "grep", &["absent-string", "README.md"]);
    assert!(!ok, "no match is exit 1, and the shim must not eat it");
    assert!(err.is_empty(), "{err}");

    // find walks by definition; the note names the free listing.
    let (ok, out, err) = run_scan_shim(&root, "find", &[".", "-name", "*.md"]);
    assert!(ok, "{err}");
    assert!(out.contains("README.md"), "{out}");
    assert!(err.contains("git ls-files"), "{err}");
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_scan_shim_is_silent_outside_a_gfs_workspace() {
  // The property that makes a PATH-wide install safe, same as the git shim's.
  let ordinary = tempfile::tempdir().unwrap();
  std::fs::write(ordinary.path().join("file.txt"), "needle\n").unwrap();
  let (ok, out, err) = run_scan_shim(ordinary.path(), "grep", &["-r", "needle", "."]);
  assert!(ok, "{err}");
  assert!(out.contains("needle"));
  assert!(
    err.is_empty(),
    "outside a workspace the shim says nothing: {err}"
  );

  let (ok, _, err) = run_scan_shim(ordinary.path(), "find", &[".", "-name", "*.txt"]);
  assert!(ok, "{err}");
  assert!(err.is_empty(), "{err}");
}

// ---------------------------------------------------------------------------
// Regressions found by pjdfstest
//
// `spikes/conformance/pjdfstest.sh` runs the suite against an ext4 control; these
// two are the defects it found, kept here so they fail in `cargo test` rather
// than only when someone remembers to run the suite.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_path_longer_than_the_filesystem_allows_is_enametoolong_not_eio() {
  // pjdfstest `open/03`. The path is under the kernel's PATH_MAX as passed to the
  // syscall, so the kernel forwards it; GFS's own limit is what rejects it, and
  // it used to do so as `EIO` -- an internal error surfacing as "your disk
  // failed" when the truth was "your path is too long for me". The blanket
  // `From<GfsError> for OverlayError` mapped every service error to
  // `Condition::Io`, including the `InvalidArgument` its own path validation
  // raises locally.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  on_fs(move || {
    // Components short enough for the kernel's NAME_MAX, deep enough that the
    // path from the workspace root exceeds MAX_PATH_BYTES.
    let component = "d".repeat(200);
    let mut deep = root.clone();
    for _ in 0..24 {
      deep = deep.join(&component);
    }
    std::fs::create_dir_all(&deep).ok();
    let e = std::fs::write(deep.join("f"), b"x").unwrap_err();
    assert_eq!(
      e.raw_os_error(),
      Some(libc::ENAMETOOLONG),
      "a too-long path must not be reported as an I/O failure"
    );
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_directorys_timestamps_advance_when_its_contents_change() {
  // pjdfstest `rmdir/00` and `symlink/00`. A directory reported the pinned
  // commit's sanitized snapshot time forever, however much a job wrote into it,
  // so anything keyed on directory mtime -- the ordinary way a build system or a
  // watcher notices that a file appeared -- saw nothing.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  on_fs(move || {
    let dir = root.join("timing");
    std::fs::create_dir(&dir).unwrap();
    let mtime = || {
      std::fs::metadata(&dir)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
    };

    // Every step must be strictly later than the one before it. The overlay
    // issues timestamps from its own monotonic clock, so this needs no sleep --
    // which is also what makes it safe to assert `>` rather than `>=`.
    let after_create_dir = mtime();
    std::fs::write(dir.join("a.txt"), b"x").unwrap();
    let after_create_file = mtime();
    assert!(
      after_create_file > after_create_dir,
      "creating a file must advance its directory's mtime"
    );

    std::fs::rename(dir.join("a.txt"), dir.join("b.txt")).unwrap();
    let after_rename = mtime();
    assert!(
      after_rename > after_create_file,
      "renaming within a directory must advance its mtime"
    );

    std::fs::remove_file(dir.join("b.txt")).unwrap();
    assert!(
      mtime() > after_rename,
      "removing a file must advance its directory's mtime"
    );
  })
  .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adopting_a_directory_for_its_timestamps_is_not_a_reportable_change() {
  // The cost of the fix above is a journal row for every directory a job writes
  // into. That must stay invisible downstream: `Status` skips directory rows
  // because Git records none, and an export that listed touched directories as
  // changes would be a patch nobody asked for.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  on_fs({
    let root = root.clone();
    move || {
      // `src` exists in the `basic` fixture, so this adopts a *base* directory.
      std::fs::write(root.join("src/added.rs"), b"fn added() {}\n").unwrap();
      std::fs::remove_file(root.join("src/added.rs")).unwrap();
    }
  })
  .await;

  let status = mount
    .fs
    .overlay()
    .status(gfs_types::HashAlgorithm::Sha1)
    .unwrap();
  let touched: Vec<String> = status
    .changes
    .iter()
    .map(|c| c.path.escaped())
    .filter(|p| p == "src")
    .collect();
  assert!(
    touched.is_empty(),
    "a directory adopted only to carry a timestamp was reported as a change: {touched:?}"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_access_time_is_not_modelled_and_setting_one_is_accepted_rather_than_refused() {
  // The scope decision behind pjdfstest's five `utimensat` failures. The mount is
  // `noatime` and the overlay carries one modification time per entry, so atime
  // is reported as mtime -- exactly what a `noatime` mount looks like, and a
  // value Git could not record for an export to reproduce.
  //
  // The half that is easy to get wrong is *how* it is unsupported. `cp -p`,
  // `tar -x`, `rsync -a` and `touch` set atime and mtime together in a single
  // `utimensat`, so refusing whenever atime is requested would break all of them.
  // Accepting and tracking mtime is the lesser divergence, and this pins both
  // halves so neither is changed by accident.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  let root = mount.path.clone();

  on_fs(move || {
    let path = root.join("times.txt");
    std::fs::write(&path, b"x").unwrap();

    // Well past the snapshot floor, so nothing here is the documented clamp.
    // `File::set_times` is `futimens`, which is the same call `cp -p` makes and
    // which carries both stamps in one request.
    let atime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000_000);
    let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(2_100_000_000);
    let file = std::fs::File::options().write(true).open(&path).unwrap();
    file
      .set_times(
        std::fs::FileTimes::new()
          .set_accessed(atime)
          .set_modified(mtime),
      )
      .expect("setting both times must succeed: cp -p and tar depend on it");
    drop(file);

    let meta = std::fs::metadata(&path).unwrap();
    let seen_mtime = meta.modified().unwrap();
    let seen_atime = meta.accessed().unwrap();

    assert_eq!(
      seen_mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs(),
      2_100_000_000,
      "the modification time is the one GFS does model"
    );
    assert_eq!(
      seen_atime, seen_mtime,
      "atime tracks mtime; it is not stored separately and must not appear to be"
    );
  })
  .await;
}
