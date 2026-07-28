//! M3.3: status, diff, and export — including the exit criterion that decides
//! whether any of it is real.
//!
//! > *An export applied to the pinned Git commit produces the same tree as the
//! > mounted workspace.*
//!
//! That is a verifier, not an assertion about formatting, so it is implemented as
//! one: check out the pinned commit with **stock Git**, apply the exported patch
//! with **stock `git apply`**, snapshot both trees with the raw-tree materializer
//! M2 built, and compare. Every part of the pipeline that GFS wrote is on one
//! side of that comparison and nothing GFS wrote is on the other.
//!
//! The suite runs against a real daemon, because from M3 the `git` shim reads the
//! journal through the control socket and the CLI does the same.

use std::path::Path;

use gfs_mount::control::{Request, Response};
use gfs_test::mount::{on_fs, Backend, Job};

fn status_of(response: Response) -> gfs_mount::control::StatusReport {
  match response {
    Response::Status(report) => *report,
    other => panic!("expected a status report, got {other:?}"),
  }
}

fn patch_of(response: Response) -> Vec<u8> {
  match response {
    Response::Diff { patch_b64url } => gfs_types::path::b64url_decode(&patch_b64url).unwrap(),
    other => panic!("expected a patch, got {other:?}"),
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clean_workspace_reports_clean() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let report = status_of(job.call(Request::Status).await);
  assert!(report.status.is_clean(), "{:?}", report.status);
  assert_eq!(report.ref_name.as_deref(), Some("refs/heads/main"));
  assert!(patch_of(job.call(Request::Diff).await).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_reports_every_shape_of_change_from_the_journal_alone() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let ws = job.workspace.clone();

  on_fs(move || {
    std::fs::write(ws.join("added.txt"), b"new file\n").unwrap();
    std::fs::write(ws.join("README.md"), b"# basic\nedited\n").unwrap();
    std::fs::remove_file(ws.join("src/lib/util.rs")).unwrap();
    std::fs::rename(ws.join("src/main.rs"), ws.join("src/renamed.rs")).unwrap();
    std::fs::set_permissions(
      ws.join("src/new.rs"),
      std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
  })
  .await;

  let report = status_of(job.call(Request::Status).await);
  let kinds: Vec<(String, gfs_overlay::ChangeKind)> = report
    .status
    .changes
    .iter()
    .map(|c| {
      (
        String::from_utf8_lossy(c.path.as_bytes()).into_owned(),
        c.kind,
      )
    })
    .collect();

  use gfs_overlay::ChangeKind::*;
  assert!(
    kinds.contains(&("added.txt".to_owned(), Added)),
    "{kinds:?}"
  );
  assert!(
    kinds.contains(&("README.md".to_owned(), Modified)),
    "{kinds:?}"
  );
  assert!(
    kinds.contains(&("src/lib/util.rs".to_owned(), Deleted)),
    "{kinds:?}"
  );
  assert!(
    kinds.contains(&("src/renamed.rs".to_owned(), Renamed)),
    "{kinds:?}"
  );
  assert!(
    kinds.contains(&("src/new.rs".to_owned(), ModeChanged)),
    "{kinds:?}"
  );
  assert!(
    report.status.directory_deletions.is_empty(),
    "an ordinary edit set needs no expansion: {:?}",
    report.status.directory_deletions
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_edited_back_to_its_original_bytes_is_not_a_change() {
  // The reason status hashes local content instead of trusting the journal. An
  // agent that writes a file, runs a test, and reverts must not leave a phantom
  // entry in every later `git status`.
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let ws = job.workspace.clone();

  on_fs(move || {
    std::fs::write(ws.join("README.md"), b"scratch\n").unwrap();
    std::fs::write(ws.join("README.md"), b"# basic\n").unwrap();
  })
  .await;

  let report = status_of(job.call(Request::Status).await);
  assert!(
    report.status.is_clean(),
    "an undone edit is not a change: {:?}",
    report.status.changes
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_diff_fetches_only_the_base_blobs_of_changed_paths() {
  // DESIGN.md section 8.5: "diff reads only changed overlay files and their base
  // blobs". `content` has a 12 MiB blob and a 4 MiB one that are not touched.
  let backend = Backend::start("content").await;
  let job = Job::start(&backend, "main").await;
  let ws = job.workspace.clone();

  on_fs(move || {
    std::fs::write(
      ws.join("crlf.txt"),
      b"line one\r\nline two\r\nline three\r\n",
    )
    .unwrap();
  })
  .await;

  let before = job.daemon.inspect().cache.bytes_fetched;
  let patch = patch_of(job.call(Request::Diff).await);
  let after = job.daemon.inspect().cache.bytes_fetched;

  assert!(!patch.is_empty());
  assert!(
    after - before < 1024,
    "a diff of one small file fetched {} bytes",
    after - before
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_export_bundle_is_atomic_checksummed_and_carries_the_base_commit() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let ws = job.workspace.clone();
  on_fs(move || std::fs::write(ws.join("added.txt"), b"new\n").unwrap()).await;

  let tmp = tempfile::tempdir().unwrap();
  let bundle = tmp.path().join("export");
  let Response::Export(report) = job
    .call(Request::Export {
      bundle: bundle.clone(),
    })
    .await
  else {
    panic!("expected an export report");
  };

  assert_eq!(report.changes, 1);
  assert!(bundle.join("manifest.json").is_file());
  assert!(bundle.join("changes.patch").is_file());
  assert!(bundle.join("CHECKSUMS").is_file());
  assert!(
    !bundle.with_extension("tmp").exists(),
    "the staging directory is gone once the bundle is published"
  );

  let manifest: serde_json::Value =
    serde_json::from_slice(&std::fs::read(bundle.join("manifest.json")).unwrap()).unwrap();
  assert_eq!(
    manifest["base_commit"].as_str().unwrap(),
    job.daemon.inspect().commit,
    "an applier has to be able to tell whether the branch moved"
  );
  assert_eq!(manifest["export_format_version"], 1);

  // The checksums describe the bundle that was actually written.
  let checksums = std::fs::read_to_string(bundle.join("CHECKSUMS")).unwrap();
  assert!(checksums.contains("manifest.json"), "{checksums}");
  assert!(checksums.contains("changes.patch"), "{checksums}");
  assert_eq!(
    checksums.lines().count(),
    3,
    "the manifest, the patch, and one content file: {checksums}"
  );

  // Re-exporting over an existing bundle replaces it rather than merging.
  let second = job
    .call(Request::Export {
      bundle: bundle.clone(),
    })
    .await;
  assert!(matches!(second, Response::Export(_)));
}

/// M3's second exit criterion, verified rather than asserted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_export_applied_to_the_pinned_commit_reproduces_the_workspace_tree() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let ws = job.workspace.clone();

  // A change of every shape the patch format has to carry.
  on_fs(move || {
    std::fs::write(ws.join("added.txt"), b"a new file\nwith two lines\n").unwrap();
    std::fs::write(
      ws.join("README.md"),
      b"# basic\n\nan added paragraph\nand another\n",
    )
    .unwrap();
    std::fs::remove_file(ws.join("src/lib/util.rs")).unwrap();
    std::fs::rename(ws.join("src/main.rs"), ws.join("src/entry.rs")).unwrap();
    std::os::unix::fs::symlink("README.md", ws.join("readme-link")).unwrap();
    // No trailing newline, which is where a patch either applies or corrupts the
    // last line.
    std::fs::write(ws.join("src/new.rs"), b"pub fn added() {}\nno newline").unwrap();
  })
  .await;

  let tmp = tempfile::tempdir().unwrap();
  let bundle = tmp.path().join("export");
  let Response::Export(_) = job
    .call(Request::Export {
      bundle: bundle.clone(),
    })
    .await
  else {
    panic!("expected an export report");
  };

  let repo = backend.repo_path.clone();
  let commit = job.daemon.inspect().commit;
  let workspace = job.workspace.clone();
  let checkout = tmp.path().join("checkout");

  let (applied, expected) = on_fs(move || {
    // Stock Git materializes the pinned commit, and stock `git apply` applies the
    // patch. Neither is GFS code, which is the point: the oracle must not share
    // an implementation with the thing under test.
    let hex = commit.split_once(':').map(|(_, h)| h).unwrap().to_owned();
    gfs_test::materialize_raw(&repo, &hex, &checkout).unwrap();
    let patch = bundle.join("changes.patch");
    // Hermetic, and that is load-bearing rather than tidy: a developer with
    // `core.autocrlf = true` in their global config makes `git apply` rewrite
    // every line ending on the way in, and the verifier then reports the export
    // as wrong when the only thing wrong is the oracle's environment.
    let output = std::process::Command::new("git")
      .current_dir(&checkout)
      .env("GIT_CONFIG_GLOBAL", "/dev/null")
      .env("GIT_CONFIG_SYSTEM", "/dev/null")
      .args([
        "-c",
        "core.autocrlf=false",
        "-c",
        "core.eol=lf",
        "apply",
        "--verbose",
        "--whitespace=nowarn",
      ])
      .arg(&patch)
      .output()
      .unwrap();
    assert!(
      output.status.success(),
      "git apply failed: {}\n--- patch ---\n{}",
      String::from_utf8_lossy(&output.stderr),
      String::from_utf8_lossy(&std::fs::read(&patch).unwrap())
    );
    (
      gfs_test::snapshot_tree(&checkout).unwrap(),
      gfs_test::snapshot_tree(&workspace).unwrap(),
    )
  })
  .await;

  // The mount carries a `.git` surface the checkout does not; DESIGN.md section
  // 8.6 puts it outside change tracking, so it is outside the comparison too.
  let differences = gfs_test::diff_trees(&comparable(applied), &comparable(expected));
  assert!(
    differences.is_empty(),
    "the applied export and the workspace differ:\n  {}",
    differences.join("\n  ")
  );
}

/// The `.git` surface, and directories that are empty on one side only.
///
/// Git does not record directories, so `git apply` removes one that its last
/// file left, and a filesystem does not. That is a real and permanent difference
/// between an applied patch and a workspace, and it is about Git's data model
/// rather than about the export -- so it is excluded from the comparison rather
/// than papered over.
fn comparable(tree: gfs_test::TreeSnapshot) -> gfs_test::TreeSnapshot {
  let directories: Vec<Vec<u8>> = tree
    .iter()
    .filter(|(_, entry)| matches!(entry, gfs_test::EntrySnapshot::Directory))
    .map(|(path, _)| path.clone())
    .collect();
  let empty: std::collections::HashSet<Vec<u8>> = directories
    .into_iter()
    .filter(|dir| {
      !tree.keys().any(|path| {
        path.len() > dir.len() && path.starts_with(dir.as_slice()) && path[dir.len()] == b'/'
      })
    })
    .collect();
  tree
    .into_iter()
    .filter(|(path, _)| !path.starts_with(b".git") && !empty.contains(path))
    .collect()
}

/// The cost model, measured rather than asserted in the abstract.
///
/// Run with `--nocapture`; `docs/reports/m3-completion.md` quotes the numbers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_costs_no_base_metadata_and_no_blob_bytes() {
  // `bigdir` is 5002 entries in one directory. Stock `git status` against a
  // partial clone stats every index entry (ADR 0005 measured 94 850 on the Linux
  // kernel); this must stat none of them.
  let backend = Backend::start("bigdir").await;
  let job = Job::start(&backend, "main").await;
  let ws = job.workspace.clone();

  on_fs(move || {
    std::fs::write(ws.join("many/edited.txt"), b"edited\n").unwrap();
    std::fs::remove_file(ws.join("many/pager.h")).unwrap();
  })
  .await;

  let before = job.daemon.inspect();
  let report = status_of(job.call(Request::Status).await);
  let after = job.daemon.inspect();

  let metadata = after.stats.metadata_requests - before.stats.metadata_requests;
  let pages = after.stats.directory_pages - before.stats.directory_pages;
  let bytes = after.cache.bytes_fetched - before.cache.bytes_fetched;
  println!(
    "M3 status over a 5002-entry directory with 2 changes: \
     {metadata} metadata requests, {pages} directory pages, {bytes} blob bytes"
  );

  assert_eq!(report.status.changes.len(), 2);
  assert_eq!(metadata, 0, "status must not stat the base");
  assert_eq!(pages, 0, "status must not list the base");
  assert_eq!(bytes, 0, "status must not fetch content");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_shim_reports_the_overlay_rather_than_a_clean_tree() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let ws = job.workspace.clone();

  let (porcelain, long, diff, names) = on_fs(move || {
    std::fs::write(ws.join("added.txt"), b"new\n").unwrap();
    std::fs::write(ws.join("README.md"), b"# edited\n").unwrap();
    let git = shim();
    (
      run_in(&ws, git, &["status", "--porcelain"]),
      run_in(&ws, git, &["status"]),
      run_in(&ws, git, &["diff"]),
      run_in(&ws, git, &["diff", "--name-only"]),
    )
  })
  .await;

  assert!(porcelain.0, "{}", porcelain.2);
  assert!(porcelain.1.contains("A  added.txt"), "{}", porcelain.1);
  assert!(porcelain.1.contains("M  README.md"), "{}", porcelain.1);

  assert!(long.1.contains("On branch main"), "{}", long.1);
  assert!(long.1.contains("new file:   added.txt"), "{}", long.1);

  assert!(diff.0, "{}", diff.2);
  assert!(
    diff.1.contains("diff --git a/README.md b/README.md"),
    "{}",
    diff.1
  );
  assert!(diff.1.contains("+# edited"), "{}", diff.1);

  let listed: Vec<&str> = names.1.lines().collect();
  assert!(listed.contains(&"added.txt"), "{listed:?}");
  assert!(listed.contains(&"README.md"), "{listed:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_shim_lists_the_merged_workspace_not_the_pinned_commit() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;
  let ws = job.workspace.clone();

  let listed = on_fs(move || {
    std::fs::write(ws.join("added.txt"), b"new\n").unwrap();
    std::fs::remove_file(ws.join("src/lib/util.rs")).unwrap();
    run_in(&ws, shim(), &["ls-files"]).1
  })
  .await;

  let listed: Vec<&str> = listed.lines().collect();
  assert!(listed.contains(&"added.txt"), "{listed:?}");
  assert!(!listed.contains(&"src/lib/util.rs"), "{listed:?}");
  assert!(listed.contains(&"README.md"), "{listed:?}");
}

// ---------------------------------------------------------------------------

fn shim() -> &'static Path {
  Path::new(env!("CARGO_BIN_EXE_gfs-git-shim"))
}

fn run_in(dir: &Path, program: &Path, args: &[&str]) -> (bool, String, String) {
  let output = std::process::Command::new(program)
    .current_dir(dir)
    .args(args)
    .output()
    .expect("running the shim");
  (
    output.status.success(),
    String::from_utf8_lossy(&output.stdout).into_owned(),
    String::from_utf8_lossy(&output.stderr).into_owned(),
  )
}
