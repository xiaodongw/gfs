//! End-to-end tests for the gRPC and HTTP surfaces, against a real server bound to
//! a real port.
//!
//! Driven through the generated client and an HTTP client rather than by calling
//! handlers directly, so the tests cover the parts that only exist on the wire:
//! metadata-based authentication, status-code mapping, header emission, ETag
//! revalidation, and range handling.

use std::sync::Arc;

use gfs_proto::v1;
use gfs_service::auth::{AllowList, CapabilityKey, StaticTokens};
use gfs_service::catalog::repositories::NewRepository;
use gfs_service::{Catalog, Server};
use gfs_types::{DisplayName, HashAlgorithm, LeasePolicy, RepositoryId, SubjectId};

const OWNER_TOKEN: &str = "token-owner";
const OUTSIDER_TOKEN: &str = "token-outsider";

struct Fixture {
  grpc: String,
  http: String,
  repo_id: RepositoryId,
  repo_path: std::path::PathBuf,
  /// The `bytes` fixture, registered alongside `basic` so the non-UTF-8 path case
  /// has a real stored path to fetch rather than only an encoding to validate.
  bytes_id: RepositoryId,
  _shutdown: tokio::sync::watch::Sender<bool>,
  _tmp: tempfile::TempDir,
  _bytes_tmp: tempfile::TempDir,
}

async fn start() -> Fixture {
  let (tmp, repo_path) = gfs_test::scratch_clone("basic").unwrap();
  let catalog = Arc::new(Catalog::open_in_memory().unwrap());
  let repo_id = RepositoryId::parse("r-api").unwrap();
  catalog
    .create_repository(&NewRepository {
      repository_id: repo_id.clone(),
      display_name: DisplayName::parse("acme/monorepo").unwrap(),
      repo_path: repo_path.clone(),
      algorithm: HashAlgorithm::Sha1,
      upstream_url: None,
      credential_ref: None,
    })
    .unwrap();

  let (bytes_tmp, bytes_path) = gfs_test::scratch_clone("bytes").unwrap();
  let bytes_id = RepositoryId::parse("r-bytes").unwrap();
  catalog
    .create_repository(&NewRepository {
      repository_id: bytes_id.clone(),
      display_name: DisplayName::parse("acme/bytes").unwrap(),
      repo_path: bytes_path,
      algorithm: HashAlgorithm::Sha1,
      upstream_url: None,
      credential_ref: None,
    })
    .unwrap();

  let owner = SubjectId::parse("job-owner").unwrap();
  let outsider = SubjectId::parse("job-outsider").unwrap();
  let authenticator = Arc::new(
    StaticTokens::new()
      .with_token(OWNER_TOKEN, owner.clone())
      .with_token(OUTSIDER_TOKEN, outsider),
  );
  // Only the owner may read the repository, so the outsider exercises the
  // existence-masking path with a *valid* credential.
  let policy = Arc::new(
    AllowList::new()
      .allow(&owner, &repo_id)
      .allow(&owner, &bytes_id),
  );

  let server = Arc::new(Server::new(
    Arc::clone(&catalog),
    authenticator,
    policy,
    CapabilityKey::generate().unwrap(),
    LeasePolicy::adr_0006(),
  ));
  server.registry.activate(&repo_id).unwrap();
  server.registry.activate(&bytes_id).unwrap();
  server.recover().await.unwrap();

  let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

  let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let http_addr = http_listener.local_addr().unwrap();
  let router = server.http_router();
  let mut http_shutdown = shutdown_rx.clone();
  tokio::spawn(async move {
    axum::serve(http_listener, router)
      .with_graceful_shutdown(async move {
        let _ = http_shutdown.changed().await;
      })
      .await
  });

  let grpc_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let grpc_addr = grpc_listener.local_addr().unwrap();
  let api = server.snapshot_api();
  let mut grpc_shutdown = shutdown_rx.clone();
  tokio::spawn(async move {
    tonic::transport::Server::builder()
      .add_service(gfs_proto::SnapshotServiceServer::new(api))
      .serve_with_incoming_shutdown(
        tokio_stream::wrappers::TcpListenerStream::new(grpc_listener),
        async move {
          let _ = grpc_shutdown.changed().await;
        },
      )
      .await
  });

  Fixture {
    grpc: format!("http://{grpc_addr}"),
    http: format!("http://{http_addr}"),
    repo_id,
    repo_path,
    bytes_id,
    _shutdown: shutdown_tx,
    _tmp: tmp,
    _bytes_tmp: bytes_tmp,
  }
}

/// A gRPC client that attaches a bearer token to every request.
async fn client(
  fixture: &Fixture,
  token: &str,
) -> v1::snapshot_service_client::SnapshotServiceClient<
  tonic::service::interceptor::InterceptedService<
    tonic::transport::Channel,
    impl FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status>,
  >,
