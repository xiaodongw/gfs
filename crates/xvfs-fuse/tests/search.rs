//! M4.5: search over the merged workspace.
//!
//! Every case here edits a *real mount* through ordinary syscalls and then
//! searches it, because the property under test is that the two halves agree
//! about one workspace. A test that drove the overlay directly would prove the
//! merge logic and nothing about whether the filesystem and the search see the
//! same tree.

mod harness;

use std::io::Write;

use harness::{Backend, Mount};
use xvfs_fuse::search::{search, SearchRequest};
use xvfs_search::query::{ExecutionStatus, SearchOutcome};
use xvfs_types::ObjectId;

async fn prepared(backend: &Backend, commit: &ObjectId) {
  let outcome = backend
    .server
    .search
    .prepare(&backend.repo_id, commit, true)
    .await
    .unwrap();
  assert!(
    matches!(outcome, xvfs_server::PrepareOutcome::Ready(_)),
    "the snapshot did not prepare: {outcome:?}"
  );
}

fn request(pattern: &str) -> SearchRequest {
  SearchRequest {
    pattern: pattern.to_owned(),
    literal: true,
    case_insensitive: false,
    scope: Vec::new(),
    include_globs: Vec::new(),
    exclude_globs: Vec::new(),
    context_before: 0,
    context_after: 0,
    max_results: 0,
    max_line_bytes: 0,
    search_ignored: false,
  }
}

async fn run(mount: &Mount, request: &SearchRequest) -> (SearchOutcome, usize) {
  search(mount.fs.client(), &mount.overlay, request)
    .await
    .unwrap()
}

fn paths(outcome: &SearchOutcome) -> Vec<Vec<u8>> {
  match outcome {
    SearchOutcome::Completed(result) => result.matches.iter().map(|m| m.path.clone()).collect(),
    SearchOutcome::FailedBeforeCompletion(reason) => panic!("the search failed: {reason}"),
  }
}

fn completion(outcome: &SearchOutcome) -> &xvfs_search::Completion {
  match outcome {
    SearchOutcome::Completed(result) => &result.completion,
    SearchOutcome::FailedBeforeCompletion(reason) => panic!("the search failed: {reason}"),
  }
}

// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clean_workspace_searches_the_pinned_commit_and_downloads_nothing() {
  // The M4.6 zero-hydration criterion, asserted here because this is where the
  // client half could break it: an implementation that read base files to make
  // its own decisions would fetch blobs and nothing else would notice.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  prepared(&backend, &mount.commit).await;

  let before = mount.fs.cache_stats();
  let (outcome, local) = run(&mount, &request("println")).await;

  assert!(!paths(&outcome).is_empty());
  assert_eq!(local, 0);
  let after = mount.fs.cache_stats();
  assert_eq!(
    after.bytes_fetched, before.bytes_fetched,
    "a server search must not hydrate the client cache"
  );
  assert_eq!(
    completion(&outcome).execution_status,
    ExecutionStatus::Complete
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_created_file_is_searched_without_contacting_the_server_for_content() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  prepared(&backend, &mount.commit).await;

  std::fs::write(mount.join("src/added.rs"), b"fn brandnewsymbol() {}\n").unwrap();

  let (outcome, local) = run(&mount, &request("brandnewsymbol")).await;
  assert_eq!(paths(&outcome), vec![b"src/added.rs".to_vec()]);
  assert_eq!(local, 1, "the match came from local content");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_edited_file_reports_the_workspaces_content_not_the_commits() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  prepared(&backend, &mount.commit).await;

  // `src/main.rs` contains `println!("bye")` in the pinned commit.
  let before = run(&mount, &request("println")).await.0;
  assert!(paths(&before).iter().any(|p| p == b"src/main.rs"));

  std::fs::write(
    mount.join("src/main.rs"),
    b"fn main() { replacedcontent(); }\n",
  )
  .unwrap();

  let after = run(&mount, &request("println")).await.0;
  assert!(
    !paths(&after).iter().any(|p| p == b"src/main.rs"),
    "the base's version of an edited file must not be reported"
  );

  let new = run(&mount, &request("replacedcontent")).await.0;
  assert_eq!(paths(&new), vec![b"src/main.rs".to_vec()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deleted_file_disappears_from_the_results() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  prepared(&backend, &mount.commit).await;

  let before = run(&mount, &request("println")).await.0;
  assert!(paths(&before).iter().any(|p| p == b"src/main.rs"));

  std::fs::remove_file(mount.join("src/main.rs")).unwrap();

  let after = run(&mount, &request("println")).await.0;
  assert!(
    !paths(&after).iter().any(|p| p == b"src/main.rs"),
    "a deleted file must produce no results"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deleted_directory_hides_everything_under_it() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  prepared(&backend, &mount.commit).await;

  std::fs::remove_file(mount.join("src/lib/util.rs")).unwrap();
  std::fs::remove_dir(mount.join("src/lib")).unwrap();

  let outcome = run(&mount, &request("util")).await.0;
  assert!(
    !paths(&outcome).iter().any(|p| p.starts_with(b"src/lib/")),
    "a removed subtree must not still be searched"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_renamed_file_reports_its_matches_at_the_new_path_and_fetches_nothing() {
  // The property M3 measured for `mv` -- zero transferred bytes -- extended to
  // search. The bytes did not change, so the server's result for the old path is
  // re-pathed rather than the content being fetched and rescanned.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  prepared(&backend, &mount.commit).await;

  let before = mount.fs.cache_stats();
  std::fs::rename(mount.join("src/main.rs"), mount.join("src/renamed.rs")).unwrap();

  let outcome = run(&mount, &request("println")).await.0;
  let found = paths(&outcome);
  assert!(
    found.iter().any(|p| p == b"src/renamed.rs"),
    "matches must follow the file, got {found:?}"
  );
  assert!(!found.iter().any(|p| p == b"src/main.rs"));
  assert_eq!(
    mount.fs.cache_stats().bytes_fetched,
    before.bytes_fetched,
    "a rename changes no bytes, so search must fetch none"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mode_change_alone_changes_nothing_about_the_results() {
  use std::os::unix::fs::PermissionsExt;

  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  prepared(&backend, &mount.commit).await;

  let before = run(&mount, &request("println")).await.0;
  let downloaded = mount.fs.cache_stats().bytes_fetched;

  std::fs::set_permissions(
    mount.join("src/main.rs"),
    std::fs::Permissions::from_mode(0o755),
  )
  .unwrap();

  let after = run(&mount, &request("println")).await.0;
  assert_eq!(paths(&before), paths(&after));
  assert_eq!(mount.fs.cache_stats().bytes_fetched, downloaded);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_replacing_a_directory_hides_the_directorys_children() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  prepared(&backend, &mount.commit).await;

  std::fs::remove_file(mount.join("src/lib/util.rs")).unwrap();
  std::fs::remove_dir(mount.join("src/lib")).unwrap();
  std::fs::write(mount.join("src/lib"), b"now a file\n").unwrap();

  let outcome = run(&mount, &request("util")).await.0;
  assert!(!paths(&outcome).iter().any(|p| p.starts_with(b"src/lib/")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ignored_files_are_skipped_unless_asked_for() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  prepared(&backend, &mount.commit).await;

  std::fs::write(mount.join(".gitignore"), b"build/\n").unwrap();
  std::fs::create_dir(mount.join("build")).unwrap();
  std::fs::write(mount.join("build/out.rs"), b"generatedsymbol\n").unwrap();

  let default = run(&mount, &request("generatedsymbol")).await.0;
  assert!(
    paths(&default).is_empty(),
    "an ignored created file must not be searched by default"
  );

  let explicit = run(
    &mount,
    &SearchRequest {
      search_ignored: true,
      ..request("generatedsymbol")
    },
  )
  .await
  .0;
  assert_eq!(paths(&explicit), vec![b"build/out.rs".to_vec()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_edit_to_a_tracked_file_survives_a_broad_ignore_rule() {
  // Git's rule, and the one that matters most here: an agent's edit must not
  // vanish because a `.gitignore` pattern happens to cover its directory.
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  prepared(&backend, &mount.commit).await;

  std::fs::write(mount.join(".gitignore"), b"src/\n").unwrap();
  std::fs::write(mount.join("src/main.rs"), b"fn main() { trackededit(); }\n").unwrap();

  let outcome = run(&mount, &request("trackededit")).await.0;
  assert_eq!(paths(&outcome), vec![b"src/main.rs".to_vec()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_binary_file_is_a_reported_coverage_gap() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  prepared(&backend, &mount.commit).await;

  let mut file = std::fs::File::create(mount.join("blob.bin")).unwrap();
  file.write_all(b"needle\0binary").unwrap();
  drop(file);

  let outcome = run(&mount, &request("needle")).await.0;
  let completion = completion(&outcome);
  assert_eq!(completion.coverage.excluded["binary"], 1);
  assert_eq!(
    xvfs_search::exit_code(&outcome, true),
    4,
    "--require-exhaustive must fail on a local coverage gap too"
  );
  assert_eq!(xvfs_search::exit_code(&outcome, false), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn results_from_both_halves_are_merged_into_one_path_order() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  prepared(&backend, &mount.commit).await;

  // `src/main.rs` and `src/new.rs` carry `pub fn`/`fn` in the pinned commit;
  // add a local file that sorts between them.
  std::fs::write(mount.join("src/mid.rs"), b"mergedneedle\n").unwrap();
  std::fs::write(mount.join("src/zzz.rs"), b"mergedneedle\n").unwrap();
  std::fs::write(mount.join("src/aaa.rs"), b"mergedneedle\n").unwrap();

  let outcome = run(&mount, &request("mergedneedle")).await.0;
  assert_eq!(
    paths(&outcome),
    vec![
      b"src/aaa.rs".to_vec(),
      b"src/mid.rs".to_vec(),
      b"src/zzz.rs".to_vec()
    ]
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_scope_applies_to_both_halves() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  prepared(&backend, &mount.commit).await;

  std::fs::write(mount.join("src/in_scope.rs"), b"scopedneedle\n").unwrap();
  // At the root, not in `docs/`: `basic`'s second commit removed the only file
  // under `docs/`, and Git does not keep an empty directory.
  std::fs::write(mount.join("out_of_scope.md"), b"scopedneedle\n").unwrap();

  let outcome = run(
    &mount,
    &SearchRequest {
      scope: b"src".to_vec(),
      ..request("scopedneedle")
    },
  )
  .await
  .0;
  assert_eq!(paths(&outcome), vec![b"src/in_scope.rs".to_vec()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_search_names_the_pinned_commit_it_answered_for() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  prepared(&backend, &mount.commit).await;

  let outcome = run(&mount, &request("println")).await.0;
  assert_eq!(completion(&outcome).commit, mount.commit.to_qualified());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn searching_before_the_snapshot_is_prepared_is_an_error_not_an_empty_answer() {
  let backend = Backend::start("basic").await;
  let mount = Mount::new(&backend, "main").await;
  // Deliberately not prepared.

  let err = search(mount.fs.client(), &mount.overlay, &request("println"))
    .await
    .unwrap_err();
  assert_eq!(err.code, xvfs_types::ErrorCode::SnapshotBuilding);
}
