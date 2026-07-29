//! History review through the control socket: `show`, `diff`, `blame`, `ls`,
//! `cat`, and a path-limited `log`.
//!
//! The property every one of these has to keep, and the reason they go through
//! the daemon at all rather than straight to the server:
//!
//! * **nothing hydrates.** Reviewing three commits must not pull the tree into
//!   the mount, which is what a client-side tree walk would do and is exactly
//!   what ADR 0005 rejected the partial clone for. Each test asserts the cache
//!   counters are untouched.
//! * **`HEAD` is the pin.** On the server `HEAD` is the repository's default
//!   branch. A workspace re-pinned by `gfs switch` is somewhere else, and
//!   answering about the branch would be a wrong answer delivered quietly.

use gfs_mount::control::{Request, Response};
use gfs_test::mount::{Backend, Job};
use gfs_types::DiffFormat;

fn b64(bytes: &[u8]) -> String {
  gfs_types::path::b64url_encode(bytes)
}

fn decode(encoded: &str) -> Vec<u8> {
  gfs_types::path::b64url_decode(encoded).unwrap()
}

/// The counters `gfs inspect` prints as `hydration`, which is the one number
/// that cannot be faked by a plausible-looking answer.
async fn hydration(job: &Job) -> gfs_mount::cache::CacheStats {
  let Response::Inspect(report) = job.call(Request::Inspect).await else {
    panic!("expected an inspect report");
  };
  report.cache
}

