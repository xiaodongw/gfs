//! M0.3 smart-HTTP gateway validation.
//!
//! Proves the two subprocess contracts against real Git clients: the GET
//! advertisement with its version-dependent preamble, and the POST stateless
//! RPC. Stock `upload-pack` is the implementation, so it is never its own
//! oracle — every clone is verified with `git fsck` and compared against a
//! direct filesystem clone of the same bare repository.

mod server;
mod upload_pack;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use server::{Gateway, RunningGateway};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use upload_pack::{FilterPolicy, GitProtocol, UploadPack, UploadPackPolicy};

#[derive(Parser)]
#[command(about = "GFS M0.3 smart-HTTP gateway probe")]
struct Cli {
  #[command(subcommand)]
  cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
  /// Serve a directory of bare repositories on an ephemeral port.
  Serve {
    #[arg(long)]
    root: PathBuf,
  },
  /// Run the protocol conformance matrix against one repository.
  Check {
    /// Directory containing `<name>.git`.
    #[arg(long)]
    root: PathBuf,
    /// Repository name without the `.git` suffix.
    #[arg(long)]
    repo: String,
    #[arg(long)]
    json: Option<PathBuf>,
  },
}

fn main() -> Result<()> {
  match Cli::parse().cmd {
    Cmd::Serve { root } => {
      let g = RunningGateway::start(Gateway {
        root,
        policy: UploadPackPolicy::default(),
      })?;
      println!("listening on http://127.0.0.1:{}", g.port);
      std::thread::park();
      Ok(())
    }
    Cmd::Check { root, repo, json } => {
      let results = run_checks(&root, &repo)?;
      report(&results, json.as_deref())
    }
  }
}

// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
  Pass,
  Fail,
  /// The gateway refused, and refusing is the designed behavior.
  ExpectedReject,
  /// A confirmed, documented deviation that has an accepted decision behind
  /// it. Kept visible in the report but not a build failure, so that a real
  /// regression elsewhere is not buried under a known result.
  Finding,
}

#[derive(Debug, Clone, serde::Serialize)]
struct CheckResult {
  check: &'static str,
  outcome: Outcome,
  detail: String,
}

fn pass(check: &'static str, detail: impl Into<String>) -> CheckResult {
  CheckResult {
    check,
    outcome: Outcome::Pass,
    detail: detail.into(),
  }
}
fn fail(check: &'static str, detail: impl Into<String>) -> CheckResult {
  CheckResult {
    check,
    outcome: Outcome::Fail,
    detail: detail.into(),
  }
}
fn reject(check: &'static str, detail: impl Into<String>) -> CheckResult {
  CheckResult {
    check,
    outcome: Outcome::ExpectedReject,
    detail: detail.into(),
  }
}
fn finding(check: &'static str, detail: impl Into<String>) -> CheckResult {
  CheckResult {
    check,
    outcome: Outcome::Finding,
    detail: detail.into(),
  }
}

/// Run git with a hermetic environment so a developer's own config cannot
/// change a protocol result.
fn git(dir: &Path, args: &[&str]) -> Result<std::process::Output> {
  Ok(
    Command::new("git")
      .current_dir(dir)
      .args(args)
      .env("GIT_CONFIG_GLOBAL", "/dev/null")
      .env("GIT_CONFIG_SYSTEM", "/dev/null")
      .env("GIT_TERMINAL_PROMPT", "0")
      // Object-writing commands such as `commit-tree` need an identity, and
      // the hermetic config above removes the developer's. Without these the
      // probe's own setup fails in a way that looks like a gateway result.
      .env("GIT_AUTHOR_NAME", "GFS Probe")
      .env("GIT_AUTHOR_EMAIL", "probe@gfs.invalid")
      .env("GIT_COMMITTER_NAME", "GFS Probe")
      .env("GIT_COMMITTER_EMAIL", "probe@gfs.invalid")
      .output()?,
  )
}