> {
  let channel = tonic::transport::Endpoint::from_shared(fixture.grpc.clone())
    .unwrap()
    .connect()
    .await
    .unwrap();
  let header: tonic::metadata::MetadataValue<_> = format!("Bearer {token}").parse().unwrap();
  v1::snapshot_service_client::SnapshotServiceClient::with_interceptor(
    channel,
    move |mut req: tonic::Request<()>| {
      req.metadata_mut().insert("authorization", header.clone());
      Ok(req)
    },
  )
}

// ---------------------------------------------------------------------------
// gRPC
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resolve_revision_returns_the_pinned_commit_and_a_sanitized_time() {
  let f = start().await;
  let mut c = client(&f, OWNER_TOKEN).await;
  let resp = c
    .resolve_revision(v1::ResolveRevisionRequest {
      repository_id: f.repo_id.to_string(),
      revision_selector: "main".to_owned(),
    })
    .await
    .unwrap()
    .into_inner();

  let expected = gfs_test::git(&f.repo_path, &["rev-parse", "main"])
    .unwrap()
    .trim()
    .to_owned();
  assert_eq!(resp.commit_oid, format!("sha1:{expected}"));
  assert_eq!(resp.ref_name.as_deref(), Some("refs/heads/main"));

  // The API never returns a raw committer timestamp: ADR 0006 makes this the base
  // filesystem clock, and a future-dated commit would become a future-dated file.
  let t = resp.snapshot_time.unwrap();
  assert!(t.secs >= gfs_types::time::MIN_SUPPORTED_UNIX_SECS);
  assert!(t.secs <= gfs_types::Timestamp::now().secs);
}

#[tokio::test]
async fn a_revision_expression_is_refused_at_the_api_boundary() {
  // The closed grammar, enforced before libgit2 sees the string. `main^{tree}` is
  // the dangerous one: it resolves, and yields a tree where every layer expects a
  // commit.
  //
  // `HEAD~1` and `main^` used to be in this list. They are answered now — parsed
  // into explicit parent hops and applied with `commit.parent(n)` — so what this
  // asserts is the part that actually mattered: nothing reaches `revparse`, and
  // `^` may be followed by digits or by nothing.
  let f = start().await;
  let mut c = client(&f, OWNER_TOKEN).await;
  for expr in [
    "main^{tree}",
    "main^{commit}",
    "v1.0..v2.0",
    "main:src",
    "HEAD@{2}",
    "main~1x",
  ] {
    let status = c
      .resolve_revision(v1::ResolveRevisionRequest {
        repository_id: f.repo_id.to_string(),
        revision_selector: expr.to_owned(),
      })
      .await
      .unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument, "{expr}");
  }
}

