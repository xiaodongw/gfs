//! The read-only WebDAV surface: `/dav/{repo}/{branch}/{path}`.
//!
//! Exists because FUSE is a poor fit on macOS; Finder speaks WebDAV natively, so
//! this surface lets a Mac browse a snapshot with no client software at all. Per
//! DESIGN.md section 6.3 it is a gateway onto the same internal services as every
//! other surface -- `AsyncRepository` and `Authorizer` -- never a second read path.
//!
//! Deliberate shape, recorded in ADR 0010:
//!
//! * **Read-only, DAV class 1.** OPTIONS advertises no LOCK support, which is what
//!   makes Finder mount the volume read-only. Every write method answers 405.
//! * **Branches are a hierarchy.** `refs/heads/topic/deep` browses as folder
//!   `topic/` containing `deep/`. Git forbids one ref being a prefix of another,
//!   so matching URL segments against branch names longest-first is unambiguous.
//! * **Byte paths stay bytes.** Segments percent-decode to `Vec<u8>` and never
//!   round-trip through `String` (ADR 0006); axum's `Path` extractor would, which
//!   is why every route lands in one raw-path dispatcher.
//! * **Branch-tip URLs are mutable**, so everything here is `no-store`; the ETags
//!   (object IDs) still give a caller exact revalidation.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::{
  limits, BytePath, EntryKind, RepositoryId, ResolvedRevision, RevisionSelector, SubjectId,
  Timestamp, TreeEntryInfo,
};

use super::http::{parse_range, request_id, set, HttpState};
use crate::audit::{self, Action, AuditRecord};
use crate::observability::{self, RequestId};
use gfs_git::AsyncRepository;

const METRIC_OPTIONS: &str = "webdav_options";
const METRIC_PROPFIND: &str = "webdav_propfind";
const METRIC_GET: &str = "webdav_get";
const METRIC_REFUSED: &str = "webdav_refused";

/// Every method this surface answers, for `Allow` headers and OPTIONS.
const ALLOWED_METHODS: &str = "OPTIONS, GET, HEAD, PROPFIND";

