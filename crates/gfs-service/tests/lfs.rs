//! End-to-end tests for server-side LFS expansion (ADR 0012), against a real
//! server bound to real ports.
//!
//! The repository is built with stock Git and a spec v1 pointer whose object
//! genuinely hashes to the pointer's oid, so the store's verify-on-put and the
//! blob endpoint's ETag both operate on honest data. What is covered: entry
//! metadata substitution and degradation, the `lfs-sha256:` key through the
//! ticket and the blob endpoint, and the commit path's re-clean.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use gfs_git::GitRepository as _;
use gfs_proto::v1;
use gfs_service::auth::{AllowList, CapabilityKey, StaticTokens};
use gfs_service::catalog::repositories::NewRepository;
use gfs_service::{Catalog, Server};
use gfs_types::{DisplayName, HashAlgorithm, LeasePolicy, ObjectId, RepositoryId, SubjectId};
use sha2::Digest as _;

const TOKEN: &str = "token-owner";
const EXPANDED: &[u8] = b"pretend these bytes are a 64 MiB model weight file\n";

fn git(args: &[&str], dir: &Path) {
  let out = Command::new("git")
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env("GIT_AUTHOR_NAME", "T")
    .env("GIT_AUTHOR_EMAIL", "t@e")
    .env("GIT_COMMITTER_NAME", "T")
    .env("GIT_COMMITTER_EMAIL", "t@e")
    .current_dir(dir)
    .args(args)
    .output()
    .expect("git runs");
  assert!(
    out.status.success(),
    "git {args:?}: {}",
    String::from_utf8_lossy(&out.stderr)
  );
}

fn content_key(content: &[u8]) -> ObjectId {
  ObjectId::from_raw(HashAlgorithm::LfsSha256, &sha2::Sha256::digest(content)).unwrap()
}

fn pointer_text(content: &[u8]) -> String {
  format!(
    "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {}\n",
    content_key(content).to_hex(),
    content.len()
  )
}

struct Fixture {
  grpc: String,
  http: String,
  repo_id: RepositoryId,
  server: Arc<Server>,
  _shutdown: tokio::sync::watch::Sender<bool>,
  _tmp: tempfile::TempDir,
}

