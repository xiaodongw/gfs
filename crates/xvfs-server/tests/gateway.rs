//! The Git smart-HTTP protocol matrix (PLAN.md M5.2).
//!
//! # The oracle problem
//!
//! The gateway's protocol engine *is* stock `git upload-pack`, so it cannot be
//! its own oracle. PLAN.md M5.2 is explicit about the consequence: every clone
//! and fetch result is verified independently. Each test that transfers objects
//! runs `git fsck` on what arrived and compares its resolved tree against a
//! **direct filesystem clone** of the same bare repository -- a path that never
//! touches the gateway, the HTTP surface, or libgit2.
//!
//! # What is deliberately not asserted
//!
//! ADR 0002: protocol v2 serves any object in a repository's object database by
//! object ID regardless of `uploadpack.allowAnySHA1InWant`. One bare repository
//! is one authorization domain, and PLAN.md M1.5 says not to write an acceptance
//! test that expects the Git path to deny it. The tests here assert *repository*
//! authorization and the absence of the reserved namespace from advertisements,
//! which is the boundary that does hold.
//!
//! # Every test is `multi_thread`
//!
//! Not a preference. These tests drive a **blocking** `git` client while the
//! server runs in the same process, so on the default current-thread runtime
//! `Command::output()` parks the only executor thread and the server can never
//! answer the request the client is waiting for. The symptom is a clean hang
//! with no output, which is why it is written down here rather than discovered
//! twice.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use xvfs_server::auth::{AllowList, CapabilityKey, StaticTokens};
use xvfs_server::catalog::repositories::NewRepository;
use xvfs_server::{Catalog, Server};
use xvfs_types::{DisplayName, HashAlgorithm, LeasePolicy, RepositoryId, SubjectId};

const OWNER_TOKEN: &str = "token-owner";
const OUTSIDER_TOKEN: &str = "token-outsider";

struct Fixture {
  base: String,
  /// The bare repository the gateway serves, for the direct-clone oracle.
  repo_path: std::path::PathBuf,
  server: Arc<Server>,
  _shutdown: tokio::sync::watch::Sender<bool>,
  _tmps: Vec<tempfile::TempDir>,
}

impl Fixture {
  fn url(&self, repository_id: &str) -> String {
    format!("{}/v1/repos/{repository_id}", self.base)
  }
}

/// Start a server serving the named fixtures, the first of which is `r-git`.
async fn start(fixtures: &[(&str, &str)]) -> Fixture {
  let catalog = Arc::new(Catalog::open_in_memory().unwrap());
  let owner = SubjectId::parse("job-owner").unwrap();
  let outsider = SubjectId::parse("job-outsider").unwrap();
  let mut policy = AllowList::new();
  let mut tmps = Vec::new();
  let mut first_path = None;

  for (id, fixture) in fixtures {
    let (tmp, path) = xvfs_test::scratch_clone(fixture).unwrap();
    let repo_id = RepositoryId::parse(id).unwrap();
    catalog
      .create_repository(&NewRepository {
        repository_id: repo_id.clone(),
        display_name: DisplayName::parse(&format!("acme/{fixture}")).unwrap(),
        repo_path: path.clone(),
        algorithm: HashAlgorithm::Sha1,
        upstream_url: None,
        credential_ref: None,
      })
      .unwrap();
    policy = policy.allow(&owner, &repo_id);
    if first_path.is_none() {
      first_path = Some(path);
    }
    tmps.push(tmp);
  }

  let authenticator = Arc::new(
    StaticTokens::new()
      .with_token(OWNER_TOKEN, owner)
      .with_token(OUTSIDER_TOKEN, outsider),
  );
  let server = Arc::new(Server::new(
    Arc::clone(&catalog),
    authenticator,
    Arc::new(policy),
    CapabilityKey::generate().unwrap(),
    LeasePolicy::adr_0006(),
  ));
  for (id, _) in fixtures {
    server
      .registry
      .activate(&RepositoryId::parse(id).unwrap())
      .unwrap();
  }
  server.recover().await.unwrap();

  let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let addr = listener.local_addr().unwrap();
  let router = server.http_router();
  let mut shutdown = shutdown_rx.clone();
  tokio::spawn(async move {
    axum::serve(listener, router)
      .with_graceful_shutdown(async move {
        let _ = shutdown.changed().await;
      })
      .await
  });

  Fixture {
    base: format!("http://{addr}"),
    repo_path: first_path.unwrap(),
    server,
    _shutdown: shutdown_tx,
    _tmps: tmps,
  }
}

// ---------------------------------------------------------------------------
// Git client helpers
// ---------------------------------------------------------------------------