#[tokio::test]
async fn an_ancestry_expression_walks_parents_and_drops_the_ref() {
  // The `basic` fixture is linear, so `main~1` is `main`'s parent and `main^` is
  // the same commit by the other spelling.
  let f = start().await;
  let mut c = client(&f, OWNER_TOKEN).await;

  macro_rules! resolve {
    ($selector:expr) => {
      c.resolve_revision(v1::ResolveRevisionRequest {
        repository_id: f.repo_id.to_string(),
        revision_selector: $selector.to_owned(),
      })
      .await
      .map(|r| r.into_inner())
    };
  }

  let tip = resolve!("main").unwrap();
  assert_eq!(tip.ref_name.as_deref(), Some("refs/heads/main"));

  let parent = resolve!("main~1").unwrap();
  assert_ne!(parent.commit_oid, tip.commit_oid);
  // The ref named the *base*, not what the walk landed on. Reporting
  // `refs/heads/main` here would say that `main~1` is where main points.
  assert_eq!(parent.ref_name, None);
  // The tree comes from the walked commit, not from the base.
  assert_ne!(parent.tree_oid, tip.tree_oid);

  // `^` and `~` agree on a non-merge commit, and both spellings compose.
  assert_eq!(resolve!("main^").unwrap().commit_oid, parent.commit_oid);
  assert_eq!(resolve!("main~0").unwrap().commit_oid, tip.commit_oid);

  // Walking off the end of history is NOT_FOUND, not a silent stop at the root.
  let status = resolve!("main~500").unwrap_err();
  assert_eq!(status.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn diff_commits_renders_the_change_between_two_commits() {
  // The facility the 2026-07-29 agent report was missing entirely. The `basic`
  // fixture's second commit rewrites `src/main.rs`, adds `src/new.rs` and
  // deletes `docs/guide.md`, so one call has to report all three.
  let f = start().await;
  let mut c = client(&f, OWNER_TOKEN).await;

  let tip = c
    .resolve_revision(v1::ResolveRevisionRequest {
      repository_id: f.repo_id.to_string(),
      revision_selector: "main".to_owned(),
    })
    .await
    .unwrap()
    .into_inner();
  let base = c
    .resolve_revision(v1::ResolveRevisionRequest {
      repository_id: f.repo_id.to_string(),
      revision_selector: "main~1".to_owned(),
    })
    .await
    .unwrap()
    .into_inner();

  let diff = c
    .diff_commits(v1::DiffCommitsRequest {
      repository_id: f.repo_id.to_string(),
      base_commit_oid: base.commit_oid.clone(),
      commit_oid: tip.commit_oid.clone(),
      ..Default::default()
    })
    .await
    .unwrap()
    .into_inner();

  let by_path: std::collections::HashMap<String, &v1::DiffFileChange> = diff
    .files
    .iter()
    .map(|f| (String::from_utf8_lossy(&f.path).into_owned(), f))
    .collect();
  assert_eq!(
    by_path["src/new.rs"].status(),
    v1::ChangeStatus::Added,
    "{:?}",
    by_path.keys()
  );
  assert_eq!(by_path["docs/guide.md"].status(), v1::ChangeStatus::Deleted);
  assert_eq!(by_path["src/main.rs"].status(), v1::ChangeStatus::Modified);
  assert!(by_path["src/main.rs"].additions >= 1);
  assert!(by_path["src/main.rs"].deletions >= 1);

  // The rendering is Git's own patch format, not an approximation: it is meant
  // to be applied.
  let patch = String::from_utf8(diff.rendered.clone()).unwrap();
  assert!(
    patch.contains("diff --git a/src/main.rs b/src/main.rs"),
    "{patch}"
  );
  assert!(
    patch.contains("+fn main() { println!(\"bye\"); }"),
    "{patch}"
  );
  assert!(patch.contains("new file mode 100644"), "{patch}");
  assert!(!diff.truncated);

  // A root commit diffs against the empty tree, which is how its whole content
  // becomes reviewable rather than unreachable.
  let root = c
    .diff_commits(v1::DiffCommitsRequest {
      repository_id: f.repo_id.to_string(),
      base_commit_oid: String::new(),
      commit_oid: base.commit_oid.clone(),
      ..Default::default()
    })
    .await
    .unwrap()
    .into_inner();
  assert!(root
    .files
    .iter()
    .all(|f| f.status() == v1::ChangeStatus::Added));

  // Path limiting reduces the change set rather than the rendering alone.
  let scoped = c
    .diff_commits(v1::DiffCommitsRequest {
      repository_id: f.repo_id.to_string(),
      base_commit_oid: base.commit_oid.clone(),
      commit_oid: tip.commit_oid.clone(),
      paths: vec![b"src/new.rs".to_vec()],
      ..Default::default()
    })
    .await
    .unwrap()
    .into_inner();
  assert_eq!(scoped.files.len(), 1);
  assert_eq!(scoped.files[0].path, b"src/new.rs");

  // `--name-only` renders paths and nothing else, but still carries the same
  // structured summary -- which is the contract that lets a caller act on the
  // change set without parsing the rendering.
  let names = c
    .diff_commits(v1::DiffCommitsRequest {
      repository_id: f.repo_id.to_string(),
      base_commit_oid: base.commit_oid,
      commit_oid: tip.commit_oid,
      format: v1::DiffFormat::NameOnly as i32,
      ..Default::default()
    })
    .await
    .unwrap()
    .into_inner();
  let listed = String::from_utf8(names.rendered).unwrap();
  assert!(listed.contains("src/new.rs"), "{listed}");
  assert!(!listed.contains("diff --git"), "{listed}");
  assert_eq!(names.files.len(), diff.files.len());
}

#[tokio::test]
async fn blame_attributes_lines_and_returns_the_file() {
  let f = start().await;
  let mut c = client(&f, OWNER_TOKEN).await;
  let tip = c
    .resolve_revision(v1::ResolveRevisionRequest {
      repository_id: f.repo_id.to_string(),
      revision_selector: "main".to_owned(),
    })
    .await
    .unwrap()
    .into_inner();

  let blame = c
    .blame(v1::BlameRequest {
      repository_id: f.repo_id.to_string(),
      commit_oid: tip.commit_oid.clone(),
      path: b"src/main.rs".to_vec(),
      authorization: None,
    })
    .await
    .unwrap()
    .into_inner();

  assert!(!blame.hunks.is_empty());
  assert!(!blame.truncated);
  // The bytes travel with the hunks, because a blame without the lines it
  // attributes is not an answer.
  assert!(String::from_utf8_lossy(&blame.content).contains("fn main()"));
  // `src/main.rs` was rewritten by the second commit, so the tip is what its
  // lines are attributed to.
  assert_eq!(blame.hunks[0].commit_oid, tip.commit_oid);
  assert_eq!(blame.hunks[0].final_start_line, 1);

  // A directory has no lines, and that is a caller error rather than an empty
  // success -- the same rule `ls` follows for a path that is not a directory.
  let status = c
    .blame(v1::BlameRequest {
      repository_id: f.repo_id.to_string(),
      commit_oid: tip.commit_oid,
      path: b"src".to_vec(),
      authorization: None,
    })
    .await
    .unwrap_err();
  assert_eq!(status.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn a_path_limited_log_only_shows_commits_that_touched_it() {
  let f = start().await;
  let mut c = client(&f, OWNER_TOKEN).await;
  let tip = c
    .resolve_revision(v1::ResolveRevisionRequest {
      repository_id: f.repo_id.to_string(),
      revision_selector: "main".to_owned(),
    })
    .await
    .unwrap()
    .into_inner();

  let all = c
    .log(v1::LogRequest {
      repository_id: f.repo_id.to_string(),
      commit_oid: tip.commit_oid.clone(),
      ..Default::default()
    })
    .await
    .unwrap()
    .into_inner();
  assert!(all.commits.len() >= 2);
  // The tree travels with each commit now, so `--format=%T` costs no extra call.
  assert!(!all.commits[0].tree_oid.is_empty());

  // `src/new.rs` arrived in the second commit and nowhere else.
  let scoped = c
    .log(v1::LogRequest {
      repository_id: f.repo_id.to_string(),
      commit_oid: tip.commit_oid.clone(),
      paths: vec![b"src/new.rs".to_vec()],
      ..Default::default()
    })
    .await
    .unwrap()
    .into_inner();
  assert_eq!(scoped.commits.len(), 1);
  assert_eq!(scoped.commits[0].commit_oid, tip.commit_oid);

  // A path no commit touched is an empty history, not an error.
  let none = c
    .log(v1::LogRequest {
      repository_id: f.repo_id.to_string(),
      commit_oid: tip.commit_oid,
      paths: vec![b"nothing/here".to_vec()],
      ..Default::default()
    })
    .await
    .unwrap()
    .into_inner();
  assert!(none.commits.is_empty());
  assert!(!none.has_more);
}

#[tokio::test]
async fn the_reserved_namespace_is_refused_with_its_own_code() {
  let f = start().await;
  let mut c = client(&f, OWNER_TOKEN).await;
  for name in ["refs/gfs/mounts/m-1", "gfs/mounts/m-1"] {
    let status = c
      .resolve_revision(v1::ResolveRevisionRequest {
        repository_id: f.repo_id.to_string(),
        revision_selector: name.to_owned(),
      })
      .await
      .unwrap_err();
    // Mapped to INVALID_ARGUMENT over gRPC, and distinguishable through the GFS
    // code in the metadata -- which is the mechanism that keeps codes sharing a
    // gRPC status apart.
    assert_eq!(status.code(), tonic::Code::InvalidArgument, "{name}");
    assert_eq!(
      status.metadata().get("gfs-error-code").unwrap(),
      "RESERVED_NAMESPACE",
      "{name}"
    );
  }
}

#[tokio::test]
async fn an_unauthenticated_request_is_refused_before_anything_else() {
  let f = start().await;
  let channel = tonic::transport::Endpoint::from_shared(f.grpc.clone())
    .unwrap()
    .connect()
    .await
    .unwrap();
  let mut c = v1::snapshot_service_client::SnapshotServiceClient::new(channel);
  let status = c
    .resolve_revision(v1::ResolveRevisionRequest {
      repository_id: f.repo_id.to_string(),
      revision_selector: "main".to_owned(),
    })
    .await
    .unwrap_err();
  assert_eq!(status.code(), tonic::Code::Unauthenticated);
}

#[tokio::test]
async fn an_authenticated_outsider_cannot_distinguish_existence() {
  // The same masked answer for a repository that exists and one that does not,
  // over the wire, with a valid credential.
  let f = start().await;
  let mut c = client(&f, OUTSIDER_TOKEN).await;

  let existing = c
    .resolve_revision(v1::ResolveRevisionRequest {
      repository_id: f.repo_id.to_string(),
      revision_selector: "main".to_owned(),
    })
    .await
    .unwrap_err();
  let absent = c
    .resolve_revision(v1::ResolveRevisionRequest {
      repository_id: "r-nonexistent".to_owned(),
      revision_selector: "main".to_owned(),
    })
    .await
    .unwrap_err();

  assert_eq!(existing.code(), tonic::Code::NotFound);
  assert_eq!(existing.code(), absent.code());
  assert_eq!(existing.message(), absent.message());
}

#[tokio::test]
async fn get_entry_reports_a_missing_path_as_not_found_and_repeats_the_commit() {
  let f = start().await;
  let mut c = client(&f, OWNER_TOKEN).await;
  let commit = c
    .resolve_revision(v1::ResolveRevisionRequest {
      repository_id: f.repo_id.to_string(),
      revision_selector: "main".to_owned(),
    })
    .await
    .unwrap()
    .into_inner()
    .commit_oid;

  let found = c
    .get_entry(v1::GetEntryRequest {
      repository_id: f.repo_id.to_string(),
      commit_oid: commit.clone(),
      path: b"README.md".to_vec(),
      authorization: None,
      want_blob_ticket: true,
    })
    .await
    .unwrap()
    .into_inner();
  let entry = found.entry.unwrap();
  assert_eq!(entry.path, b"README.md");
  assert_eq!(entry.kind, v1::EntryKind::Regular as i32);
  assert!(entry.blob_ticket.is_some(), "a ticket was requested");
  // The response repeats the resolved commit, so a caller that logs or caches it
  // knows which generation it holds.
  assert_eq!(found.commit_oid, commit);

  let status = c
    .get_entry(v1::GetEntryRequest {
      repository_id: f.repo_id.to_string(),
      commit_oid: commit,
      path: b"no/such/file".to_vec(),
      authorization: None,
      want_blob_ticket: false,
    })
    .await
    .unwrap_err();
  assert_eq!(status.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn a_traversal_path_is_rejected_rather_than_normalized() {
  let f = start().await;
  let mut c = client(&f, OWNER_TOKEN).await;
  let commit = c
    .resolve_revision(v1::ResolveRevisionRequest {
      repository_id: f.repo_id.to_string(),
      revision_selector: "main".to_owned(),
    })
    .await
    .unwrap()
    .into_inner()
    .commit_oid;

  for bad in [
    &b"../etc/passwd"[..],
    b"/etc/passwd",
    b"src/../../etc",
    b"src//main.rs",
    b"with\0nul",
  ] {
    let status = c
      .get_entry(v1::GetEntryRequest {
        repository_id: f.repo_id.to_string(),
        commit_oid: commit.clone(),
        path: bad.to_vec(),
        authorization: None,
        want_blob_ticket: false,
      })
      .await
      .unwrap_err();
    assert_eq!(
      status.code(),
      tonic::Code::InvalidArgument,
      "{:?}",
      String::from_utf8_lossy(bad)
    );
  }
}

#[tokio::test]
async fn list_directory_pages_and_clamps_an_oversized_page_size() {
  let f = start().await;
  let mut c = client(&f, OWNER_TOKEN).await;
  let commit = c
    .resolve_revision(v1::ResolveRevisionRequest {
      repository_id: f.repo_id.to_string(),
      revision_selector: "main".to_owned(),
    })
    .await
    .unwrap()
    .into_inner()
    .commit_oid;

  // One entry per page, walked to completion.
  let mut seen = Vec::new();
  let mut token = Vec::new();
  loop {
    let page = c
      .list_directory(v1::ListDirectoryRequest {
        repository_id: f.repo_id.to_string(),
        commit_oid: commit.clone(),
        path: Vec::new(),
        page_token: token.clone(),
        page_size: 1,
        authorization: None,
        want_blob_tickets: false,
      })
      .await
      .unwrap()
      .into_inner();
    assert_eq!(page.commit_oid, commit);
    assert!(page.entries.len() <= 1);
    for e in page.entries {
      seen.push(e.path);
    }
    if page.next_page_token.is_empty() {
      break;
    }
    token = page.next_page_token;
  }

  let expected = gfs_test::git(&f.repo_path, &["ls-tree", "--name-only", "HEAD"])
    .unwrap()
    .lines()
    .count();
  assert_eq!(seen.len(), expected, "paging lost or duplicated entries");

  // An absurd page size is clamped, not rejected: a smaller page plus a token is a
  // better outcome than an error the client has to learn to handle.
  let clamped = c
    .list_directory(v1::ListDirectoryRequest {
      repository_id: f.repo_id.to_string(),
      commit_oid: commit,
      path: Vec::new(),
      page_token: Vec::new(),
      page_size: u32::MAX,
      authorization: None,
      want_blob_tickets: false,
    })
    .await
    .unwrap()
    .into_inner();
  assert!(clamped.entries.len() <= gfs_types::limits::MAX_DIRECTORY_PAGE_SIZE);
}

#[tokio::test]
async fn batch_get_entry_returns_positional_results_with_per_path_errors() {
  // The positional contract is what makes per-item errors usable: a client can
  // always match a result to the path it asked about.
  let f = start().await;
  let mut c = client(&f, OWNER_TOKEN).await;
  let commit = c
    .resolve_revision(v1::ResolveRevisionRequest {
      repository_id: f.repo_id.to_string(),
      revision_selector: "main".to_owned(),
    })
    .await
    .unwrap()
    .into_inner()
    .commit_oid;

  let resp = c
    .batch_get_entry(v1::BatchGetEntryRequest {
      repository_id: f.repo_id.to_string(),
      commit_oid: commit.clone(),
      paths: vec![
        b"README.md".to_vec(),
        b"no/such/file".to_vec(),
        b"../traversal".to_vec(),
        b"src/main.rs".to_vec(),
      ],
      authorization: None,
      want_blob_tickets: false,
    })
    .await
    .unwrap()
    .into_inner();

  assert_eq!(resp.results.len(), 4, "same length as the request");
  assert_eq!(resp.commit_oid, commit);

  match &resp.results[0].result {
    Some(v1::entry_result::Result::Entry(e)) => assert_eq!(e.path, b"README.md"),
    other => panic!("expected an entry, got {other:?}"),
  }
  match &resp.results[1].result {
    Some(v1::entry_result::Result::Error(e)) => assert_eq!(e.code, "NOT_FOUND"),
    other => panic!("expected NOT_FOUND, got {other:?}"),
  }
  // An invalid path fails that item only; it does not discard the batch.
  match &resp.results[2].result {
    Some(v1::entry_result::Result::Error(e)) => assert_eq!(e.code, "INVALID_ARGUMENT"),
    other => panic!("expected INVALID_ARGUMENT, got {other:?}"),
  }
  match &resp.results[3].result {
    Some(v1::entry_result::Result::Entry(e)) => assert_eq!(e.path, b"src/main.rs"),
    other => panic!("expected an entry, got {other:?}"),
  }
}

#[tokio::test]
async fn a_mount_can_be_created_renewed_and_released_over_grpc() {
  let f = start().await;
  let mut c = client(&f, OWNER_TOKEN).await;

  let mount = c
    .create_mount(v1::CreateMountRequest {
      repository_id: f.repo_id.to_string(),
      revision_selector: "main".to_owned(),
      requested_ttl_seconds: 0,
    })
    .await
    .unwrap()
    .into_inner();

  assert!(!mount.mount_capability.is_empty());
  assert!(mount.heartbeat_interval_seconds > 0);
  assert_eq!(mount.ref_name.as_deref(), Some("refs/heads/main"));

  let renewed = c
    .renew_mount(v1::RenewMountRequest {
      mount_id: mount.mount_id.clone(),
      mount_capability: mount.mount_capability.clone(),
    })
    .await
    .unwrap()
    .into_inner();
  assert!(!renewed.mount_capability.is_empty());
  assert!(renewed.lease_expiry.unwrap().secs >= mount.lease_expiry.unwrap().secs);

  // A capability from another subject cannot release it.
  let mut outsider = client(&f, OUTSIDER_TOKEN).await;
  let status = outsider
    .release_mount(v1::ReleaseMountRequest {
      mount_id: mount.mount_id.clone(),
      mount_capability: mount.mount_capability.clone(),
    })
    .await
    .unwrap_err();
  assert_eq!(status.code(), tonic::Code::NotFound);

  c.release_mount(v1::ReleaseMountRequest {
    mount_id: mount.mount_id,
    mount_capability: mount.mount_capability,
  })
  .await
  .unwrap();
}

#[tokio::test]
async fn a_capability_reaches_a_commit_that_a_force_push_made_unreachable() {
  // The end-to-end version of M1's object-authorization rule, over the wire.
  let f = start().await;
  let mut c = client(&f, OWNER_TOKEN).await;

  let mount = c
    .create_mount(v1::CreateMountRequest {
      repository_id: f.repo_id.to_string(),
      revision_selector: "main".to_owned(),
      requested_ttl_seconds: 0,
    })
    .await
    .unwrap()
    .into_inner();

  let older = gfs_test::git(&f.repo_path, &["rev-parse", "v1.0"])
    .unwrap()
    .trim()
    .to_owned();
  gfs_test::git(&f.repo_path, &["update-ref", "refs/heads/main", &older]).unwrap();
  gfs_test::git(&f.repo_path, &["update-ref", "-d", "refs/heads/feature"]).unwrap();
  gfs_test::git(&f.repo_path, &["tag", "-d", "v2.0"]).unwrap();
  gfs_test::git(&f.repo_path, &["tag", "-d", "tree-tag"]).unwrap();

  // Without the capability the commit is masked.
  let status = c
    .get_entry(v1::GetEntryRequest {
      repository_id: f.repo_id.to_string(),
      commit_oid: mount.commit_oid.clone(),
      path: b"src/new.rs".to_vec(),
      authorization: None,
      want_blob_ticket: false,
    })
    .await
    .unwrap_err();
  assert_eq!(status.code(), tonic::Code::NotFound);

  // With it, the pinned snapshot is still fully readable -- which is the whole
  // point of the retention lease.
  let entry = c
    .get_entry(v1::GetEntryRequest {
      repository_id: f.repo_id.to_string(),
      commit_oid: mount.commit_oid.clone(),
      path: b"src/new.rs".to_vec(),
      authorization: Some(v1::SnapshotAuthorization {
        mount_capability: mount.mount_capability,
      }),
      want_blob_ticket: false,
    })
    .await
    .unwrap()
    .into_inner();
  assert_eq!(entry.entry.unwrap().path, b"src/new.rs");
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

async fn http_get(
  url: &str,
  token: Option<&str>,
  extra: &[(&str, &str)],
) -> (http::StatusCode, http::HeaderMap, Vec<u8>) {
  use http_body_util::BodyExt;
  use hyper_util::rt::TokioIo;

  let uri: http::Uri = url.parse().unwrap();
  let authority = uri.authority().unwrap().to_string();
  let stream = tokio::net::TcpStream::connect(authority.clone())
    .await
    .unwrap();
  let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
    .await
    .unwrap();
  tokio::spawn(conn);

  let mut builder = http::Request::builder().uri(&uri).header("host", authority);
  if let Some(t) = token {
    builder = builder.header("authorization", format!("Bearer {t}"));
  }
  for (k, v) in extra {
    builder = builder.header(*k, *v);
  }
  let req = builder.body(String::new()).unwrap();
  let resp = sender.send_request(req).await.unwrap();
  let (parts, body) = resp.into_parts();
  let bytes = body.collect().await.unwrap().to_bytes().to_vec();
  (parts.status, parts.headers, bytes)
}

#[tokio::test]
async fn health_and_readiness_are_separate_endpoints() {
  let f = start().await;
  let (status, _, body) = http_get(&format!("{}/healthz", f.http), None, &[]).await;
  assert_eq!(status, http::StatusCode::OK);
  assert_eq!(body, b"ok\n");
  let (status, _, _) = http_get(&format!("{}/readyz", f.http), None, &[]).await;
  assert_eq!(status, http::StatusCode::OK);
}

#[tokio::test]
async fn file_by_revision_returns_raw_bytes_and_the_resolved_commit() {
  let f = start().await;
  let path_b64 = gfs_types::path::b64url_encode(b"README.md");
  let url = format!(
    "{}/v1/repos/{}/file?rev=main&path_b64url={path_b64}",
    f.http, f.repo_id
  );
  let (status, headers, body) = http_get(&url, Some(OWNER_TOKEN), &[]).await;

  assert_eq!(status, http::StatusCode::OK);
  assert_eq!(body, b"# basic\n");
  // The resolved commit is returned so a caller can convert this convenience call
  // into an immutable, cacheable blob request.
  let expected = gfs_test::git(&f.repo_path, &["rev-parse", "main"])
    .unwrap()
    .trim()
    .to_owned();
  assert_eq!(
    headers.get("x-gfs-commit").unwrap(),
    &format!("sha1:{expected}")
  );
  assert_eq!(headers.get("x-gfs-mode").unwrap(), "100644");
  assert!(headers.get("x-gfs-blob-oid").is_some());
  // Never cached: the same URL names a different file once the branch moves.
  assert_eq!(headers.get("cache-control").unwrap(), "no-store");
  assert!(headers.get("x-request-id").is_some());
}

#[tokio::test]
async fn a_non_utf8_path_survives_the_base64url_query_parameter() {
  // The reason DESIGN.md section 7.3 chose base64url over a percent-encoded path
  // segment: a proxy in the middle is entitled to normalize the latter, and these
  // bytes are not valid UTF-8 so they have no percent-encoded spelling a URL parser
  // is obliged to preserve.
  let f = start().await;
  let name: &[u8] = b"latin1-\xff-name.txt";
  let path_b64 = gfs_types::path::b64url_encode(name);
  let url = format!(
    "{}/v1/repos/{}/file?rev=main&path_b64url={path_b64}",
    f.http, f.bytes_id
  );
  let (status, headers, body) = http_get(&url, Some(OWNER_TOKEN), &[]).await;
  assert_eq!(
    status,
    http::StatusCode::OK,
    "a non-UTF-8 path must be fetchable: {}",
    String::from_utf8_lossy(&body)
  );
  assert_eq!(body, b"content\n");
  assert!(headers.get("x-gfs-blob-oid").is_some());

  // And an undecodable parameter is an INVALID_ARGUMENT, not a panic or a silently
  // empty path.
  let bad = format!(
    "{}/v1/repos/{}/file?rev=main&path_b64url=%21%21%21",
    f.http, f.bytes_id
  );
  let (status, _, _) = http_get(&bad, Some(OWNER_TOKEN), &[]).await;
  assert_eq!(status, http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_immutable_blob_endpoint_requires_a_ticket() {
  // DESIGN.md section 7.3: repository access is not enough. Without the ticket, a
  // caller with repository access could read any blob OID it could guess or
  // observe, including one reachable only from another subject's retained commit.
  let f = start().await;
  let mut c = client(&f, OWNER_TOKEN).await;
  let commit = c
    .resolve_revision(v1::ResolveRevisionRequest {
      repository_id: f.repo_id.to_string(),
      revision_selector: "main".to_owned(),
    })
    .await
    .unwrap()
    .into_inner()
    .commit_oid;
  let entry = c
    .get_entry(v1::GetEntryRequest {
      repository_id: f.repo_id.to_string(),
      commit_oid: commit,
      path: b"README.md".to_vec(),
      authorization: None,
      want_blob_ticket: true,
    })
    .await
    .unwrap()
    .into_inner()
    .entry
    .unwrap();
  let ticket = entry.blob_ticket.unwrap();

  // With the ticket.
  let url = format!(
    "{}/v1/repos/{}/blobs/{}?ticket={}",
    f.http, f.repo_id, entry.oid, ticket
  );
  let (status, headers, body) = http_get(&url, Some(OWNER_TOKEN), &[]).await;
  assert_eq!(status, http::StatusCode::OK);
  assert_eq!(body, b"# basic\n");
  // The ETag *is* the object ID, so a match proves the client holds these exact
  // bytes -- there is no staleness window to reason about.
  assert_eq!(headers.get("etag").unwrap(), &format!("\"{}\"", entry.oid));
  assert_eq!(
    headers.get("cache-control").unwrap(),
    "public, max-age=31536000, immutable"
  );
  assert_eq!(headers.get("accept-ranges").unwrap(), "bytes");

  // A forged or absent ticket is refused.
  let bad = format!(
    "{}/v1/repos/{}/blobs/{}?ticket=gfs1.AAAA.AAAA",
    f.http, f.repo_id, entry.oid
  );
  let (status, _, _) = http_get(&bad, Some(OWNER_TOKEN), &[]).await;
  assert_eq!(status, http::StatusCode::UNAUTHORIZED);

  // Another subject's request with the owner's ticket is masked.
  let (status, _, _) = http_get(&url, Some(OUTSIDER_TOKEN), &[]).await;
  assert_eq!(status, http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn blob_revalidation_and_range_requests_work() {
  let f = start().await;
  let mut c = client(&f, OWNER_TOKEN).await;
  let commit = c
    .resolve_revision(v1::ResolveRevisionRequest {
      repository_id: f.repo_id.to_string(),
      revision_selector: "main".to_owned(),
    })
    .await
    .unwrap()
    .into_inner()
    .commit_oid;
  let entry = c
    .get_entry(v1::GetEntryRequest {
      repository_id: f.repo_id.to_string(),
      commit_oid: commit,
      path: b"README.md".to_vec(),
      authorization: None,
      want_blob_ticket: true,
    })
    .await
    .unwrap()
    .into_inner()
    .entry
    .unwrap();
  let url = format!(
    "{}/v1/repos/{}/blobs/{}?ticket={}",
    f.http,
    f.repo_id,
    entry.oid,
    entry.blob_ticket.unwrap()
  );

  // Revalidation.
  let etag = format!("\"{}\"", entry.oid);
  let (status, _, body) = http_get(&url, Some(OWNER_TOKEN), &[("if-none-match", &etag)]).await;
  assert_eq!(status, http::StatusCode::NOT_MODIFIED);
  assert!(body.is_empty());

  // A range.
  let (status, headers, body) = http_get(&url, Some(OWNER_TOKEN), &[("range", "bytes=2-5")]).await;
  assert_eq!(status, http::StatusCode::PARTIAL_CONTENT);
  assert_eq!(body, b"basi");
  assert_eq!(headers.get("content-range").unwrap(), "bytes 2-5/8");

  // An unsatisfiable range reports the real length so the client can correct it.
  let (status, headers, _) = http_get(&url, Some(OWNER_TOKEN), &[("range", "bytes=100-200")]).await;
  assert_eq!(status, http::StatusCode::RANGE_NOT_SATISFIABLE);
  assert_eq!(headers.get("content-range").unwrap(), "bytes */8");
}

#[tokio::test]
async fn an_http_error_body_carries_the_stable_code_and_the_request_id() {
  let f = start().await;
  let path_b64 = gfs_types::path::b64url_encode(b"no/such/file");
  let url = format!(
    "{}/v1/repos/{}/file?rev=main&path_b64url={path_b64}",
    f.http, f.repo_id
  );
  let (status, headers, body) =
    http_get(&url, Some(OWNER_TOKEN), &[("x-request-id", "trace-abc123")]).await;

  assert_eq!(status, http::StatusCode::NOT_FOUND);
  let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
  assert_eq!(json["code"], "NOT_FOUND");
  // The client's identifier is preserved so its trace and the server's join.
  assert_eq!(json["request_id"], "trace-abc123");
  assert_eq!(headers.get("x-request-id").unwrap(), "trace-abc123");
}

#[tokio::test]
async fn a_webhook_records_a_ref_event_idempotently() {
  use http_body_util::BodyExt;
  use hyper_util::rt::TokioIo;

  let f = start().await;
  let commit = gfs_test::git(&f.repo_path, &["rev-parse", "main"])
    .unwrap()
    .trim()
    .to_owned();
  let url = format!("{}/v1/repos/{}/ref-events", f.http, f.repo_id);
  let uri: http::Uri = url.parse().unwrap();
  let authority = uri.authority().unwrap().to_string();

  let post = |body: String| {
    let uri = uri.clone();
    let authority = authority.clone();
    async move {
      let stream = tokio::net::TcpStream::connect(authority.clone())
        .await
        .unwrap();
      let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
      tokio::spawn(conn);
      let req = http::Request::builder()
        .method("POST")
        .uri(&uri)
        .header("host", authority)
        .header("authorization", format!("Bearer {OWNER_TOKEN}"))
        .header("content-type", "application/json")
        .body(body)
        .unwrap();
      let resp = sender.send_request(req).await.unwrap();
      let (parts, body) = resp.into_parts();
      let bytes = body.collect().await.unwrap().to_bytes().to_vec();
      (parts.status, bytes)
    }
  };

  let payload = format!(r#"{{"ref_name":"refs/heads/topic","new_oid":"sha1:{commit}"}}"#);
  let (status, body) = post(payload.clone()).await;
  assert_eq!(status, http::StatusCode::ACCEPTED);
  let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
  assert_eq!(json["outcome"], "Created");

  // Delivered twice: the second collapses, because the event key is
  // (repository, ref, old_oid, new_oid).
  let (status, body) = post(payload).await;
  assert_eq!(status, http::StatusCode::ACCEPTED);
  let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
  assert_eq!(json["outcome"], "Unchanged");

  // The reserved namespace is not a mirrored ref.
  let (status, _) = post(format!(
    r#"{{"ref_name":"refs/gfs/mounts/m-1","new_oid":"sha1:{commit}"}}"#
  ))
  .await;
  assert_eq!(status, http::StatusCode::BAD_REQUEST);
}
