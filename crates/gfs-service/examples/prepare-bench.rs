//! M4.6: what snapshot preparation costs.
//!
//! PLAN.md M4.6 asks for cold/warm branch and arbitrary-commit preparation
//! numbers. This measures the three that differ in kind:
//!
//! * **cold** — no manifest, no postings, no blob keys. A full parallel tree
//!   walk plus classification and trigram ingest of every unique blob.
//! * **warm** — the same commit again. Must be a claim lookup and nothing else;
//!   ADR 0004's "repeated preparation does not rebuild" claim, timed.
//! * **arbitrary commit** — an ancestor, prepared on demand against an index
//!   that already holds the tip. Most of its blobs are already interned, so this
//!   is the number that decides whether on-demand search for any commit is
//!   affordable, which is the claim ADR 0004 decision 1 rests on.
//!
//! An on-disk index, not an in-memory one: `Server::with_search_index` is what
//! the real binary uses, and an in-memory measurement would leave out every
//! SQLite write the real path pays for.
//!
//! ```sh
//! cargo run --release --example prepare-bench -- ~/gfs-corpus/mirrors/vscode.git main
//! ```
//!
//! The third argument is how far back the arbitrary commit is (default 100).

use std::sync::Arc;
use std::time::{Duration, Instant};

use gfs_service::auth::{AllowList, CapabilityKey, StaticTokens};
use gfs_service::catalog::repositories::NewRepository;
use gfs_service::{Catalog, PrepareOutcome, Server};
use gfs_types::{
  DisplayName, HashAlgorithm, LeasePolicy, ObjectId, RepositoryId, RevisionSelector, SubjectId,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
  let mut args = std::env::args().skip(1);
  let repo_path: std::path::PathBuf = args
    .next()
    .ok_or("usage: prepare-bench <repo.git> <revision> [depth]")?
    .into();
  let revision = args.next().unwrap_or_else(|| "main".to_owned());
  let depth: usize = args.next().map(|d| d.parse()).transpose()?.unwrap_or(100);

  let index_dir = tempfile::tempdir()?;
  let server = build_server(&repo_path, &index_dir.path().join("search.sqlite"))?;
  let repo_id = RepositoryId::parse("r-bench").unwrap();

  let tip = server
    .registry
    .repository(&repo_id)?
    .resolve(RevisionSelector::parse(&revision, HashAlgorithm::Sha1)?)
    .await?
    .commit;

  println!("# Snapshot preparation");
  println!();
  println!("repository: {}", repo_path.display());
  println!(
    "revision:   {revision} ({})",
    tip.to_hex().chars().take(12).collect::<String>()
  );
  println!();

  // Cold. The index directory is empty, so nothing below is a cache hit.
  let cold = time(&server, &repo_id, &tip).await?;
  report("cold branch tip", cold);

  // Warm. Same commit, same process, index now populated.
  let warm = time(&server, &repo_id, &tip).await?;
  report("warm branch tip", warm);

  // An ancestor. Its blobs mostly exist already; its manifest does not.
  match ancestor(&repo_path, &revision, depth) {
    Ok(older) => {
      let arbitrary = time(&server, &repo_id, &older).await?;
      report(&format!("arbitrary commit (HEAD~{depth})"), arbitrary);
    }
    Err(e) => println!("arbitrary commit: skipped ({e})"),
  }

  // Warm search, as a distribution rather than one sample. DESIGN.md section 13
  // states the target as a **p95**, so a single query cannot answer it however
  // fast it is: the tail is the claim.
  println!();
  println!("warm search over {} queries:", QUERIES.len());
  let mut latencies = Vec::new();
  let mut total_matches = 0usize;
  let mut total_candidates = 0u64;
  let mut eligible = 0u64;
  for (pattern, literal, case_insensitive) in QUERIES {
    let started = Instant::now();
    let result = server
      .search
      .search(
        &repo_id,
        &tip,
        gfs_search::Query {
          pattern: (*pattern).to_owned(),
          literal: *literal,
          case_insensitive: *case_insensitive,
          ..Default::default()
        },
      )
      .await?;
    latencies.push(started.elapsed());
    total_matches += result.matches.len();
    total_candidates += result.completion.candidates_considered;
    eligible = result.completion.coverage.eligible_paths;
  }
  latencies.sort();
  println!("  eligible paths     {eligible:>9}");
  println!("  p50                {:>9}", ms(percentile(&latencies, 50)));
  println!("  p95                {:>9}", ms(percentile(&latencies, 95)));
  println!("  max                {:>9}", ms(*latencies.last().unwrap()));
  println!("  matches (total)    {total_matches:>9}");
  println!("  candidate blobs    {total_candidates:>9}");

  // The index on disk, against ADR 0004's projection. Measured after both
  // snapshots exist, so it covers two manifests rather than one: what matters is
  // how it grows per retained snapshot, and one manifest cannot show that. Pass
  // a depth of 0 to measure a single snapshot and take the difference.
  //
  // Close the index before measuring it. SQLite checkpoints the write-ahead log
  // into the database on the last connection close, and a `-wal` read while the
  // store is still open is whatever has not been folded in yet -- a transient,
  // not the steady-state cost ADR 0004 priced. On vscode the difference is 70
  // MiB against 79, so measuring the open database would have reported nearly
  // double.
  drop(server);
  tokio::time::sleep(Duration::from_millis(500)).await;

  println!();
  println!("index on disk, after checkpoint:");
  report_index(index_dir.path());

  Ok(())
}

