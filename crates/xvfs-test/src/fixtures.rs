//! The fixture matrix, re-homed from the M0.3 spike.
//!
//! Built with **stock Git**, never with libgit2, so that libgit2 is always the
//! thing under test and never also the thing producing the input. Every fixture
//! exists because some row of PLAN.md section 12's test matrix would otherwise be
//! untested, and each carries the reason in its `rationale` so a failing case
//! explains itself without a reader reverse-engineering the setup.
//!
//! # Caching
//!
//! Fixtures are built once into `target/xvfs-fixtures/<version>/` and reused. The
//! reason is cost, not convenience: `content` writes 16 MiB and `bigdir` writes
//! 5000 files, and rebuilding those per test -- let alone per test *binary* --
//! would dominate the suite. Bump [`FIXTURE_VERSION`] to invalidate everything
//! after changing a builder, which is the only way a stale fixture can occur.
//!
//! Publication is a directory rename, so concurrent test binaries in separate
//! processes cannot observe a half-built fixture: a loser's rename fails and it
//! adopts the winner's copy.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Bump to invalidate every cached fixture.
pub const FIXTURE_VERSION: &str = "v1";

#[derive(Debug)]
pub struct Fixture {
  pub name: &'static str,
  /// Why this fixture exists.
  pub rationale: &'static str,
  pub build: fn(&Path) -> Result<()>,
  /// Whether the pinned libgit2 can open it at all.
  ///
  /// `false` for `reftable` and `sha256`, which exist precisely to prove the
  /// format gate rejects them (ADR 0001). A test that iterates the matrix has to
  /// know which rows are expected to be unopenable, or it cannot tell a correct
  /// rejection from a regression.
  pub openable: bool,
}

pub const FIXTURES: &[Fixture] = &[
  Fixture {
    name: "empty",
    rationale: "no commits; unborn HEAD must not be reported as an error state",
    build: build_empty,
    openable: true,
  },
  Fixture {
    name: "basic",
    rationale: "baseline trees, branches, lightweight and annotated tags",
    build: build_basic,
    openable: true,
  },
  Fixture {
    name: "modes",
    rationale: "executable bit, symlinks (relative/absolute/escaping/loop), gitlink",
    build: build_modes,
    openable: true,
  },
  Fixture {
    name: "bytes",
    rationale: "non-UTF-8, newline, quote and space in path names",
    build: build_bytes,
    openable: true,
  },
  Fixture {
    name: "content",
    rationale: "empty, CRLF, no final newline, NUL bytes, huge line, large blob",
    build: build_content,
    openable: true,
  },
  Fixture {
    name: "bigdir",
    rationale: "5000 entries in one tree; directory pagination and readdir cost",
    build: build_bigdir,
    openable: true,
  },
  Fixture {
    name: "deep",
    rationale: "40 nested path components; per-component tree traversal",
    build: build_deep,
    openable: true,
  },
  Fixture {
    name: "packed",
    rationale: "all objects and refs packed; the normal server-side shape",
    build: build_packed,
    openable: true,
  },
  Fixture {
    name: "reftable",
    rationale: "reftable ref backend; ADR 0001 rejects it at ingest",
    build: build_reftable,
    openable: false,
  },
  Fixture {
    name: "sha256",
    rationale: "SHA-256 object format; unreachable through git2-rs (ADR 0001)",
    build: build_sha256,
    openable: false,
  },
  Fixture {
    name: "attrs",
    rationale: ".gitattributes text/eol and an LFS pointer; the mount serves raw bytes",
    build: build_attrs,
    openable: true,
  },
];

pub fn fixture(name: &str) -> &'static Fixture {
  FIXTURES
    .iter()
    .find(|f| f.name == name)
    .unwrap_or_else(|| panic!("no fixture named {name:?}"))
}

