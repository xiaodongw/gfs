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

use gfs_service::auth::{AllowList, CapabilityKey, StaticTokens};
use gfs_service::catalog::repositories::NewRepository;
use gfs_service::gateway::{FilterPolicy, UploadPackPolicy};
use gfs_service::{Catalog, Server};
use gfs_types::{DisplayName, HashAlgorithm, LeasePolicy, RepositoryId, SubjectId};

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
  start_with(fixtures, UploadPackPolicy::default()).await
}

/// [`start`] with a non-default gateway policy.
async fn start_with(fixtures: &[(&str, &str)], git_policy: UploadPackPolicy) -> Fixture {
  let catalog = Arc::new(Catalog::open_in_memory().unwrap());
  let owner = SubjectId::parse("job-owner").unwrap();
  let outsider = SubjectId::parse("job-outsider").unwrap();
  // Allow-all for the owner rather than a grant per repository, so a test can
  // register one more repository on a running fixture. The outsider still has
  // no grant at all, which is what the masking assertions rest on.
  let policy = AllowList::new().allow_all_repositories(&owner);
  let mut tmps = Vec::new();
  let mut first_path = None;

  for (id, fixture) in fixtures {
    let (tmp, path) = gfs_test::scratch_clone(fixture).unwrap();
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
  let server = Arc::new(
    Server::new(
      Arc::clone(&catalog),
      authenticator,
      Arc::new(policy),
      CapabilityKey::generate().unwrap(),
      LeasePolicy::adr_0006(),
    )
    .with_git_policy(git_policy),
  );
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
/// The environment is fixed for the same reason `gfs_test::git` fixes it: a
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
async fn only_the_two_smart_services_are_served() {
  let fx = start(&[("r-git", "basic")]).await;
  // Dumb HTTP is not served at all: falling through would expose the object
  // directory as static files and bypass every check in the gateway. An
  // unknown service is refused the same way.
  let (status, _, _) = http_get(&format!("{}/info/refs", fx.url("r-git")), OWNER_TOKEN, None).await;
  assert_eq!(status, 400);
  let (status, _, _) = http_get(
    &format!("{}/info/refs?service=git-annex", fx.url("r-git")),
    OWNER_TOKEN,
    None,
  )
  .await;
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
      "user.email=t@gfs.invalid",
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
      gfs_types::RevisionSelector::parse("main", HashAlgorithm::Sha1).unwrap(),
      &SubjectId::parse("job-owner").unwrap(),
      None,
    )
    .await
    .unwrap();
  let anchor = gfs_types::revision::lease_anchor_ref(grant.mount_id.as_str());

  // Sanity: the anchor really exists, or the assertions below prove nothing.
  let refs = gfs_test::git(&fx.repo_path, &["show-ref"]).unwrap();
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
      !listing.contains("refs/gfs/"),
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
// Filter policy
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn filtering_is_advertised_exactly_when_policy_enables_it() {
  // Advertised under the default policy, and a clone that asks for it really
  // is partial -- Git records a promisor remote only when the server agreed.
  let fx = start(&[("r-git", "basic")]).await;
  let (_, _, body) = http_get(
    &format!("{}/info/refs?service=git-upload-pack", fx.url("r-git")),
    OWNER_TOKEN,
    None,
  )
  .await;
  assert!(
    String::from_utf8_lossy(&body).contains("filter"),
    "the default policy serves blob:none, so `filter` must be advertised"
  );
  let tmp = tempfile::tempdir().unwrap();
  let checkout = tmp.path().join("c");
  let out = git_client(
    &[
      "clone",
      "-q",
      // `--no-checkout` is load-bearing: a checkout of a `blob:none` clone
      // lazily fetches every blob in the working tree, so without it the
      // filtered and unfiltered cases end up byte-identical on disk.
      "--no-checkout",
      "--filter=blob:none",
      &fx.url("r-git"),
      checkout.to_str().unwrap(),
    ],
    Some(OWNER_TOKEN),
  );
  assert!(out.status.success(), "{}", stderr(&out));
  assert!(
    !blob_is_local(&checkout, "HEAD:README.md"),
    "the filter was advertised but the server sent the blobs anyway"
  );

  // Switched off, the capability is absent. **A client then silently degrades
  // to a full clone** rather than failing -- measured, and the reason this test
  // does not assert a non-zero exit. The observable difference is that the
  // result is not a partial clone.
  let fx = start_with(
    &[("r-git", "basic")],
    UploadPackPolicy {
      filter: FilterPolicy::Disabled,
      ..Default::default()
    },
  )
  .await;
  let (_, _, body) = http_get(
    &format!("{}/info/refs?service=git-upload-pack", fx.url("r-git")),
    OWNER_TOKEN,
    None,
  )
  .await;
  assert!(!String::from_utf8_lossy(&body).contains("filter"));

  let tmp = tempfile::tempdir().unwrap();
  let checkout = tmp.path().join("c");
  let out = git_client(
    &[
      "clone",
      "-q",
      // `--no-checkout` is load-bearing: a checkout of a `blob:none` clone
      // lazily fetches every blob in the working tree, so without it the
      // filtered and unfiltered cases end up byte-identical on disk.
      "--no-checkout",
      "--filter=blob:none",
      &fx.url("r-git"),
      checkout.to_str().unwrap(),
    ],
    Some(OWNER_TOKEN),
  );
  assert!(out.status.success(), "{}", stderr(&out));
  assert!(
    blob_is_local(&checkout, "HEAD:README.md"),
    "the clone was filtered even though the capability was withdrawn"
  );

  // And a client that sends the filter line anyway -- withdrawing a capability
  // is advice, not enforcement -- is refused by the gateway's own validation.
  let mut request = Vec::new();
  request.extend_from_slice(&pkt(b"command=fetch\n"));
  request.extend_from_slice(b"0001");
  request.extend_from_slice(&pkt(b"filter blob:none\n"));
  request.extend_from_slice(b"0000");
  let (status, _, _) = http_post(
    &format!("{}/git-upload-pack", fx.url("r-git")),
    OWNER_TOKEN,
    Some("version=2"),
    &request,
    false,
  )
  .await;
  assert_eq!(status, 403, "an unadvertised filter was not refused");
}

/// Whether a blob is present in the clone's own object database.
///
/// `GIT_NO_LAZY_FETCH` is what makes this an honest question. Without it a
/// partial clone silently fetches the missing blob from its promisor remote and
/// every check reports "present", which is exactly the difference under test.
///
/// The client's `remote.origin.partialclonefilter` is **not** usable for this:
/// Git records it from the command line whether or not the server honoured the
/// filter, so it says what was asked for, not what happened.
fn blob_is_local(checkout: &Path, rev_path: &str) -> bool {
  Command::new("git")
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env("GIT_NO_LAZY_FETCH", "1")
    .args(["-C", checkout.to_str().unwrap(), "cat-file", "-e", rev_path])
    .output()
    .expect("spawning git")
    .status
    .success()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_filter_git_would_allow_but_policy_does_not_fails_closed() {
  let fx = start(&[("r-git", "basic")]).await;
  // `blob:limit` and `tree:<depth>` are inside families Git's own
  // `uploadpackfilter.*` granularity cannot separate from what is allowed, so
  // these are refused by the gateway's own validation rather than by Git.
  for filter in [
    "--filter=tree:0",
    "--filter=blob:limit=1k",
    "--filter=object:type=blob",
    "--filter=combine:blob:none+tree:0",
  ] {
    let tmp = tempfile::tempdir().unwrap();
    let out = git_client(
      &[
        "clone",
        "-q",
        filter,
        &fx.url("r-git"),
        tmp.path().join("c").to_str().unwrap(),
      ],
      Some(OWNER_TOKEN),
    );
    assert!(!out.status.success(), "{filter} was served");
  }
  // And the permitted one still works, so the test above is not passing because
  // filtering is broken outright.
  let tmp = tempfile::tempdir().unwrap();
  clone_and_verify(&fx, &["--filter=blob:none"], tmp.path());
}

// ---------------------------------------------------------------------------
// Repository shapes
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_repository_shapes_in_the_fixture_matrix_all_clone() {
  // One server, several repositories: `content` is 16 MiB and forces the
  // response past a single read chunk, `bigdir` has 5000 entries in one tree,
  // `bytes` has paths that are not valid UTF-8, `deep` is 40 components, and
  // `packed` is the normal server-side shape with everything in a pack.
  let fixtures = [
    ("r-git", "packed"),
    ("r-content", "content"),
    ("r-bigdir", "bigdir"),
    ("r-bytes", "bytes"),
    ("r-deep", "deep"),
  ];
  let fx = start(&fixtures).await;

  for (id, name) in fixtures {
    let tmp = tempfile::tempdir().unwrap();
    let via_gateway = tmp.path().join("gateway");
    let out = git_client(
      &["clone", "-q", &fx.url(id), via_gateway.to_str().unwrap()],
      Some(OWNER_TOKEN),
    );
    assert!(out.status.success(), "{name}: {}", stderr(&out));
    fsck_clean(&via_gateway);

    let direct = tmp.path().join("direct");
    let bare = fx
      .server
      .catalog
      .get_repository(&RepositoryId::parse(id).unwrap())
      .unwrap()
      .unwrap()
      .repo_path;
    direct_clone(&bare, &direct);
    assert_eq!(tree_of(&via_gateway), tree_of(&direct), "{name}");
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_repository_advertises_nothing_and_clones_cleanly() {
  // An unborn HEAD is not an error state. Git warns and produces an empty
  // clone; what must not happen is a hang or a 500.
  let fx = start(&[("r-git", "empty")]).await;
  let tmp = tempfile::tempdir().unwrap();
  let out = git_client(
    &[
      "clone",
      "-q",
      &fx.url("r-git"),
      tmp.path().join("c").to_str().unwrap(),
    ],
    Some(OWNER_TOKEN),
  );
  assert!(out.status.success(), "{}", stderr(&out));
  let out = git_client(
    &[
      "-C",
      tmp.path().join("c").to_str().unwrap(),
      "rev-parse",
      "HEAD",
    ],
    None,
  );
  assert!(
    !out.status.success(),
    "an empty clone must have no HEAD commit"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_alternates_based_repository_serves_the_objects_it_borrows() {
  // A repository whose objects live in another repository's object database.
  // The gateway never sets `GIT_ALTERNATE_OBJECT_DIRECTORIES` -- the allow-list
  // environment deliberately omits it -- so this only works if Git reads
  // `objects/info/alternates` itself, which is the property under test.
  let fx = start(&[("r-git", "basic")]).await;
  let tmp = tempfile::tempdir().unwrap();
  let borrower = tmp.path().join("borrower.git");
  let out = git_client(
    &[
      "clone",
      "-q",
      "--bare",
      "--shared",
      fx.repo_path.to_str().unwrap(),
      borrower.to_str().unwrap(),
    ],
    None,
  );
  assert!(out.status.success(), "{}", stderr(&out));
  assert!(borrower.join("objects/info/alternates").is_file());

  let borrow_fx = register_extra(&fx, "r-alt", &borrower).await;
  let checkout = tmp.path().join("clone");
  let out = git_client(
    &["clone", "-q", &borrow_fx, checkout.to_str().unwrap()],
    Some(OWNER_TOKEN),
  );
  assert!(out.status.success(), "{}", stderr(&out));
  fsck_clean(&checkout);
  let direct = tmp.path().join("direct");
  direct_clone(&fx.repo_path, &direct);
  assert_eq!(tree_of(&checkout), tree_of(&direct));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_corrupt_object_fails_the_transfer_rather_than_producing_a_broken_clone() {
  let fx = start(&[("r-git", "basic")]).await;
  // Find a loose blob and replace its content with something that is not a
  // valid zlib stream. `transfer.fsckObjects` plus Git's own decompression is
  // what has to notice.
  let objects = fx.repo_path.join("objects");
  let corrupted = std::fs::read_dir(&objects)
    .unwrap()
    .flatten()
    .filter(|shard| {
      let name = shard.file_name();
      name.len() == 2
        && name
          .to_string_lossy()
          .chars()
          .all(|c| c.is_ascii_hexdigit())
    })
    .find_map(|shard| std::fs::read_dir(shard.path()).unwrap().flatten().next())
    .map(|object| object.path());
  let corrupted = corrupted.expect("the fixture had no loose object to corrupt");
  // Loose objects are written read-only, so the file is replaced rather than
  // overwritten in place.
  std::fs::remove_file(&corrupted).unwrap();
  std::fs::write(&corrupted, b"this is not a git object").unwrap();

  let tmp = tempfile::tempdir().unwrap();
  let out = git_client(
    &[
      "clone",
      "-q",
      &fx.url("r-git"),
      tmp.path().join("c").to_str().unwrap(),
    ],
    Some(OWNER_TOKEN),
  );
  assert!(
    !out.status.success(),
    "a corrupt object must break the transfer, not arrive silently"
  );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repository_format_gfs_cannot_serve_never_reaches_the_gateway() {
  // ADR 0001 rejects reftable and SHA-256 at ingest. The gateway inherits that
  // for free by going through `require_servable`, and this asserts the
  // inheritance rather than assuming it: an unservable repository is masked as
  // absent, not served partially by stock Git which *can* read both formats.
  for fixture in ["reftable", "sha256"] {
    let catalog = Arc::new(Catalog::open_in_memory().unwrap());
    let (_tmp, path) = gfs_test::scratch_clone(fixture).unwrap();
    let repo_id = RepositoryId::parse("r-git").unwrap();
    catalog
      .create_repository(&NewRepository {
        repository_id: repo_id.clone(),
        display_name: DisplayName::parse("acme/unsupported").unwrap(),
        repo_path: path,
        algorithm: HashAlgorithm::Sha1,
        upstream_url: None,
        credential_ref: None,
      })
      .unwrap();
    let owner = SubjectId::parse("job-owner").unwrap();
    let server = Arc::new(Server::new(
      Arc::clone(&catalog),
      Arc::new(StaticTokens::new().with_token(OWNER_TOKEN, owner.clone())),
      Arc::new(AllowList::new().allow(&owner, &repo_id)),
      CapabilityKey::generate().unwrap(),
      LeasePolicy::adr_0006(),
    ));
    assert!(
      server.registry.activate(&repo_id).is_err(),
      "{fixture} must be refused at ingest"
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = server.http_router();
    let handle = tokio::spawn(async move { axum::serve(listener, router).await });

    let (status, _, _) = http_get(
      &format!("http://{addr}/v1/repos/r-git/info/refs?service=git-upload-pack"),
      OWNER_TOKEN,
      None,
    )
    .await;
    assert_eq!(status, 404, "{fixture} was reachable over Git");
    handle.abort();
  }
}

// ---------------------------------------------------------------------------
// M5.3: repository configuration cannot widen the sandbox
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hostile_repository_config_cannot_reopen_anything_the_gateway_closed() {
  let fx = start(&[("r-git", "basic")]).await;
  let grant = fx
    .server
    .mounts
    .create_mount(
      &RepositoryId::parse("r-git").unwrap(),
      gfs_types::RevisionSelector::parse("main", HashAlgorithm::Sha1).unwrap(),
      &SubjectId::parse("job-owner").unwrap(),
      None,
    )
    .await
    .unwrap();
  let anchor = gfs_types::revision::lease_anchor_ref(grant.mount_id.as_str());

  // Everything an operator could be tricked into importing, or an attacker
  // could write if they reached the object directory.
  for (key, value) in [
    // `transfer.hideRefs` is a list and `!` negates an entry.
    ("transfer.hideRefs", "!refs/gfs/"),
    ("uploadpack.hideRefs", "!refs/gfs/"),
    ("uploadpack.allowAnySHA1InWant", "true"),
    ("uploadpack.allowFilter", "true"),
    ("uploadpackfilter.allow", "true"),
    ("uploadpackfilter.tree.allow", "true"),
    // Git documents that it ignores this one from repository-level config
    // precisely because fetching from an untrusted repository would otherwise
    // be remote code execution. Asserted rather than trusted.
    ("uploadpack.packObjectsHook", "/bin/false"),
  ] {
    gfs_test::git(&fx.repo_path, &["config", key, value]).unwrap();
  }

  // The reserved namespace is still hidden.
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
    assert!(out.status.success(), "v{version}: {}", stderr(&out));
    assert!(
      !stdout(&out).contains("refs/gfs/"),
      "repository config un-hid the reserved namespace under v{version}"
    );
  }
  assert!(!anchor.is_empty());

  // The filter the repository tried to enable is still refused.
  let tmp = tempfile::tempdir().unwrap();
  let out = git_client(
    &[
      "clone",
      "-q",
      "--filter=tree:0",
      &fx.url("r-git"),
      tmp.path().join("c").to_str().unwrap(),
    ],
    Some(OWNER_TOKEN),
  );
  assert!(!out.status.success(), "repository config re-enabled tree:0");

  // And an ordinary clone still works, which is what proves the hook was
  // ignored rather than run.
  let tmp = tempfile::tempdir().unwrap();
  clone_and_verify(&fx, &[], tmp.path());
}

// ---------------------------------------------------------------------------
// Request bodies, limits, and malformed input
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gzip_request_body_is_accepted_and_a_decompression_bomb_is_not() {
  use std::io::Write;
  let fx = start(&[("r-git", "basic")]).await;
  let url = format!("{}/git-upload-pack", fx.url("r-git"));

  // A real v2 `ls-refs` request, gzipped the way a Git client may send it.
  let mut request = Vec::new();
  request.extend_from_slice(&pkt(b"command=ls-refs\n"));
  request.extend_from_slice(&pkt(b"agent=test\n"));
  request.extend_from_slice(b"0001");
  request.extend_from_slice(&pkt(b"peel\n"));
  request.extend_from_slice(b"0000");

  let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
  encoder.write_all(&request).unwrap();
  let gzipped = encoder.finish().unwrap();

  let (status, headers, body) =
    http_post(&url, OWNER_TOKEN, Some("version=2"), &gzipped, true).await;
  assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
  assert_eq!(
    headers.get("content-type").map(String::as_str),
    Some("application/x-git-upload-pack-result")
  );
  assert!(String::from_utf8_lossy(&body).contains("refs/heads/main"));

  // 4 MiB of one byte compresses far past the 100:1 ratio cap, and the cap has
  // to fire during inflation rather than after it.
  let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
  encoder.write_all(&vec![b'a'; 4 * 1024 * 1024]).unwrap();
  let bomb = encoder.finish().unwrap();
  let (status, _, _) = http_post(&url, OWNER_TOKEN, Some("version=2"), &bomb, true).await;
  assert_eq!(status, 429, "the bomb was not refused");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_requests_are_refused_without_taking_the_server_with_them() {
  // The fuzz sweep PLAN.md M5.2 asks for, written as a fixed corpus rather than
  // a random one: a randomized sweep that fails is not reproducible, and every
  // shape below is one a real client or a real attacker produces.
  let fx = start(&[("r-git", "basic")]).await;
  let rpc = format!("{}/git-upload-pack", fx.url("r-git"));

  let malformed: Vec<Vec<u8>> = vec![
    b"".to_vec(),
    b"0000".to_vec(),
    b"zzzz".to_vec(),
    b"0003".to_vec(),
    b"0001".to_vec(),
    b"ffff".to_vec(),      // a length past Git's maximum
    b"0010short".to_vec(), // truncated payload
    pkt(b"command=nonsense\n"),
    pkt(b"want \n"),
    pkt(b"filter \n"),
    pkt(b"filter blob:none extra\n"),
    vec![0u8; 4096],      // NUL bytes where a length belongs
    b"0004".repeat(4096), // many empty-payload packets
  ];
  for body in &malformed {
    let (status, _, _) = http_post(&rpc, OWNER_TOKEN, Some("version=2"), body, false).await;
    assert!(
      (200..600).contains(&status),
      "malformed body produced no HTTP response"
    );
    assert!(
      status != 500,
      "malformed body reached an internal error: {status}"
    );
  }

  // Repository selection: the traversal shapes `RepositoryId::parse` exists to
  // refuse, none of which may become a filesystem path.
  for name in [
    "..",
    "../..",
    ".%2e/",
    "a/../../etc",
    "%2e%2e",
    ".git",
    "a b",
    "a\nb",
    "",
  ] {
    let (status, _, _) = http_get(
      &format!(
        "{}/v1/repos/{name}/info/refs?service=git-upload-pack",
        fx.base
      ),
      OWNER_TOKEN,
      None,
    )
    .await;
    assert!(
      (400..500).contains(&status),
      "repository name {name:?} produced {status}"
    );
  }

  // Headers: a hostile `Git-Protocol` must not negotiate anything or escape
  // into the environment.
  for protocol in [
    "version=2:evil=1",
    "version=99",
    &"version=2".repeat(1000),
    "\u{7f}",
  ] {
    let (status, _, _) = http_get(
      &format!("{}/info/refs?service=git-upload-pack", fx.url("r-git")),
      OWNER_TOKEN,
      Some(protocol),
    )
    .await;
    assert!(
      (200..500).contains(&status),
      "{protocol:?} produced {status}"
    );
  }

  // Still alive and still correct.
  let tmp = tempfile::tempdir().unwrap();
  clone_and_verify(&fx, &[], tmp.path());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admission_control_refuses_rather_than_queues() {
  let fx = start_with(
    &[("r-git", "basic")],
    UploadPackPolicy {
      max_concurrent_processes: 1,
      ..Default::default()
    },
  )
  .await;
  // Held directly rather than by racing two real clones: the property under
  // test is that exhaustion produces an actionable refusal, and a race would
  // test the scheduler instead.
  let held = Arc::clone(&fx.server.gateway.admission)
    .try_acquire_owned()
    .expect("the only permit");

  let (status, _, _) = http_get(
    &format!("{}/info/refs?service=git-upload-pack", fx.url("r-git")),
    OWNER_TOKEN,
    None,
  )
  .await;
  assert_eq!(
    status, 429,
    "an exhausted process budget must refuse, not queue"
  );

  drop(held);
  let (status, _, _) = http_get(
    &format!("{}/info/refs?service=git-upload-pack", fx.url("r-git")),
    OWNER_TOKEN,
    None,
  )
  .await;
  assert_eq!(status, 200, "the permit was not returned");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_disconnects_mid_transfer_releases_its_child() {
  use tokio::io::AsyncWriteExt;

  let fx = start(&[("r-git", "content")]).await;
  let before = fx.server.gateway.admission.available_permits();

  // Send the request, read nothing, and hang up. The 16 MiB fixture guarantees
  // the child is still producing output when the socket closes.
  {
    let authority = fx.base.strip_prefix("http://").unwrap();
    let mut socket = tokio::net::TcpStream::connect(authority).await.unwrap();
    socket
      .write_all(
        format!(
          "GET /v1/repos/r-git/info/refs?service=git-upload-pack HTTP/1.1\r\n\
           Host: {authority}\r\nAuthorization: Bearer {OWNER_TOKEN}\r\n\r\n"
        )
        .as_bytes(),
      )
      .await
      .unwrap();
    // Dropped without reading a byte.
  }

  // The permit comes back only when the pump task ends, and the pump task ends
  // only when the child has been reaped. Waiting on the permit is therefore a
  // direct assertion about reaping without inspecting the process table.
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
  while fx.server.gateway.admission.available_permits() < before {
    assert!(
      std::time::Instant::now() < deadline,
      "a disconnected transfer never released its process permit"
    );
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
  }

  // And the server is still serving.
  let (status, _, _) = http_get(
    &format!("{}/info/refs?service=git-upload-pack", fx.url("r-git")),
    OWNER_TOKEN,
    None,
  )
  .await;
  assert_eq!(status, 200);
}

// ---------------------------------------------------------------------------
// Client version matrix
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_configured_git_client_version_clones() {
  // PLAN.md M5.2 asks for multiple maintained Git client versions on Linux and
  // at least one other OS. Only the pinned 2.53.0 is installed here, so the
  // matrix is driven by `GFS_GIT_CLIENTS` -- a colon-separated list of `git`
  // binaries -- and reports what it actually ran. An unset variable means the
  // row was **not** covered, which the M5 report records as a carried-forward
  // gap rather than a pass.
  let Ok(clients) = std::env::var("GFS_GIT_CLIENTS") else {
    eprintln!(
      "SKIPPED: set GFS_GIT_CLIENTS=/path/to/git:/path/to/other-git to run the \
       client version matrix. Only the pinned client is covered without it."
    );
    return;
  };

  let fx = start(&[("r-git", "basic")]).await;
  let direct_tmp = tempfile::tempdir().unwrap();
  let direct = direct_tmp.path().join("direct");
  direct_clone(&fx.repo_path, &direct);

  for binary in clients.split(':').filter(|s| !s.is_empty()) {
    for version in ["0", "2"] {
      let tmp = tempfile::tempdir().unwrap();
      let checkout = tmp.path().join("c");
      let out = Command::new(binary)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
          "-c",
          &format!("http.extraHeader=Authorization: Bearer {OWNER_TOKEN}"),
          "-c",
          &format!("protocol.version={version}"),
          "clone",
          "-q",
          &fx.url("r-git"),
          checkout.to_str().unwrap(),
        ])
        .output()
        .unwrap_or_else(|e| panic!("{binary} is not runnable: {e}"));
      assert!(
        out.status.success(),
        "{binary} v{version}: {}",
        String::from_utf8_lossy(&out.stderr)
      );
      assert_eq!(tree_of(&checkout), tree_of(&direct), "{binary} v{version}");
      eprintln!("ok   {binary} protocol v{version}");
    }
  }
}

// ---------------------------------------------------------------------------
// Push: receive-pack confined to the caller's work namespace
// ---------------------------------------------------------------------------

/// The work-ref root the fixture's owner token folds to.
const OWNER_WORK_ROOT: &str = "refs/gfs/work/job-owner";

/// A ref's value in the served bare repository, straight off the filesystem.
fn server_ref(fx: &Fixture, name: &str) -> Option<String> {
  let out = git_client(
    &["-C", fx.repo_path.to_str().unwrap(), "rev-parse", name],
    None,
  );
  out.status.success().then(|| stdout(&out).trim().to_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_push_lands_in_the_callers_work_namespace_and_is_fsck_clean() {
  let fx = start(&[("r-git", "basic")]).await;
  let tmp = tempfile::tempdir().unwrap();
  let clone = clone_and_verify(&fx, &[], tmp.path());

  // A local commit, the way a workspace's real `.git` produces one.
  std::fs::write(clone.join("pushed.txt"), "pushed through the gateway\n").unwrap();
  let out = git_client(&["-C", clone.to_str().unwrap(), "add", "pushed.txt"], None);
  assert!(out.status.success(), "{}", stderr(&out));
  let out = Command::new("git")
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env("GIT_AUTHOR_NAME", "a")
    .env("GIT_AUTHOR_EMAIL", "a@example.com")
    .env("GIT_COMMITTER_NAME", "a")
    .env("GIT_COMMITTER_EMAIL", "a@example.com")
    .args([
      "-C",
      clone.to_str().unwrap(),
      "commit",
      "-q",
      "-m",
      "local work",
    ])
    .output()
    .unwrap();
  assert!(out.status.success(), "{}", stderr(&out));

  let out = git_client(
    &[
      "-C",
      clone.to_str().unwrap(),
      "push",
      "-q",
      "origin",
      &format!("HEAD:{OWNER_WORK_ROOT}/feature"),
    ],
    Some(OWNER_TOKEN),
  );
  assert!(out.status.success(), "push failed: {}", stderr(&out));

  // The ref exists on the server at exactly the pushed commit, and the
  // repository survives fsck with the pushed objects in it.
  let local = git_client(&["-C", clone.to_str().unwrap(), "rev-parse", "HEAD"], None);
  let pushed = server_ref(&fx, &format!("{OWNER_WORK_ROOT}/feature"));
  assert_eq!(pushed.as_deref(), Some(stdout(&local).trim()));
  fsck_clean(&fx.repo_path);

  // A second push of the same branch fast-forwards through the CAS the
  // advertisement provides.
  std::fs::write(clone.join("pushed.txt"), "amended\n").unwrap();
  let out = Command::new("git")
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env("GIT_AUTHOR_NAME", "a")
    .env("GIT_AUTHOR_EMAIL", "a@example.com")
    .env("GIT_COMMITTER_NAME", "a")
    .env("GIT_COMMITTER_EMAIL", "a@example.com")
    .args(["-C", clone.to_str().unwrap(), "commit", "-aq", "-m", "more"])
    .output()
    .unwrap();
  assert!(out.status.success(), "{}", stderr(&out));
  let out = git_client(
    &[
      "-C",
      clone.to_str().unwrap(),
      "push",
      "-q",
      "origin",
      &format!("HEAD:{OWNER_WORK_ROOT}/feature"),
    ],
    Some(OWNER_TOKEN),
  );
  assert!(out.status.success(), "second push failed: {}", stderr(&out));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_push_outside_branches_and_the_work_namespace_is_refused_by_name() {
  let fx = start(&[("r-git", "basic")]).await;
  let tmp = tempfile::tempdir().unwrap();
  let clone = clone_and_verify(&fx, &[], tmp.path());

  for refused in [
    "HEAD:refs/tags/v999".to_owned(),
    "HEAD:refs/gfs/work/job-outsider/steal".to_owned(),
    "HEAD:refs/gfs/mounts/m-fake".to_owned(),
  ] {
    let out = git_client(
      &["-C", clone.to_str().unwrap(), "push", "origin", &refused],
      Some(OWNER_TOKEN),
    );
    assert!(!out.status.success(), "{refused} must be refused");
  }
  // Nothing moved.
  assert_eq!(server_ref(&fx, "refs/tags/v999"), None);
  assert_eq!(server_ref(&fx, "refs/gfs/work/job-outsider/steal"), None);

  // An outsider cannot push anywhere, including their own would-be namespace:
  // repository authorization comes first.
  let out = git_client(
    &[
      "-C",
      clone.to_str().unwrap(),
      "push",
      "origin",
      "HEAD:refs/gfs/work/job-outsider/feature",
    ],
    Some(OUTSIDER_TOKEN),
  );
  assert!(!out.status.success(), "an outsider push must be refused");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_push_to_a_branch_lands_on_the_branch() {
  // The fork contract, end to end: `git push origin <branch>` updates the
  // gateway's real branch, the way any other Git host would take it.
  let fx = start(&[("r-git", "basic")]).await;
  let tmp = tempfile::tempdir().unwrap();
  let clone = clone_and_verify(&fx, &[], tmp.path());

  std::fs::write(clone.join("pushed.txt"), "onto a real branch\n").unwrap();
  let out = git_client(&["-C", clone.to_str().unwrap(), "add", "pushed.txt"], None);
  assert!(out.status.success(), "{}", stderr(&out));
  let out = Command::new("git")
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env("GIT_AUTHOR_NAME", "a")
    .env("GIT_AUTHOR_EMAIL", "a@example.com")
    .env("GIT_COMMITTER_NAME", "a")
    .env("GIT_COMMITTER_EMAIL", "a@example.com")
    .args([
      "-C",
      clone.to_str().unwrap(),
      "commit",
      "-q",
      "-m",
      "local work",
    ])
    .output()
    .unwrap();
  assert!(out.status.success(), "{}", stderr(&out));

  // A fast-forward of `main` itself, and a new branch: both are ordinary
  // branch pushes now.
  for refspec in ["HEAD:refs/heads/main", "HEAD:refs/heads/topic"] {
    let out = git_client(
      &[
        "-C",
        clone.to_str().unwrap(),
        "push",
        "-q",
        "origin",
        refspec,
      ],
      Some(OWNER_TOKEN),
    );
    assert!(out.status.success(), "{refspec}: {}", stderr(&out));
  }
  let local = git_client(&["-C", clone.to_str().unwrap(), "rev-parse", "HEAD"], None);
  let local = stdout(&local).trim().to_owned();
  assert_eq!(server_ref(&fx, "refs/heads/main").as_deref(), Some(&*local));
  assert_eq!(
    server_ref(&fx, "refs/heads/topic").as_deref(),
    Some(&*local)
  );
  fsck_clean(&fx.repo_path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_push_advertisement_shows_branches_and_the_callers_own_subtree() {
  let fx = start(&[("r-git", "basic")]).await;

  // Plant refs a leak would show: another subject's work branch and a lease
  // anchor. `main` exists from the fixture.
  let head = server_ref(&fx, "refs/heads/main").unwrap();
  for name in [
    &format!("{OWNER_WORK_ROOT}/mine"),
    "refs/gfs/work/someone-else/theirs",
    "refs/gfs/mounts/m-1",
  ] {
    let out = git_client(
      &[
        "-C",
        fx.repo_path.to_str().unwrap(),
        "update-ref",
        name,
        &head,
      ],
      None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
  }

  let url = format!("{}/info/refs?service=git-receive-pack", fx.url("r-git"));
  let (status, headers, body) = http_get(&url, OWNER_TOKEN, None).await;
  assert_eq!(status, 200);
  assert_eq!(
    headers.get("content-type").map(String::as_str),
    Some("application/x-git-receive-pack-advertisement")
  );
  let body = String::from_utf8_lossy(&body);
  assert!(
    body.starts_with("001f# service=git-receive-pack"),
    "the service preamble is required: {body:?}"
  );
  assert!(
    body.contains(&format!("{OWNER_WORK_ROOT}/mine")),
    "the caller's own work refs are advertised: {body}"
  );
  assert!(
    body.contains("refs/heads/main"),
    "branches take pushes, so they are advertised: {body}"
  );
  for hidden in ["someone-else", "refs/gfs/mounts"] {
    assert!(
      !body.contains(hidden),
      "{hidden} must not be advertised to a pusher: {body}"
    );
  }
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

fn pkt(payload: &[u8]) -> Vec<u8> {
  let mut framed = format!("{:04x}", payload.len() + 4).into_bytes();
  framed.extend_from_slice(payload);
  framed
}

/// Register one more repository on a running fixture and return its clone URL.
async fn register_extra(fx: &Fixture, id: &str, path: &Path) -> String {
  let repo_id = RepositoryId::parse(id).unwrap();
  fx.server
    .catalog
    .create_repository(&NewRepository {
      repository_id: repo_id.clone(),
      display_name: DisplayName::parse(&format!("acme/{id}")).unwrap(),
      repo_path: path.to_path_buf(),
      algorithm: HashAlgorithm::Sha1,
      upstream_url: None,
      credential_ref: None,
    })
    .unwrap();
  fx.server.registry.activate(&repo_id).unwrap();
  fx.url(id)
}

type HttpResponse = (u16, std::collections::HashMap<String, String>, Vec<u8>);

async fn http_get(url: &str, token: &str, git_protocol: Option<&str>) -> HttpResponse {
  http_get_raw(url, Some(token), git_protocol).await
}

/// `POST` a raw body, optionally declaring it gzipped.
async fn http_post(
  url: &str,
  token: &str,
  git_protocol: Option<&str>,
  body: &[u8],
  gzipped: bool,
) -> HttpResponse {
  use tokio::io::{AsyncReadExt, AsyncWriteExt};

  let rest = url.strip_prefix("http://").expect("http url");
  let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
  let mut head = format!(
    "POST /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\
     Authorization: Bearer {token}\r\n\
     Content-Type: application/x-git-upload-pack-request\r\n\
     Content-Length: {}\r\n",
    body.len()
  );
  if gzipped {
    head.push_str("Content-Encoding: gzip\r\n");
  }
  if let Some(protocol) = git_protocol {
    head.push_str(&format!("Git-Protocol: {protocol}\r\n"));
  }
  head.push_str("\r\n");

  let mut stream = tokio::net::TcpStream::connect(authority).await.unwrap();
  stream.write_all(head.as_bytes()).await.unwrap();
  stream.write_all(body).await.unwrap();
  let mut raw = Vec::new();
  stream.read_to_end(&mut raw).await.unwrap();
  parse_response(&raw)
}

/// A minimal HTTP/1.1 client.
///
/// Hand-written rather than pulled in: the tests need to send a `Git-Protocol`
/// header and read a raw body, and a client library would add a dependency for
/// two requests' worth of work.
async fn http_get_raw(url: &str, token: Option<&str>, git_protocol: Option<&str>) -> HttpResponse {
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
  parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> HttpResponse {
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
