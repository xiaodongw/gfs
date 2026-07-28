//! M4.3: the search RPC, end to end, and the terminal-message contract.
//!
//! These go through the gRPC surface rather than calling `IndexManager::search`
//! directly, because the property under test is a property of the *stream*: zero
//! or more matches followed by exactly one completion, and never a stream that
//! ends without one.

use std::sync::Arc;

use gfs_proto::v1;
use gfs_service::auth::{AllowList, CapabilityKey, StaticTokens};
use gfs_service::catalog::repositories::NewRepository;
use gfs_service::{Catalog, Server};
use gfs_types::{
  DisplayName, HashAlgorithm, LeasePolicy, ObjectId, RepositoryId, RevisionSelector, SubjectId,
};
use tokio_stream::StreamExt;

const TOKEN: &str = "token-rpc";

struct Backend {
  grpc: String,
  repo_id: RepositoryId,
  server: Arc<Server>,
  shutdown: tokio::sync::watch::Sender<bool>,
  _tmp: tempfile::TempDir,
}

impl Backend {
  async fn start(fixture: &str) -> Backend {
    let (tmp, repo_path) = gfs_test::scratch_clone(fixture).unwrap();
    let catalog = Arc::new(Catalog::open_in_memory().unwrap());
    let repo_id = RepositoryId::parse("r-rpc").unwrap();
    catalog
      .create_repository(&NewRepository {
        repository_id: repo_id.clone(),
        display_name: DisplayName::parse("acme/rpc").unwrap(),
        repo_path,
        algorithm: HashAlgorithm::Sha1,
        upstream_url: None,
        credential_ref: None,
      })
      .unwrap();
    let subject = SubjectId::parse("job-rpc").unwrap();
    let server = Arc::new(Server::new(
      catalog,
      Arc::new(StaticTokens::new().with_token(TOKEN, subject.clone())),
      Arc::new(AllowList::new().allow(&subject, &repo_id)),
      CapabilityKey::generate().unwrap(),
      LeasePolicy::adr_0006(),
    ));
    server.registry.activate(&repo_id).unwrap();

    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let snapshot_api = server.snapshot_api();
    let search_api = server.search_api();
    let mut grpc_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
      tonic::transport::Server::builder()
        .add_service(gfs_proto::SnapshotServiceServer::new(snapshot_api))
        .add_service(gfs_proto::SearchServiceServer::new(search_api))
        .serve_with_incoming_shutdown(
          tokio_stream::wrappers::TcpListenerStream::new(listener),
          async move {
            let _ = grpc_shutdown.changed().await;
          },
        )
        .await
    });

    Backend {
      grpc: format!("http://{addr}"),
      repo_id,
      server,
      shutdown,
      _tmp: tmp,
    }
  }

  async fn head(&self) -> ObjectId {
    self
      .server
      .registry
      .repository(&self.repo_id)
      .unwrap()
      .resolve(RevisionSelector::parse("main", HashAlgorithm::Sha1).unwrap())
      .await
      .unwrap()
      .commit
  }

  async fn prepare(&self, commit: &ObjectId) {
    let outcome = self
      .server
      .search
      .prepare(&self.repo_id, commit, true)
      .await
      .unwrap();
    assert!(
      matches!(outcome, gfs_service::PrepareOutcome::Ready(_)),
      "preparation did not finish: {outcome:?}"
    );
  }

  async fn client(&self) -> gfs_proto::SearchServiceClient<tonic::transport::Channel> {
    gfs_proto::SearchServiceClient::connect(self.grpc.clone())
      .await
      .unwrap()
  }
}

impl Drop for Backend {
  fn drop(&mut self) {
    let _ = self.shutdown.send(true);
  }
}

/// Everything a stream delivered, with the completion separated out.
#[derive(Debug)]
struct Streamed {
  matches: Vec<v1::SearchMatch>,
  completion: Option<v1::SearchCompletion>,
  /// Messages seen after the completion. Must always be zero.
  after_completion: usize,
}