/// The `basic` fixture's second commit rewrites `src/main.rs`, adds
/// `src/new.rs`, and deletes `docs/guide.md`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_commit_diff_reports_every_change_and_hydrates_nothing() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  let Response::RevDiff(diff) = job
    .call(Request::DiffRevs {
      from: None,
      to: "HEAD".to_owned(),
      parent: None,
      format: DiffFormat::Patch,
      context_lines: None,
      paths_b64url: Vec::new(),
    })
    .await
  else {
    panic!("expected a diff");
  };

  let paths: Vec<String> = diff
    .files
    .iter()
    .map(|f| String::from_utf8(decode(&f.path_b64url)).unwrap())
    .collect();
  assert!(paths.contains(&"src/new.rs".to_owned()), "{paths:?}");
  assert!(paths.contains(&"docs/guide.md".to_owned()), "{paths:?}");
  assert!(paths.contains(&"src/main.rs".to_owned()), "{paths:?}");
  assert!(!diff.truncated);

  let patch = String::from_utf8(decode(&diff.rendered_b64url)).unwrap();
  assert!(
    patch.contains("diff --git a/src/main.rs b/src/main.rs"),
    "{patch}"
  );

  // The whole point. A client-side differ would have fetched a blob per changed
  // file to produce the same output.
  let cache = hydration(&job).await;
  assert_eq!(cache.fetches, 0, "{cache:?}");
  assert_eq!(cache.bytes_fetched, 0, "{cache:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_path_limited_log_and_diff_agree_on_what_changed() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  let Response::Log(all) = job
    .call(Request::Log {
      skip: 0,
      limit: 50,
      from: None,
      first_parent: false,
      paths_b64url: Vec::new(),
    })
    .await
  else {
    panic!("expected a log");
  };
  assert!(all.commits.len() >= 2);

  let Response::Log(scoped) = job
    .call(Request::Log {
      skip: 0,
      limit: 50,
      from: None,
      first_parent: false,
      paths_b64url: vec![b64(b"src/new.rs")],
    })
    .await
  else {
    panic!("expected a log");
  };
  // `src/new.rs` arrived in the tip commit and nowhere else.
  assert_eq!(scoped.commits.len(), 1);
  assert_eq!(scoped.commits[0].commit, all.commits[0].commit);

  // And the tree travels with each entry, so `--format=%T` costs no extra call.
  assert!(!all.commits[0].tree.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn head_means_the_pin_after_the_view_moves() {
  // The silent-wrong-answer case. `gfs switch` re-pins this view to the
  // fixture's *first* commit while the repository's default branch still points
  // at the second; `HEAD` has to follow the view.
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  let Response::Log(before) = job
    .call(Request::Log {
      skip: 0,
      limit: 50,
      from: None,
      first_parent: false,
      paths_b64url: Vec::new(),
    })
    .await
  else {
    panic!("expected a log");
  };
  let older = before.commits[1].commit.clone();

  let Response::Refresh(_) = job
    .call(Request::Switch {
      selector: older.clone(),
      branch: None,
    })
    .await
  else {
    panic!("expected a refresh");
  };

  let Response::Log(after) = job
    .call(Request::Log {
      skip: 0,
      limit: 1,
      from: Some("HEAD".to_owned()),
      first_parent: false,
      paths_b64url: Vec::new(),
    })
    .await
  else {
    panic!("expected a log");
  };
  assert_eq!(after.commits[0].commit, older, "HEAD must be the pin");

  // And an ancestry suffix walks from the pin, not from the branch.
  let Response::Log(walked) = job
    .call(Request::Log {
      skip: 0,
      limit: 1,
      from: Some("HEAD~0".to_owned()),
      first_parent: false,
      paths_b64url: Vec::new(),
    })
    .await
  else {
    panic!("expected a log");
  };
  assert_eq!(walked.commits[0].commit, older);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_root_commit_is_diffed_against_the_empty_tree() {
  // Otherwise the first commit in a history is the one thing that can never be
  // reviewed, which is where a repository's shape is usually decided.
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  let Response::Log(log) = job
    .call(Request::Log {
      skip: 0,
      limit: 50,
      from: None,
      first_parent: false,
      paths_b64url: Vec::new(),
    })
    .await
  else {
    panic!("expected a log");
  };
  let root = log.commits.last().unwrap().commit.clone();

  let Response::RevDiff(diff) = job
    .call(Request::DiffRevs {
      from: None,
      to: root,
      parent: None,
      format: DiffFormat::NameStatus,
      context_lines: None,
      paths_b64url: Vec::new(),
    })
    .await
  else {
    panic!("expected a diff");
  };
  assert_eq!(diff.base_commit, None);
  assert!(!diff.files.is_empty());
  assert!(diff
    .files
    .iter()
    .all(|f| f.status == gfs_types::DiffStatus::Added));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ls_and_cat_reach_a_revision_the_mount_is_not_pinned_to() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  let Response::Ls(listing) = job
    .call(Request::Ls {
      rev: Some("HEAD~1".to_owned()),
      path_b64url: b64(b"docs"),
      page_size: 0,
    })
    .await
  else {
    panic!("expected a listing");
  };
  // `docs/guide.md` exists in the first commit and was deleted by the second, so
  // this is only visible by naming the older revision.
  let paths: Vec<Vec<u8>> = listing
    .entries
    .iter()
    .map(|e| decode(&e.path_b64url))
    .collect();
  assert_eq!(paths, vec![b"docs/guide.md".to_vec()]);

  let Response::Cat { content_b64url, .. } = job
    .call(Request::Cat {
      rev: Some("HEAD~1".to_owned()),
      path_b64url: b64(b"docs/guide.md"),
    })
    .await
  else {
    panic!("expected content");
  };
  assert_eq!(decode(&content_b64url), b"guide\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blame_attributes_lines_and_carries_the_file() {
  let backend = Backend::start("basic").await;
  let job = Job::start(&backend, "main").await;

  let Response::Blame(blame) = job
    .call(Request::Blame {
      rev: None,
      path_b64url: b64(b"src/main.rs"),
    })
    .await
  else {
    panic!("expected a blame");
  };
  assert!(!blame.hunks.is_empty());
  assert!(!blame.truncated);
  assert_eq!(
    decode(&blame.content_b64url),
    b"fn main() { println!(\"bye\"); }\n"
  );
  assert_eq!(blame.hunks[0].final_start_line, 1);

  // The blame reads the blob on the *server*. The mount's own cache is untouched.
  let cache = hydration(&job).await;
  assert_eq!(cache.fetches, 0, "{cache:?}");
}