/// Run a Git client with a hermetic environment and a bearer credential.
///
/// The environment is fixed for the same reason `xvfs_test::git` fixes it: a
/// developer's `~/.gitconfig` must not change what the protocol matrix sees.
fn git_client(args: &[&str], token: Option<&str>) -> std::process::Output {
  let mut command = Command::new("git");
  command
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env("GIT_TERMINAL_PROMPT", "0")
    .env("GIT_ASKPASS", "/bin/true");
  if let Some(token) = token {
    command.arg("-c");
    command.arg(format!("http.extraHeader=Authorization: Bearer {token}"));
  }
  command.args(args);
  command.output().expect("spawning git")
}

fn stderr(out: &std::process::Output) -> String {
  String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &std::process::Output) -> String {
  String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The independent oracle: clone the bare repository straight off the
/// filesystem, with no gateway, no HTTP, and no libgit2 anywhere in the path.
fn direct_clone(repo: &Path, into: &Path) {
  let out = git_client(
    &[
      "clone",
      "-q",
      "--no-local",
      repo.to_str().unwrap(),
      into.to_str().unwrap(),
    ],
    None,
  );
  assert!(
    out.status.success(),
    "direct clone failed: {}",
    stderr(&out)
  );
}

/// `HEAD`'s recursive tree listing, as the comparable form of "same content".
fn tree_of(checkout: &Path) -> String {
  let out = git_client(
    &["-C", checkout.to_str().unwrap(), "ls-tree", "-r", "HEAD"],
    None,
  );
  assert!(out.status.success(), "ls-tree failed: {}", stderr(&out));
  stdout(&out)
}

fn fsck_clean(checkout: &Path) {
  let out = git_client(
    &["-C", checkout.to_str().unwrap(), "fsck", "--no-progress"],
    None,
  );
  assert!(
    out.status.success(),
    "git fsck reported a problem: {}",
    stderr(&out)
  );
}

/// Clone through the gateway and verify the result against the direct clone.
///
/// Returns the cloned working directory so a caller can make further assertions.
fn clone_and_verify(fx: &Fixture, extra: &[&str], tmp: &Path) -> std::path::PathBuf {
  let via_gateway = tmp.join("gateway");
  let mut args: Vec<String> = vec!["clone".into(), "-q".into()];
  args.extend(extra.iter().map(|s| (*s).to_owned()));
  args.push(fx.url("r-git"));
  args.push(via_gateway.display().to_string());
  let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
  let out = git_client(&arg_refs, Some(OWNER_TOKEN));
  assert!(
    out.status.success(),
    "clone {extra:?} failed: {}",
    stderr(&out)
  );

  fsck_clean(&via_gateway);
  let direct = tmp.join("direct");
  direct_clone(&fx.repo_path, &direct);
  assert_eq!(
    tree_of(&via_gateway),
    tree_of(&direct),
    "clone {extra:?} produced a different tree than a direct filesystem clone"
  );
  via_gateway
}

// ---------------------------------------------------------------------------
// Advertisement framing
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_v0_advertisement_carries_the_service_preamble_and_v2_does_not() {
  let fx = start(&[("r-git", "basic")]).await;
  let url = format!("{}/info/refs?service=git-upload-pack", fx.url("r-git"));

  // v0/v1: `upload-pack --http-backend-info-refs` emits only the advertisement,
  // so the gateway must write the `# service=` pkt-line and a flush packet the
  // way `git-http-backend` does.
  let (status, headers, body) = http_get(&url, OWNER_TOKEN, None).await;
  assert_eq!(status, 200);
  assert_eq!(
    headers.get("content-type").map(String::as_str),
    Some("application/x-git-upload-pack-advertisement")
  );
  assert!(headers
    .get("cache-control")
    .is_some_and(|v| v.contains("no-cache")));
  assert!(
    body.starts_with(b"001e# service=git-upload-pack\n0000"),
    "v0 advertisement began {:?}",
    String::from_utf8_lossy(&body[..40.min(body.len())])
  );

  // v2: the preamble must be absent, because the body already begins with
  // upload-pack's own `version 2` pkt-line.
  let (status, _, body) = http_get(&url, OWNER_TOKEN, Some("version=2")).await;
  assert_eq!(status, 200);
  assert!(
    body.starts_with(b"000eversion 2\n"),
    "v2 advertisement began {:?}",
    String::from_utf8_lossy(&body[..40.min(body.len())])
  );
  assert!(!body.starts_with(b"001e# service="));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_upload_pack_service_is_served() {
  let fx = start(&[("r-git", "basic")]).await;
  // Push is refused with a status a client can act on, rather than a 404 that
  // reads as "no such repository".
  let (status, _, _) = http_get(
    &format!("{}/info/refs?service=git-receive-pack", fx.url("r-git")),
    OWNER_TOKEN,
    None,
  )
  .await;
  assert_eq!(status, 403);

  // Dumb HTTP is not served at all: falling through would expose the object
  // directory as static files and bypass every check in the gateway.
  let (status, _, _) = http_get(&format!("{}/info/refs", fx.url("r-git")), OWNER_TOKEN, None).await;
  assert_eq!(status, 400);
}

// ---------------------------------------------------------------------------
// Clone and fetch
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stock_client_clones_over_both_protocol_versions() {
  let fx = start(&[("r-git", "basic")]).await;
  for version in ["0", "2"] {
    let tmp = tempfile::tempdir().unwrap();
    let checkout = clone_and_verify(
      &fx,
      &["-c", &format!("protocol.version={version}")],
      tmp.path(),
    );
    // A tag that peels to a tree exists in this fixture (M0.3 finding 4); the
    // Git path must transfer it without complaint even though the snapshot API
    // refuses to resolve it.
    let tags = git_client(&["-C", checkout.to_str().unwrap(), "tag"], None);
    assert!(stdout(&tags).contains("tree-tag"));
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shallow_and_blob_none_clones_match_the_direct_clone() {
  let fx = start(&[("r-git", "basic")]).await;

  let tmp = tempfile::tempdir().unwrap();
  clone_and_verify(&fx, &["--depth", "1"], tmp.path());

  // `blob:none` is the one filter policy permits. The clone is a promisor
  // remote, so checking out the tree fetches the blobs back through the same
  // gateway -- which is the second half of what makes this test meaningful.
  let tmp = tempfile::tempdir().unwrap();
  clone_and_verify(&fx, &["--filter=blob:none"], tmp.path());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fetch_after_the_branch_moves_transfers_the_new_commit() {
  let fx = start(&[("r-git", "basic")]).await;
  let tmp = tempfile::tempdir().unwrap();
  let checkout = clone_and_verify(&fx, &[], tmp.path());

  // Move the branch in the served repository, the way an upstream push would.
  let bare = fx.repo_path.clone();
  let work = tmp.path().join("push");
  direct_clone(&bare, &work);
  std::fs::write(work.join("added-later.txt"), b"after the clone\n").unwrap();
  for args in [
    vec!["-C", work.to_str().unwrap(), "add", "-A"],
    vec![
      "-c",
      "user.email=t@xvfs.invalid",
      "-c",
      "user.name=T",
      "-C",
      work.to_str().unwrap(),
      "commit",
      "-q",
      "-m",
      "later",
    ],
    vec!["-C", work.to_str().unwrap(), "push", "-q", "origin", "main"],
  ] {
    let out = git_client(&args, None);
    assert!(out.status.success(), "{args:?}: {}", stderr(&out));
  }

  let out = git_client(
    &["-C", checkout.to_str().unwrap(), "fetch", "-q", "origin"],
    Some(OWNER_TOKEN),
  );
  assert!(out.status.success(), "fetch failed: {}", stderr(&out));
  let out = git_client(
    &[
      "-C",
      checkout.to_str().unwrap(),
      "cat-file",
      "-p",
      "origin/main:added-later.txt",
    ],
    None,
  );
  assert!(
    out.status.success(),
    "fetched object missing: {}",
    stderr(&out)
  );
  assert_eq!(stdout(&out), "after the clone\n");
}

// ---------------------------------------------------------------------------
// The reserved namespace
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_lease_anchor_is_invisible_to_every_git_client() {
  let fx = start(&[("r-git", "basic")]).await;
  let grant = fx
    .server
    .mounts
    .create_mount(
      &RepositoryId::parse("r-git").unwrap(),
      xvfs_types::RevisionSelector::parse("main", HashAlgorithm::Sha1).unwrap(),
      &SubjectId::parse("job-owner").unwrap(),
      None,
    )
    .await
    .unwrap();
  let anchor = xvfs_types::revision::lease_anchor_ref(grant.mount_id.as_str());

  // Sanity: the anchor really exists, or the assertions below prove nothing.
  let refs = xvfs_test::git(&fx.repo_path, &["show-ref"]).unwrap();
  assert!(refs.contains(&anchor), "the lease anchor was never created");

  // v0 advertises refs in the GET; v2 answers `ls-refs` in the POST. Both paths
  // have to hide the namespace, and they are different code in Git.
  for version in ["0", "2"] {
    let out = git_client(
      &[
        "-c",
        &format!("protocol.version={version}"),
        "ls-remote",
        &fx.url("r-git"),
      ],
      Some(OWNER_TOKEN),
    );
    assert!(
      out.status.success(),
      "ls-remote v{version}: {}",
      stderr(&out)
    );
    let listing = stdout(&out);
    assert!(
      !listing.contains("refs/xvfs/"),
      "protocol v{version} advertised the reserved namespace:\n{listing}"
    );
    assert!(
      listing.contains("refs/heads/main"),
      "v{version} advertised nothing"
    );
  }

  // And asking for it by name is refused rather than quietly returning nothing.
  let out = git_client(
    &[
      "-c",
      "protocol.version=2",
      "ls-remote",
      &fx.url("r-git"),
      &anchor,
    ],
    Some(OWNER_TOKEN),
  );
  assert!(stdout(&out).is_empty(), "{}", stdout(&out));
}

// ---------------------------------------------------------------------------
// Authorization
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_traffic_uses_the_same_repository_authorization_as_every_other_surface() {
  let fx = start(&[("r-git", "basic")]).await;
  let url = format!("{}/info/refs?service=git-upload-pack", fx.url("r-git"));

  // No credential: 401 with a challenge, so a client with a credential helper
  // can retry rather than reporting an unrecoverable failure.
  let (status, headers, _) = http_get_raw(&url, None, None).await;
  assert_eq!(status, 401);
  assert!(headers.contains_key("www-authenticate"));

  // A valid credential for a subject with no grant on this repository gets the
  // same masked answer the snapshot API gives: NOT_FOUND, not PERMISSION_DENIED,
  // because a distinct status would answer the existence question.
  let (status, _, _) = http_get(&url, OUTSIDER_TOKEN, None).await;
  assert_eq!(status, 404);

  // And an unknown repository is indistinguishable from an unauthorized one.
  let (status, _, _) = http_get(
    &format!(
      "{}/info/refs?service=git-upload-pack",
      fx.url("r-does-not-exist")
    ),
    OWNER_TOKEN,
    None,
  )
  .await;
  assert_eq!(status, 404);

  // A real clone with no credential fails rather than falling back to anything.
  let tmp = tempfile::tempdir().unwrap();
  let out = git_client(
    &[
      "clone",
      "-q",
      &fx.url("r-git"),
      tmp.path().join("nope").to_str().unwrap(),
    ],
    None,
  );
  assert!(!out.status.success());
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

async fn http_get(
  url: &str,
  token: &str,
  git_protocol: Option<&str>,
) -> (u16, std::collections::HashMap<String, String>, Vec<u8>) {
  http_get_raw(url, Some(token), git_protocol).await
}

/// A minimal HTTP/1.1 client.
///
/// Hand-written rather than pulled in: the tests need to send a `Git-Protocol`
/// header and read a raw body, and a client library would add a dependency for
/// two requests' worth of work.
async fn http_get_raw(
  url: &str,
  token: Option<&str>,
  git_protocol: Option<&str>,
) -> (u16, std::collections::HashMap<String, String>, Vec<u8>) {
  use tokio::io::{AsyncReadExt, AsyncWriteExt};

  let rest = url.strip_prefix("http://").expect("http url");
  let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
  let mut request = format!("GET /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n");
  if let Some(token) = token {
    request.push_str(&format!("Authorization: Bearer {token}\r\n"));
  }
  if let Some(protocol) = git_protocol {
    request.push_str(&format!("Git-Protocol: {protocol}\r\n"));
  }
  request.push_str("\r\n");

  let mut stream = tokio::net::TcpStream::connect(authority).await.unwrap();
  stream.write_all(request.as_bytes()).await.unwrap();
  let mut raw = Vec::new();
  stream.read_to_end(&mut raw).await.unwrap();

  let split = raw
    .windows(4)
    .position(|w| w == b"\r\n\r\n")
    .expect("response headers");
  let head = String::from_utf8_lossy(&raw[..split]).into_owned();
  let mut body = raw[split + 4..].to_vec();

  let mut lines = head.lines();
  let status: u16 = lines
    .next()
    .and_then(|l| l.split_whitespace().nth(1))
    .and_then(|s| s.parse().ok())
    .expect("status line");
  let mut headers = std::collections::HashMap::new();
  for line in lines {
    if let Some((name, value)) = line.split_once(':') {
      headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
  }
  if headers.get("transfer-encoding").map(String::as_str) == Some("chunked") {
    body = dechunk(&body);
  }
  (status, headers, body)
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
  let mut out = Vec::new();
  while let Some(eol) = data.windows(2).position(|w| w == b"\r\n") {
    let size = usize::from_str_radix(String::from_utf8_lossy(&data[..eol]).trim(), 16).unwrap_or(0);
    data = &data[eol + 2..];
    if size == 0 || data.len() < size {
      break;
    }
    out.extend_from_slice(&data[..size]);
    data = &data[(size + 2).min(data.len())..];
  }
  out
}
