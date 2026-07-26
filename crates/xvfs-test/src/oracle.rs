//! Correctness oracles: an independent answer to compare XVFS against.
//!
//! PLAN.md section 12 names the first of these explicitly, and the reason is
//! subtle enough to be worth restating. A mount serves **raw blob bytes**. A
//! normal `git checkout` does not: it applies `.gitattributes` `text`/`eol`
//! conversion, `core.autocrlf`, clean/smudge filters, and LFS. Comparing a mount
//! against a checkout therefore reports differences that are *correct* behaviour
//! on both sides, and a suite built that way either fails constantly or gets
//! taught to ignore real differences.
//!
//! [`materialize_raw`] avoids that by reconstructing the tree from `git ls-tree`
//! and `git cat-file`, which do no conversion at all. It is the oracle for
//! ordinary comparisons.
//!
//! [`materialize_checkout`] is the *other* oracle, used once, on purpose: PLAN.md
//! M2.4 requires running the same comparison with filters enabled on a repository
//! that uses `.gitattributes`, and recording the divergence as documented
//! expected behaviour rather than as a failure.
//!
//! Both are built with stock Git, never with libgit2, so libgit2 is always the
//! thing under test and never also the thing producing the answer.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use sha1::{Digest, Sha1};

use crate::fixtures::git_bytes;

/// What a comparison looks at.
///
/// Content is compared by digest rather than by bytes so that a 12 MiB blob does
/// not have to be held twice in memory to be checked, and so that a failure
/// message stays readable.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum EntrySnapshot {
  File {
    executable: bool,
    size: u64,
    sha1: String,
  },
  Symlink {
    target: Vec<u8>,
  },
  /// A directory. Recorded because an empty one is meaningful: it is how a
  /// gitlink appears, and a tree that lost it would still compare equal on files
  /// alone.
  Directory,
}

/// A whole tree, keyed by path from the root.
pub type TreeSnapshot = BTreeMap<Vec<u8>, EntrySnapshot>;

/// Reconstruct a commit's tree with raw blob bytes.
///
/// `git ls-tree -r -z` for the paths and modes, `git cat-file` for the content.
/// Neither applies any checkout-time conversion, which is the point.
pub fn materialize_raw(repo: &Path, revision: &str, destination: &Path) -> Result<()> {
  std::fs::create_dir_all(destination)?;
  // Bytes, not a `String`. `-z` emits unquoted path bytes, and a Git path need
  // not be UTF-8: reading this listing lossily makes the *oracle* report a
  // U+FFFD-mangled name and the mount look wrong for having the real one. That
  // is not hypothetical -- it is what the first run of the `bytes` comparison
  // did.
  let listing = git_bytes(
    repo,
    &[
      OsStr::new("ls-tree"),
      OsStr::new("-r"),
      OsStr::new("-z"),
      OsStr::new("--full-tree"),
      OsStr::new(revision),
    ],
  )?;

  for record in listing.split(|b| *b == 0).filter(|r| !r.is_empty()) {
    // `<mode> SP <type> SP <oid> TAB <path>`
    let tab = record
      .iter()
      .position(|b| *b == b'\t')
      .with_context(|| "malformed ls-tree record: no tab".to_owned())?;
    let (meta, path) = record.split_at(tab);
    let path = &path[1..];
    // The metadata half is always ASCII, so a `String` is safe here and nowhere
    // else in this loop.
    let meta = std::str::from_utf8(meta).context("ls-tree metadata is not ASCII")?;
    let fields: Vec<&str> = meta.split_whitespace().collect();
    let [mode, _kind, oid] = fields.as_slice() else {
      bail!("malformed ls-tree metadata: {meta:?}");
    };

    let full = destination.join(OsStr::from_bytes(path));
    if let Some(parent) = full.parent() {
      std::fs::create_dir_all(parent)?;
    }

    match *mode {
      "100644" | "100755" => {
        let bytes = cat_file(repo, oid)?;
        std::fs::write(&full, &bytes)?;
        let mode = if *mode == "100755" { 0o755 } else { 0o644 };
        std::fs::set_permissions(&full, std::fs::Permissions::from_mode(mode))?;
      }
      "120000" => {
        // A symlink's target *is* its blob content.
        let target = cat_file(repo, oid)?;
        std::os::unix::fs::symlink(OsStr::from_bytes(&target), &full)?;
      }
      // A submodule. XVFS presents it as an empty read-only directory, and the
      // oracle has to agree or every fixture with one fails for the wrong reason.
      "160000" => {
        std::fs::create_dir_all(&full)?;
      }
      other => bail!(
        "unsupported mode {other} at {}",
        String::from_utf8_lossy(path)
      ),
    }
  }
  Ok(())
}

/// A real `git checkout`, with every filter Git would normally apply.
///
/// Used only for the divergence comparison PLAN.md M2.4 asks for.
pub fn materialize_checkout(repo: &Path, revision: &str, destination: &Path) -> Result<()> {
  std::fs::create_dir_all(destination)?;
  let out = Command::new("git")
    .arg("--git-dir")
    .arg(repo)
    .arg("--work-tree")
    .arg(destination)
    .args(["checkout", "-f", revision, "--", "."])
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    // Its own index, so the fixture's repository is left untouched.
    .env("GIT_INDEX_FILE", destination.join(".checkout-index"))
    .output()
    .context("running git checkout")?;
  if !out.status.success() {
    bail!(
      "git checkout failed: {}",
      String::from_utf8_lossy(&out.stderr).trim()
    );
  }
  let _ = std::fs::remove_file(destination.join(".checkout-index"));
  Ok(())
}