/// The search index's files, listed rather than totalled.
///
/// Per file because the `-wal` is not steady-state cost: it is whatever a
/// checkpoint has not folded in yet, and a single total would report a
/// transient as though it were the storage this design was chosen for. ADR 0004
/// priced the steady state, so the steady state has to be separable here.
fn report_index(dir: &std::path::Path) {
  let mut files: Vec<(String, u64)> = std::fs::read_dir(dir)
    .into_iter()
    .flatten()
    .flatten()
    .filter_map(|entry| {
      let name = entry.file_name().to_string_lossy().into_owned();
      entry.metadata().ok().map(|m| (name, m.len()))
    })
    .collect();
  files.sort();
  for (name, bytes) in files {
    println!("  {name:<20} {:>9.1} MiB", bytes as f64 / (1024.0 * 1024.0));
  }
}

/// The query mix, as `(pattern, literal, case_insensitive)`.
///
/// Fixed rather than harvested from the repository, so the same run on two
/// machines compares. Chosen to span what actually costs different amounts: a
/// literal appearing everywhere reads many candidate blobs, a rare one reads
/// almost none, a three-byte literal is the shortest the trigram index can bound
/// at all, and a regex whose literal is short leans on the matcher rather than
/// on the index. `zzq...` is here because a query that finds nothing must also
/// be fast — that is the shape an agent issues most and notices least.
const QUERIES: &[(&str, bool, bool)] = &[
  ("authorize", true, false),
  ("static", true, false),
  ("return", true, false),
  ("const", true, false),
  ("buffer", true, false),
  ("Result", true, false),
  ("initialize", true, false),
  ("TODO", true, false),
  ("mutex", true, false),
  ("zzq-no-such-symbol-anywhere", true, false),
  ("Authorize", true, true),
  ("MUTEX", true, true),
  ("get", true, false),
  ("register_", true, false),
  ("_handler", true, false),
  (r"struct\s+\w+", false, false),
  (r"return\s+NULL", false, false),
  ("alloc|free", false, false),
  ("^static", false, false),
  ("init.*device", false, false),
];

/// The `p`-th percentile of a sorted slice, nearest-rank.
fn percentile(sorted: &[Duration], p: usize) -> Duration {
  if sorted.is_empty() {
    return Duration::ZERO;
  }
  let rank = (p * sorted.len()).div_ceil(100).max(1);
  sorted[rank - 1]
}

fn build_server(
  repo_path: &std::path::Path,
  index_path: &std::path::Path,
) -> Result<Arc<Server>, Box<dyn std::error::Error>> {
  let catalog = Arc::new(Catalog::open_in_memory()?);
  let repo_id = RepositoryId::parse("r-bench").unwrap();
  catalog.create_repository(&NewRepository {
    repository_id: repo_id.clone(),
    display_name: DisplayName::parse("bench/repo").unwrap(),
    repo_path: repo_path.to_path_buf(),
    algorithm: HashAlgorithm::Sha1,
    upstream_url: None,
    credential_ref: None,
  })?;
  let subject = SubjectId::parse("job-bench").unwrap();
  let server = Server::new(
    catalog,
    Arc::new(StaticTokens::new().with_token("t", subject.clone())),
    Arc::new(AllowList::new().allow(&subject, &repo_id)),
    CapabilityKey::generate()?,
    LeasePolicy::adr_0006(),
  )
  .with_search_index(index_path)?;
  server.registry.activate(&repo_id)?;
  Ok(Arc::new(server))
}

/// Prepare, waiting out a build that outlives the RPC deadline.
///
/// `prepare` returns `Building` rather than blocking, because a client's
/// deadline must not decide whether a build survives. A benchmark wants the
/// whole thing, so it polls.
async fn time(
  server: &Arc<Server>,
  repo_id: &RepositoryId,
  commit: &ObjectId,
) -> Result<Duration, Box<dyn std::error::Error>> {
  let started = Instant::now();
  loop {
    match server.search.prepare(repo_id, commit, true).await? {
      PrepareOutcome::Ready(_) => return Ok(started.elapsed()),
      PrepareOutcome::Building { .. } => {
        tokio::time::sleep(Duration::from_millis(200)).await;
      }
      PrepareOutcome::Failed { reason } => return Err(reason.into()),
    }
  }
}

/// An ancestor, resolved by `git` rather than by GFS.
///
/// [`RevisionSelector`] has no `~N` grammar on purpose — DESIGN.md keeps the
/// selector surface to refs and object IDs — so the benchmark asks `git` for the
/// object ID and then hands GFS the one form it does take. That also keeps the
/// commit being measured independent of anything GFS believes about history.
fn ancestor(
  repo_path: &std::path::Path,
  revision: &str,
  depth: usize,
) -> Result<ObjectId, Box<dyn std::error::Error>> {
  let output = std::process::Command::new("git")
    .arg("-C")
    .arg(repo_path)
    .arg("rev-parse")
    .arg(format!("{revision}~{depth}"))
    .output()?;
  if !output.status.success() {
    return Err(String::from_utf8_lossy(&output.stderr).trim().into());
  }
  let hex = String::from_utf8(output.stdout)?;
  Ok(ObjectId::from_hex(HashAlgorithm::Sha1, hex.trim())?)
}

fn report(label: &str, elapsed: Duration) {
  println!("{label:<30} {:>9}", ms(elapsed));
}

fn ms(d: Duration) -> String {
  if d.as_secs() >= 1 {
    format!("{:.2} s", d.as_secs_f64())
  } else {
    format!("{} ms", d.as_millis())
  }
}
