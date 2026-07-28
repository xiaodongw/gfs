//! M0.4 search representation spike.
//!
//! Produces the number the M0.4 exit gate turns on: steady-state manifest bytes
//! per retained snapshot, projected over realistic commit churn. Index build
//! time is reported too, but PLAN.md is explicit that it is not what decides
//! whether on-demand search for arbitrary commits is affordable.

mod index;
mod query;
mod tantivy_cmp;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use index::{BlobRegistry, TrigramIndex};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Parser)]
#[command(about = "GFS M0.4 search representation probe")]
struct Cli {
  #[command(subcommand)]
  cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
  /// Build the index for one snapshot and report size and time.
  Build {
    #[arg(long)]
    repo: PathBuf,
    #[arg(long, default_value = "HEAD")]
    rev: String,
    #[arg(long)]
    json: Option<PathBuf>,
  },
  /// Build manifests for N successive first-parent commits and project
  /// steady-state retained storage. This is the exit-gate measurement.
  Manifests {
    #[arg(long)]
    repo: PathBuf,
    #[arg(long, default_value = "HEAD")]
    rev: String,
    #[arg(long, default_value_t = 20)]
    commits: usize,
    /// How many snapshots a plausible workload retains at once.
    #[arg(long, default_value_t = 200)]
    retained: usize,
    #[arg(long)]
    json: Option<PathBuf>,
  },
  /// Run a query against a snapshot and print the completion contract.
  Query {
    #[arg(long)]
    repo: PathBuf,
    #[arg(long, default_value = "HEAD")]
    rev: String,
    pattern: String,
    #[arg(long)]
    regex: bool,
    #[arg(long)]
    path_prefix: Option<String>,
    #[arg(long, default_value_t = 50)]
    limit: usize,
    #[arg(long)]
    require_exhaustive: bool,
  },
  /// Compare results against ripgrep over a raw materialized tree.
  Verify {
    #[arg(long)]
    repo: PathBuf,
    #[arg(long, default_value = "HEAD")]
    rev: String,
    #[arg(long)]
    rg: Option<PathBuf>,
    #[arg(long, num_args = 1..)]
    patterns: Vec<String>,
  },
  /// Build a per-snapshot Tantivy index for size and time comparison.
  Tantivy {
    #[arg(long)]
    repo: PathBuf,
    #[arg(long, default_value = "HEAD")]
    rev: String,
  },
}

fn open(repo: &Path) -> Result<git2::Repository> {
  git2::Repository::open_bare(repo)
    .or_else(|_| git2::Repository::open(repo))
    .with_context(|| format!("opening {}", repo.display()))
}

fn resolve(repo: &git2::Repository, rev: &str) -> Result<git2::Oid> {
  Ok(repo.revparse_single(rev)?.peel_to_commit()?.id())
}

fn mib(b: u64) -> f64 {
  b as f64 / 1048576.0
}

fn write_json(p: &Path, v: &serde_json::Value) -> Result<()> {
  if let Some(parent) = p.parent() {
    std::fs::create_dir_all(parent).ok();
  }
  std::fs::write(p, serde_json::to_string_pretty(v)?)?;
  println!("\njson report: {}", p.display());
  Ok(())
}

fn main() -> Result<()> {
  match Cli::parse().cmd {
    Cmd::Build { repo, rev, json } => build(&repo, &rev, json),
    Cmd::Manifests {
      repo,
      rev,
      commits,
      retained,
      json,
    } => manifests(&repo, &rev, commits, retained, json),
    Cmd::Query {
      repo,
      rev,
      pattern,
      regex,
      path_prefix,
      limit,
      require_exhaustive,
    } => run_query(
      &repo,
      &rev,
      &pattern,
      regex,
      path_prefix,
      limit,
      require_exhaustive,
    ),
    Cmd::Verify {
      repo,
      rev,
      rg,
      patterns,
    } => verify(&repo, &rev, rg, &patterns),
    Cmd::Tantivy { repo, rev } => tantivy_cmp::build(&repo, &rev),
  }
}

