//! The M0.3 fixture matrix.
//!
//! Built with stock Git so that libgit2 is always the thing under test and
//! never also the thing producing the input. Every fixture exists because some
//! row of the PLAN.md section 12 test matrix would otherwise be untested.

use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Fixture {
    pub name: &'static str,
    /// Why this fixture exists. Printed in the report so a failing row explains
    /// itself without a reader having to reverse-engineer the setup.
    pub rationale: &'static str,
    pub build: fn(&Path) -> Result<()>,
}

pub const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "empty",
        rationale: "no commits; unborn HEAD must not be reported as an error state",
        build: build_empty,
    },
    Fixture {
        name: "basic",
        rationale: "baseline trees, branches, lightweight and annotated tags",
        build: build_basic,
    },
    Fixture {
        name: "modes",
        rationale: "executable bit, symlinks (relative/absolute/escaping), gitlink",
        build: build_modes,
    },
    Fixture {
        name: "bytes",
        rationale: "non-UTF-8, newline, quote and space in path names",
        build: build_bytes,
    },
    Fixture {
        name: "content",
        rationale: "empty, CRLF, no final newline, NUL bytes, huge line, large blob",
        build: build_content,
    },
    Fixture {
        name: "bigdir",
        rationale: "5000 entries in one tree; directory pagination and readdir cost",
        build: build_bigdir,
    },
    Fixture {
        name: "deep",
        rationale: "40 nested path components; per-component tree traversal",
        build: build_deep,
    },
    Fixture {
        name: "packed",
        rationale: "all objects and refs packed; the normal server-side shape",
        build: build_packed,
    },
    Fixture {
        name: "reftable",
        rationale: "reftable ref backend; DESIGN.md 5.1 claims libgit2 cannot read it",
        build: build_reftable,
    },
    Fixture {
        name: "sha256",
        rationale: "SHA-256 object format; libgit2 support is experimental",
        build: build_sha256,
    },
    Fixture {
        name: "attrs",
        rationale: ".gitattributes text/eol and an LFS pointer; the mount serves raw bytes",
        build: build_attrs,
    },
];

/// Run git with a fixed, hermetic environment.
///
/// The user's real `~/.gitconfig` must not reach a fixture: `core.autocrlf` or
/// an `init.defaultBranch` set on this machine would silently change what the
/// conformance checks see, and the results have to be reproducible elsewhere.
pub fn git(dir: &Path, args: &[&OsStr]) -> Result<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "XVFS Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@xvfs.invalid")
        .env("GIT_COMMITTER_NAME", "XVFS Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@xvfs.invalid")
        // Fixed timestamps keep object IDs stable across runs, which makes a
        // diff of two report files meaningful.
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

fn g(dir: &Path, args: &[&str]) -> Result<String> {
    let owned: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    git(dir, &owned)
}

fn init(dir: &Path, extra: &[&str]) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut args = vec!["init", "-q", "--initial-branch=main"];
    args.extend_from_slice(extra);
    args.push(".");
    g(dir, &args)?;
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
    g(dir, &["add", "-A", "."])?;
    g(dir, &["commit", "-q", "-m", msg])?;
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
    g(dir, &["tag", "v1.0"])?;

    write(dir, "src/main.rs", b"fn main() { println!(\"bye\"); }\n")?;
    write(dir, "src/new.rs", b"pub fn added() {}\n")?;
    std::fs::remove_file(dir.join("docs/guide.md"))?;
    commit_all(dir, "second")?;
    g(dir, &["tag", "-a", "v2.0", "-m", "annotated release"])?;
    g(dir, &["branch", "feature"])?;
    Ok(())
}

fn build_modes(dir: &Path) -> Result<()> {
    init(dir, &[])?;
    write(dir, "plain.txt", b"plain\n")?;
    write(dir, "script.sh", b"#!/bin/sh\necho hi\n")?;
    g(dir, &["add", "-A", "."])?;
    g(dir, &["update-index", "--chmod=+x", "script.sh"])?;

    std::os::unix::fs::symlink("plain.txt", dir.join("rel-link"))?;
    std::os::unix::fs::symlink("/etc/passwd", dir.join("abs-link"))?;
    std::os::unix::fs::symlink("../../../etc/shadow", dir.join("escape-link"))?;
    std::os::unix::fs::symlink("loop-b", dir.join("loop-a"))?;
    std::os::unix::fs::symlink("loop-a", dir.join("loop-b"))?;
    g(dir, &["add", "-A", "."])?;

    // A gitlink without materializing a real submodule. `--cacheinfo` writes the
    // 160000 entry directly, which is all the tree needs; XVFS never recurses
    // into it.
    g(
        dir,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000,0000000000000000000000000000000000000001,vendor/submodule",
        ],
    )?;
    g(dir, &["commit", "-q", "-m", "modes"])?;
    Ok(())
}

fn build_bytes(dir: &Path) -> Result<()> {
    init(dir, &[])?;
    // Invalid UTF-8 (lone 0xff) and a Latin-1 sequence. Real repositories have
    // these; the Linux tree has had non-UTF-8 filenames historically.
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
    // `add -A` handles these; core.quotepath only affects display, not storage.
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
    // Above the 8 MiB default content-search cutoff in DESIGN.md section 7.5.
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
    g(dir, &["gc", "-q", "--aggressive"])?;
    g(dir, &["pack-refs", "--all"])?;
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
    g(dir, &["tag", "-a", "v1.0", "-m", "annotated"])?;
    Ok(())
}

fn build_attrs(dir: &Path) -> Result<()> {
    init(dir, &[])?;
    write(
        dir,
        ".gitattributes",
        b"*.txt text eol=crlf\n*.bin -text\n*.psd filter=lfs diff=lfs merge=lfs -text\n",
    )?;
    // Stored LF in the object database; a real checkout would emit CRLF. The
    // mount serves the stored bytes, which is the divergence DESIGN.md section
    // 12 documents. This fixture is what makes that divergence testable.
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

/// Build every fixture as a worktree repo plus its bare mirror.
///
/// The bare mirror is what the server actually serves, so conformance runs
/// against the bare form; the worktree form stays for oracle comparisons that
/// need a checkout.
pub fn build_all(root: &Path, only: Option<&str>) -> Result<Vec<(String, PathBuf)>> {
    std::fs::create_dir_all(root)?;
    let mut built = Vec::new();
    for f in FIXTURES {
        if let Some(only) = only {
            if only != f.name {
                continue;
            }
        }
        let work = root.join("work").join(f.name);
        let bare = root.join("bare").join(format!("{}.git", f.name));
        if bare.exists() {
            built.push((f.name.to_string(), bare));
            continue;
        }
        if work.exists() {
            std::fs::remove_dir_all(&work)?;
        }
        (f.build)(&work).with_context(|| format!("building fixture {}", f.name))?;

        std::fs::create_dir_all(bare.parent().unwrap())?;
        // `clone --bare` from a reftable source would convert the backend, which
        // would destroy the point of that fixture. Copy the directory instead.
        let ref_format = g(&work, &["rev-parse", "--show-ref-format"])
            .unwrap_or_default()
            .trim()
            .to_string();
        if ref_format == "reftable" {
            copy_dir(&work.join(".git"), &bare)?;
            g(&bare, &["config", "core.bare", "true"])?;
        } else {
            let bare_s = bare.to_string_lossy().into_owned();
            g(&work, &["clone", "-q", "--bare", ".", &bare_s])?;
        }
        built.push((f.name.to_string(), bare));
    }
    Ok(built)
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), to)?;
        }
    }
    Ok(())
}
