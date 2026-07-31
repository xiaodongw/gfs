//! Smoke tests for the read-only WebDAV surface, against a real server on a
//! real port.
//!
//! Driven over the wire because everything WebDAV-specific only exists there:
//! the PROPFIND extension method, the Depth header, the multistatus body, the
//! Basic challenge, and href encoding. Assertions are Litmus-style protocol
//! basics; Finder itself is validated manually (docs/manual-test.md).

use std::sync::Arc;

use gfs_service::auth::{AllowList, CapabilityKey, StaticTokens};
use gfs_service::catalog::repositories::NewRepository;
use gfs_service::{Catalog, Server};
use gfs_types::{DisplayName, HashAlgorithm, LeasePolicy, RepositoryId, SubjectId};

const OWNER_TOKEN: &str = "token-owner";
const OUTSIDER_TOKEN: &str = "token-outsider";

struct Fixture {
  http: String,
  _shutdown: tokio::sync::watch::Sender<bool>,
  _tmp: tempfile::TempDir,
  _bytes_tmp: tempfile::TempDir,
}

/// Serve `basic` (as `r-api`, owner-only, with an added `topic/deep` branch for
/// the slash-branch hierarchy) and `bytes` (as `r-bytes`, for href encoding).
async fn start() -> Fixture {
  let (tmp, repo_path) = gfs_test::scratch_clone("basic").unwrap();
  // A branch whose name contains a slash, created here rather than in the shared
  // fixture: `basic` already has a `feature` branch, and this surface is exactly
  // the place where `topic/deep` must browse as nested folders.
  gfs_test::fixtures::git(&repo_path, &["branch", "topic/deep"]).unwrap();

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
  // The outsider holds a *valid* credential and no repository grant, exercising
  // the existence-masking path.
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
    http: format!("http://{addr}"),
    _shutdown: shutdown_tx,
    _tmp: tmp,
    _bytes_tmp: bytes_tmp,
  }
}