/// Run git with a fixed, hermetic environment.
///
/// The developer's real `~/.gitconfig` must not reach a fixture. A `core.autocrlf`
/// or `init.defaultBranch` set on one machine would silently change what the
/// conformance checks see, and every one of these results has to be reproducible
/// elsewhere. Fixed author and committer dates keep object IDs stable across
/// runs, which is what lets a test assert on a specific OID at all.
pub fn git_raw(dir: &Path, args: &[&OsStr]) -> Result<String> {
  let out = Command::new("git")
    .current_dir(dir)
    .args(args)
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env("GIT_AUTHOR_NAME", "XVFS Fixture")
    .env("GIT_AUTHOR_EMAIL", "fixture@xvfs.invalid")
    .env("GIT_COMMITTER_NAME", "XVFS Fixture")
    .env("GIT_COMMITTER_EMAIL", "fixture@xvfs.invalid")
    .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
    .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
    .output()
    .with_context(|| format!("spawning git {args:?}"))?;
  if !out.status.success() {
    bail!(
      "git {:?} failed in {}: {}",
      args,
      dir.display(),
      String::from_utf8_lossy(&out.stderr).trim()
    );
  }
  Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run git with string arguments.
pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
  let owned: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
  git_raw(dir, &owned)
}

fn init(dir: &Path, extra: &[&str]) -> Result<()> {
  std::fs::create_dir_all(dir)?;
  let mut args = vec!["init", "-q", "--initial-branch=main"];
  args.extend_from_slice(extra);
  args.push(".");
  git(dir, &args)?;
  Ok(())
}

fn write(dir: &Path, rel: &str, content: &[u8]) -> Result<()> {
  let p = dir.join(rel);
  if let Some(parent) = p.parent() {
    std::fs::create_dir_all(parent)?;
  }
  std::fs::write(p, content)?;
  Ok(())
}

fn commit_all(dir: &Path, msg: &str) -> Result<()> {
  git(dir, &["add", "-A", "."])?;
  git(dir, &["commit", "-q", "-m", msg])?;
  Ok(())
}

fn build_empty(dir: &Path) -> Result<()> {
  init(dir, &[])
}

fn build_basic(dir: &Path) -> Result<()> {
  init(dir, &[])?;
  write(dir, "README.md", b"# basic\n")?;
  write(dir, "src/main.rs", b"fn main() { println!(\"hi\"); }\n")?;
  write(dir, "src/lib/util.rs", b"pub fn util() {}\n")?;
  write(dir, "docs/guide.md", b"guide\n")?;
  commit_all(dir, "initial")?;
  git(dir, &["tag", "v1.0"])?;

  write(dir, "src/main.rs", b"fn main() { println!(\"bye\"); }\n")?;
  write(dir, "src/new.rs", b"pub fn added() {}\n")?;
  std::fs::remove_file(dir.join("docs/guide.md"))?;
  commit_all(dir, "second")?;
  git(dir, &["tag", "-a", "v2.0", "-m", "annotated release"])?;
  git(dir, &["branch", "feature"])?;

  // A tag that peels to a **tree**, not a commit.
  //
  // M0.3 found this in the wild: the Linux kernel's `v2.6.11` tag does exactly
  // this. ADR 0006 records the decision to reject it with a typed error rather
  // than resolve it, because a tree OID where every layer expects a commit
  // produces a snapshot nobody can read. Without a fixture the rule is untested.
  let tree = git(dir, &["rev-parse", "HEAD^{tree}"])?.trim().to_owned();
  git(
    dir,
    &["tag", "-a", "tree-tag", "-m", "points at a tree", &tree],
  )?;
  Ok(())
}

