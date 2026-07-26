//! M0.3 Git integration validation probe.
//!
//! Answers, with measurements rather than documentation: what does the pinned
//! libgit2 build actually do with the repository shapes XVFS intends to host,
//! and does it agree with stock Git.

// The `GitRepository` trait and the algorithm-generic types are a proof of
// concept for M1.1's `xvfs-types`/`xvfs-git`, so parts of their surface are
// deliberately built out ahead of a caller in this probe.
#![allow(dead_code)]

mod checks;
mod fixtures;
mod gitrepo;
mod model;

use anyhow::{Context, Result};
use checks::Outcome;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "XVFS M0.3 libgit2 / stock Git conformance probe")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Report the pinned toolchain versions and build configuration.
    Versions,
    /// Build the fixture matrix.
    Fixtures {
        #[arg(long, default_value = "fixtures")]
        root: PathBuf,
        #[arg(long)]
        only: Option<String>,
    },
    /// Run the conformance matrix and emit a report.
    Conformance {
        #[arg(long, default_value = "fixtures")]
        root: PathBuf,
        #[arg(long)]
        only: Option<String>,
        #[arg(long)]
        json: Option<PathBuf>,
    },
    /// Run the checks against an existing repository, e.g. a corpus mirror.
    Repo {
        path: PathBuf,
        #[arg(long)]
        json: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Versions => versions(),
        Cmd::Fixtures { root, only } => {
            for (name, path) in fixtures::build_all(&root, only.as_deref())? {
                println!("{name:12} {}", path.display());
            }
            Ok(())
        }
        Cmd::Conformance { root, only, json } => {
            let built = fixtures::build_all(&root, only.as_deref())?;
            let mut results = Vec::new();
            for (name, path) in &built {
                results.extend(checks::run_all(name, path));
            }
            report(&results, json.as_deref())
        }
        Cmd::Repo { path, json } => {
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "repo".into());
            let results = checks::run_all(&name, &path);
            report(&results, json.as_deref())
        }
    }
}

fn versions() -> Result<()> {
    let v = git2::Version::get();
    let (major, minor, patch) = v.libgit2_version();
    let git = std::process::Command::new("git").arg("--version").output()?;

    println!("libgit2              {major}.{minor}.{patch}");
    println!("stock git            {}", String::from_utf8_lossy(&git.stdout).trim());
    println!();
    println!("libgit2 build configuration");
    println!("  threads            {}", v.threads());
    println!("  https              {}", v.https());
    println!("  ssh                {}", v.ssh());
    println!("  nsec               {}", v.nsec());
    println!(
        "  experimental sha256  {}",
        if gitrepo::build_has_sha256() {
            "yes"
        } else {
            "no  (default build: SHA-256 repositories are rejected at ingest)"
        }
    );
    Ok(())
}

fn report(results: &[checks::CheckResult], json: Option<&std::path::Path>) -> Result<()> {
    let width = results
        .iter()
        .map(|r| r.fixture.len())
        .max()
        .unwrap_or(8)
        .max(8);

    let mut current = "";
    for r in results {
        if r.fixture != current {
            current = &r.fixture;
            println!();
        }
        let mark = match r.outcome {
            Outcome::Pass => "PASS  ",
            Outcome::Fail => "FAIL  ",
            Outcome::ExpectedReject => "REJECT",
            Outcome::Skipped => "skip  ",
        };
        println!("{mark} {:width$}  {:28}  {}", r.fixture, r.check, r.detail);
    }

    let count = |o: Outcome| results.iter().filter(|r| r.outcome == o).count();
    let fail = count(Outcome::Fail);
    println!(
        "\n{} passed, {fail} failed, {} expected rejections, {} skipped",
        count(Outcome::Pass),
        count(Outcome::ExpectedReject),
        count(Outcome::Skipped)
    );

    if let Some(path) = json {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(path, serde_json::to_string_pretty(results)?)
            .with_context(|| format!("writing {}", path.display()))?;
        println!("json report: {}", path.display());
    }

    // A failing matrix exits non-zero so CI can gate on it.
    if fail > 0 {
        std::process::exit(1);
    }
    Ok(())
}
