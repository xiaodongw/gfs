//! The write half: trees, commits, and reserved-namespace refs.
//!
//! Every assertion is checked with **stock Git**, not by reading back through
//! libgit2. That is the same principle `gfs-test`'s oracles follow: a bug shared
//! between the writer and its check cannot hide, and the thing that actually
//! matters here is that stock Git accepts what GFS wrote.

use gfs_git::{CommitSignature, GitRepository, Libgit2Repository, TreeChange, TreeChangeKind};
use gfs_types::{mode, BytePath, ObjectId};

const TREE_CACHE_BYTES: usize = 1 << 20;

/// A scratch clone of a fixture, writable and disposable.
fn scratch() -> (tempfile::TempDir, Libgit2Repository, std::path::PathBuf) {
  let (tmp, path) = gfs_test::scratch_clone("basic").unwrap();
  let repo = Libgit2Repository::open(&path, 4, TREE_CACHE_BYTES).unwrap();
  (tmp, repo, path)
}

fn sig() -> CommitSignature {
  CommitSignature {
    name: "Test".to_owned(),
    email: "test@example.invalid".to_owned(),
    // Fixed, not "now": a commit is then a pure function of its inputs, so a
    // failure is reproducible rather than time-dependent.
    when_secs: 1_700_000_000,
    offset_minutes: 0,
  }
}

fn head_commit(repo: &Libgit2Repository) -> ObjectId {
  repo.read_ref("refs/heads/main").unwrap().expect("main")
}

fn upsert(path: &str, content: &str) -> TreeChange {
  TreeChange {
    path: BytePath::new(path.as_bytes().to_vec()),
    kind: TreeChangeKind::Upsert {
      mode: mode::REGULAR,
      content: content.as_bytes().to_vec(),
    },
  }
}

#[test]
fn a_written_commit_is_one_stock_git_can_read() {
  let (_tmp, repo, path) = scratch();
  let base = head_commit(&repo);

  let tree = repo
    .write_tree(Some(&base), &[upsert("NOTES.md", "written by gfs\n")])
    .unwrap();
  let commit = repo
    .create_commit(
      &tree,
      std::slice::from_ref(&base),
      &sig(),
      &sig(),
      "a new commit\n",
    )
    .unwrap();

  // The oracle: stock Git, reading the object GFS wrote.
  let hex = commit.to_hex();
  let subject = gfs_test::git(&path, &["log", "-1", "--format=%s", &hex]).unwrap();
  assert_eq!(subject.trim(), "a new commit");

  let content = gfs_test::git(&path, &["show", &format!("{hex}:NOTES.md")]).unwrap();
  assert_eq!(content, "written by gfs\n");

  let parent = gfs_test::git(&path, &["rev-parse", &format!("{hex}^")]).unwrap();
  assert_eq!(parent.trim(), base.to_hex(), "the base must be the parent");
}

#[test]
fn unchanged_paths_survive_and_deletions_take_effect() {
  let (_tmp, repo, path) = scratch();
  let base = head_commit(&repo);
  let before = gfs_test::git(&path, &["ls-tree", "-r", "--name-only", &base.to_hex()]).unwrap();
  let a_file = before
    .lines()
    .next()
    .expect("the fixture has a file")
    .to_owned();

  let tree = repo
    .write_tree(
      Some(&base),
      &[
        upsert("added.txt", "new\n"),
        TreeChange {
          path: BytePath::new(a_file.as_bytes().to_vec()),
          kind: TreeChangeKind::Delete,
        },
      ],
    )
    .unwrap();
  let commit = repo
    .create_commit(&tree, &[base], &sig(), &sig(), "add and delete\n")
    .unwrap();

  let after = gfs_test::git(&path, &["ls-tree", "-r", "--name-only", &commit.to_hex()]).unwrap();
  assert!(after.lines().any(|l| l == "added.txt"), "{after}");
  assert!(
    !after.lines().any(|l| l == a_file),
    "{a_file} should be gone: {after}"
  );
  // Everything the caller did not mention must still be there. This is the
  // property that makes a commit cost its diff rather than the whole tree.
  let untouched = before.lines().filter(|l| *l != a_file).count();
  assert_eq!(
    after.lines().filter(|l| *l != "added.txt").count(),
    untouched
  );
}