fn build_modes(dir: &Path) -> Result<()> {
  init(dir, &[])?;
  write(dir, "plain.txt", b"plain\n")?;
  write(dir, "script.sh", b"#!/bin/sh\necho hi\n")?;
  // The executable bit is set on the file itself, not with
  // `update-index --chmod=+x`. The `update-index` form works only until the next
  // `git add`, which re-stages from the working tree and resets the mode to 644 --
  // and this fixture adds symlinks afterwards, so it would have silently lost the
  // executable entry it exists to provide.
  std::fs::set_permissions(
    dir.join("script.sh"),
    std::os::unix::fs::PermissionsExt::from_mode(0o755),
  )?;

  std::os::unix::fs::symlink("plain.txt", dir.join("rel-link"))?;
  std::os::unix::fs::symlink("/etc/passwd", dir.join("abs-link"))?;
  std::os::unix::fs::symlink("../../../etc/shadow", dir.join("escape-link"))?;
  std::os::unix::fs::symlink("loop-b", dir.join("loop-a"))?;
  std::os::unix::fs::symlink("loop-a", dir.join("loop-b"))?;
  git(dir, &["add", "-A", "."])?;

  // A gitlink without materializing a real submodule. `--cacheinfo` writes the
  // 160000 entry directly, which is all the tree needs; XVFS never recurses into
  // it. ADR 0006 confirms submodules are present in the real corpus, so this is a
  // live compatibility case rather than a hypothetical.
  git(
    dir,
    &[
      "update-index",
      "--add",
      "--cacheinfo",
      "160000,0000000000000000000000000000000000000001,vendor/submodule",
    ],
  )?;
  git(dir, &["commit", "-q", "-m", "modes"])?;
  Ok(())
}

fn build_bytes(dir: &Path) -> Result<()> {
  init(dir, &[])?;
  // Invalid UTF-8 (a lone 0xff) and a Latin-1 sequence. ADR 0006's measurement
  // says no current corpus tip has these, and the fixture stays anyway: byte
  // handling is far cheaper to keep tested than to retrofit.
  let names: &[&[u8]] = &[
    b"latin1-\xff-name.txt",
    b"latin1-caf\xe9.txt",
    b"with space.txt",
    b"with\"quote.txt",
    b"with\nnewline.txt",
    b"unicode-\xc3\xa9\xe2\x9c\x93.txt",
    b"back\\slash.txt",
  ];
  for n in names {
    let p = dir.join(OsStr::from_bytes(n));
    std::fs::write(&p, b"content\n")
      .with_context(|| format!("writing {:?}", String::from_utf8_lossy(n)))?;
  }
  commit_all(dir, "byte paths")?;
  Ok(())
}

fn build_content(dir: &Path) -> Result<()> {
  init(dir, &[])?;
  write(dir, "empty.txt", b"")?;
  write(dir, "crlf.txt", b"line one\r\nline two\r\n")?;
  write(dir, "no-final-newline.txt", b"no trailing newline")?;
  write(dir, "binary.bin", &[0u8, 1, 2, 0, 255, 254, 0, 42])?;
  // A single line long enough to break naive line-oriented indexing.
  write(dir, "huge-line.txt", &vec![b'x'; 4 * 1024 * 1024])?;
  // Above the 8 MiB content-search cutoff in DESIGN.md section 7.5, so the
  // coverage-exclusion path has something real to exclude.
  write(dir, "large-blob.bin", &vec![0xABu8; 12 * 1024 * 1024])?;
  write(dir, "utf16.txt", b"\xff\xfeh\0e\0l\0l\0o\0")?;
  commit_all(dir, "content shapes")?;
  Ok(())
}

fn build_bigdir(dir: &Path) -> Result<()> {
  init(dir, &[])?;
  let big = dir.join("many");
  std::fs::create_dir_all(&big)?;
  for i in 0..5000 {
    std::fs::write(big.join(format!("file-{i:05}.txt")), format!("{i}\n"))?;
  }
  // A file and a directory whose names straddle the sort-key boundary that
  // ADR 0005 measured: `pager.h` sorts before `pager/` because `.` precedes `/`,
  // so a page boundary between them is where naive name-based pagination drops an
  // entry.
  write(dir, "many/pager.h", b"header\n")?;
  write(dir, "many/pager/impl.c", b"impl\n")?;
  commit_all(dir, "bigdir")?;
  Ok(())
}

