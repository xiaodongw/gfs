//! `gfs clone`'s server half: bringing an upstream under management.
//!
//! High-level only, per AGENTS.md. The unit tests in `ingest.rs` cover URL
//! parsing; these cover the things only a real Git can answer -- that a mirror
//! is created and fetched, that a second call syncs instead of duplicating, and
//! that `HEAD` follows the *upstream's* default branch rather than whatever this
//! Git version would have picked.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use gfs_service::catalog::Catalog;
use gfs_service::ingest::{self, IngestConfig};
use gfs_service::registry::Registry;

/// A bare upstream whose default branch is deliberately not `main` or `master`.
///
/// That is the whole point of the fixture: a mirror created by `init_bare` gets
/// this Git's default branch, so an upstream on `trunk` is what distinguishes
/// "asked the upstream" from "assumed the local default".
fn upstream(root: &Path, branch: &str) -> String {
  let work = root.join("work");
  let git = |args: &[&str], dir: &Path| {
    let out = Command::new("git")
      .env("GIT_CONFIG_GLOBAL", "/dev/null")
      .env("GIT_CONFIG_SYSTEM", "/dev/null")
      .env("GIT_AUTHOR_NAME", "T")
      .env("GIT_AUTHOR_EMAIL", "t@e")
      .env("GIT_COMMITTER_NAME", "T")
      .env("GIT_COMMITTER_EMAIL", "t@e")
      .current_dir(dir)
      .args(args)
      .output()
      .expect("git runs");
    assert!(
      out.status.success(),
      "git {args:?}: {}",
      String::from_utf8_lossy(&out.stderr)
    );
  };
  std::fs::create_dir_all(&work).unwrap();
  git(
    &["init", "-q", &format!("--initial-branch={branch}"), "."],
    &work,
  );
  std::fs::write(work.join("README.md"), b"hello\n").unwrap();
  git(&["add", "-A"], &work);
  git(&["commit", "-qm", "first"], &work);

  let bare = root.join("upstream.git");
  git(
    &[
      "clone",
      "-q",
      "--bare",
      work.to_str().unwrap(),
      bare.to_str().unwrap(),
    ],
    root,
  );
  format!("file://{}", bare.display())
}

fn harness(root: &Path) -> (Arc<Catalog>, Arc<Registry>, IngestConfig) {
  let catalog = Arc::new(Catalog::open(&root.join("catalog.sqlite")).unwrap());
  let registry = Arc::new(Registry::new(Arc::clone(&catalog)));
  let config = IngestConfig {
    repos_root: root.join("repos"),
    git_binary: PathBuf::from("git"),
  };
  (catalog, registry, config)
}

#[test]
fn a_clone_creates_a_mirror_and_activates_it() {
  let tmp = tempfile::tempdir().unwrap();
  let url = upstream(tmp.path(), "trunk");
  let (catalog, registry, config) = harness(tmp.path());

  let outcome = ingest::ingest(&catalog, &registry, &config, &url, None).unwrap();

  assert!(outcome.created, "the first clone creates");
  assert_eq!(
    outcome.default_branch, "trunk",
    "HEAD must follow the upstream, not this Git's init default"
  );
  let record = catalog
    .get_repository(&outcome.repository_id)
    .unwrap()
    .expect("the repository is in the catalog");
  assert_eq!(record.upstream_url.as_deref(), Some(url.as_str()));
  assert!(record.repo_path.join("HEAD").is_file(), "a bare mirror");
}

#[test]
fn cloning_the_same_url_twice_syncs_rather_than_duplicating() {
  // The property the whole "one clone, many views" model rests on: a second
  // caller asking for a URL already present must not get a second mirror, or
  // the two copies drift and mounts disagree about history.
  let tmp = tempfile::tempdir().unwrap();
  let url = upstream(tmp.path(), "main");
  let (catalog, registry, config) = harness(tmp.path());

  let first = ingest::ingest(&catalog, &registry, &config, &url, None).unwrap();
  let second = ingest::ingest(&catalog, &registry, &config, &url, None).unwrap();

  assert!(first.created);
  assert!(!second.created, "the second call is a sync");
  assert_eq!(first.repository_id, second.repository_id);

  let mirrors: Vec<_> = std::fs::read_dir(&config.repos_root)
    .unwrap()
    .map(|e| e.unwrap().file_name())
    .collect();
  assert_eq!(mirrors.len(), 1, "exactly one mirror: {mirrors:?}");
}

#[test]
fn a_new_upstream_commit_arrives_on_the_second_clone() {
  let tmp = tempfile::tempdir().unwrap();
  let url = upstream(tmp.path(), "main");
  let (catalog, registry, config) = harness(tmp.path());
  let first = ingest::ingest(&catalog, &registry, &config, &url, None).unwrap();

  // Move the upstream on, then sync.
  let work = tmp.path().join("work");
  let bare = tmp.path().join("upstream.git");
  let bare = bare.to_str().unwrap();
  std::fs::write(work.join("second.txt"), b"more\n").unwrap();
  // Pushed to the bare path rather than to `origin`: `work` was the *source*
  // the bare repository was cloned from, so it has no remote of its own.
  for args in [
    vec!["add", "-A"],
    vec!["commit", "-qm", "second"],
    vec!["push", "-q", bare, "main"],
  ] {
    let out = Command::new("git")
      .env("GIT_CONFIG_GLOBAL", "/dev/null")
      .env("GIT_CONFIG_SYSTEM", "/dev/null")
      .env("GIT_AUTHOR_NAME", "T")
      .env("GIT_AUTHOR_EMAIL", "t@e")
      .env("GIT_COMMITTER_NAME", "T")
      .env("GIT_COMMITTER_EMAIL", "t@e")
      .current_dir(&work)
      .args(&args)
      .output()
      .unwrap();
    assert!(
      out.status.success(),
      "git {args:?}: {}",
      String::from_utf8_lossy(&out.stderr)
    );
  }

  ingest::ingest(&catalog, &registry, &config, &url, None).unwrap();

  let record = catalog
    .get_repository(&first.repository_id)
    .unwrap()
    .unwrap();
  let out = Command::new("git")
    .args(["log", "--oneline", "refs/heads/main"])
    .current_dir(&record.repo_path)
    .output()
    .unwrap();
  let log = String::from_utf8_lossy(&out.stdout);
  assert!(log.contains("second"), "the sync must fetch: {log}");
}

#[test]
fn an_unreachable_upstream_leaves_no_half_registered_repository() {
  // Ordering matters: the catalog row is written only after the fetch succeeds,
  // so a failed clone must leave nothing for the next caller to trip over.
  let tmp = tempfile::tempdir().unwrap();
  let (catalog, registry, config) = harness(tmp.path());
  let url = format!("file://{}/does-not-exist.git", tmp.path().display());

  let err = ingest::ingest(&catalog, &registry, &config, &url, None).unwrap_err();
  assert!(
    format!("{err}").contains("fetch"),
    "the failure should name the fetch: {err}"
  );

  let id = ingest::repository_id_for(&url).unwrap();
  assert!(
    catalog.get_repository(&id).unwrap().is_none(),
    "no catalog row may survive a failed clone"
  );
}