/// A bare repository with two LFS pointers — one whose object the store will
/// hold, one left degraded — plus an ordinary file, served by a full server.
async fn start() -> Fixture {
  let tmp = tempfile::tempdir().unwrap();
  let work = tmp.path().join("work");
  std::fs::create_dir_all(&work).unwrap();
  git(&["init", "-q", "--initial-branch=main", "."], &work);
  std::fs::write(work.join(".gitattributes"), "*.bin filter=lfs -text\n").unwrap();
  std::fs::write(work.join("model.bin"), pointer_text(EXPANDED)).unwrap();
  std::fs::write(work.join("degraded.bin"), pointer_text(b"never fetched")).unwrap();
  std::fs::write(work.join("README.md"), "# lfs\n").unwrap();
  git(&["add", "-A"], &work);
  git(&["commit", "-qm", "first"], &work);
  let bare = tmp.path().join("repo.git");
  git(
    &[
      "clone",
      "-q",
      "--bare",
      work.to_str().unwrap(),
      bare.to_str().unwrap(),
    ],
    tmp.path(),
  );

  let catalog = Arc::new(Catalog::open_in_memory().unwrap());
  let repo_id = RepositoryId::parse("r-lfs").unwrap();
  catalog
    .create_repository(&NewRepository {
      repository_id: repo_id.clone(),
      display_name: DisplayName::parse("acme/lfs").unwrap(),
      repo_path: bare,
      algorithm: HashAlgorithm::Sha1,
      upstream_url: None,
      credential_ref: None,
    })
    .unwrap();

  let owner = SubjectId::parse("job-owner").unwrap();
  let authenticator = Arc::new(StaticTokens::new().with_token(TOKEN, owner.clone()));
  let policy = Arc::new(AllowList::new().allow(&owner, &repo_id));

  let server = Server::new(
    Arc::clone(&catalog),
    authenticator,
    policy,
    CapabilityKey::generate().unwrap(),
    LeasePolicy::adr_0006(),
  )
  .with_lfs_store(&tmp.path().join("lfs"))
  .unwrap();
  let server = Arc::new(server);
  server
    .registry
    .lfs_store()
    .unwrap()
    .put(&repo_id, &content_key(EXPANDED), EXPANDED)
    .unwrap();
  server.registry.activate(&repo_id).unwrap();
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
  let snapshot = server.snapshot_api();
  let repository = server.repository_api();
  let mut grpc_shutdown = shutdown_rx.clone();
  tokio::spawn(async move {
    tonic::transport::Server::builder()
      .add_service(gfs_proto::SnapshotServiceServer::new(snapshot))
      .add_service(gfs_proto::RepositoryServiceServer::new(repository))
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
    server,
    _shutdown: shutdown_tx,
    _tmp: tmp,
  }
}

fn authed<T>(request: T) -> tonic::Request<T> {
  let mut request = tonic::Request::new(request);
  request.metadata_mut().insert(
    "authorization",
    format!("Bearer {TOKEN}").parse().unwrap(),
  );
  request
}

async fn get_entry(
  f: &Fixture,
  commit: &str,
  path: &[u8],
) -> v1::TreeEntry {
  let channel = tonic::transport::Endpoint::from_shared(f.grpc.clone())
    .unwrap()
    .connect()
    .await
    .unwrap();
  let mut client = gfs_proto::SnapshotServiceClient::new(channel);
  client
    .get_entry(authed(v1::GetEntryRequest {
      repository_id: f.repo_id.to_string(),
      commit_oid: commit.to_owned(),
      path: path.to_vec(),
      authorization: None,
      want_blob_ticket: true,
    }))
    .await
    .unwrap()
    .into_inner()
    .entry
    .unwrap()
}

async fn resolve_main(f: &Fixture) -> String {
  let repo = f.server.registry.repository(&f.repo_id).unwrap();
  let selector = gfs_types::RevisionSelector::parse("main", HashAlgorithm::Sha1).unwrap();
  repo.resolve(selector).await.unwrap().commit.to_qualified()
}

async fn http_get(url: &str) -> (http::StatusCode, Vec<u8>, http::HeaderMap) {
  use http_body_util::BodyExt;
  use hyper_util::rt::TokioIo;
  let uri: http::Uri = url.parse().unwrap();
  let authority = uri.authority().unwrap().to_string();
  let stream = tokio::net::TcpStream::connect(authority.clone()).await.unwrap();
  let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
    .await
    .unwrap();
  tokio::spawn(conn);
  let request = http::Request::builder()
    .uri(uri.path_and_query().unwrap().as_str())
    .header("host", authority)
    .header("authorization", format!("Bearer {TOKEN}"))
    .body(String::new())
    .unwrap();
  let response = sender.send_request(request).await.unwrap();
  let status = response.status();
  let headers = response.headers().clone();
  let body = response.into_body().collect().await.unwrap().to_bytes().to_vec();
  (status, body, headers)
}

#[tokio::test]
async fn an_lfs_entry_serves_expanded_metadata_and_content_end_to_end() {
  let f = start().await;
  let commit = resolve_main(&f).await;

  // Entry metadata reports the expanded identity: the lfs-sha256 content key
  // and the expanded size, with a ticket bound to that key.
  let entry = get_entry(&f, &commit, b"model.bin").await;
  assert_eq!(entry.oid, content_key(EXPANDED).to_qualified());
  assert_eq!(entry.size, EXPANDED.len() as u64);
  let ticket = entry.blob_ticket.expect("a blob ticket");

  // The blob endpoint serves the expanded bytes for that key, ETagged by it.
  let url = format!(
    "{}/v1/repos/{}/blobs/{}?ticket={ticket}",
    f.http, f.repo_id, entry.oid
  );
  let (status, body, headers) = http_get(&url).await;
  assert_eq!(status, http::StatusCode::OK);
  assert_eq!(body, EXPANDED);
  assert_eq!(headers.get("etag").unwrap(), &format!("\"{}\"", entry.oid));

  // An entry whose object the store lacks degrades to its pointer: git blob
  // identity, pointer-sized, pointer bytes behind the ticket.
  let degraded = get_entry(&f, &commit, b"degraded.bin").await;
  assert!(degraded.oid.starts_with("sha1:"), "oid was {}", degraded.oid);
  assert_eq!(degraded.size, pointer_text(b"never fetched").len() as u64);
  let ticket = degraded.blob_ticket.expect("a blob ticket");
  let url = format!(
    "{}/v1/repos/{}/blobs/{}?ticket={ticket}",
    f.http, f.repo_id, degraded.oid
  );
  let (status, body, _) = http_get(&url).await;
  assert_eq!(status, http::StatusCode::OK);
  assert_eq!(body, pointer_text(b"never fetched").as_bytes());

  // Ordinary files are untouched.
  let readme = get_entry(&f, &commit, b"README.md").await;
  assert!(readme.oid.starts_with("sha1:"));
  assert_eq!(readme.size, b"# lfs\n".len() as u64);
}

#[tokio::test]
async fn committing_content_to_an_lfs_path_stores_the_object_and_writes_a_pointer() {
  let f = start().await;
  let base = resolve_main(&f).await;
  let edited = b"an edited model weight, straight from the overlay".to_vec();

  let channel = tonic::transport::Endpoint::from_shared(f.grpc.clone())
    .unwrap()
    .connect()
    .await
    .unwrap();
  let mut client = gfs_proto::RepositoryServiceClient::new(channel);
  let response = client
    .commit_changes(authed(v1::CommitChangesRequest {
      repository_id: f.repo_id.to_string(),
      base_commit_oid: base,
      branch: "main".to_owned(),
      message: "edit the model".to_owned(),
      author_name: "T".to_owned(),
      author_email: "t@e".to_owned(),
      changes: vec![v1::FileChange {
        path: b"model.bin".to_vec(),
        kind: v1::ChangeKind::Modified as i32,
        mode: 0o100644,
        content: edited.clone(),
      }],
      authorization: None,
      deleted_directories: vec![],
    }))
    .await
    .unwrap()
    .into_inner();

  // The committed tree holds a fresh, correct pointer — not the content.
  let commit = ObjectId::parse_qualified(&response.commit_oid).unwrap();
  let repo = f.server.registry.repository(&f.repo_id).unwrap();
  let pointers = repo.lfs_pointers(commit).await.unwrap();
  let entry = pointers
    .iter()
    .find(|e| e.path.as_bytes() == b"model.bin")
    .expect("model.bin is still a pointer in the tree");
  assert_eq!(entry.pointer.oid, content_key(&edited));
  assert_eq!(entry.pointer.size, edited.len() as u64);

  // And the store now holds the edited object, so the new revision's entry
  // expands. Read through the repository handle rather than GetEntry: the
  // commit lives only on the caller's work ref, which the snapshot API masks
  // without a mount capability (ADR 0002) — correct, and not what this test
  // is about.
  assert!(f
    .server
    .registry
    .lfs_store()
    .unwrap()
    .contains(&f.repo_id, &content_key(&edited)));
  let commit = ObjectId::parse_qualified(&response.commit_oid).unwrap();
  let expanded = f
    .server
    .registry
    .blocking_repository(&f.repo_id)
    .unwrap()
    .entry(&commit, &gfs_types::BytePath::new("model.bin"))
    .unwrap()
    .unwrap();
  assert_eq!(expanded.oid, content_key(&edited));
  assert_eq!(expanded.size, edited.len() as u64);
}