fn build(repo_path: &Path, rev: &str, json: Option<PathBuf>) -> Result<()> {
  let repo = open(repo_path)?;
  let oid = resolve(&repo, rev)?;
  let mut registry = BlobRegistry::default();
  let mut idx = TrigramIndex::default();

  let t = Instant::now();
  let (manifest, stats) = index::build_snapshot(&repo, oid, &mut registry, &mut idx, true)?;
  let total_ms = t.elapsed().as_secs_f64() * 1000.0;

  let storage = manifest.storage();
  let postings = idx.serialized_bytes();
  let excl_total: usize = stats.excluded.values().sum();

  println!("commit                {}", &manifest.commit[..12]);
  println!("tip file entries      {}", stats.entries);
  println!("unique blobs          {}", registry.len());
  println!(
    "indexed blobs         {}  ({:.1} MiB of content)",
    stats.indexed_blobs,
    mib(stats.indexed_bytes)
  );
  println!(
    "excluded blobs        {excl_total}  ({:.2}% of unique blobs, {:.1} MiB)",
    100.0 * excl_total as f64 / registry.len().max(1) as f64,
    mib(stats.excluded_bytes)
  );
  for (reason, n) in &stats.excluded {
    println!("  {reason:?}: {n}");
  }
  println!();
  println!("tree walk             {:.0} ms", stats.walk_ms);
  println!("index build           {:.0} ms", stats.index_ms);
  println!("total                 {total_ms:.0} ms");
  println!();
  println!(
    "trigram postings      {:.1} MiB ({} distinct trigrams)",
    mib(postings),
    idx.postings.len()
  );
  println!("manifest path table   {:.2} MiB", mib(storage.path_table));
  println!(
    "manifest bitmap       {:.1} KiB",
    storage.bitmap as f64 / 1024.0
  );
  println!(
    "manifest reverse      {:.2} MiB",
    mib(storage.reverse_table)
  );
  println!("manifest TOTAL        {:.2} MiB", mib(storage.total()));
  println!();
  println!(
    "posting bytes per byte of indexed content: {:.2}",
    postings as f64 / stats.indexed_bytes.max(1) as f64
  );

  if let Some(p) = json {
    write_json(
      &p,
      &serde_json::json!({
          "commit": manifest.commit,
          "entries": stats.entries,
          "unique_blobs": registry.len(),
          "indexed_blobs": stats.indexed_blobs,
          "indexed_bytes": stats.indexed_bytes,
          "excluded_blobs": excl_total,
          "excluded_bytes": stats.excluded_bytes,
          "walk_ms": stats.walk_ms,
          "index_ms": stats.index_ms,
          "total_ms": total_ms,
          "postings_bytes": postings,
          "distinct_trigrams": idx.postings.len(),
          "manifest": storage,
      }),
    )?;
  }
  Ok(())
}

