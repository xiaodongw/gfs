//! Spike: what does raw Git cost against a projected object store?
//!
//! ADR 0005 rejected a real `.git` in the mount because `git status` stats every
//! index entry — 94 850 first-time FUSE lookups on the Linux kernel. That number
//! was measured with Git's defaults, and `spikes/reports/m05-git-surface.md`
//! records the gap this spike closes: "`core.untrackedCache` and
//! `core.fsmonitor` can reduce the sweep substantially and were not evaluated;
//! neither is available through a FUSE mount without further work, which is why
//! they do not change the decision."
//!
//! `core.fsmonitor` is a hook program and GFS's overlay journal is exactly the
//! modified-path set it asks for, so the second clause is the one worth testing.
//!
//! The shape under test is stock Git throughout, not new invention:
//!
//! * the agent's `.git` is **local disk** — `HEAD`, `refs/`, `index`, and any
//!   object it writes — which is what `git worktree` already does with its
//!   per-worktree state directory;
//! * its object database is `objects/info/alternates` pointing at a read-only
//!   projection of the gateway's, which is what `git clone --shared` and
//!   worktrees already do;
//! * the working tree is the projection.
//!
//! This binary mounts the projection and runs commands against it, reporting per
//! command what the filesystem was asked for, bucketed by whether it landed on
//! the working tree or the object store. The driver is `measure.sh`.

mod fs;

use anyhow::{bail, Context, Result};
use clap::Parser;
use fs::{CounterReport, PassthroughFs, Shared};

use fuser::{Config, MountOption, SessionACL};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

#[derive(Parser)]
#[command(about = "GFS spike: raw Git over a projected object store")]
struct Cli {
  /// The directory to project. The driver stages `tree/` and `objects/` in it.
  #[arg(long)]
  lower: PathBuf,
  /// Where to mount the projection.
  #[arg(long)]
  mnt: PathBuf,
  /// Entry and attribute TTL. Long by default, as DESIGN.md section 8.2
  /// specifies for an immutable base.
  #[arg(long, default_value_t = 60)]
  ttl: u64,
  /// Report every entry with this Unix timestamp for atime/mtime/ctime, the way
  /// a real mount reports `snapshot_time`. Omit to pass the lower file's own
  /// mtime through.
  #[arg(long)]
  snapshot_time: Option<i64>,
  /// A command to run against the mount, repeatable. Counters are reset before
  /// each one, so run number two of the same command is the warm number.
  #[arg(long = "run")]
  runs: Vec<String>,
  /// `KEY=VALUE` in every command's environment, repeatable. This is where
  /// `GIT_DIR` and `GIT_WORK_TREE` belong: putting them in the command string
  /// makes every measurement's label a path prefix instead of the command.
  #[arg(long = "env")]
  envs: Vec<String>,
  /// Fail working-tree reads with `EDQUOT` past this many bytes, modelling
  /// DESIGN.md section 8.4's hard hydration budget. Zero disables it.
  #[arg(long, default_value_t = 0)]
  worktree_budget: u64,
  /// Written to the mount point path as `$GFS_MNT` for each command.
  #[arg(long)]
  json: Option<PathBuf>,
}

#[derive(serde::Serialize)]
struct RunReport {
  command: String,
  exit_code: i32,
  wall_ms: u128,
  stdout_tail: String,
  ops: HashMap<String, CounterReport>,
}

#[derive(serde::Serialize)]
struct Report {
  lower: String,
  mnt: String,
  ttl_seconds: u64,
  runs: Vec<RunReport>,
}

