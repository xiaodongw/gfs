//! A generator for very large snapshots.
//!
//! M1's first exit criterion is that "a test can resolve a revision, list a
//! million-entry snapshot one directory at a time, and fetch an individual file
//! without cloning". That needs a million-entry snapshot, and building one has to be
//! cheap enough that the test is runnable rather than theoretical.
//!
//! # Why `fast-import`, and why every file shares one blob
//!
//! The naive approaches do not scale. `git add` of a million files writes a million
//! working-tree files and re-reads the index each time; `update-index --cacheinfo`
//! is a process per entry. `git fast-import` takes one stream on stdin and writes
//! packed objects directly, with no working tree at all.
//!
//! Every file also points at the **same** blob mark. That is not a shortcut that
//! weakens the test: the criterion is about *snapshot* scale -- tree entry count,
//! path table size, and directory paging -- and those are unaffected by whether the
//! blobs differ. What it removes is a million distinct blobs, which would dominate
//! both build time and disk for no additional coverage. The result is one blob and
//! `dirs + 1` trees, so a million-entry snapshot costs tens of megabytes rather
//! than gigabytes.
//!
//! Built with stock Git, like every other fixture, so libgit2 remains the thing
//! under test.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

/// The contents every generated file shares.
const SHARED_CONTENT: &[u8] = b"x\n";

/// Path to a cached large-snapshot fixture, building it if necessary.
///
/// The repository is bare and has one commit on `main` containing
/// `dirs * files_per_dir` files, spread as `d<NNNN>/f<NNNNNN>.txt`, plus a
/// sort-key-boundary pair in the first directory so paging is exercised at scale
/// and not only at small sizes.
pub fn big_tree(dirs: usize, files_per_dir: usize) -> Result<PathBuf> {
  let root = super::fixtures::cache_root();
  let name = format!("bigtree-{dirs}x{files_per_dir}.git");
  let published = root.join("bare").join(&name);
  if published.join("HEAD").is_file() {
    return Ok(published);
  }

  std::fs::create_dir_all(root.join("bare"))?;
  std::fs::create_dir_all(root.join("tmp"))?;
  let staging = root
    .join("tmp")
    .join(format!("{name}-{}", std::process::id()));
  if staging.exists() {
    std::fs::remove_dir_all(&staging)?;
  }

  super::fixtures::git(
    &root,
    &[
      "init",
      "--bare",
      "--quiet",
      "--initial-branch=main",
      "--ref-format=files",
      "--object-format=sha1",
      staging.to_str().context("staging path is not UTF-8")?,
    ],
  )?;

  import(&staging, dirs, files_per_dir)?;

  if std::fs::rename(&staging, &published).is_err() {
    let _ = std::fs::remove_dir_all(&staging);
  }
  Ok(published)
}

fn import(repo: &Path, dirs: usize, files_per_dir: usize) -> Result<()> {
  let mut child = Command::new("git")
    .current_dir(repo)
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .args(["fast-import", "--quiet", "--done"])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .context("spawning git fast-import")?;

  {
    let stdin = child.stdin.as_mut().context("fast-import stdin")?;
    // A buffered writer matters here: a million unbuffered `M` lines would be a
    // million small writes into the pipe.
    let mut out = std::io::BufWriter::with_capacity(1 << 20, stdin);

    writeln!(out, "blob")?;
    writeln!(out, "mark :1")?;
    writeln!(out, "data {}", SHARED_CONTENT.len())?;
    out.write_all(SHARED_CONTENT)?;
    writeln!(out)?;

    writeln!(out, "commit refs/heads/main")?;
    writeln!(out, "mark :2")?;
    // A fixed identity and timestamp, so object IDs are stable across runs and a
    // test may assert on one.
    writeln!(
      out,
      "author XVFS Fixture <fixture@xvfs.invalid> 1577836800 +0000"
    )?;
    writeln!(
      out,
      "committer XVFS Fixture <fixture@xvfs.invalid> 1577836800 +0000"
    )?;
    let message = "large snapshot";
    writeln!(out, "data {}", message.len())?;
    writeln!(out, "{message}")?;

    for d in 0..dirs {
      for f in 0..files_per_dir {
        writeln!(out, "M 100644 :1 d{d:04}/f{f:06}.txt")?;
      }
    }

    // The sort-key boundary ADR 0005 measured, at scale: `pager.h` sorts before
    // `pager/` because `.` (0x2e) precedes `/` (0x2f). A page boundary between them
    // is where name-based pagination drops an entry, and a large tree is where a
    // page boundary is guaranteed to fall somewhere awkward.
    writeln!(out, "M 100644 :1 d0000/pager.h")?;
    writeln!(out, "M 100644 :1 d0000/pager/impl.c")?;

    writeln!(out, "done")?;
    out.flush()?;
  }
  // Dropping stdin closes the pipe, which fast-import needs to finish.
  drop(child.stdin.take());

  let out = child.wait_with_output()?;
  if !out.status.success() {
    bail!(
      "git fast-import failed: {}",
      String::from_utf8_lossy(&out.stderr).trim()
    );
  }
  Ok(())
}

/// How many entries [`big_tree`] produces for the given dimensions.
pub fn expected_entries(dirs: usize, files_per_dir: usize) -> usize {
  // The two extra entries are the sort-key boundary pair, one of which is a
  // directory containing one file -- so `dirs * files_per_dir + 2` files live in
  // `dirs + 1` directories.
  dirs * files_per_dir + 2
}