fn build_deep(dir: &Path) -> Result<()> {
  init(dir, &[])?;
  let mut rel = String::new();
  for i in 0..40 {
    rel.push_str(&format!("d{i:02}/"));
  }
  rel.push_str("leaf.txt");
  write(dir, &rel, b"deep\n")?;
  commit_all(dir, "deep")?;
  Ok(())
}

fn build_packed(dir: &Path) -> Result<()> {
  build_basic(dir)?;
  git(dir, &["gc", "-q", "--aggressive"])?;
  git(dir, &["pack-refs", "--all"])?;
  Ok(())
}

fn build_reftable(dir: &Path) -> Result<()> {
  init(dir, &["--ref-format=reftable"])?;
  write(dir, "README.md", b"# reftable\n")?;
  commit_all(dir, "initial")?;
  Ok(())
}

fn build_sha256(dir: &Path) -> Result<()> {
  init(dir, &["--object-format=sha256"])?;
  write(dir, "README.md", b"# sha256\n")?;
  write(dir, "src/main.rs", b"fn main() {}\n")?;
  commit_all(dir, "initial")?;
  git(dir, &["tag", "-a", "v1.0", "-m", "annotated"])?;
  Ok(())
}

fn build_attrs(dir: &Path) -> Result<()> {
  init(dir, &[])?;
  write(
    dir,
    ".gitattributes",
    b"*.txt text eol=crlf\n*.bin -text\n*.psd filter=lfs diff=lfs merge=lfs -text\n",
  )?;
  // Stored LF in the object database; a real checkout would emit CRLF. The mount
  // serves the stored bytes, which is the divergence DESIGN.md section 12
  // documents. This fixture is what makes that divergence testable rather than
  // merely asserted.
  write(dir, "converted.txt", b"alpha\nbeta\n")?;
  write(
    dir,
    "asset.psd",
    b"version https://git-lfs.github.com/spec/v1\n\
      oid sha256:0000000000000000000000000000000000000000000000000000000000000000\n\
      size 12345\n",
  )?;
  commit_all(dir, "attributes")?;
  Ok(())
}

// ---------------------------------------------------------------------------
// Cached construction
// ---------------------------------------------------------------------------

/// The cache root: `target/xvfs-fixtures/<version>/`.
///
/// Under `target/` so `cargo clean` clears it and `.gitignore` already covers it.
fn cache_root() -> PathBuf {
  // `OUT_DIR` is not set for a test binary, so the target directory is derived
  // from the test executable's own path: `target/debug/deps/<name>` gives
  // `target/` three levels up. Falling back to a temp directory keeps this
  // working under an unusual layout at the cost of rebuilding.
  let base = std::env::current_exe()
    .ok()
    .and_then(|p| p.ancestors().nth(3).map(Path::to_path_buf))
    .unwrap_or_else(std::env::temp_dir);
  base.join("xvfs-fixtures").join(FIXTURE_VERSION)
}

/// Path to a fixture's **bare** repository, building it if necessary.
///
/// The bare form is what the server serves, so conformance runs against it.
pub fn bare(name: &str) -> PathBuf {
  ensure_built(name).expect("fixture build failed")
}

/// Path to a fixture's worktree, building it if necessary.
///
/// Kept for oracle comparisons that need a real checkout -- the raw-tree
/// materializer in PLAN.md section 12, and the `git status` comparisons the shim
/// is measured against.
pub fn worktree(name: &str) -> PathBuf {
  ensure_built(name).expect("fixture build failed");
  cache_root().join("work").join(name)
}

/// A throwaway clone of a fixture's bare repository.
///
/// Tests that mutate refs -- force push, branch deletion, `git gc` -- must use one
/// of these rather than the shared cached fixture, or they corrupt every other
/// test's input. Returns the `TempDir` so the caller controls its lifetime;
/// dropping it removes the clone.
pub fn scratch_clone(name: &str) -> Result<(tempfile::TempDir, PathBuf)> {
  let src = bare(name);
  let dir = tempfile::tempdir()?;
  let dst = dir.path().join("scratch.git");
  copy_dir(&src, &dst)?;
  Ok((dir, dst))
}