pub fn router(state: HttpState) -> Router {
  Router::new()
    // matchit's `{*rest}` does not match an empty remainder, so the two root
    // forms are spelled out.
    .route("/dav", any(dispatch))
    .route("/dav/", any(dispatch))
    .route("/dav/{*rest}", any(dispatch))
    // PROPFIND bodies are a few hundred bytes of XML; anything larger is not a
    // WebDAV client this surface serves.
    .layer(tower_http::limit::RequestBodyLimitLayer::new(64 * 1024))
    .layer(tower_http::timeout::TimeoutLayer::with_status_code(
      StatusCode::GATEWAY_TIMEOUT,
      limits::DEFAULT_REQUEST_TIMEOUT,
    ))
    .with_state(state)
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// One dispatcher for every method, because axum's `MethodFilter` cannot express
/// PROPFIND and its `Path` extractor percent-decodes into UTF-8 `String`, which
/// ADR 0006 forbids for tree paths. The raw request URI is the source of truth.
async fn dispatch(State(state): State<HttpState>, request: axum::extract::Request) -> Response {
  let started = Instant::now();
  let (parts, _body) = request.into_parts();
  let rid = request_id(&parts.headers);

  if parts.method == Method::OPTIONS {
    return options_response(&rid, started);
  }

  let is_propfind = parts.method.as_str() == "PROPFIND";
  let is_read = parts.method == Method::GET || parts.method == Method::HEAD;
  if !is_propfind && !is_read {
    observability::record_request(
      METRIC_REFUSED,
      Some(ErrorCode::InvalidArgument),
      started.elapsed(),
    );
    let mut response = StatusCode::METHOD_NOT_ALLOWED.into_response();
    set(
      response.headers_mut(),
      header::ALLOW.as_str(),
      ALLOWED_METHODS,
    );
    finish_headers(&mut response, &rid);
    return response;
  }

  let metric = if is_propfind {
    METRIC_PROPFIND
  } else {
    METRIC_GET
  };
  let action = if is_propfind {
    Action::ListDirectory
  } else {
    Action::ReadBlob
  };

  let result = async {
    // The same posture as the Git gateway: Basic (token as password) or Bearer,
    // and an absent header is tried as the empty token so the dev server's
    // no-configuration mapping to `dev` keeps working.
    let token = crate::gateway::credential(&parts.headers).unwrap_or_default();
    let identity = state.authz.authenticate(&token).map_err(|error| {
      if token.is_empty() {
        GfsError::new(ErrorCode::Unauthenticated, "no credential presented")
      } else {
        error
      }
    })?;
    let segments = parse_segments(parts.uri.path())?;
    Ok((identity, segments))
  }
  .await;

  let (identity, segments) = match result {
    Ok(v) => v,
    Err(e) => return fail(metric, action, &e, &rid, started),
  };

  let outcome = if is_propfind {
    propfind(&state, &identity.subject, &segments, &parts.headers).await
  } else {
    read(
      &state,
      &identity.subject,
      &segments,
      &parts.headers,
      parts.method == Method::HEAD,
    )
    .await
  };

  match outcome {
    Ok(Served {
      mut response,
      repo,
      commit,
      path,
    }) => {
      audit::success(
        action,
        &AuditRecord {
          subject: Some(&identity.subject),
          repository_id: repo.as_ref(),
          commit: commit.as_ref(),
          path: path.as_ref(),
          request_id: Some(rid.as_str()),
          ..Default::default()
        },
      );
      observability::record_request(metric, None, started.elapsed());
      finish_headers(&mut response, &rid);
      response
    }
    Err(e) => fail(metric, action, &e, &rid, started),
  }
}

/// A successful response plus what the audit record should say about it.
struct Served {
  response: Response,
  repo: Option<RepositoryId>,
  commit: Option<gfs_types::ObjectId>,
  path: Option<BytePath>,
}

fn options_response(rid: &RequestId, started: Instant) -> Response {
  // Unauthenticated by design: it discloses nothing repository-specific, and
  // Finder probes OPTIONS before it is willing to ask the user for a credential.
  // Class 1 only -- advertising LOCK (class 2) is what would make Finder try to
  // mount read-write.
  let mut response = StatusCode::OK.into_response();
  let h = response.headers_mut();
  set(h, "dav", "1");
  set(h, header::ALLOW.as_str(), ALLOWED_METHODS);
  finish_headers(&mut response, rid);
  observability::record_request(METRIC_OPTIONS, None, started.elapsed());
  response
}

fn fail(
  metric: &'static str,
  action: Action,
  error: &GfsError,
  rid: &RequestId,
  started: Instant,
) -> Response {
  audit::failure(
    action,
    &AuditRecord {
      request_id: Some(rid.as_str()),
      ..Default::default()
    },
    error.code,
  );
  observability::record_request(metric, Some(error.code), started.elapsed());
  let status =
    StatusCode::from_u16(error.code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
  let mut response = (status, format!("{}\n", error.message)).into_response();
  set(
    response.headers_mut(),
    header::CONTENT_TYPE.as_str(),
    "text/plain; charset=utf-8",
  );
  // Without the challenge, Finder (like Git) reports a bare failure and never
  // prompts for a credential.
  if error.code == ErrorCode::Unauthenticated {
    set(
      response.headers_mut(),
      header::WWW_AUTHENTICATE.as_str(),
      "Basic realm=\"GFS\"",
    );
  }
  finish_headers(&mut response, rid);
  response
}

/// Headers every `/dav` response carries: the whole namespace is branch-tip
/// addressed, so nothing here may be cached -- the same URL names different
/// bytes once the branch moves. The object-ID ETags still allow exact 304
/// revalidation, which is all a correct cache needs.
fn finish_headers(response: &mut Response, rid: &RequestId) {
  let h = response.headers_mut();
  h.insert(
    header::CACHE_CONTROL,
    header::HeaderValue::from_static("no-store"),
  );
  set(h, observability::REQUEST_ID_KEY, rid.as_str());
}

// ---------------------------------------------------------------------------
// The URL namespace
// ---------------------------------------------------------------------------

/// Split the raw (still percent-encoded) request path into decoded byte
/// segments. Trailing slashes are tolerated everywhere -- Finder sends both
/// forms for collections -- and hrefs in responses are always canonical.
fn parse_segments(path: &str) -> Result<Vec<Vec<u8>>, GfsError> {
  let bad = |m: &str| GfsError::new(ErrorCode::InvalidArgument, m);
  let rest = path
    .strip_prefix("/dav")
    .ok_or_else(|| bad("not a /dav path"))?;
  let mut segments = Vec::new();
  for raw in rest.split('/') {
    if raw.is_empty() {
      continue;
    }
    let decoded = percent_decode(raw).ok_or_else(|| bad("malformed percent-escape"))?;
    // An encoded slash would make the branch/path split ambiguous, and NUL is
    // never a valid byte in a Git path.
    if decoded.contains(&b'/') || decoded.contains(&0) {
      return Err(bad("a path segment may not encode '/' or NUL"));
    }
    segments.push(decoded);
  }
  Ok(segments)
}

/// What a `/dav` URL names.
enum DavNode {
  /// `/dav/` -- the repositories this subject may see.
  Root,
  /// `/dav/{repo}/` or a synthetic branch-namespace folder such as
  /// `/dav/{repo}/topic/` when only `topic/deep` exists. `prefix` is empty for
  /// the repository root, otherwise the namespace path without a trailing slash.
  Namespace {
    repo: RepositoryId,
    branches: Vec<String>,
    prefix: Vec<u8>,
  },
  /// A tree: the branch root (empty `path`, tree = `resolved.tree`) or a
  /// directory inside it (`oid` = the tree entry's OID).
  Tree {
    repo: RepositoryId,
    resolved: ResolvedRevision,
    path: BytePath,
    oid: gfs_types::ObjectId,
  },
  /// A file, executable, or symlink. A symlink is served as a small file whose
  /// content is the target -- that *is* the blob, and WebDAV has no symlinks.
  Blob {
    repo: RepositoryId,
    resolved: ResolvedRevision,
    path: BytePath,
    entry: TreeEntryInfo,
  },
}

async fn branch_names(repo: &AsyncRepository) -> Result<Vec<String>, GfsError> {
  Ok(
    repo
      .visible_refs()
      .await?
      .into_iter()
      .filter_map(|(name, _)| name.strip_prefix("refs/heads/").map(str::to_owned))
      .collect(),
  )
}

async fn resolve_node(
  state: &HttpState,
  subject: &SubjectId,
  segments: &[Vec<u8>],
) -> Result<DavNode, GfsError> {
  let Some(first) = segments.first() else {
    return Ok(DavNode::Root);
  };
  // A repository id is ASCII by grammar; anything else is masked exactly like an
  // id the subject may not see.
  let repo_id = std::str::from_utf8(first)
    .map_err(|_| GfsError::not_found("no such repository"))
    .and_then(RepositoryId::parse)?;
  state.authz.authorize_repository(subject, &repo_id)?;
  let algorithm = state.registry.require_servable(&repo_id)?.algorithm;
  let repo = state.registry.repository(&repo_id)?;
  let branches = branch_names(&repo).await?;
  let rest = &segments[1..];

  if rest.is_empty() {
    return Ok(DavNode::Namespace {
      repo: repo_id,
      branches,
      prefix: Vec::new(),
    });
  }

  // Longest prefix of the remaining segments that names a branch wins; Git's
  // invariant that no ref is a prefix of another ref makes the match unique,
  // so longest-first is merely defensive.
  for k in (1..=rest.len()).rev() {
    let candidate = join_segments(&rest[..k]);
    let Some(branch) = branches.iter().find(|b| b.as_bytes() == candidate) else {
      continue;
    };
    let selector = RevisionSelector::parse(&format!("refs/heads/{branch}"), algorithm)?;
    let resolved = repo.resolve(selector).await?;
    let path = BytePath::new(join_segments(&rest[k..]));
    if path.is_empty() {
      let oid = resolved.tree.clone();
      return Ok(DavNode::Tree {
        repo: repo_id,
        resolved,
        path,
        oid,
      });
    }
    path.validate()?;
    let entry = repo
      .entry(resolved.commit.clone(), path.clone())
      .await?
      .ok_or_else(|| GfsError::not_found("no such path in this branch"))?;
    return match entry.kind {
      EntryKind::Directory => Ok(DavNode::Tree {
        repo: repo_id,
        resolved,
        path,
        oid: entry.oid.clone(),
      }),
      kind if kind.has_blob_content() => Ok(DavNode::Blob {
        repo: repo_id,
        resolved,
        path,
        entry,
      }),
      // A gitlink names a repository this server may not even hold, and an
      // unsupported mode has nothing servable behind it. Both are also absent
      // from listings, so this is unreachable through browsing.
      _ => Err(GfsError::not_found("no such path in this branch")),
    };
  }

  // Not a branch: a synthetic namespace folder if any branch lives under it.
  let joined = join_segments(rest);
  let mut with_slash = joined.clone();
  with_slash.push(b'/');
  if branches
    .iter()
    .any(|b| b.as_bytes().starts_with(&with_slash))
  {
    return Ok(DavNode::Namespace {
      repo: repo_id,
      branches,
      prefix: joined,
    });
  }
  Err(GfsError::not_found("no such branch or path"))
}

fn join_segments(segments: &[Vec<u8>]) -> Vec<u8> {
  let mut out = Vec::new();
  for (i, segment) in segments.iter().enumerate() {
    if i > 0 {
      out.push(b'/');
    }
    out.extend_from_slice(segment);
  }
  out
}

// ---------------------------------------------------------------------------
// PROPFIND
// ---------------------------------------------------------------------------

/// One `<D:response>` worth of properties.
struct PropNode {
  href: String,
  /// Raw name bytes; emitted as `displayname` only when XML-safe UTF-8. The
  /// href always carries the exact bytes percent-encoded.
  name: Vec<u8>,
  collection: bool,
  size: Option<u64>,
  etag: Option<String>,
  modified: Option<Timestamp>,
}

async fn propfind(
  state: &HttpState,
  subject: &SubjectId,
  segments: &[Vec<u8>],
  headers: &HeaderMap,
) -> Result<Served, GfsError> {
  let depth = match depth(headers) {
    Some(d @ (0 | 1)) => d,
    // RFC 4918 defines a missing Depth as infinity, and infinity over a
    // monorepo is a tree walk nobody meant to request. Refused with the
    // precondition body the RFC names for exactly this.
    _ => {
      return Ok(Served {
        response: propfind_finite_depth_response(),
        repo: None,
        commit: None,
        path: None,
      })
    }
  };

  let node = resolve_node(state, subject, segments).await?;
  let self_href = href_for(segments, !matches!(node, DavNode::Blob { .. }));
  let self_name = segments.last().cloned().unwrap_or_else(|| b"dav".to_vec());

  let mut nodes = Vec::new();
  let (repo, commit, path) = match &node {
    DavNode::Root => {
      nodes.push(PropNode {
        href: self_href.clone(),
        name: self_name,
        collection: true,
        size: None,
        etag: None,
        modified: None,
      });
      if depth == 1 {
        for id in authorized_repositories(state, subject).await? {
          nodes.push(PropNode {
            href: format!("{}{}/", self_href, percent_encode(id.as_str().as_bytes())),
            name: id.as_str().as_bytes().to_vec(),
            collection: true,
            size: None,
            etag: None,
            modified: None,
          });
        }
      }
      (None, None, None)
    }
    DavNode::Namespace {
      repo,
      branches,
      prefix,
    } => {
      nodes.push(PropNode {
        href: self_href.clone(),
        name: self_name,
        collection: true,
        size: None,
        etag: None,
        modified: None,
      });
      if depth == 1 {
        // Branch and namespace children render identically, and both omit
        // etag/date deliberately: emitting them would cost one resolve() per
        // branch on a thousand-branch repository, for a value Finder shows
        // as a folder date nobody reads.
        for child in namespace_children(branches, prefix) {
          nodes.push(PropNode {
            href: format!("{}{}/", self_href, percent_encode(&child)),
            name: child,
            collection: true,
            size: None,
            etag: None,
            modified: None,
          });
        }
      }
      (Some(repo.clone()), None, None)
    }
    DavNode::Tree {
      repo,
      resolved,
      path,
      oid,
    } => {
      nodes.push(PropNode {
        href: self_href.clone(),
        name: self_name,
        collection: true,
        size: None,
        etag: Some(oid.to_qualified()),
        modified: Some(resolved.snapshot_time),
      });
      if depth == 1 {
        let handle = state.registry.repository(repo)?;
        list_tree_children(&handle, resolved, path, &self_href, &mut nodes).await?;
      }
      (
        Some(repo.clone()),
        Some(resolved.commit.clone()),
        Some(path.clone()),
      )
    }
    DavNode::Blob {
      repo,
      resolved,
      path,
      entry,
    } => {
      // Depth 1 on a non-collection degrades to Depth 0, per the RFC.
      nodes.push(PropNode {
        href: self_href.clone(),
        name: self_name,
        collection: false,
        size: Some(entry.size),
        etag: Some(entry.oid.to_qualified()),
        modified: Some(resolved.snapshot_time),
      });
      (
        Some(repo.clone()),
        Some(resolved.commit.clone()),
        Some(path.clone()),
      )
    }
  };

  let body = multistatus(&nodes);
  let mut response = (StatusCode::MULTI_STATUS, body).into_response();
  set(
    response.headers_mut(),
    header::CONTENT_TYPE.as_str(),
    "application/xml; charset=\"utf-8\"",
  );
  Ok(Served {
    response,
    repo,
    commit,
    path,
  })
}

fn depth(headers: &HeaderMap) -> Option<u8> {
  let value = headers.get("depth")?.to_str().ok()?;
  match value.trim() {
    "0" => Some(0),
    "1" => Some(1),
    _ => None,
  }
}

fn propfind_finite_depth_response() -> Response {
  let body = concat!(
    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n",
    "<D:error xmlns:D=\"DAV:\"><D:propfind-finite-depth/></D:error>\n"
  );
  let mut response = (StatusCode::FORBIDDEN, body).into_response();
  set(
    response.headers_mut(),
    header::CONTENT_TYPE.as_str(),
    "application/xml; charset=\"utf-8\"",
  );
  response
}

/// The repositories this subject may see, for the root listing. Unauthorized
/// repositories are simply absent, matching the 404 masking everywhere else.
async fn authorized_repositories(
  state: &HttpState,
  subject: &SubjectId,
) -> Result<Vec<RepositoryId>, GfsError> {
  let catalog = Arc::clone(&state.catalog);
  let records = tokio::task::spawn_blocking(move || catalog.list_repositories())
    .await
    .map_err(crate::util::join_error)??;
  Ok(
    records
      .into_iter()
      .filter(|r| r.state.is_servable())
      .filter(|r| {
        state
          .authz
          .authorize_repository(subject, &r.repository_id)
          .is_ok()
      })
      .map(|r| r.repository_id)
      .collect(),
  )
}

/// The immediate children of a branch-namespace node: the deduplicated next
/// segment of every branch under `prefix`.
fn namespace_children(branches: &[String], prefix: &[u8]) -> BTreeSet<Vec<u8>> {
  let mut out = BTreeSet::new();
  for branch in branches {
    let bytes = branch.as_bytes();
    let below = if prefix.is_empty() {
      Some(bytes)
    } else {
      bytes
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix(b"/"))
    };
    if let Some(below) = below {
      if let Some(first) = below.split(|&c| c == b'/').next() {
        if !first.is_empty() {
          out.insert(first.to_vec());
        }
      }
    }
  }
  out
}

async fn list_tree_children(
  repo: &AsyncRepository,
  resolved: &ResolvedRevision,
  path: &BytePath,
  parent_href: &str,
  nodes: &mut Vec<PropNode>,
) -> Result<(), GfsError> {
  let mut after: Option<Vec<u8>> = None;
  loop {
    let page = repo
      .list_directory(
        resolved.commit.clone(),
        path.clone(),
        after.take(),
        limits::MAX_DIRECTORY_PAGE_SIZE,
      )
      .await?;
    for entry in page.entries {
      let Some(name) = entry.path.file_name().map(|n| n.to_vec()) else {
        continue;
      };
      match entry.kind {
        EntryKind::Directory => nodes.push(PropNode {
          href: format!("{parent_href}{}/", percent_encode(&name)),
          name,
          collection: true,
          size: None,
          etag: Some(entry.oid.to_qualified()),
          modified: Some(resolved.snapshot_time),
        }),
        kind if kind.has_blob_content() => nodes.push(PropNode {
          href: format!("{parent_href}{}", percent_encode(&name)),
          name,
          collection: false,
          size: Some(entry.size),
          etag: Some(entry.oid.to_qualified()),
          modified: Some(resolved.snapshot_time),
        }),
        // Gitlinks and unsupported modes are omitted: a node that can never be
        // opened breaks Finder's copy of the enclosing folder, which is worse
        // than absence.
        _ => {}
      }
    }
    match page.next_page_token {
      Some(token) => after = Some(token),
      None => return Ok(()),
    }
  }
}

fn multistatus(nodes: &[PropNode]) -> String {
  let mut body = String::with_capacity(nodes.len() * 256 + 128);
  body.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
  body.push_str("<D:multistatus xmlns:D=\"DAV:\">\n");
  for node in nodes {
    body.push_str("<D:response>\n<D:href>");
    // Hrefs are percent-encoded to the unreserved set, which is XML-safe by
    // construction.
    body.push_str(&node.href);
    body.push_str("</D:href>\n<D:propstat>\n<D:prop>\n");
    if node.collection {
      body.push_str("<D:resourcetype><D:collection/></D:resourcetype>\n");
    } else {
      body.push_str("<D:resourcetype/>\n");
      body.push_str("<D:getcontenttype>application/octet-stream</D:getcontenttype>\n");
    }
    if let Some(name) = displayable(&node.name) {
      body.push_str("<D:displayname>");
      body.push_str(&xml_escape(name));
      body.push_str("</D:displayname>\n");
    }
    if let Some(size) = node.size {
      body.push_str("<D:getcontentlength>");
      body.push_str(&size.to_string());
      body.push_str("</D:getcontentlength>\n");
    }
    if let Some(etag) = &node.etag {
      body.push_str("<D:getetag>&quot;");
      body.push_str(&xml_escape(etag));
      body.push_str("&quot;</D:getetag>\n");
    }
    if let Some(modified) = node.modified {
      body.push_str("<D:getlastmodified>");
      body.push_str(&http_date(modified));
      body.push_str("</D:getlastmodified>\n");
    }
    body.push_str("</D:prop>\n<D:status>HTTP/1.1 200 OK</D:status>\n");
    body.push_str("</D:propstat>\n</D:response>\n");
  }
  body.push_str("</D:multistatus>\n");
  body
}

/// A name is worth a `displayname` only when it is UTF-8 with no control
/// characters; otherwise the property is omitted and the percent-encoded href
/// still carries the exact bytes.
fn displayable(name: &[u8]) -> Option<&str> {
  let text = std::str::from_utf8(name).ok()?;
  if text.chars().any(|c| c < '\u{20}' || c == '\u{7f}') {
    return None;
  }
  Some(text)
}

// ---------------------------------------------------------------------------
// GET / HEAD
// ---------------------------------------------------------------------------

async fn read(
  state: &HttpState,
  subject: &SubjectId,
  segments: &[Vec<u8>],
  headers: &HeaderMap,
  head: bool,
) -> Result<Served, GfsError> {
  let node = resolve_node(state, subject, segments).await?;
  let (repo, resolved, path, entry) = match node {
    DavNode::Blob {
      repo,
      resolved,
      path,
      entry,
    } => (repo, resolved, path, entry),
    // Collections have no GET body here; PROPFIND is the listing. A directory
    // index page is a feature no WebDAV client uses.
    _ => {
      let mut response = StatusCode::METHOD_NOT_ALLOWED.into_response();
      set(
        response.headers_mut(),
        header::ALLOW.as_str(),
        "OPTIONS, PROPFIND",
      );
      return Ok(Served {
        response,
        repo: None,
        commit: None,
        path: None,
      });
    }
  };

  let etag = format!("\"{}\"", entry.oid.to_qualified());
  let audit_ids = (
    Some(repo.clone()),
    Some(resolved.commit.clone()),
    Some(path.clone()),
  );

  if let Some(inm) = headers
    .get(header::IF_NONE_MATCH)
    .and_then(|v| v.to_str().ok())
  {
    if inm.split(',').any(|candidate| candidate.trim() == etag) {
      return Ok(Served {
        response: StatusCode::NOT_MODIFIED.into_response(),
        repo: audit_ids.0,
        commit: audit_ids.1,
        path: audit_ids.2,
      });
    }
  }

  if head {
    // A HEAD needs no blob read at all: the tree entry already carries the
    // size, and Finder stats far more often than it opens.
    let mut response = StatusCode::OK.into_response();
    let h = response.headers_mut();
    set(h, header::CONTENT_LENGTH.as_str(), &entry.size.to_string());
    file_headers(h, &etag);
    return Ok(Served {
      response,
      repo: audit_ids.0,
      commit: audit_ids.1,
      path: audit_ids.2,
    });
  }

  let handle = state.registry.repository(&repo)?;
  let bytes = handle.read_blob(entry.oid.clone()).await?;
  let total = bytes.len() as u64;
  let range = headers
    .get(header::RANGE)
    .and_then(|v| v.to_str().ok())
    .map(|v| parse_range(v, total));

  let response = match range {
    Some(Err(e)) => {
      let mut response = (
        StatusCode::RANGE_NOT_SATISFIABLE,
        format!("{}\n", e.message),
      )
        .into_response();
      set(
        response.headers_mut(),
        header::CONTENT_RANGE.as_str(),
        &format!("bytes */{total}"),
      );
      response
    }
    Some(Ok((start, end))) => {
      let slice = bytes[start as usize..=end as usize].to_vec();
      metrics::counter!(observability::metric::BLOB_BYTES_SERVED).increment(slice.len() as u64);
      let mut response = (StatusCode::PARTIAL_CONTENT, Body::from(slice)).into_response();
      let h = response.headers_mut();
      set(
        h,
        header::CONTENT_RANGE.as_str(),
        &format!("bytes {start}-{end}/{total}"),
      );
      file_headers(h, &etag);
      response
    }
    None => {
      metrics::counter!(observability::metric::BLOB_BYTES_SERVED).increment(total);
      let mut response = (StatusCode::OK, bytes).into_response();
      file_headers(response.headers_mut(), &etag);
      response
    }
  };
  Ok(Served {
    response,
    repo: audit_ids.0,
    commit: audit_ids.1,
    path: audit_ids.2,
  })
}

fn file_headers(h: &mut HeaderMap, etag: &str) {
  h.insert(
    header::CONTENT_TYPE,
    header::HeaderValue::from_static("application/octet-stream"),
  );
  h.insert(
    header::ACCEPT_RANGES,
    header::HeaderValue::from_static("bytes"),
  );
  set(h, header::ETAG.as_str(), etag);
}

// ---------------------------------------------------------------------------
// Encoding helpers
// ---------------------------------------------------------------------------

/// The canonical href for a node: absolute path, each component
/// percent-encoded, collections with a trailing slash.
fn href_for(segments: &[Vec<u8>], collection: bool) -> String {
  let mut href = String::from("/dav/");
  for (i, segment) in segments.iter().enumerate() {
    if i > 0 {
      href.push('/');
    }
    href.push_str(&percent_encode(segment));
  }
  if collection && !segments.is_empty() {
    href.push('/');
  }
  href
}

/// Percent-encode arbitrary bytes, keeping only RFC 3986's unreserved set.
/// Deliberately aggressive: everything else (including `&`, `<`, `%`) becomes
/// `%XX`, which also makes hrefs XML-safe with no second escaping pass.
fn percent_encode(bytes: &[u8]) -> String {
  let mut out = String::with_capacity(bytes.len());
  for &b in bytes {
    match b {
      b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
      _ => {
        out.push('%');
        out.push(
          char::from_digit(u32::from(b >> 4), 16)
            .unwrap()
            .to_ascii_uppercase(),
        );
        out.push(
          char::from_digit(u32::from(b & 0xf), 16)
            .unwrap()
            .to_ascii_uppercase(),
        );
      }
    }
  }
  out
}

/// Percent-decode one URL segment to raw bytes. `+` is *not* a space in a path
/// segment -- that rule is for query strings only.
fn percent_decode(segment: &str) -> Option<Vec<u8>> {
  let bytes = segment.as_bytes();
  let mut out = Vec::with_capacity(bytes.len());
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] == b'%' {
      let hi = hex_value(*bytes.get(i + 1)?)?;
      let lo = hex_value(*bytes.get(i + 2)?)?;
      out.push((hi << 4) | lo);
      i += 3;
    } else {
      out.push(bytes[i]);
      i += 1;
    }
  }
  Some(out)
}

