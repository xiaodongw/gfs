//! M4.6: what a broken search must never look like.
//!
//! ADR 0004's contract exists for one failure: an agent receives few or no
//! results, concludes the symbol does not exist, and acts on it. Every test here
//! injects a fault and asserts the answer is still distinguishable from an
//! honest empty result — which is exit code 1, and which nothing in this file is
//! allowed to produce.
//!
//! | Injected fault | Outcome | Exit |
//! | --- | --- | ---: |
//! | Stream ends with no completion | `FailedBeforeCompletion` | 2 |
//! | Matches, then no completion | `FailedBeforeCompletion` | 2 |
//! | Mid-stream `Status` error | `FailedBeforeCompletion` | 2 |
//! | The connection is cut | never `Completed` | 2 |
//! | Coverage gap, `--require-exhaustive` | `Completed` | 4 |
//! | Honest empty result | `Completed` | 1 |
//!
//! # Why the transport faults use a stub server
//!
//! The property under test belongs to the *client*: `SnapshotClient::search`
//! collects a stream, and a stream that ended without a terminal message has to
//! become `FailedBeforeCompletion` rather than an empty `SearchResult`. Driving
//! that from the real server would mean arranging for the real server to be
//! wrong, which it is not. A stub that ends the stream early injects exactly the
//! one fault, at exactly the point the contract names, on every run.
//!
//! [`a_severed_connection_is_never_an_answer`] is the counterpart, and does cut
//! a real TCP connection: it proves the stub is describing something the
//! transport can actually do.

use std::sync::Arc;

use gfs_mount::client::{MountBinding, SnapshotClient};
use gfs_proto::v1;
use gfs_proto::{SearchService, SearchServiceServer};
use gfs_search::{exit_code, ExecutionStatus, SearchOutcome};
use gfs_test::mount::{Backend, TOKEN};
use gfs_types::{HashAlgorithm, ObjectId, RepositoryId, Timestamp};
use tonic::{Request, Response, Status};

// ---------------------------------------------------------------------------
// A server that can be told to be wrong
// ---------------------------------------------------------------------------

/// How the stub ends its stream.
#[derive(Clone, Copy)]
enum Ending {
  /// Clean EOF with no completion. The dangerous one: nothing about it looks
  /// like an error, so a client that trusted the stream would report the matches
  /// it happened to receive as the answer.
  NoCompletion,
  /// A `Status` mid-stream, the way a reset connection surfaces in tonic.
  Error,
  /// The contract satisfied, so the fixtures below are known to be capable of
  /// producing a usable answer.
  Completion,
}

struct StubSearch {
  matches: usize,
  ending: Ending,
}

#[tonic::async_trait]
impl SearchService for StubSearch {
  type SearchStream =
    std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<v1::SearchResponse, Status>> + Send>>;

  async fn search(
    &self,
    _request: Request<v1::SearchRequest>,
  ) -> Result<Response<Self::SearchStream>, Status> {
    let mut messages: Vec<Result<v1::SearchResponse, Status>> = (0..self.matches)
      .map(|i| {
        Ok(v1::SearchResponse {
          message: Some(v1::search_response::Message::Match(v1::SearchMatch {
            path: format!("src/file{i}.rs").into_bytes(),
            line: 1,
            column: 1,
            matched: b"needle".to_vec(),
            line_text: b"let needle = 1;".to_vec(),
            before: Vec::new(),
            after: Vec::new(),
            blob_oid: String::new(),
            line_truncated: false,
          })),
        })
      })
      .collect();

    match self.ending {
      Ending::NoCompletion => {}
      Ending::Error => messages.push(Err(Status::unavailable("the connection was reset"))),
      Ending::Completion => messages.push(Ok(v1::SearchResponse {
        message: Some(v1::search_response::Message::Completion(
          v1::SearchCompletion {
            execution_status: v1::ExecutionStatus::Complete as i32,
            commit_oid: fake_commit().to_qualified(),
            ..Default::default()
          },
        )),
      })),
    }

    Ok(Response::new(Box::pin(tokio_stream::iter(messages))))
  }
}

fn fake_commit() -> ObjectId {
  ObjectId::from_raw(HashAlgorithm::Sha1, &[7u8; 20]).unwrap()
}

/// A client pointed at a stub, over a real gRPC connection.
///
/// The binding is synthetic because the stub ignores the request entirely; what
/// is real is the transport and the client's stream collection, which is what
/// the contract lives in.
async fn client_for(stub: StubSearch) -> Arc<SnapshotClient> {
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let addr = listener.local_addr().unwrap();
  tokio::spawn(async move {
    tonic::transport::Server::builder()
      .add_service(SearchServiceServer::new(stub))
      .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
      .await
  });

  connect(&format!("http://{addr}")).await
}