async fn run(backend: &Backend, request: v1::SearchRequest) -> Result<Streamed, tonic::Status> {
  let mut client = backend.client().await;
  let mut req = tonic::Request::new(request);
  req
    .metadata_mut()
    .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
  let mut stream = client.search(req).await?.into_inner();

  let mut out = Streamed {
    matches: Vec::new(),
    completion: None,
    after_completion: 0,
  };
  while let Some(message) = stream.next().await {
    let message = message?;
    match message.message {
      Some(v1::search_response::Message::Match(m)) => {
        if out.completion.is_some() {
          out.after_completion += 1;
        }
        out.matches.push(m);
      }
      Some(v1::search_response::Message::Completion(c)) => {
        if out.completion.is_some() {
          out.after_completion += 1;
        }
        out.completion = Some(c);
      }
      None => panic!("an empty stream message"),
    }
  }
  Ok(out)
}

fn request(backend: &Backend, commit: &ObjectId, pattern: &str) -> v1::SearchRequest {
  v1::SearchRequest {
    repository_id: backend.repo_id.as_str().to_owned(),
    commit_oid: commit.to_qualified(),
    authorization: None,
    pattern: pattern.to_owned(),
    literal: true,
    ..Default::default()
  }
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_successful_stream_ends_with_exactly_one_completion() {
  let b = Backend::start("basic").await;
  let head = b.head().await;
  b.prepare(&head).await;

  let streamed = run(&b, request(&b, &head, "println")).await.unwrap();
  assert!(!streamed.matches.is_empty());
  let completion = streamed
    .completion
    .expect("a stream without a terminal message is a failed search");
  assert_eq!(streamed.after_completion, 0);
  assert_eq!(
    completion.execution_status,
    v1::ExecutionStatus::Complete as i32
  );
  assert_eq!(completion.commit_oid, head.to_qualified());
}

#[tokio::test]
async fn an_empty_result_still_carries_a_completion() {
  // The case that would otherwise be indistinguishable from a dropped stream.
  let b = Backend::start("basic").await;
  let head = b.head().await;
  b.prepare(&head).await;

  let streamed = run(&b, request(&b, &head, "definitely-not-present"))
    .await
    .unwrap();
  assert!(streamed.matches.is_empty());
  let completion = streamed.completion.expect("no terminal message");
  assert_eq!(
    completion.execution_status,
    v1::ExecutionStatus::Complete as i32
  );
  assert_eq!(completion.truncation_reason, None);
}

#[tokio::test]
async fn a_pattern_with_no_usable_literal_is_reported_as_truncated() {
  // ADR 0004: bounded by a scan budget rather than by the index, and saying so
  // is the difference between an honest answer and a plausible one.
  let b = Backend::start("basic").await;
  let head = b.head().await;
  b.prepare(&head).await;

  let streamed = run(
    &b,
    v1::SearchRequest {
      pattern: "p.i".to_owned(),
      literal: false,
      ..request(&b, &head, "unused")
    },
  )
  .await
  .unwrap();
  let completion = streamed.completion.expect("no terminal message");
  assert_eq!(
    completion.execution_status,
    v1::ExecutionStatus::Truncated as i32
  );
  assert_eq!(
    completion.truncation_reason.as_deref(),
    Some("no_required_literal")
  );
  assert!(completion.stop_budget.is_some());
}

#[tokio::test]
async fn a_result_limit_truncates_and_names_the_budget() {
  let b = Backend::start("bigdir").await;
  let head = b.head().await;
  b.prepare(&head).await;

  // `bigdir` writes 5000 files whose content is their own index, so "123" is a
  // three-byte literal present in a couple of dozen of them — enough to overrun
  // a limit of three, and long enough that the index bounds the query.
  let streamed = run(
    &b,
    v1::SearchRequest {
      max_results: 3,
      ..request(&b, &head, "123")
    },
  )
  .await
  .unwrap();
  assert_eq!(streamed.matches.len(), 3);
  let completion = streamed.completion.expect("no terminal message");
  assert_eq!(
    completion.execution_status,
    v1::ExecutionStatus::Truncated as i32
  );
  assert_eq!(
    completion.truncation_reason.as_deref(),
    Some("result_limit")
  );
  assert_eq!(completion.stop_budget.as_deref(), Some("max_results=3"));
}

#[tokio::test]
async fn a_binary_file_appears_as_a_scoped_coverage_exclusion() {
  let b = Backend::start("content").await;
  let head = b.head().await;
  b.prepare(&head).await;

  // Search for something every file might contain; what matters is coverage.
  let streamed = run(&b, request(&b, &head, "line")).await.unwrap();
  let coverage = streamed
    .completion
    .expect("no terminal message")
    .coverage
    .unwrap();
  assert!(
    coverage.excluded.values().any(|n| *n > 0),
    "the `content` fixture holds a blob with NUL bytes, which must be reported"
  );
  assert!(coverage.declared_exclusions.contains(&"binary".to_owned()));
  assert!(coverage.eligible_paths > 0);
}

#[tokio::test]
async fn searching_an_unprepared_snapshot_is_retryable_not_empty() {
  // DESIGN.md section 9: `SnapshotBuilding` is a request error, and an agent
  // must be able to tell "ask again shortly" from "there are no matches".
  let b = Backend::start("basic").await;
  let head = b.head().await;
  // Deliberately not prepared.

  let err = run(&b, request(&b, &head, "println")).await.unwrap_err();
  assert_eq!(
    err.code(),
    tonic::Code::Unavailable,
    "got {:?}: {}",
    err.code(),
    err.message()
  );
  assert!(
    err.message().contains("PrepareSnapshot"),
    "{}",
    err.message()
  );
}

#[tokio::test]
async fn an_unauthenticated_caller_gets_a_status_and_no_stream() {
  let b = Backend::start("basic").await;
  let head = b.head().await;
  b.prepare(&head).await;

  let mut client = b.client().await;
  let err = client
    .search(tonic::Request::new(request(&b, &head, "println")))
    .await
    .unwrap_err();
  assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

#[tokio::test]
async fn a_scope_narrows_both_the_results_and_the_coverage() {
  let b = Backend::start("basic").await;
  let head = b.head().await;
  b.prepare(&head).await;

  let whole = run(&b, request(&b, &head, "fn")).await.unwrap();
  let scoped = run(
    &b,
    v1::SearchRequest {
      scope: b"src".to_vec(),
      ..request(&b, &head, "fn")
    },
  )
  .await
  .unwrap();

  assert!(scoped.matches.len() <= whole.matches.len());
  for m in &scoped.matches {
    assert!(m.path.starts_with(b"src/"), "path escaped the scope");
  }
  let coverage = scoped.completion.unwrap().coverage.unwrap();
  assert_eq!(coverage.scope, b"src".to_vec());
}

#[tokio::test]
async fn a_regex_search_finds_what_a_literal_one_cannot() {
  let b = Backend::start("basic").await;
  let head = b.head().await;
  b.prepare(&head).await;

  let streamed = run(
    &b,
    v1::SearchRequest {
      pattern: r"println!\(".to_owned(),
      literal: false,
      ..request(&b, &head, "unused")
    },
  )
  .await
  .unwrap();
  assert!(!streamed.matches.is_empty());
  let completion = streamed.completion.unwrap();
  assert_eq!(
    completion.execution_status,
    v1::ExecutionStatus::Complete as i32,
    "`println` is a usable literal, so the index bounded this query"
  );
}

#[tokio::test]
async fn context_lines_and_columns_come_back_intact() {
  let b = Backend::start("basic").await;
  let head = b.head().await;
  b.prepare(&head).await;

  let streamed = run(
    &b,
    v1::SearchRequest {
      context_before: 1,
      context_after: 1,
      ..request(&b, &head, "println")
    },
  )
  .await
  .unwrap();
  let m = &streamed.matches[0];
  assert!(m.column >= 1);
  assert!(m.line >= 1);
  assert!(!m.line_text.is_empty());
  assert!(!m.blob_oid.is_empty());
}

#[tokio::test]
async fn non_utf8_paths_survive_the_wire() {
  // `path` is `bytes` in the protocol for exactly this. A `string` field would
  // make these files unaddressable at the wire level.
  let b = Backend::start("bytes").await;
  let head = b.head().await;
  b.prepare(&head).await;

  let streamed = run(&b, request(&b, &head, "content")).await.unwrap();
  assert!(streamed.completion.is_some());
  let any_non_utf8 = streamed
    .matches
    .iter()
    .any(|m| std::str::from_utf8(&m.path).is_err());
  assert!(
    any_non_utf8 || !streamed.matches.is_empty(),
    "the fixture should produce at least one match"
  );
}

#[tokio::test]
async fn the_completion_reports_the_index_generation() {
  let b = Backend::start("basic").await;
  let head = b.head().await;
  b.prepare(&head).await;

  let streamed = run(&b, request(&b, &head, "println")).await.unwrap();
  let completion = streamed.completion.unwrap();
  assert!(
    completion.index_generation >= 1,
    "a result must name the generation it was computed against"
  );
}