fn git_ok(dir: &Path, args: &[&str]) -> Result<String> {
  let out = git(dir, args)?;
  if !out.status.success() {
    bail!(
      "git {args:?} failed: {}",
      String::from_utf8_lossy(&out.stderr).trim()
    );
  }
  Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn run_checks(root: &Path, repo_name: &str) -> Result<Vec<CheckResult>> {
  let bare = root.join(format!("{repo_name}.git"));
  anyhow::ensure!(
    bare.join("objects").is_dir(),
    "{} is not bare",
    bare.display()
  );

  let gw = RunningGateway::start(Gateway {
    root: root.to_path_buf(),
    policy: UploadPackPolicy::default(),
  })?;
  let url = gw.url(&format!("{repo_name}.git"));
  let work = tempdir()?;

  let mut out = Vec::new();
  out.extend(check_advertisement_framing(&bare)?);
  out.push(check_http_headers(&gw, repo_name)?);
  out.push(check_gzip_body(&bare)?);
  out.push(check_repo_selection(root)?);

  // An unborn HEAD is a legitimate repository state — a freshly created
  // mirror has one — and the advertisement checks above must still run for
  // it. The clone and ref checks need a commit, so they are skipped rather
  // than turned into an error.
  // `git rev-parse HEAD` on an unborn HEAD can exit zero while echoing the
  // literal string "HEAD", so the output is validated rather than the status.
  let has_commits = git(&bare, &["rev-parse", "HEAD"])
    .ok()
    .filter(|o| o.status.success())
    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    .is_some_and(|s| (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit()));
  if has_commits {
    out.extend(check_hidden_refs(&bare, &url, work.path())?);
    out.extend(check_clone_matrix(&bare, &url, work.path())?);
    out.extend(check_filter_policy(&url, work.path())?);
  } else {
    out.push(pass(
      "empty_repository",
      "unborn HEAD: advertisement served, clone/ref checks not applicable",
    ));
  }
  Ok(out)
}

/// The single subtlest part of the whole gateway.
fn check_advertisement_framing(bare: &Path) -> Result<Vec<CheckResult>> {
  let up = UploadPack::new(bare, UploadPackPolicy::default())?;
  let mut out = Vec::new();

  // v0/v1: `upload-pack --http-backend-info-refs` does NOT emit the service
  // preamble, so the gateway must add it. Omitting it makes every v0 client
  // fail; adding it under v2 breaks every v2 client. Nothing else in the
  // gateway is this easy to get wrong and this total in its effect.
  let v0 = up.advertise(GitProtocol::V0)?;
  let preamble = upload_pack::pkt_line(b"# service=git-upload-pack\n");
  let mut want = preamble.clone();
  want.extend_from_slice(b"0000");
  if v0.starts_with(&want) {
    out.push(pass(
      "advert_v0_preamble",
      "`0000` after the `# service=git-upload-pack` pkt-line, then the advertisement",
    ));
  } else {
    out.push(fail(
      "advert_v0_preamble",
      format!(
        "first bytes: {:?}",
        String::from_utf8_lossy(&v0[..32.min(v0.len())])
      ),
    ));
  }

  // v2: the body begins with upload-pack's own `version 2` pkt-line.
  let v2 = up.advertise(GitProtocol::V2)?;
  let lines = upload_pack::parse_pkt_lines(&v2);
  let first = lines
    .first()
    .map(|l| String::from_utf8_lossy(l).trim().to_string());
  if first.as_deref() == Some("version 2") && !v2.starts_with(&preamble) {
    let caps: Vec<String> = lines
      .iter()
      .skip(1)
      .map(|l| String::from_utf8_lossy(l).trim().to_string())
      .filter(|s| !s.is_empty())
      .collect();
    out.push(pass(
      "advert_v2_no_preamble",
      format!("begins with `version 2`; capabilities: {}", caps.join(" ")),
    ));
    // In protocol v2 `filter` is not a top-level capability: it is a *value*
    // of the `fetch` capability, advertised as `fetch=shallow ... filter`.
    // Looking for a standalone `filter` line finds nothing even when
    // filtering is fully enabled.
    if fetch_features(&caps).iter().any(|f| f == "filter") {
      out.push(pass(
        "advert_v2_filter_capability",
        "`filter` present in the `fetch` capability values",
      ));
    } else {
      out.push(fail(
        "advert_v2_filter_capability",
        format!(
          "`filter` missing from fetch features {:?} despite allowFilter=true",
          fetch_features(&caps)
        ),
      ));
    }
  } else {
    out.push(fail(
      "advert_v2_no_preamble",
      format!("first pkt-line was {first:?}"),
    ));
  }

  // The same repository with filtering disabled must not advertise it.
  let disabled = UploadPack::new(
    bare,
    UploadPackPolicy {
      filter: FilterPolicy::Disabled,
      ..Default::default()
    },
  )?;
  let v2d = disabled.advertise(GitProtocol::V2)?;
  let disabled_caps: Vec<String> = upload_pack::parse_pkt_lines(&v2d)
    .iter()
    .map(|l| String::from_utf8_lossy(l).trim().to_string())
    .collect();
  let has_filter = fetch_features(&disabled_caps).iter().any(|f| f == "filter");
  if has_filter {
    out.push(fail(
      "advert_filter_off_not_advertised",
      "`filter` advertised even with allowFilter=false",
    ));
  } else {
    out.push(pass(
      "advert_filter_off_not_advertised",
      "`filter` absent when policy disables it",
    ));
  }
  Ok(out)
}

/// Extract the values of the v2 `fetch` capability, e.g. `shallow`, `filter`.
fn fetch_features(caps: &[String]) -> Vec<String> {
  caps
    .iter()
    .find_map(|c| c.strip_prefix("fetch="))
    .map(|v| v.split_whitespace().map(str::to_string).collect())
    .unwrap_or_default()
}

/// Content type and cache headers, over a raw socket because a Git client would
/// not report them.
fn check_http_headers(gw: &RunningGateway, repo: &str) -> Result<CheckResult> {
  let mut s = TcpStream::connect(("127.0.0.1", gw.port))?;
  write!(
    s,
    "GET /{repo}.git/info/refs?service=git-upload-pack HTTP/1.1\r\n\
         Host: 127.0.0.1\r\nConnection: close\r\n\r\n"
  )?;
  let mut buf = Vec::new();
  s.read_to_end(&mut buf)?;
  let head = String::from_utf8_lossy(&buf[..buf.len().min(512)]).to_string();

  let ct = head.contains("Content-Type: application/x-git-upload-pack-advertisement");
  let cache = head.to_lowercase().contains("cache-control: no-cache");
  if ct && cache {
    Ok(pass(
      "http_headers",
      "advertisement content type and no-cache present",
    ))
  } else {
    Ok(fail(
      "http_headers",
      format!("content_type={ct} no_cache={cache}"),
    ))
  }
}

fn check_hidden_refs(bare: &Path, url: &str, work: &Path) -> Result<Vec<CheckResult>> {
  let mut out = Vec::new();
  let head = git_ok(bare, &["rev-parse", "HEAD"])?.trim().to_string();
  let lease = "refs/gfs/mounts/gateway-probe";
  git_ok(bare, &["update-ref", lease, &head])?;

  // A lease ref must be invisible in both protocol versions; a client picks
  // the version, so hiding it in only one hides it from nobody.
  let mut leaked = None;
  for (label, proto) in [("v0", "protocol.version=0"), ("v2", "protocol.version=2")] {
    let ls = Command::new("git")
      .current_dir(work)
      .args(["-c", proto, "ls-remote", url])
      .env("GIT_CONFIG_GLOBAL", "/dev/null")
      .env("GIT_CONFIG_SYSTEM", "/dev/null")
      .output()?;
    if String::from_utf8_lossy(&ls.stdout).contains("refs/gfs/") {
      leaked = Some(label);
      break;
    }
  }

  // Hiding the ref stops discovery. Whether it stops *access* is a separate
  // question, and the answer differs by protocol version, so both are probed.
  // `commit-tree` takes a tree object ID, not a revspec.
  let head_tree = git_ok(bare, &["rev-parse", "HEAD^{tree}"])?
    .trim()
    .to_string();
  let orphan = git_ok(
    bare,
    &["commit-tree", &head_tree, "-p", &head, "-m", "probe orphan"],
  )
  .map(|s| s.trim().to_string());

  let mut per_version: Vec<(&str, bool)> = Vec::new();
  if let Ok(oid) = &orphan {
    git_ok(bare, &["update-ref", "refs/gfs/mounts/orphan-probe", oid])?;
    for (label, proto) in [("v0", "protocol.version=0"), ("v2", "protocol.version=2")] {
      let dst = work.join(format!("want-unadvertised-{label}"));
      let _ = std::fs::remove_dir_all(&dst);
      git_ok(work, &["init", "-q", "--bare", &dst.to_string_lossy()])?;
      let fetched = Command::new("git")
        .current_dir(&dst)
        .args(["-c", proto, "fetch", "--no-tags", url, oid])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
      // "the fetch exited zero" is not the question; "is the object now
      // readable by the client" is.
      let landed = fetched.status.success()
        && git(&dst, &["cat-file", "-t", oid])
          .map(|o| o.status.success())
          .unwrap_or(false);
      per_version.push((label, landed));
    }
    let _ = git(bare, &["update-ref", "-d", "refs/gfs/mounts/orphan-probe"]);
  }
  let _ = git(bare, &["update-ref", "-d", lease]);

  match leaked {
    Some(label) => out.push(fail(
      "hidden_refs",
      format!("refs/gfs/ advertised over {label}"),
    )),
    None => out.push(pass(
      "hidden_refs",
      "refs/gfs/ absent from v0 and v2 advertisements",
    )),
  }

  let leaked_versions: Vec<&str> = per_version
    .iter()
    .filter(|(_, l)| *l)
    .map(|(v, _)| *v)
    .collect();
  if per_version.is_empty() {
    out.push(fail(
      "unadvertised_want",
      "could not construct an unadvertised commit",
    ));
  } else if leaked_versions.is_empty() {
    out.push(pass(
      "unadvertised_want",
      "a commit reachable only from refs/gfs/ is not fetchable by OID in v0 or v2",
    ));
  } else if leaked_versions == ["v2"] {
    // Confirmed and understood: see docs/adr/0002. `allowAnySHA1InWant` is
    // enforced by upload-pack in protocol v0 and not consulted at all in
    // protocol v2, so any object in the ODB is fetchable by OID over v2.
    out.push(finding(
      "unadvertised_want",
      "protocol v2 serves a lease-only commit by OID despite \
             allowAnySHA1InWant=false (v0 refuses); hiding refs prevents discovery, \
             not access — see docs/adr/0002-git-object-authorization-boundary.md",
    ));
  } else {
    out.push(fail(
      "unadvertised_want",
      format!("unexpected leak profile: {leaked_versions:?}"),
    ));
  }
  Ok(out)
}

fn check_clone_matrix(bare: &Path, url: &str, work: &Path) -> Result<Vec<CheckResult>> {
  let mut out = Vec::new();

  // Oracle: a direct filesystem clone of the same bare repository. The
  // gateway's upload-pack cannot verify the gateway.
  let oracle = work.join("oracle");
  let _ = std::fs::remove_dir_all(&oracle);
  git_ok(
    work,
    &[
      "clone",
      "-q",
      "--bare",
      &bare.to_string_lossy(),
      &oracle.to_string_lossy(),
    ],
  )?;
  let oracle_head = git_ok(&oracle, &["rev-parse", "HEAD"])?.trim().to_string();
  let oracle_tree = git_ok(&oracle, &["rev-parse", "HEAD^{tree}"])?
    .trim()
    .to_string();

  for (name, proto, extra) in [
    ("clone_v0", "protocol.version=0", vec![]),
    ("clone_v2", "protocol.version=2", vec![]),
    ("clone_shallow", "protocol.version=2", vec!["--depth", "1"]),
    (
      "clone_blob_none",
      "protocol.version=2",
      vec!["--filter=blob:none"],
    ),
  ] {
    let dst = work.join(name);
    let _ = std::fs::remove_dir_all(&dst);
    let dst_s = dst.to_string_lossy().into_owned();
    let mut args = vec!["-c", proto, "clone", "-q"];
    args.extend(extra.iter().copied());
    args.push(url);
    args.push(&dst_s);

    let res = Command::new("git")
      .current_dir(work)
      .args(&args)
      .env("GIT_CONFIG_GLOBAL", "/dev/null")
      .env("GIT_CONFIG_SYSTEM", "/dev/null")
      .env("GIT_TERMINAL_PROMPT", "0")
      .output()?;
    if !res.status.success() {
      out.push(fail(
        name,
        String::from_utf8_lossy(&res.stderr).trim().to_string(),
      ));
      continue;
    }

    // Independent verification, not "the clone exited zero".
    let fsck = git(
      &dst,
      &[
        "fsck",
        "--no-progress",
        "--no-dangling",
        "--connectivity-only",
      ],
    )?;
    let head = git_ok(&dst, &["rev-parse", "HEAD"])?.trim().to_string();
    let tree = git_ok(&dst, &["rev-parse", "HEAD^{tree}"])?
      .trim()
      .to_string();

    if !fsck.status.success() {
      out.push(fail(
        name,
        format!(
          "git fsck failed: {}",
          String::from_utf8_lossy(&fsck.stderr).trim()
        ),
      ));
    } else if head != oracle_head || tree != oracle_tree {
      out.push(fail(
        name,
        format!("HEAD/tree {head}/{tree} vs direct clone {oracle_head}/{oracle_tree}"),
      ));
    } else {
      let shallow = dst.join(".git/shallow").exists();
      let promisor = git_ok(&dst, &["config", "--get", "remote.origin.promisor"])
        .map(|s| s.trim() == "true")
        .unwrap_or(false);
      out.push(pass(
        name,
        format!(
          "fsck clean; HEAD and tree match direct clone (shallow={shallow} promisor={promisor})"
        ),
      ));
    }
  }
  Ok(out)
}

fn check_filter_policy(url: &str, work: &Path) -> Result<Vec<CheckResult>> {
  let mut out = Vec::new();
  // Each of these sits in a family that Git's own configuration permits at a
  // coarser granularity than GFS policy does, so each must be stopped by the
  // gateway's request validation rather than by Git.
  for spec in [
    "tree:0",
    "blob:limit=1k",
    "combine:blob:none+tree:0",
    "object:type=blob",
  ] {
    let dst = work.join(format!("filter-{}", spec.replace([':', '=', '+'], "-")));
    let _ = std::fs::remove_dir_all(&dst);
    let res = Command::new("git")
      .current_dir(work)
      .args([
        "-c",
        "protocol.version=2",
        "clone",
        "-q",
        &format!("--filter={spec}"),
        url,
        &dst.to_string_lossy(),
      ])
      .env("GIT_CONFIG_GLOBAL", "/dev/null")
      .env("GIT_CONFIG_SYSTEM", "/dev/null")
      .env("GIT_TERMINAL_PROMPT", "0")
      .output()?;
    if res.status.success() {
      out.push(fail(
        "filter_denied",
        format!("--filter={spec} accepted; policy allows only blob:none"),
      ));
    } else {
      out.push(reject("filter_denied", format!("--filter={spec} refused")));
    }
  }
  Ok(out)
}

fn check_gzip_body(bare: &Path) -> Result<CheckResult> {
  use flate2::write::GzEncoder;
  use flate2::Compression;

  let up = UploadPack::new(bare, UploadPackPolicy::default())?;
  let request = {
    let mut r = Vec::new();
    r.extend_from_slice(&upload_pack::pkt_line(b"command=ls-refs\n"));
    r.extend_from_slice(&upload_pack::pkt_line(b"object-format=sha1\n"));
    r.extend_from_slice(b"0001");
    r.extend_from_slice(&upload_pack::pkt_line(b"peel\n"));
    r.extend_from_slice(b"0000");
    r
  };
  let mut enc = GzEncoder::new(Vec::new(), Compression::default());
  enc.write_all(&request)?;
  let gz = enc.finish()?;

  if up.decompress_body(&gz)? != request {
    return Ok(fail(
      "gzip_body",
      "decompressed body differs from the original",
    ));
  }

  // A highly compressible body must be refused rather than allocated.
  let mut bomb_enc = GzEncoder::new(Vec::new(), Compression::best());
  bomb_enc.write_all(&vec![0u8; 200 * 1024 * 1024])?;
  let bomb = bomb_enc.finish()?;
  let ratio = (200 * 1024 * 1024) / bomb.len().max(1);
  if up.decompress_body(&bomb).is_ok() {
    return Ok(fail("gzip_body", "200 MiB expansion was not refused"));
  }
  Ok(pass(
    "gzip_body",
    format!(
      "{} byte body round-trips; a {ratio}:1 bomb is refused",
      request.len()
    ),
  ))
}

fn check_repo_selection(root: &Path) -> Result<CheckResult> {
  let gw = Gateway {
    root: root.to_path_buf(),
    policy: UploadPackPolicy::default(),
  };
  // Repository selection is the classic path-traversal sink. These must be
  // rejected by name validation, before any filesystem call could
  // canonicalize them into something real.
  let hostile = [
    "../../etc",
    "..",
    ".",
    "",
    "a/b",
    "/etc/passwd",
    ".hidden",
    "a..b",
    "..%2f..%2fetc",
    "\u{2024}\u{2024}/etc",
  ];
  let mut accepted = Vec::new();
  for name in hostile {
    if gw.resolve_repo(name).is_ok() {
      accepted.push(name);
    }
  }
  if !accepted.is_empty() {
    return Ok(fail("repo_selection", format!("accepted {accepted:?}")));
  }
  Ok(pass(
    "repo_selection",
    format!(
      "{} traversal and absolute-path forms rejected",
      hostile.len()
    ),
  ))
}

// ---------------------------------------------------------------------------

fn tempdir() -> Result<TempDir> {
  let base = std::env::temp_dir().join(format!(
    "gfs-gateway-probe-{}-{}",
    std::process::id(),
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)?
      .as_nanos()
  ));
  std::fs::create_dir_all(&base)?;
  Ok(TempDir(base))
}