fn main() -> Result<()> {
  let cli = Cli::parse();
  if cli.runs.is_empty() {
    bail!("nothing to measure: pass at least one --run");
  }
  std::fs::create_dir_all(&cli.mnt).context("create mount point")?;
  let lower = cli.lower.canonicalize().context("canonicalize --lower")?;
  let mnt = cli.mnt.canonicalize().context("canonicalize --mnt")?;

  let snapshot_time = cli
    .snapshot_time
    .map(|t| UNIX_EPOCH + Duration::from_secs(t.max(0) as u64));
  let shared = Arc::new(Shared::new(lower.clone(), cli.ttl, snapshot_time, cli.worktree_budget));
  // `Config` is #[non_exhaustive] in fuser 0.18, so it is built by mutation.
  let mut config = Config::default();
  config.mount_options = vec![
    MountOption::FSName("gfs-projection".into()),
    MountOption::RO,
    MountOption::NoSuid,
    MountOption::NoDev,
    MountOption::NoAtime,
  ];
  config.acl = SessionACL::Owner;
  // Git reads packfiles from several threads and `status` is parallel by
  // default; one event-loop thread would serialize the mount and make the wall
  // times a measurement of this probe rather than of Git.
  config.n_threads = Some(8);
  config.clone_fd = true;
  let session = fuser::spawn_mount(
    PassthroughFs {
      shared: Arc::clone(&shared),
    },
    &mnt,
    &config,
  )
  .context("mount the projection")?;

  let mut runs = Vec::new();
  for command in &cli.runs {
    // Reset *after* the mount has settled, so the mount's own startup traffic
    // is not billed to the first command.
    shared.reset();
    let started = Instant::now();
    let mut proc = std::process::Command::new("sh");
    proc
      .arg("-c")
      .arg(command)
      .env("GFS_MNT", &mnt)
      .env("GFS_LOWER", &lower);
    for kv in &cli.envs {
      let (k, v) = kv
        .split_once('=')
        .with_context(|| format!("--env wants KEY=VALUE, got {kv:?}"))?;
      proc.env(k, v);
    }
    let out = proc.output().with_context(|| format!("run {command:?}"))?;
    let wall_ms = started.elapsed().as_millis();
    let ops = shared.report();
    let mut stdout_tail = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if stdout_tail.len() > 400 {
      // Keep the *end*: a command's answer is usually its last line, and an
      // unbounded capture would make the report unreadable.
      stdout_tail = format!("...{}", &stdout_tail[stdout_tail.len() - 400..]);
    }
    if !out.status.success() {
      let mut err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
      if err.len() > 300 {
        err.truncate(300);
      }
      stdout_tail = format!("{stdout_tail}\n[stderr] {err}");
      println!("    [stderr] {err}");
    }
    println!(
      "{:<58} {:>6} ms  exit {}",
      truncate(command, 58),
      wall_ms,
      out.status.code().unwrap_or(-1)
    );
    for class in [
      "worktree",
      "pack_data",
      "pack_idx",
      "loose_object",
      "git_meta",
    ] {
      if let Some(c) = ops.get(class) {
        if c.lookup + c.getattr + c.read + c.readdir + c.readlink == 0 {
          continue;
        }
        println!(
          "    {class:<13} lookup {:>7} (enoent {:>6})  getattr {:>7}  open {:>6}  \
           read {:>6} = {:>10} B  readdir {:>5} ({} entries)",
          c.lookup,
          c.lookup_enoent,
          c.getattr,
          c.open,
          c.read,
          c.read_bytes,
          c.readdir,
          c.readdir_entries
        );
        // What a chunked fetcher would have to download for the same reads. The
        // gap between this and `read_bytes` is the amplification a chunk size
        // costs, which is the whole question when the access pattern is a binary
        // search rather than a scan.
        if c.read_bytes > 0 {
          let cols: Vec<String> = fs::CHUNK_SIZES
            .iter()
            .zip(&c.chunked_bytes)
            .map(|(size, bytes)| {
              format!(
                "{}={} MiB (x{:.1})",
                human_chunk(*size),
                bytes / 1048576,
                *bytes as f64 / c.read_bytes as f64
              )
            })
            .collect();
          println!("    {:<13} chunked: {}", "", cols.join("  "));
        }
      }
    }
    runs.push(RunReport {
      command: command.clone(),
      exit_code: out.status.code().unwrap_or(-1),
      wall_ms,
      stdout_tail,
      ops,
    });
  }

  // Unmount explicitly rather than dropping: an orphaned mount is the failure
  // M0.2 recorded as needing a manual `fusermount3 -u`, and a measurement
  // harness that leaves one behind poisons the next run.
  session
    .umount_and_join()
    .context("unmount the projection")?;
  let report = Report {
    lower: lower.display().to_string(),
    mnt: mnt.display().to_string(),
    ttl_seconds: cli.ttl,
    runs,
  };
  if let Some(path) = &cli.json {
    std::fs::write(path, serde_json::to_string_pretty(&report)?)
      .with_context(|| format!("write {}", path.display()))?;
  }
  Ok(())
}

fn human_chunk(bytes: u64) -> String {
  if bytes >= 1 << 20 {
    format!("{}M", bytes >> 20)
  } else {
    format!("{}K", bytes >> 10)
  }
}

fn truncate(s: &str, n: usize) -> String {
  if s.len() <= n {
    s.to_owned()
  } else {
    format!("{}...", &s[..n - 3])
  }
}