async fn connect(endpoint: &str) -> Arc<SnapshotClient> {
  SnapshotClient::connect(
    endpoint,
    // Never used: nothing here fetches a blob.
    "http://127.0.0.1:1",
    TOKEN,
    MountBinding {
      repository_id: RepositoryId::parse("r-fault").unwrap(),
      commit: fake_commit(),
      algorithm: HashAlgorithm::Sha1,
      snapshot_time: Timestamp::new(0, 0),
    },
    "capability".to_owned(),
  )
  .await
  .unwrap()
}

fn query() -> gfs_search::Query {
  gfs_search::Query {
    pattern: "needle".to_owned(),
    literal: true,
    ..Default::default()
  }
}

/// The exit code an agent would act on.
fn code(outcome: &SearchOutcome) -> i32 {
  exit_code(outcome, false)
}

// ---------------------------------------------------------------------------
// Transport loss before the terminal message
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_stream_that_ends_without_a_completion_is_a_failed_search_not_an_empty_one() {
  // The pair the whole contract exists to separate. Both deliver zero results;
  // one means the symbol is absent and the other means nobody ever said.
  let client = client_for(StubSearch {
    matches: 0,
    ending: Ending::NoCompletion,
  })
  .await;

  let outcome = client.search(&query(), 0).await.unwrap();
  assert!(
    matches!(outcome, SearchOutcome::FailedBeforeCompletion(_)),
    "a stream with no terminal message must not decode as an empty result: {outcome:?}"
  );
  assert_eq!(code(&outcome), 2);
  assert_ne!(
    code(&outcome),
    1,
    "exit 1 would mean 'the symbol is absent'"
  );
}

#[tokio::test]
async fn matches_without_a_completion_are_discarded_rather_than_reported() {
  // The subtler half. Three matches arrived, so there *is* something to show,
  // and showing it is exactly wrong: without the terminal message nobody knows
  // whether three is the answer or the first three of three hundred.
  let client = client_for(StubSearch {
    matches: 3,
    ending: Ending::NoCompletion,
  })
  .await;

  let outcome = client.search(&query(), 0).await.unwrap();
  match &outcome {
    SearchOutcome::FailedBeforeCompletion(reason) => {
      assert!(
        reason.contains("without a completion"),
        "the reason must name the missing terminal message: {reason}"
      );
    }
    other => panic!("partial results were presented as an answer: {other:?}"),
  }
  assert_eq!(code(&outcome), 2);
}

#[tokio::test]
async fn a_mid_stream_transport_error_is_a_failed_search() {
  let client = client_for(StubSearch {
    matches: 2,
    ending: Ending::Error,
  })
  .await;

  let outcome = client.search(&query(), 0).await.unwrap();
  match &outcome {
    SearchOutcome::FailedBeforeCompletion(reason) => {
      assert!(reason.contains("reset"), "{reason}");
    }
    other => panic!("a reset stream produced an answer: {other:?}"),
  }
  assert_eq!(code(&outcome), 2);
}

#[tokio::test]
async fn a_stream_that_keeps_its_contract_is_usable() {
  // The control. Without it every assertion above would still pass against a
  // client that failed unconditionally.
  let client = client_for(StubSearch {
    matches: 2,
    ending: Ending::Completion,
  })
  .await;

  let outcome = client.search(&query(), 0).await.unwrap();
  let SearchOutcome::Completed(result) = &outcome else {
    panic!("a well-formed stream must produce an answer: {outcome:?}");
  };
  assert_eq!(result.matches.len(), 2);
  assert_eq!(
    result.completion.execution_status,
    ExecutionStatus::Complete
  );
  assert_eq!(code(&outcome), 0);
}

#[tokio::test]
async fn a_severed_connection_is_never_an_answer() {
  // A real TCP cut rather than a stub's early return, so the stubs above are
  // known to be describing something the transport does. Where the cut lands in
  // the HTTP/2 framing is not controlled -- it may break the request before the
  // response headers or after some matches -- so the assertion is the one that
  // holds either way, and it is the one that matters: nothing here is an answer.
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let addr = listener.local_addr().unwrap();
  tokio::spawn(async move {
    // Accept, read a little, and drop. Every connection dies mid-conversation.
    while let Ok((mut socket, _)) = listener.accept().await {
      tokio::spawn(async move {
        let mut buffer = [0u8; 256];
        let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buffer).await;
        drop(socket);
      });
    }
  });

  let client = connect(&format!("http://{addr}")).await;
  match client.search(&query(), 0).await {
    // The stream opened and then died.
    Ok(outcome) => {
      assert!(
        matches!(outcome, SearchOutcome::FailedBeforeCompletion(_)),
        "a severed connection produced an answer: {outcome:?}"
      );
      assert_eq!(code(&outcome), 2);
    }
    // The connection died before the response headers, so there was never a
    // stream to collect. Also not an answer, and the caller cannot mistake an
    // `Err` for one.
    Err(e) => {
      assert!(
        !e.message.is_empty(),
        "a failure must say something a caller can report"
      );
    }
  }
}

// ---------------------------------------------------------------------------
// The same question against the real server
// ---------------------------------------------------------------------------