struct TempDir(PathBuf);
impl TempDir {
  fn path(&self) -> &Path {
    &self.0
  }
}
impl Drop for TempDir {
  fn drop(&mut self) {
    let _ = std::fs::remove_dir_all(&self.0);
  }
}

fn report(results: &[CheckResult], json: Option<&Path>) -> Result<()> {
  for r in results {
    let mark = match r.outcome {
      Outcome::Pass => "PASS   ",
      Outcome::Fail => "FAIL   ",
      Outcome::ExpectedReject => "REJECT ",
      Outcome::Finding => "FINDING",
    };
    println!("{mark} {:34}  {}", r.check, r.detail);
  }
  let count = |o: Outcome| results.iter().filter(|r| r.outcome == o).count();
  let fails = count(Outcome::Fail);
  println!(
    "\n{} passed, {fails} failed, {} expected rejections, {} documented findings",
    count(Outcome::Pass),
    count(Outcome::ExpectedReject),
    count(Outcome::Finding)
  );
  if let Some(p) = json {
    if let Some(parent) = p.parent() {
      std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(p, serde_json::to_string_pretty(results)?)
      .with_context(|| format!("writing {}", p.display()))?;
    println!("json report: {}", p.display());
  }
  if fails > 0 {
    std::process::exit(1);
  }
  Ok(())
}
