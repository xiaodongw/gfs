//! The write path's server half: commit onto a work branch, then push it out.
//!
//! High-level, per AGENTS.md, and checked with **stock Git** rather than by
//! reading back through libgit2 -- what matters is that the real Git server
//! accepts what GFS made.
//!
//! These exercise `gfs-git` and `mirror::push` directly rather than through
//! gRPC. The RPC layer above them is validation and authorization, which the
//! service's own tests cover; what needs a real repository is the object
//! writing and the subprocess.

use std::path::{Path, PathBuf};
use std::process::Command;

use gfs_git::{CommitSignature, GitRepository, Libgit2Repository, TreeChange, TreeChangeKind};
use gfs_service::mirror;
use gfs_types::{mode, BytePath, ObjectId};

const TREE_CACHE_BYTES: usize = 1 << 20;

fn git(args: &[&str], dir: &Path) -> String {
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
  String::from_utf8_lossy(&out.stdout).into_owned()
}

/// An upstream, and a mirror of it -- the shape `gfs clone` leaves behind.
fn upstream_and_mirror(root: &Path) -> (PathBuf, PathBuf, String) {
  let work = root.join("work");
  std::fs::create_dir_all(&work).unwrap();
  git(&["init", "-q", "--initial-branch=main", "."], &work);
  std::fs::write(work.join("README.md"), b"hello\n").unwrap();
  std::fs::create_dir_all(work.join("src")).unwrap();
  std::fs::write(work.join("src/main.rs"), b"fn main(){}\n").unwrap();
  git(&["add", "-A"], &work);
  git(&["commit", "-qm", "first"], &work);

  let upstream = root.join("upstream.git");
  git(
    &[
      "clone",
      "-q",
      "--bare",
      work.to_str().unwrap(),
      upstream.to_str().unwrap(),
    ],
    root,
  );

  let url = format!("file://{}", upstream.display());
  let mirror_path = root.join("mirror.git");
  mirror::init_bare(&mirror_path, Path::new("git")).unwrap();
  mirror::fetch(&mirror_path, &url, None, Path::new("git")).unwrap();
  (upstream, mirror_path, url)
}

fn sig() -> CommitSignature {
  CommitSignature {
    name: "Test".to_owned(),
    email: "test@example.invalid".to_owned(),
    when_secs: 1_700_000_000,
    offset_minutes: 0,
  }
}

/// The three changes a commit has to carry: modify, add, delete.
fn changes() -> Vec<TreeChange> {
  vec![
    TreeChange {
      path: BytePath::new(b"README.md".to_vec()),
      kind: TreeChangeKind::Upsert {
        mode: mode::REGULAR,
        content: b"hello\na new line\n".to_vec(),
      },
    },
    TreeChange {
      path: BytePath::new(b"src/added.rs".to_vec()),
      kind: TreeChangeKind::Upsert {
        mode: mode::REGULAR,
        content: b"pub fn added() {}\n".to_vec(),
      },
    },
    TreeChange {
      path: BytePath::new(b"src/main.rs".to_vec()),
      kind: TreeChangeKind::Delete,
    },
  ]
}

fn commit_on(repo: &Libgit2Repository, base: &ObjectId, work_ref: &str) -> ObjectId {
  let tree = repo.write_tree(Some(base), &changes()).unwrap();
  let commit = repo
    .create_commit(
      &tree,
      std::slice::from_ref(base),
      &sig(),
      &sig(),
      "the workspace's changes\n",
    )
    .unwrap();
  let expected = repo.read_ref(work_ref).unwrap();
  repo
    .update_work_ref(work_ref, &commit, expected.as_ref())
    .unwrap();
  commit
}

#[test]
fn a_commit_carries_the_modify_add_and_delete_together() {
  let tmp = tempfile::tempdir().unwrap();
  let (_upstream, mirror_path, _url) = upstream_and_mirror(tmp.path());
  let repo = Libgit2Repository::open(&mirror_path, 4, TREE_CACHE_BYTES).unwrap();
  let base = repo.read_ref("refs/heads/main").unwrap().unwrap();

  let work_ref = "refs/gfs/work/dev/feature";
  repo.update_work_ref(work_ref, &base, None).unwrap();
  let commit = commit_on(&repo, &base, work_ref);

  let listed = git(
    &["ls-tree", "-r", "--name-only", &commit.to_hex()],
    &mirror_path,
  );
  let names: Vec<&str> = listed.lines().collect();
  assert!(names.contains(&"src/added.rs"), "{names:?}");
  assert!(!names.contains(&"src/main.rs"), "deleted: {names:?}");
  let readme = git(
    &["show", &format!("{}:README.md", commit.to_hex())],
    &mirror_path,
  );
  assert_eq!(readme, "hello\na new line\n");
}