fn cat_file(repo: &Path, oid: &str) -> Result<Vec<u8>> {
  let out = Command::new("git")
    .current_dir(repo)
    .args(["cat-file", "blob", oid])
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .output()
    .context("running git cat-file")?;
  if !out.status.success() {
    bail!(
      "git cat-file {oid} failed: {}",
      String::from_utf8_lossy(&out.stderr).trim()
    );
  }
  Ok(out.stdout)
}

/// Snapshot a directory tree for comparison.
///
/// `.git` is skipped at the root. DESIGN.md section 8.6 puts whatever occupies it
/// outside change tracking, search, and export, so including it would compare a
/// synthesized surface against a checkout's real repository and find only
/// differences that mean nothing.
pub fn snapshot_tree(root: &Path) -> Result<TreeSnapshot> {
  let mut out = BTreeMap::new();
  walk(root, &[], &mut out)?;
  Ok(out)
}

fn walk(root: &Path, relative: &[u8], out: &mut TreeSnapshot) -> Result<()> {
  let here = if relative.is_empty() {
    root.to_path_buf()
  } else {
    root.join(OsStr::from_bytes(relative))
  };
  for entry in std::fs::read_dir(&here)
    .with_context(|| format!("reading {}", here.display()))?
    .flatten()
  {
    let name = entry.file_name();
    let name = name.as_bytes();
    if relative.is_empty() && name == b".git" {
      continue;
    }
    let mut child = relative.to_vec();
    if !child.is_empty() {
      child.push(b'/');
    }
    child.extend_from_slice(name);

    let meta = std::fs::symlink_metadata(entry.path())
      .with_context(|| format!("stat {}", entry.path().display()))?;
    let kind = meta.file_type();
    if kind.is_symlink() {
      let target = std::fs::read_link(entry.path())?;
      out.insert(
        child,
        EntrySnapshot::Symlink {
          target: target.as_os_str().as_bytes().to_vec(),
        },
      );
    } else if kind.is_dir() {
      out.insert(child.clone(), EntrySnapshot::Directory);
      walk(root, &child, out)?;
    } else {
      let bytes = std::fs::read(entry.path())
        .with_context(|| format!("reading {}", entry.path().display()))?;
      let mut hasher = Sha1::new();
      hasher.update(&bytes);
      out.insert(
        child,
        EntrySnapshot::File {
          // Only the executable bit: XVFS reports a read-only base, so comparing
          // the full mode would report `0444` against `0644` for every file and
          // say nothing about correctness.
          executable: meta.permissions().mode() & 0o111 != 0,
          size: bytes.len() as u64,
          sha1: hex(&hasher.finalize()),
        },
      );
    }
  }
  Ok(())
}

fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A readable description of how two trees differ.
///
/// Returns an empty vector when they match. Built as a list rather than an
/// assertion so a caller can *expect* specific differences, which is what the
/// filtered-checkout comparison needs.
pub fn diff_trees(left: &TreeSnapshot, right: &TreeSnapshot) -> Vec<String> {
  let mut differences = Vec::new();
  for (path, entry) in left {
    match right.get(path) {
      None => differences.push(format!("only in left: {}", escape(path))),
      Some(other) if other != entry => differences.push(format!(
        "differs: {} ({entry:?} vs {other:?})",
        escape(path)
      )),
      Some(_) => {}
    }
  }
  for path in right.keys() {
    if !left.contains_key(path) {
      differences.push(format!("only in right: {}", escape(path)));
    }
  }
  differences
}

fn escape(path: &[u8]) -> String {
  xvfs_types::BytePath::new(path.to_vec()).escaped()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_raw_materializer_reproduces_a_fixture_including_modes_and_links() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = crate::fixtures::bare("modes");
    materialize_raw(&repo, "main", tmp.path()).unwrap();

    let snapshot = snapshot_tree(tmp.path()).unwrap();
    assert!(matches!(
      snapshot.get(b"script.sh".as_slice()),
      Some(EntrySnapshot::File {
        executable: true,
        ..
      })
    ));
    assert_eq!(
      snapshot.get(b"rel-link".as_slice()),
      Some(&EntrySnapshot::Symlink {
        target: b"plain.txt".to_vec()
      })
    );
    // The gitlink, as an empty directory.
    assert_eq!(
      snapshot.get(b"vendor/submodule".as_slice()),
      Some(&EntrySnapshot::Directory)
    );
  }

  #[test]
  fn the_raw_materializer_does_not_convert_line_endings() {
    // The whole reason this oracle exists rather than a checkout: `attrs`
    // declares `*.txt text eol=crlf`, so a checkout writes CRLF and the raw
    // bytes are LF. XVFS serves the raw bytes.
    let raw = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let repo = crate::fixtures::bare("attrs");
    materialize_raw(&repo, "main", raw.path()).unwrap();
    materialize_checkout(&repo, "main", checkout.path()).unwrap();

    let raw_bytes = std::fs::read(raw.path().join("converted.txt")).unwrap();
    let checkout_bytes = std::fs::read(checkout.path().join("converted.txt")).unwrap();
    assert!(!raw_bytes.windows(2).any(|w| w == b"\r\n"), "raw stays LF");
    assert!(
      checkout_bytes.windows(2).any(|w| w == b"\r\n"),
      "a real checkout applies eol=crlf"
    );
    assert_ne!(raw_bytes, checkout_bytes);
  }

  #[test]
  fn diffing_identical_trees_reports_nothing() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let repo = crate::fixtures::bare("basic");
    materialize_raw(&repo, "main", a.path()).unwrap();
    materialize_raw(&repo, "main", b.path()).unwrap();
    assert!(diff_trees(
      &snapshot_tree(a.path()).unwrap(),
      &snapshot_tree(b.path()).unwrap()
    )
    .is_empty());
  }
}