/// The exit-gate measurement: what retained snapshots cost at steady state.
fn manifests(
  repo_path: &Path,
  rev: &str,
  commits: usize,
  retained: usize,
  json: Option<PathBuf>,
) -> Result<()> {
  let repo = open(repo_path)?;
  let head = resolve(&repo, rev)?;

  // First-parent history: what a branch's snapshot sequence actually looks
  // like, where each tip differs from the last by one commit or merge.
  let mut chain = Vec::new();
  let mut c = repo.find_commit(head)?;
  for _ in 0..commits {
    chain.push(c.id());
    match c.parent(0) {
      Ok(p) => c = p,
      Err(_) => break,
    }
  }
  chain.reverse();

  let mut registry = BlobRegistry::default();
  let mut idx = TrigramIndex::default();
  let mut rows = Vec::new();
  let mut prev: Option<roaring::RoaringBitmap> = None;

  println!("| # | commit | entries | new blobs | manifest MiB | changed keys | build ms |");
  println!("| ---: | --- | ---: | ---: | ---: | ---: | ---: |");
  for (i, oid) in chain.iter().enumerate() {
    let t = Instant::now();
    // Content indexing is off: this measures manifest storage and tree-walk
    // cost. Inflating every blob of every commit would dominate the timing
    // without changing a single manifest byte.
    let (m, stats) = index::build_snapshot(&repo, *oid, &mut registry, &mut idx, false)?;
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let storage = m.storage();
    let changed = match &prev {
      Some(p) => (&m.members ^ p).len(),
      None => m.members.len(),
    };
    println!(
      "| {} | {} | {} | {} | {:.2} | {} | {:.0} |",
      i + 1,
      &m.commit[..12],
      stats.entries,
      stats.new_blobs,
      mib(storage.total()),
      changed,
      ms
    );
    rows.push((storage, ms));
    prev = Some(m.members);
  }

  let n = rows.len().max(1) as f64;
  let mean = |f: fn(&index::ManifestStorage) -> u64| -> f64 {
    rows.iter().map(|(s, _)| f(s) as f64).sum::<f64>() / n
  };
  let avg_total = mean(|s| s.total());
  let avg_ms: f64 = rows.iter().map(|(_, m)| m).sum::<f64>() / n;

  println!();
  println!(
    "mean manifest per snapshot   {:.2} MiB",
    avg_total / 1048576.0
  );
  println!(
    "  path table                 {:.2} MiB",
    mean(|s| s.path_table) / 1048576.0
  );
  println!(
    "  membership bitmap          {:.1} KiB",
    mean(|s| s.bitmap) / 1024.0
  );
  println!(
    "  reverse table              {:.2} MiB",
    mean(|s| s.reverse_table) / 1048576.0
  );
  println!("mean full rebuild            {avg_ms:.0} ms");
  println!();
  println!(
    "PROJECTION: {retained} concurrently retained snapshots -> {:.2} GiB of manifests",
    avg_total * retained as f64 / 1073741824.0
  );
  println!(
    "            the shared blob registry stays at {} unique blobs regardless",
    registry.len()
  );

  if let Some(p) = json {
    write_json(
      &p,
      &serde_json::json!({
          "snapshots_measured": rows.len(),
          "mean_manifest_bytes": avg_total,
          "mean_path_table_bytes": mean(|s| s.path_table),
          "mean_bitmap_bytes": mean(|s| s.bitmap),
          "mean_reverse_table_bytes": mean(|s| s.reverse_table),
          "mean_build_ms": avg_ms,
          "retained_projection": retained,
          "projected_bytes": avg_total * retained as f64,
          "unique_blobs": registry.len(),
      }),
    )?;
  }
  Ok(())
}

fn run_query(
  repo_path: &Path,
  rev: &str,
  pattern: &str,
  is_regex: bool,
  path_prefix: Option<String>,
  limit: usize,
  require_exhaustive: bool,
) -> Result<()> {
  let repo = open(repo_path)?;
  let oid = resolve(&repo, rev)?;
  let mut registry = BlobRegistry::default();
  let mut idx = TrigramIndex::default();
  let (manifest, _) = index::build_snapshot(&repo, oid, &mut registry, &mut idx, true)?;

  let input = query::SearchInput {
    pattern,
    literal: !is_regex,
    path_prefix: path_prefix.as_deref().map(str::as_bytes),
    budget: query::Budget {
      max_results: limit,
      ..Default::default()
    },
    index_generation: 1,
  };
  let outcome = query::search(&repo, &manifest, &registry, &idx, &input)?;
  let code = query::exit_code(&outcome, require_exhaustive);

  if let query::SearchOutcome::Completed(r) = &outcome {
    for m in r.matches.iter().take(20) {
      println!("{}:{}:{}: {}", m.path, m.line, m.column, m.snippet.trim());
    }
    if r.matches.len() > 20 {
      println!("... {} more", r.matches.len() - 20);
    }
    println!();
    // Both dimensions, always. Text mode puts them on stderr so they cannot
    // be mistaken for results by something parsing stdout.
    eprintln!("{}", serde_json::to_string_pretty(&r.completion)?);
    eprintln!(
      "matches={} candidates={} bytes_read={} elapsed={:.0}ms exit={code}",
      r.matches.len(),
      r.candidates_considered,
      r.bytes_read,
      r.elapsed_ms
    );
  }
  std::process::exit(code);
}