fn hex_value(byte: u8) -> Option<u8> {
  match byte {
    b'0'..=b'9' => Some(byte - b'0'),
    b'a'..=b'f' => Some(byte - b'a' + 10),
    b'A'..=b'F' => Some(byte - b'A' + 10),
    _ => None,
  }
}

fn xml_escape(text: &str) -> String {
  let mut out = String::with_capacity(text.len());
  for c in text.chars() {
    match c {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      '"' => out.push_str("&quot;"),
      '\'' => out.push_str("&apos;"),
      _ => out.push(c),
    }
  }
  out
}

/// Format a timestamp as an RFC 1123 HTTP date (`getlastmodified`'s format).
///
/// Hand-rolled because the workspace has no time crate and this is the only
/// consumer. The civil-date math is Howard Hinnant's days-from-epoch algorithm.
fn http_date(ts: Timestamp) -> String {
  const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
  const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
  ];
  let days = ts.secs.div_euclid(86_400);
  let second_of_day = ts.secs.rem_euclid(86_400);
  let (year, month, day) = civil_from_days(days);
  // 1970-01-01 was a Thursday; index 0 is Sunday.
  let weekday = (days + 4).rem_euclid(7) as usize;
  format!(
    "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
    WEEKDAYS[weekday],
    day,
    MONTHS[(month - 1) as usize],
    year,
    second_of_day / 3600,
    (second_of_day % 3600) / 60,
    second_of_day % 60,
  )
}