#[test]
fn a_nested_path_creates_the_directories_it_needs() {
  let (_tmp, repo, path) = scratch();
  let base = head_commit(&repo);
  let tree = repo
    .write_tree(Some(&base), &[upsert("a/b/c/deep.txt", "deep\n")])
    .unwrap();
  let commit = repo
    .create_commit(&tree, &[base], &sig(), &sig(), "nested\n")
    .unwrap();

  let content = gfs_test::git(
    &path,
    &["show", &format!("{}:a/b/c/deep.txt", commit.to_hex())],
  )
  .unwrap();
  assert_eq!(content, "deep\n");
}

#[test]
fn a_ref_outside_the_reserved_namespace_is_refused() {
  // The rule that keeps unpushed work alive. `refs/heads/*` is a mirror-fetch
  // destination and the fetch prunes, so a branch written there that upstream
  // does not have is deleted by the next sync -- taking every commit on it.
  let (_tmp, repo, _path) = scratch();
  let base = head_commit(&repo);

  let err = repo
    .update_work_ref("refs/heads/sneaky", &base, None)
    .unwrap_err();
  let text = format!("{err}");
  assert!(text.contains("reserved namespace"), "{text}");
  assert!(text.contains("prunes"), "the reason must be stated: {text}");
}

#[test]
fn creating_a_work_ref_twice_is_a_conflict_not_an_overwrite() {
  let (_tmp, repo, _path) = scratch();
  let base = head_commit(&repo);
  let name = "refs/gfs/work/tester/feature";

  repo.update_work_ref(name, &base, None).unwrap();
  let err = repo.update_work_ref(name, &base, None).unwrap_err();
  assert!(format!("{err}").contains("already exists"), "{err}");
}

#[test]
fn a_work_ref_update_is_a_compare_and_swap() {
  // Two views of one mirror may commit to the same work branch at the same
  // moment. Last-write-wins would drop one commit while reporting success, so
  // the loser has to be told.
  let (_tmp, repo, _path) = scratch();
  let base = head_commit(&repo);
  let name = "refs/gfs/work/tester/race";
  repo.update_work_ref(name, &base, None).unwrap();

  let tree_a = repo
    .write_tree(Some(&base), &[upsert("a.txt", "a\n")])
    .unwrap();
  let a = repo
    .create_commit(&tree_a, std::slice::from_ref(&base), &sig(), &sig(), "a\n")
    .unwrap();
  let tree_b = repo
    .write_tree(Some(&base), &[upsert("b.txt", "b\n")])
    .unwrap();
  let b = repo
    .create_commit(&tree_b, std::slice::from_ref(&base), &sig(), &sig(), "b\n")
    .unwrap();

  // The first writer expected `base` and wins.
  repo.update_work_ref(name, &a, Some(&base)).unwrap();
  // The second expected `base` too, and must lose rather than clobber `a`.
  let err = repo.update_work_ref(name, &b, Some(&base)).unwrap_err();
  assert!(
    format!("{err}").contains("moved"),
    "the loser must be told to retry: {err}"
  );
  assert_eq!(
    repo.read_ref(name).unwrap().unwrap(),
    a,
    "the winner's commit must still be there"
  );
}

#[test]
fn an_absent_ref_reads_as_none_rather_than_failing() {
  let (_tmp, repo, _path) = scratch();
  assert!(repo
    .read_ref("refs/gfs/work/nobody/nothing")
    .unwrap()
    .is_none());
}

#[test]
fn a_mode_a_commit_cannot_carry_is_refused() {
  let (_tmp, repo, _path) = scratch();
  let base = head_commit(&repo);
  let err = repo
    .write_tree(
      Some(&base),
      &[TreeChange {
        path: BytePath::new(b"weird".to_vec()),
        kind: TreeChangeKind::Upsert {
          mode: mode::DIRECTORY,
          content: Vec::new(),
        },
      }],
    )
    .unwrap_err();
  assert!(format!("{err}").contains("file mode"), "{err}");
}