/// A real search, through the real server, reduced to its outcome.
async fn real_search(
  backend: &Backend,
  commit: &ObjectId,
  query: gfs_search::Query,
) -> SearchOutcome {
  let result = backend
    .server
    .search
    .search(&backend.repo_id, commit, query)
    .await
    .unwrap();
  SearchOutcome::Completed(result)
}

async fn prepared_head(backend: &Backend) -> ObjectId {
  let commit = backend
    .server
    .registry
    .repository(&backend.repo_id)
    .unwrap()
    .resolve(gfs_types::RevisionSelector::parse("main", HashAlgorithm::Sha1).unwrap())
    .await
    .unwrap()
    .commit;
  let outcome = backend
    .server
    .search
    .prepare(&backend.repo_id, &commit, true)
    .await
    .unwrap();
  assert!(matches!(outcome, gfs_service::PrepareOutcome::Ready(_)));
  commit
}

#[tokio::test]
async fn every_way_a_search_can_come_back_short_has_its_own_exit_code() {
  // The matrix, in one test, because the contract is about the *differences*
  // between these codes and asserting them in separate tests would let two of
  // them collapse into each other without any test failing.
  let backend = Backend::start("content").await;
  let commit = prepared_head(&backend).await;

  // 1. An honest empty result. The baseline everything else must differ from.
  let empty = real_search(
    &backend,
    &commit,
    gfs_search::Query {
      pattern: "zzz-not-in-this-repository-zzz".to_owned(),
      literal: true,
      ..Default::default()
    },
  )
  .await;
  assert_eq!(exit_code(&empty, false), 1);

  // 2. A budget stopped it. Same absence of results, different meaning.
  let truncated = real_search(
    &backend,
    &commit,
    gfs_search::Query {
      pattern: "e".to_owned(),
      literal: true,
      budget: gfs_search::Budget {
        max_candidates: 1,
        ..Default::default()
      },
      ..Default::default()
    },
  )
  .await;
  assert_eq!(
    exit_code(&truncated, false),
    3,
    "a candidate budget must not be reported as an honest answer"
  );

  // 3. A pattern the index cannot bound. Complete-looking, and not complete.
  let unbounded = real_search(
    &backend,
    &commit,
    gfs_search::Query {
      pattern: "z.z".to_owned(),
      ..Default::default()
    },
  )
  .await;
  assert_eq!(exit_code(&unbounded, false), 3);

  // 4. A coverage gap. `content` holds a binary file and a 12 MiB blob, so this
  //    search is complete over a corpus that is smaller than the request.
  let gapped = real_search(
    &backend,
    &commit,
    gfs_search::Query {
      pattern: "line one".to_owned(),
      literal: true,
      ..Default::default()
    },
  )
  .await;
  let SearchOutcome::Completed(result) = &gapped else {
    unreachable!()
  };
  assert!(
    result.completion.coverage.has_gaps(),
    "the `content` fixture exists to produce exclusions: {:?}",
    result.completion.coverage.excluded
  );
  assert_eq!(exit_code(&gapped, false), 0, "gaps warn by default");
  assert_eq!(
    exit_code(&gapped, true),
    4,
    "--require-exhaustive turns a gap into a failure"
  );

  // 5. The stream never finished. Not producible from the real server, which is
  //    the point: it is a transport property, and it is covered above.
  let lost = SearchOutcome::FailedBeforeCompletion("connection reset".to_owned());
  assert_eq!(exit_code(&lost, false), 2);

  // And the whole point: no two of these are the same code.
  let codes = [
    exit_code(&empty, false),
    exit_code(&truncated, false),
    exit_code(&lost, false),
    exit_code(&gapped, true),
  ];
  let mut distinct = codes.to_vec();
  distinct.sort_unstable();
  distinct.dedup();
  assert_eq!(
    distinct.len(),
    codes.len(),
    "two different failures share an exit code: {codes:?}"
  );
}

#[tokio::test]
async fn each_declared_exclusion_reason_is_counted_separately() {
  // Grouped by reason, not totalled. A caller deciding whether to trust an empty
  // answer needs to know *what* was skipped: a binary file is expected, an index
  // gap is not.
  let backend = Backend::start("content").await;
  let commit = prepared_head(&backend).await;

  let outcome = real_search(
    &backend,
    &commit,
    gfs_search::Query {
      pattern: "line".to_owned(),
      literal: true,
      ..Default::default()
    },
  )
  .await;
  let SearchOutcome::Completed(result) = &outcome else {
    unreachable!()
  };
  let excluded = &result.completion.coverage.excluded;

  assert_eq!(
    excluded.get("oversized"),
    Some(&1),
    "the 12 MiB blob: {excluded:?}"
  );
  assert!(
    excluded.get("binary").copied().unwrap_or(0) >= 2,
    "binary.bin and utf16.txt: {excluded:?}"
  );
  assert!(
    result
      .completion
      .coverage
      .declared_exclusions
      .contains(&"oversized".to_owned()),
    "the policy must declare what it excludes even where a scope has none"
  );
}