/// Serializes fixture construction within one process.
///
/// The directory rename handles *cross-process* races, but not threads inside one
/// test binary: without this, several tests calling `bare("basic")` at once run
/// `git init` into the same staging directory and fail with "File exists" and
/// "index.lock: File exists". After the first build the guarded section is a
/// single `is_file` check, so holding a global lock costs nothing.
static BUILD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn ensure_built(name: &str) -> Result<PathBuf> {
  // Poisoning is ignored: a panic in one builder leaves the staging directory
  // behind but not the published fixture, so a later attempt is still sound.
  let _guard = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
  let f = fixture(name);
  let root = cache_root();
  let bare_path = root.join("bare").join(format!("{}.git", f.name));
  let work_path = root.join("work").join(f.name);

  if bare_path.join("HEAD").is_file() {
    return Ok(bare_path);
  }

  std::fs::create_dir_all(root.join("bare"))?;
  std::fs::create_dir_all(root.join("work"))?;
  std::fs::create_dir_all(root.join("tmp"))?;

  // Build into a private directory, then publish by rename. Two test binaries in
  // separate processes may reach this point at once; the rename makes the loser's
  // attempt fail rather than let it observe a half-built fixture. The counter
  // keeps a retry after a failed build from reusing a dirty staging directory.
  static ATTEMPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
  let unique = format!(
    "{}-{}-{}",
    f.name,
    std::process::id(),
    ATTEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
  );

  if !work_path.join(".git").is_dir() && !work_path.join("HEAD").is_file() {
    let staging = root.join("tmp").join(format!("work-{unique}"));
    if staging.exists() {
      std::fs::remove_dir_all(&staging)?;
    }
    (f.build)(&staging).with_context(|| format!("building fixture {}", f.name))?;
    if std::fs::rename(&staging, &work_path).is_err() {
      // Another process published first. Its copy is equivalent; discard ours.
      let _ = std::fs::remove_dir_all(&staging);
    }
  }

  let staging = root.join("tmp").join(format!("bare-{unique}"));
  if staging.exists() {
    std::fs::remove_dir_all(&staging)?;
  }
  // `clone --bare` from a reftable source would *convert* the backend, which
  // would destroy the only reason that fixture exists. Copy the directory.
  let ref_format = git(&work_path, &["rev-parse", "--show-ref-format"])
    .unwrap_or_default()
    .trim()
    .to_owned();
  if ref_format == "reftable" {
    copy_dir(&work_path.join(".git"), &staging)?;
    // Written directly rather than through `git config`, because this build of
    // Git can read the reftable repository but the assertion should not depend on
    // that.
    let cfg = staging.join("config");
    let existing = std::fs::read_to_string(&cfg).unwrap_or_default();
    if !existing.contains("bare = true") {
      std::fs::write(&cfg, existing.replace("bare = false", "bare = true"))?;
    }
  } else {
    let staging_s = staging.to_string_lossy().into_owned();
    git(&work_path, &["clone", "-q", "--bare", ".", &staging_s])?;
  }
  if std::fs::rename(&staging, &bare_path).is_err() {
    let _ = std::fs::remove_dir_all(&staging);
  }
  Ok(bare_path)
}

pub fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
  std::fs::create_dir_all(dst)?;
  for entry in std::fs::read_dir(src)? {
    let entry = entry?;
    let to = dst.join(entry.file_name());
    let ft = entry.file_type()?;
    if ft.is_dir() {
      copy_dir(&entry.path(), &to)?;
    } else if ft.is_symlink() {
      let target = std::fs::read_link(entry.path())?;
      let _ = std::os::unix::fs::symlink(target, &to);
    } else {
      std::fs::copy(entry.path(), to)?;
    }
  }
  Ok(())
}