/// Correctness against ripgrep over a raw materialized tree.
fn verify(repo_path: &Path, rev: &str, rg: Option<PathBuf>, patterns: &[String]) -> Result<()> {
  let rg = rg
    .or_else(|| {
      let p = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cargo/bin/rg");
      p.exists().then_some(p)
    })
    .context("no ripgrep binary; pass --rg or run `cargo install ripgrep`")?;

  let repo = open(repo_path)?;
  let oid = resolve(&repo, rev)?;

  // Materialized with ls-tree + cat-file semantics, not `git checkout`, so
  // `.gitattributes` conversion cannot make the oracle disagree with the raw
  // bytes GFS serves. This is PLAN.md section 12's stated oracle.
  let dir = std::env::temp_dir().join(format!("gfs-verify-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir)?;
  let materialized = materialize(&repo, oid, &dir)?;
  println!("materialized {materialized} files to compare against\n");

  let mut registry = BlobRegistry::default();
  let mut idx = TrigramIndex::default();
  let (manifest, _) = index::build_snapshot(&repo, oid, &mut registry, &mut idx, true)?;

  println!("| pattern | gfs | ripgrep | agree |");
  println!("| --- | ---: | ---: | :---: |");
  let mut disagreements = 0;
  for pat in patterns {
    let input = query::SearchInput {
      pattern: pat,
      literal: true,
      path_prefix: None,
      budget: query::Budget {
        max_results: usize::MAX,
        ..Default::default()
      },
      index_generation: 1,
    };
    let ours = match query::search(&repo, &manifest, &registry, &idx, &input)? {
      query::SearchOutcome::Completed(r) => r,
      query::SearchOutcome::FailedBeforeCompletion(e) => anyhow::bail!("search failed: {e}"),
    };

    // The same corpus policy on both sides. Without `--no-ignore`, ripgrep
    // would apply .gitignore rules the server index does not, and the two
    // would be answering different questions.
    let out = std::process::Command::new(&rg)
      .args([
        "--no-messages",
        "--no-config",
        "--no-ignore",
        "--hidden",
        "--max-filesize",
        &index::MAX_INDEXED_BYTES.to_string(),
        "--fixed-strings",
        "--count-matches",
        "--no-heading",
        pat,
      ])
      .arg(&dir)
      .output()?;
    let rg_total: u64 = String::from_utf8_lossy(&out.stdout)
      .lines()
      .filter_map(|l| l.rsplit_once(':'))
      .filter_map(|(_, n)| n.trim().parse::<u64>().ok())
      .sum();

    let ours_total = ours.matches.len() as u64;
    let agree = ours_total == rg_total;
    if !agree {
      disagreements += 1;
    }
    println!(
      "| `{pat}` | {ours_total} | {rg_total} | {} |",
      if agree { "yes" } else { "**NO**" }
    );
  }
  let _ = std::fs::remove_dir_all(&dir);
  println!();
  if disagreements > 0 {
    println!("{disagreements} pattern(s) disagreed with ripgrep");
    std::process::exit(1);
  }
  println!("all patterns agree with ripgrep over the raw materialized tree");
  Ok(())
}

/// Write the raw blob bytes of a commit into a directory.
fn materialize(repo: &git2::Repository, oid: git2::Oid, dir: &Path) -> Result<usize> {
  use std::os::unix::ffi::OsStrExt;
  let tree = repo.find_commit(oid)?.tree()?;
  let odb = repo.odb()?;
  let mut files: Vec<(Vec<u8>, git2::Oid)> = Vec::new();
  tree.walk(git2::TreeWalkMode::PreOrder, |d, e| {
    if e.filemode() == 0o100644 || e.filemode() == 0o100755 {
      let mut p = d.as_bytes().to_vec();
      p.extend_from_slice(e.name_bytes());
      files.push((p, e.id()));
    }
    git2::TreeWalkResult::Ok
  })?;
  let n = files.len();
  for (path, oid) in files {
    let full = dir.join(std::ffi::OsStr::from_bytes(&path));
    if let Some(parent) = full.parent() {
      std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&full, odb.read(oid)?.data())?;
  }
  Ok(n)
}