/// One request with an arbitrary method, which is the point: PROPFIND does not
/// exist in any client library's method enum.
async fn dav(
  method: &str,
  url: &str,
  authorization: Option<&str>,
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

  let mut builder = http::Request::builder()
    .method(http::Method::from_bytes(method.as_bytes()).unwrap())
    .uri(&uri)
    .header("host", authority);
  if let Some(value) = authorization {
    builder = builder.header("authorization", value);
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

fn bearer(token: &str) -> String {
  format!("Bearer {token}")
}

/// Standard padded base64, test-local: the crate under test must not provide
/// the encoder that checks its own decoder.
fn base64(input: &[u8]) -> String {
  const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  let mut out = String::new();
  for chunk in input.chunks(3) {
    let b = [
      chunk[0],
      chunk.get(1).copied().unwrap_or(0),
      chunk.get(2).copied().unwrap_or(0),
    ];
    let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
    out.push(ALPHABET[(n >> 18) as usize & 63] as char);
    out.push(ALPHABET[(n >> 12) as usize & 63] as char);
    out.push(if chunk.len() > 1 {
      ALPHABET[(n >> 6) as usize & 63] as char
    } else {
      '='
    });
    out.push(if chunk.len() > 2 {
      ALPHABET[n as usize & 63] as char
    } else {
      '='
    });
  }
  out
}

const DEPTH_1: &[(&str, &str)] = &[("depth", "1")];

#[tokio::test]
async fn options_advertises_dav_class_1_and_read_only_methods() {
  let f = start().await;
  // Unauthenticated by design: Finder probes OPTIONS before asking for a
  // credential.
  let (status, headers, _) = dav("OPTIONS", &format!("{}/dav/", f.http), None, &[]).await;
  assert_eq!(status, http::StatusCode::OK);
  assert_eq!(headers.get("dav").unwrap(), "1");
  let allow = headers.get("allow").unwrap().to_str().unwrap();
  assert!(allow.contains("PROPFIND"), "{allow}");
  assert!(allow.contains("GET"), "{allow}");
  assert!(!allow.contains("PUT"), "read-only surface: {allow}");
  assert!(!allow.contains("LOCK"), "class 1 only: {allow}");
}

#[tokio::test]
async fn missing_credentials_get_a_basic_challenge() {
  let f = start().await;
  let (status, headers, _) = dav("PROPFIND", &format!("{}/dav/", f.http), None, DEPTH_1).await;
  assert_eq!(status, http::StatusCode::UNAUTHORIZED);
  // Without the challenge Finder reports a bare error and never prompts.
  assert_eq!(
    headers.get("www-authenticate").unwrap(),
    "Basic realm=\"GFS\""
  );
}

#[tokio::test]
async fn basic_auth_with_the_token_as_password_is_accepted() {
  let f = start().await;
  let value = format!(
    "Basic {}",
    base64(format!("finder:{OWNER_TOKEN}").as_bytes())
  );
  let (status, _, body) = dav(
    "PROPFIND",
    &format!("{}/dav/", f.http),
    Some(&value),
    DEPTH_1,
  )
  .await;
  assert_eq!(status, http::StatusCode::MULTI_STATUS);
  let text = String::from_utf8(body).unwrap();
  assert!(text.contains("/dav/r-api/"), "{text}");
}

#[tokio::test]
async fn propfind_root_lists_only_authorized_repositories() {
  let f = start().await;
  let (status, headers, body) = dav(
    "PROPFIND",
    &format!("{}/dav/", f.http),
    Some(&bearer(OWNER_TOKEN)),
    DEPTH_1,
  )
  .await;
  assert_eq!(status, http::StatusCode::MULTI_STATUS);
  assert_eq!(headers.get("cache-control").unwrap(), "no-store");
  let text = String::from_utf8(body).unwrap();
  assert!(text.contains("<D:href>/dav/r-api/</D:href>"), "{text}");
  assert!(text.contains("<D:href>/dav/r-bytes/</D:href>"), "{text}");

  // The outsider's valid credential sees an empty root, not an error: the
  // repositories it may not read are simply absent.
  let (status, _, body) = dav(
    "PROPFIND",
    &format!("{}/dav/", f.http),
    Some(&bearer(OUTSIDER_TOKEN)),
    DEPTH_1,
  )
  .await;
  assert_eq!(status, http::StatusCode::MULTI_STATUS);
  let text = String::from_utf8(body).unwrap();
  assert!(!text.contains("r-api"), "{text}");
  assert_eq!(text.matches("<D:response>").count(), 1, "{text}");

  // Named directly, the ungranted repository masks as 404, exactly like the
  // other surfaces.
  let (status, _, _) = dav(
    "PROPFIND",
    &format!("{}/dav/r-api/", f.http),
    Some(&bearer(OUTSIDER_TOKEN)),
    DEPTH_1,
  )
  .await;
  assert_eq!(status, http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn propfind_depth_1_of_a_branch_root_matches_the_fixture() {
  let f = start().await;
  for url in [
    format!("{}/dav/r-api/main/", f.http),
    // The same collection without the trailing slash must answer identically.
    format!("{}/dav/r-api/main", f.http),
  ] {
    let (status, _, body) = dav("PROPFIND", &url, Some(&bearer(OWNER_TOKEN)), DEPTH_1).await;
    assert_eq!(status, http::StatusCode::MULTI_STATUS);
    let text = String::from_utf8(body).unwrap();
    // `basic` at main: README.md, src/ (docs/guide.md was deleted in the second
    // commit). Hrefs are canonical regardless of the request form.
    assert!(
      text.contains("<D:href>/dav/r-api/main/README.md</D:href>"),
      "{text}"
    );
    assert!(
      text.contains("<D:href>/dav/r-api/main/src/</D:href>"),
      "{text}"
    );
    assert!(!text.contains("docs"), "{text}");
    assert!(
      text.contains("<D:getcontentlength>8</D:getcontentlength>"),
      "README.md is 8 bytes: {text}"
    );
    assert!(text.contains("<D:getlastmodified>"), "{text}");
    assert!(text.contains("<D:getetag>&quot;sha1:"), "{text}");
  }
}

#[tokio::test]
async fn a_slash_branch_browses_as_nested_collections() {
  let f = start().await;
  let list = |url: String| {
    let auth = bearer(OWNER_TOKEN);
    async move {
      let (status, _, body) = dav("PROPFIND", &url, Some(&auth), DEPTH_1).await;
      assert_eq!(status, http::StatusCode::MULTI_STATUS, "{url}");
      String::from_utf8(body).unwrap()
    }
  };
  let repo = list(format!("{}/dav/r-api/", f.http)).await;
  assert!(repo.contains("<D:href>/dav/r-api/main/</D:href>"), "{repo}");
  assert!(
    repo.contains("<D:href>/dav/r-api/feature/</D:href>"),
    "{repo}"
  );
  // The slash branch shows as its namespace folder, not as `topic/deep`.
  assert!(
    repo.contains("<D:href>/dav/r-api/topic/</D:href>"),
    "{repo}"
  );
  assert!(!repo.contains("deep"), "{repo}");

  let namespace = list(format!("{}/dav/r-api/topic/", f.http)).await;
  assert!(
    namespace.contains("<D:href>/dav/r-api/topic/deep/</D:href>"),
    "{namespace}"
  );

  // The branch tip itself lists the tree.
  let branch = list(format!("{}/dav/r-api/topic/deep/", f.http)).await;
  assert!(
    branch.contains("<D:href>/dav/r-api/topic/deep/README.md</D:href>"),
    "{branch}"
  );
}

#[tokio::test]
async fn get_serves_file_bytes_with_etag_revalidation_and_ranges() {
  let f = start().await;
  let url = format!("{}/dav/r-api/main/README.md", f.http);
  let auth = bearer(OWNER_TOKEN);

  let (status, headers, body) = dav("GET", &url, Some(&auth), &[]).await;
  assert_eq!(status, http::StatusCode::OK);
  assert_eq!(body, b"# basic\n");
  assert_eq!(headers.get("cache-control").unwrap(), "no-store");
  assert_eq!(headers.get("accept-ranges").unwrap(), "bytes");
  let etag = headers.get("etag").unwrap().to_str().unwrap().to_owned();
  assert!(etag.starts_with("\"sha1:"), "{etag}");

  let (status, _, body) = dav("GET", &url, Some(&auth), &[("if-none-match", &etag)]).await;
  assert_eq!(status, http::StatusCode::NOT_MODIFIED);
  assert!(body.is_empty());

  let (status, headers, body) = dav("GET", &url, Some(&auth), &[("range", "bytes=0-1")]).await;
  assert_eq!(status, http::StatusCode::PARTIAL_CONTENT);
  assert_eq!(body, b"# ");
  assert_eq!(headers.get("content-range").unwrap(), "bytes 0-1/8");

  // HEAD answers from the tree entry alone; the size must still be right.
  let (status, headers, body) = dav("HEAD", &url, Some(&auth), &[]).await;
  assert_eq!(status, http::StatusCode::OK);
  assert!(body.is_empty());
  assert_eq!(headers.get("content-length").unwrap(), "8");

  // A collection has no GET body; PROPFIND is the listing.
  let (status, headers, _) = dav(
    "GET",
    &format!("{}/dav/r-api/main/src/", f.http),
    Some(&auth),
    &[],
  )
  .await;
  assert_eq!(status, http::StatusCode::METHOD_NOT_ALLOWED);
  assert!(headers
    .get("allow")
    .unwrap()
    .to_str()
    .unwrap()
    .contains("PROPFIND"));
}

#[tokio::test]
async fn unknown_branches_and_paths_are_not_found() {
  let f = start().await;
  let auth = bearer(OWNER_TOKEN);
  for url in [
    format!("{}/dav/r-nope/", f.http),
    format!("{}/dav/r-api/nope/", f.http),
    format!("{}/dav/r-api/main/nope.txt", f.http),
    // `topic` alone is a namespace, but a file under it that skips the branch
    // level names nothing.
    format!("{}/dav/r-api/topic/nope.txt", f.http),
  ] {
    let (status, _, _) = dav("PROPFIND", &url, Some(&auth), DEPTH_1).await;
    assert_eq!(status, http::StatusCode::NOT_FOUND, "{url}");
  }
}

#[tokio::test]
async fn infinite_and_missing_depth_are_refused() {
  let f = start().await;
  let auth = bearer(OWNER_TOKEN);
  let url = format!("{}/dav/r-api/main/", f.http);
  // RFC 4918: a missing Depth header *is* infinity, and infinity over a
  // monorepo is a tree walk nobody meant to request.
  for extra in [&[][..], &[("depth", "infinity")][..]] {
    let (status, _, body) = dav("PROPFIND", &url, Some(&auth), extra).await;
    assert_eq!(status, http::StatusCode::FORBIDDEN);
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("propfind-finite-depth"), "{text}");
  }
}

#[tokio::test]
async fn write_methods_are_refused_with_allow() {
  let f = start().await;
  let auth = bearer(OWNER_TOKEN);
  let url = format!("{}/dav/r-api/main/README.md", f.http);
  for method in [
    "PUT",
    "DELETE",
    "MKCOL",
    "MOVE",
    "COPY",
    "PROPPATCH",
    "LOCK",
    "POST",
  ] {
    let (status, headers, _) = dav(method, &url, Some(&auth), &[]).await;
    assert_eq!(status, http::StatusCode::METHOD_NOT_ALLOWED, "{method}");
    let allow = headers.get("allow").unwrap().to_str().unwrap();
    assert!(allow.contains("PROPFIND"), "{method}: {allow}");
  }
}

#[tokio::test]
async fn non_utf8_names_round_trip_through_percent_encoded_hrefs() {
  let f = start().await;
  let auth = bearer(OWNER_TOKEN);
  let (status, _, body) = dav(
    "PROPFIND",
    &format!("{}/dav/r-bytes/main/", f.http),
    Some(&auth),
    DEPTH_1,
  )
  .await;
  assert_eq!(status, http::StatusCode::MULTI_STATUS);
  let text = String::from_utf8(body).unwrap();
  // A space, a Latin-1 byte, and a newline: all bytes live only in the href.
  assert!(
    text.contains("/dav/r-bytes/main/with%20space.txt"),
    "{text}"
  );
  assert!(
    text.contains("/dav/r-bytes/main/latin1-caf%E9.txt"),
    "{text}"
  );
  assert!(
    text.contains("/dav/r-bytes/main/with%0Anewline.txt"),
    "{text}"
  );

  // The escaped href, requested back, serves the stored bytes.
  let (status, _, body) = dav(
    "GET",
    &format!("{}/dav/r-bytes/main/latin1-caf%E9.txt", f.http),
    Some(&auth),
    &[],
  )
  .await;
  assert_eq!(status, http::StatusCode::OK);
  assert_eq!(body, b"content\n");
}