/// Proleptic-Gregorian civil date from days since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
  let z = z + 719_468;
  let era = z.div_euclid(146_097);
  let day_of_era = z.rem_euclid(146_097);
  let year_of_era =
    (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
  let year = year_of_era + era * 400;
  let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
  let mp = (5 * day_of_year + 2) / 153;
  let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
  let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
  (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
  use super::*;

  // Only the two pure hand-rolled pieces with real correctness risk get unit
  // tests, matching the `parse_range` precedent; everything else is covered by
  // the smoke tests in tests/webdav.rs.

  #[test]
  fn http_dates_match_known_values() {
    // Expected strings produced by `date -u -d @<secs> '+%a, %d %b %Y %H:%M:%S GMT'`.
    assert_eq!(
      http_date(Timestamp::new(0, 0)),
      "Thu, 01 Jan 1970 00:00:00 GMT"
    );
    assert_eq!(
      http_date(Timestamp::new(1_069_904_561, 0)),
      "Thu, 27 Nov 2003 03:42:41 GMT"
    );
    assert_eq!(
      http_date(Timestamp::new(1_784_156_115, 0)),
      "Wed, 15 Jul 2026 22:55:15 GMT"
    );
    // Leap day.
    assert_eq!(
      http_date(Timestamp::new(1_709_164_800, 0)),
      "Thu, 29 Feb 2024 00:00:00 GMT"
    );
  }

  #[test]
  fn percent_codec_round_trips_arbitrary_bytes() {
    let raw: Vec<u8> = (0u8..=255).collect();
    let encoded = percent_encode(&raw);
    assert!(encoded.is_ascii());
    assert_eq!(percent_decode(&encoded).unwrap(), raw);
    // Unreserved bytes pass through untouched; everything else is escaped.
    assert_eq!(percent_encode(b"a-b_c.d~1"), "a-b_c.d~1");
    assert_eq!(percent_encode(b"a b&<c>"), "a%20b%26%3Cc%3E");
    // Malformed escapes are refused, not guessed at.
    assert!(percent_decode("%zz").is_none());
    assert!(percent_decode("%2").is_none());
  }
}