#[test]
fn a_push_puts_the_work_branch_on_the_real_upstream() {
  let tmp = tempfile::tempdir().unwrap();
  let (upstream, mirror_path, url) = upstream_and_mirror(tmp.path());
  let repo = Libgit2Repository::open(&mirror_path, 4, TREE_CACHE_BYTES).unwrap();
  let base = repo.read_ref("refs/heads/main").unwrap().unwrap();
  let work_ref = "refs/gfs/work/dev/feature";
  repo.update_work_ref(work_ref, &base, None).unwrap();
  let commit = commit_on(&repo, &base, work_ref);

  mirror::push(
    &mirror_path,
    &url,
    work_ref,
    "feature",
    false,
    None,
    Path::new("git"),
  )
  .unwrap();

  // The upstream is the oracle: the branch has to be there, under its *public*
  // name, carrying the commit GFS made.
  let landed = git(&["rev-parse", "refs/heads/feature"], &upstream);
  assert_eq!(landed.trim(), commit.to_hex());
  let names = git(
    &["ls-tree", "-r", "--name-only", "refs/heads/feature"],
    &upstream,
  );
  assert!(names.contains("src/added.rs"), "{names}");
  assert!(!names.contains("src/main.rs"), "{names}");
}

#[test]
fn a_pushed_branch_is_not_pruned_by_the_next_sync() {
  // The trap the reserved namespace exists to avoid, asserted end to end: the
  // work ref must survive a fetch that prunes `refs/heads/*`.
  let tmp = tempfile::tempdir().unwrap();
  let (_upstream, mirror_path, url) = upstream_and_mirror(tmp.path());
  let repo = Libgit2Repository::open(&mirror_path, 4, TREE_CACHE_BYTES).unwrap();
  let base = repo.read_ref("refs/heads/main").unwrap().unwrap();
  let work_ref = "refs/gfs/work/dev/survives";
  repo.update_work_ref(work_ref, &base, None).unwrap();
  let commit = commit_on(&repo, &base, work_ref);

  mirror::fetch(&mirror_path, &url, None, Path::new("git")).unwrap();

  assert_eq!(
    repo.read_ref(work_ref).unwrap(),
    Some(commit),
    "an unpushed work branch must survive a pruning fetch"
  );
}

#[test]
fn committing_from_a_stale_base_is_refused_rather_than_clobbering() {
  // Two views of one mirror, both pinned to the same base. The second must lose
  // rather than drop the first's commit.
  let tmp = tempfile::tempdir().unwrap();
  let (_upstream, mirror_path, _url) = upstream_and_mirror(tmp.path());
  let repo = Libgit2Repository::open(&mirror_path, 4, TREE_CACHE_BYTES).unwrap();
  let base = repo.read_ref("refs/heads/main").unwrap().unwrap();
  let work_ref = "refs/gfs/work/dev/contended";
  repo.update_work_ref(work_ref, &base, None).unwrap();

  let first = commit_on(&repo, &base, work_ref);

  // The second view still believes the branch is at `base`.
  let tree = repo
    .write_tree(
      Some(&base),
      &[TreeChange {
        path: BytePath::new(b"other.txt".to_vec()),
        kind: TreeChangeKind::Upsert {
          mode: mode::REGULAR,
          content: b"other\n".to_vec(),
        },
      }],
    )
    .unwrap();
  let second = repo
    .create_commit(
      &tree,
      std::slice::from_ref(&base),
      &sig(),
      &sig(),
      "concurrent\n",
    )
    .unwrap();
  let err = repo
    .update_work_ref(work_ref, &second, Some(&base))
    .unwrap_err();

  assert!(format!("{err}").contains("moved"), "{err}");
  assert_eq!(
    repo.read_ref(work_ref).unwrap(),
    Some(first),
    "the first commit must survive"
  );
}

#[test]
fn a_push_of_a_branch_that_does_not_exist_fails_without_touching_upstream() {
  let tmp = tempfile::tempdir().unwrap();
  let (upstream, mirror_path, url) = upstream_and_mirror(tmp.path());

  let err = mirror::push(
    &mirror_path,
    &url,
    "refs/gfs/work/dev/nothing-here",
    "nothing-here",
    false,
    None,
    Path::new("git"),
  )
  .unwrap_err();
  assert!(format!("{err}").contains("push rejected"), "{err}");

  let refs = git(&["for-each-ref", "--format=%(refname)"], &upstream);
  assert!(
    !refs.contains("nothing-here"),
    "upstream must be untouched: {refs}"
  );
}
