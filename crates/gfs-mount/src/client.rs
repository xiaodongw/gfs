//! The snapshot API client the filesystem reads through.
//!
//! Two transports, because DESIGN.md section 6.3 kept them separate on purpose:
//! gRPC for metadata, where a typed request/response and per-item error detail
//! matter, and HTTP for blob bytes, where ranges, `ETag` revalidation, and
//! cacheability matter and gRPC expresses none of them well.
//!
//! # Every call names the commit
//!
//! The client is constructed *around* one pinned commit and there is no method
//! that takes a revision selector. DESIGN.md section 6.2 makes a branch name only
//! a selector; resolving it belongs to `CreateMount`, once, before the mount
//! exists. A filesystem that could re-resolve would be able to serve two
//! generations of a tree through one mount, which is the failure the pinning rule
//! exists to prevent.
//!
//! # The capability is mutable, the commit is not
//!
//! The mount capability is replaced on every heartbeat renewal (`RenewMount`
//! returns a fresh one), so it lives behind a lock while the repository, commit,
//! and algorithm are fixed at construction.

use std::sync::{Arc, RwLock};

use bytes::Bytes;
use gfs_proto::convert;
use gfs_proto::v1;
use gfs_types::error::{ErrorCode, GfsError};
use gfs_types::{
  BytePath, CommitMeta, HashAlgorithm, MountId, ObjectId, RepositoryId, Timestamp, TreeEntryInfo,
};
use http_body_util::{BodyExt, Empty};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

type Grpc = v1::snapshot_service_client::SnapshotServiceClient<tonic::transport::Channel>;
type SearchGrpc = v1::search_service_client::SearchServiceClient<tonic::transport::Channel>;
type Http = hyper_util::client::legacy::Client<HttpConnector, Empty<Bytes>>;

/// Everything the client needs that does not change for the life of a mount.
#[derive(Clone, Debug)]
pub struct MountBinding {
  pub repository_id: RepositoryId,
  pub commit: ObjectId,
  pub algorithm: HashAlgorithm,
  pub snapshot_time: Timestamp,
}

/// What a revwalk should cover. The wire's `LogRequest` in domain terms.
#[derive(Clone, Debug, Default)]
pub struct LogQuery {
  pub skip: u32,
  pub limit: u32,
  pub first_parent: bool,
  pub paths: Vec<BytePath>,
}

/// What to diff and how to render it.
#[derive(Clone, Debug, Default)]
pub struct DiffQuery {
  pub paths: Vec<BytePath>,
  pub format: gfs_types::DiffFormat,
  /// `None` takes the server's default of 3. `Some(0)` genuinely means no
  /// context, which is why this is an `Option` rather than a bare count.
  pub context_lines: Option<u32>,
  /// Zero takes the server's default.
  pub max_bytes: u64,
}

/// A rendered commit-to-commit diff.
#[derive(Clone, Debug)]
pub struct RevDiff {
  pub rendered: Vec<u8>,
  pub files: Vec<gfs_types::DiffFileChange>,
  pub truncated: bool,
}

/// Blame hunks and the bytes they describe.
#[derive(Clone, Debug)]
pub struct Blame {
  pub hunks: Vec<gfs_types::BlameHunk>,
  pub content: Vec<u8>,
  pub truncated: bool,
}

/// One page of a directory listing.
#[derive(Clone, Debug)]
pub struct DirectoryPage {
  pub entries: Vec<TreeEntryInfo>,
  /// Empty when this was the last page.
  pub next_page_token: Vec<u8>,
}

/// One page of a recursive listing: whole directories, never a partial one.
#[derive(Debug, Default)]
pub struct TreePage {
  /// Every entry of every directory in `directories`, in walk order.
  pub entries: Vec<TreeEntryInfo>,
  /// The directories this page describes completely, the walk root included.
  /// An empty directory appears here with no entries, which is the only way to
  /// tell "listed, and empty" from "not listed".
  pub directories: Vec<BytePath>,
  /// Empty when the walk reached the end of the subtree.
  pub next_page_token: Vec<u8>,
}

pub struct SnapshotClient {
  grpc: Grpc,
  /// The same channel, a different service. Multiplexed over one connection,
  /// so a search costs no extra handshake.
  search_grpc: SearchGrpc,
  http: Http,
  http_endpoint: String,
  token: String,
  binding: MountBinding,
  /// Replaced by every successful heartbeat renewal.
  capability: RwLock<String>,
}

impl std::fmt::Debug for SnapshotClient {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    // Neither the bearer token nor the capability appears: both are credentials,
    // and a struct dump into a log is how credentials leak.
    f.debug_struct("SnapshotClient")
      .field("http_endpoint", &self.http_endpoint)
      .field("binding", &self.binding)
      .finish_non_exhaustive()
  }
}

impl SnapshotClient {
  /// Connect to the gRPC endpoint and build the blob HTTP client.
  pub async fn connect(
    grpc_endpoint: &str,
    http_endpoint: &str,
    token: &str,
    binding: MountBinding,
    capability: String,
  ) -> Result<Arc<Self>, GfsError> {
    let channel = tonic::transport::Endpoint::from_shared(grpc_endpoint.to_owned())
      .map_err(|e| GfsError::invalid(format!("invalid gRPC endpoint: {e}")))?
      .connect()
      .await
      .map_err(|e| {
        GfsError::new(
          ErrorCode::Unavailable,
          format!("connecting to the GFS server: {e}"),
        )
      })?;

    Ok(Arc::new(SnapshotClient {
      // The decode ceiling is raised from tonic's 4 MiB default to the
      // protocol's own `MAX_RESPONSE_BYTES`. `ListTree` is why: one page of a
      // recursive listing is thousands of entries, and a response the server is
      // allowed to build must be one the client is allowed to read.
      grpc: Grpc::new(channel.clone())
        .max_decoding_message_size(gfs_types::limits::MAX_RESPONSE_BYTES),
      search_grpc: SearchGrpc::new(channel),
      http: hyper_util::client::legacy::Client::builder(TokioExecutor::new()).build_http(),
      http_endpoint: http_endpoint.trim_end_matches('/').to_owned(),
      token: token.to_owned(),
      binding,
      capability: RwLock::new(capability),
    }))
  }

  pub fn binding(&self) -> &MountBinding {
    &self.binding
  }

  /// Install the capability a renewal returned.
  pub fn set_capability(&self, capability: String) {
    *self.capability.write().expect("capability lock") = capability;
  }

  fn capability(&self) -> String {
    self.capability.read().expect("capability lock").clone()
  }

  /// The current capability, for writing into `mount.json`.
  ///
  /// Deliberately verbose. This is the one call that takes a credential out of
  /// the client, and a reviewer reading `state.rs` should be able to see that it
  /// was an explicit decision rather than an accessor someone reached for.
  pub fn capability_for_persistence(&self) -> String {
    self.capability()
  }

  /// The authorization proof for a commit-scoped call.
  ///
  /// Always sent, even while the commit is still reachable from a visible ref.
  /// The alternative -- sending it only after a reachability check fails -- would
  /// make the *first* read after a force push fail before the client learned it
  /// needed the capability, which is exactly the moment a mount must not break.
  fn authorization(&self) -> Option<v1::SnapshotAuthorization> {
    let capability = self.capability();
    if capability.is_empty() {
      None
    } else {
      Some(v1::SnapshotAuthorization {
        mount_capability: capability,
      })
    }
  }

  fn authed<T>(&self, message: T) -> Result<tonic::Request<T>, GfsError> {
    let mut request = tonic::Request::new(message);
    if !self.token.is_empty() {
      let value = format!("Bearer {}", self.token)
        .parse()
        .map_err(|_| GfsError::invalid("token is not a valid header value"))?;
      request.metadata_mut().insert("authorization", value);
    }
    Ok(request)
  }

  /// One path's metadata, or `None` when the commit has no such path.
  ///
  /// The `None` is deliberate: a negative lookup is the most common FUSE result
  /// during a build, and the gRPC `NOT_FOUND` status lets the caller cache it as
  /// negative without inspecting a body.
  pub async fn get_entry(
    &self,
    path: &BytePath,
    want_blob_ticket: bool,
  ) -> Result<Option<TreeEntryInfo>, GfsError> {
    self
      .get_entry_at(&self.binding.commit, path, want_blob_ticket)
      .await
  }

  /// The same, in any commit this mount's capability reaches.
  ///
  /// The capability is why this exists rather than the caller talking to the
  /// server itself: after `gfs switch -c` the workspace is pinned to a commit on
  /// a branch in the reserved namespace, which is reachable from no visible ref.
  /// A direct request for it is refused — correctly — and only the mount's own
  /// capability opens it.
  pub async fn get_entry_at(
    &self,
    commit: &ObjectId,
    path: &BytePath,
    want_blob_ticket: bool,
  ) -> Result<Option<TreeEntryInfo>, GfsError> {
    let request = self.authed(v1::GetEntryRequest {
      repository_id: self.binding.repository_id.as_str().to_owned(),
      commit_oid: commit.to_qualified(),
      path: path.as_bytes().to_vec(),
      authorization: self.authorization(),
      want_blob_ticket,
    })?;
    match self.grpc.clone().get_entry(request).await {
      Ok(response) => {
        let entry = response
          .into_inner()
          .entry
          .ok_or_else(|| GfsError::internal("server returned no entry"))?;
        Ok(Some(entry.try_into_domain(self.binding.algorithm)?))
      }
      Err(status) => {
        let error = convert::from_status(&status);
        if error.code == ErrorCode::NotFound {
          Ok(None)
        } else {
          Err(error)
        }
      }
    }
  }

  pub async fn list_directory(
    &self,
    path: &BytePath,
    page_token: Vec<u8>,
    page_size: u32,
    want_blob_tickets: bool,
  ) -> Result<DirectoryPage, GfsError> {
    self
      .list_directory_at(
        &self.binding.commit,
        path,
        page_token,
        page_size,
        want_blob_tickets,
      )
      .await
  }

  /// The same, in any commit this mount's capability reaches. See
  /// [`SnapshotClient::get_entry_at`] for why the capability is the point.
  pub async fn list_directory_at(
    &self,
    commit: &ObjectId,
    path: &BytePath,
    page_token: Vec<u8>,
    page_size: u32,
    want_blob_tickets: bool,
  ) -> Result<DirectoryPage, GfsError> {
    let request = self.authed(v1::ListDirectoryRequest {
      repository_id: self.binding.repository_id.as_str().to_owned(),
      commit_oid: commit.to_qualified(),
      path: path.as_bytes().to_vec(),
      page_token,
      page_size,
      authorization: self.authorization(),
      want_blob_tickets,
    })?;
    let page = self
      .grpc
      .clone()
      .list_directory(request)
      .await
      .map_err(|s| convert::from_status(&s))?
      .into_inner();
    let mut entries = Vec::with_capacity(page.entries.len());
    for entry in page.entries {
      entries.push(entry.try_into_domain(self.binding.algorithm)?);
    }
    Ok(DirectoryPage {
      entries,
      next_page_token: page.next_page_token,
    })
  }

  /// A whole subtree's directories in one round trip.
  ///
  /// The call a recognized walk makes instead of one `ListDirectory` per
  /// directory. Every directory named in the response is complete, so each can
  /// be cached as an authoritative listing — including the ones with no entries,
  /// which is why `directories` is carried separately from `entries`.
  pub async fn list_tree(
    &self,
    root: &BytePath,
    page_token: Vec<u8>,
    max_entries: u32,
  ) -> Result<TreePage, GfsError> {
    let request = self.authed(v1::ListTreeRequest {
      repository_id: self.binding.repository_id.as_str().to_owned(),
      commit_oid: self.binding.commit.to_qualified(),
      path: root.as_bytes().to_vec(),
      authorization: self.authorization(),
      max_entries,
      page_token,
    })?;
    let page = self
      .grpc
      .clone()
      .list_tree(request)
      .await
      .map_err(|s| convert::from_status(&s))?
      .into_inner();
    let mut entries = Vec::with_capacity(page.entries.len());
    for entry in page.entries {
      entries.push(entry.try_into_domain(self.binding.algorithm)?);
    }
    Ok(TreePage {
      entries,
      directories: page.directories.into_iter().map(BytePath::new).collect(),
      next_page_token: page.next_page_token,
    })
  }

  /// Many paths in one round trip.
  ///
  /// Per-path failures are folded into `None` rather than surfaced: the caller is
  /// a prefetch path, and a batch is an optimisation whose partial failure must
  /// degrade to an individual `GetEntry` rather than to an error.
  pub async fn batch_get_entry(
    &self,
    paths: &[BytePath],
    want_blob_tickets: bool,
  ) -> Result<Vec<Option<TreeEntryInfo>>, GfsError> {
    let request = self.authed(v1::BatchGetEntryRequest {
      repository_id: self.binding.repository_id.as_str().to_owned(),
      commit_oid: self.binding.commit.to_qualified(),
      paths: paths.iter().map(|p| p.as_bytes().to_vec()).collect(),
      authorization: self.authorization(),
      want_blob_tickets,
    })?;
    let response = self
      .grpc
      .clone()
      .batch_get_entry(request)
      .await
      .map_err(|s| convert::from_status(&s))?
      .into_inner();
    Ok(
      response
        .results
        .into_iter()
        .map(|result| match result.result {
          Some(v1::entry_result::Result::Entry(e)) => {
            e.try_into_domain(self.binding.algorithm).ok()
          }
          _ => None,
        })
        .collect(),
    )
  }

  /// Extend the lease, installing the fresh capability the server returns.
  ///
  /// Idempotent by contract, which is what makes a heartbeat that cannot tell
  /// whether its last attempt landed recoverable: the previous capability stays
  /// valid until its own expiry, so a renewal whose *response* was lost does not
  /// strand the daemon holding a token the server has forgotten.
  pub async fn renew_mount(&self, mount_id: &MountId) -> Result<Timestamp, GfsError> {
    let request = self.authed(v1::RenewMountRequest {
      mount_id: mount_id.as_str().to_owned(),
      mount_capability: self.capability(),
    })?;
    let response = self
      .grpc
      .clone()
      .renew_mount(request)
      .await
      .map_err(|s| convert::from_status(&s))?
      .into_inner();
    self.set_capability(response.mount_capability);
    Ok(
      response
        .lease_expiry
        .map(|t| Timestamp::new(t.secs, t.nanos))
        .unwrap_or_else(Timestamp::now),
    )
  }

  /// Release the lease eagerly. Expiry is the crash fallback, not the normal path.
  pub async fn release_mount(&self, mount_id: &MountId) -> Result<(), GfsError> {
    let request = self.authed(v1::ReleaseMountRequest {
      mount_id: mount_id.as_str().to_owned(),
      mount_capability: self.capability(),
    })?;
    self
      .grpc
      .clone()
      .release_mount(request)
      .await
      .map_err(|s| convert::from_status(&s))?;
    Ok(())
  }

  pub async fn get_commit(&self) -> Result<CommitMeta, GfsError> {
    self.get_commit_at(&self.binding.commit).await
  }

  /// Metadata for any commit the caller may read, not only the pin.
  ///
  /// Needed because `gfs show <rev>` has to learn which parent to diff against
  /// before it can ask for the diff, and that commit is by definition not the
  /// one the mount is pinned to.
  pub async fn get_commit_at(&self, commit: &ObjectId) -> Result<CommitMeta, GfsError> {
    let request = self.authed(v1::GetCommitRequest {
      repository_id: self.binding.repository_id.as_str().to_owned(),
      commit_oid: commit.to_qualified(),
      authorization: self.authorization(),
    })?;
    self
      .grpc
      .clone()
      .get_commit(request)
      .await
      .map_err(|s| convert::from_status(&s))?
      .into_inner()
      .try_into_domain(self.binding.algorithm)
  }

  /// The ancestry of `from`, newest first. `None` walks the pinned commit.
  ///
  /// Returns the page and whether ancestry remains beyond it.
  pub async fn log(
    &self,
    from: Option<&ObjectId>,
    options: &LogQuery,
  ) -> Result<(Vec<CommitMeta>, bool), GfsError> {
    let request = self.authed(v1::LogRequest {
      repository_id: self.binding.repository_id.as_str().to_owned(),
      commit_oid: from.unwrap_or(&self.binding.commit).to_qualified(),
      authorization: self.authorization(),
      skip: options.skip,
      limit: options.limit,
      first_parent: options.first_parent,
      paths: options
        .paths
        .iter()
        .map(|p| p.as_bytes().to_vec())
        .collect(),
    })?;
    let response = self
      .grpc
      .clone()
      .log(request)
      .await
      .map_err(|s| convert::from_status(&s))?
      .into_inner();
    let mut commits = Vec::with_capacity(response.commits.len());
    for commit in response.commits {
      commits.push(commit.try_into_domain(self.binding.algorithm)?);
    }
    Ok((commits, response.has_more))
  }

  /// Resolve a revision expression against the gateway.
  ///
  /// The one method that takes a *selector*, and the module docstring's rule
  /// still holds: this does not re-pin anything. It answers "what commit does
  /// `HEAD~1` mean" for `gfs show` and `gfs diff`, which read history rather than
  /// the mounted tree, and the filesystem never calls it.
  pub async fn resolve(&self, selector: &str) -> Result<ObjectId, GfsError> {
    let request = self.authed(v1::ResolveRevisionRequest {
      repository_id: self.binding.repository_id.as_str().to_owned(),
      revision_selector: selector.to_owned(),
    })?;
    let response = self
      .grpc
      .clone()
      .resolve_revision(request)
      .await
      .map_err(|s| convert::from_status(&s))?
      .into_inner();
    convert::try_oid(&response.commit_oid, self.binding.algorithm, "commit_oid")
  }

  /// Every ref the repository shows this credential, with tags peeled.
  ///
  /// One call at pin time; the seed turns the answer into `packed-refs`. Not a
  /// filesystem path — a workspace's ref view is fixed at the pin, exactly like
  /// its branch ref and its index, so re-asking mid-generation would only be a
  /// way to disagree with them.
  pub async fn list_refs(&self) -> Result<Vec<gfs_types::RefTarget>, GfsError> {
    let request = self.authed(v1::ListRefsRequest {
      repository_id: self.binding.repository_id.as_str().to_owned(),
    })?;
    let response = self
      .grpc
      .clone()
      .list_refs(request)
      .await
      .map_err(|s| convert::from_status(&s))?
      .into_inner();
    response
      .refs
      .into_iter()
      .map(|r| r.try_into_domain(self.binding.algorithm))
      .collect()
  }

  /// What changed between two commits, rendered by the server.
  ///
  /// `from` is `None` for a root commit, which is diffed against the empty tree.
  /// Nothing is hydrated: the patch is built where the object database is.
  pub async fn diff_commits(
    &self,
    from: Option<&ObjectId>,
    to: &ObjectId,
    query: &DiffQuery,
  ) -> Result<RevDiff, GfsError> {
    let request = self.authed(v1::DiffCommitsRequest {
      repository_id: self.binding.repository_id.as_str().to_owned(),
      base_commit_oid: from.map(ObjectId::to_qualified).unwrap_or_default(),
      commit_oid: to.to_qualified(),
      authorization: self.authorization(),
      paths: query.paths.iter().map(|p| p.as_bytes().to_vec()).collect(),
      format: v1::DiffFormat::from(query.format) as i32,
      context_lines: query.context_lines.unwrap_or(0),
      max_bytes: query.max_bytes,
      zero_context: query.context_lines == Some(0),
    })?;
    let response = self
      .grpc
      .clone()
      .diff_commits(request)
      .await
      .map_err(|s| convert::from_status(&s))?
      .into_inner();
    let mut files = Vec::with_capacity(response.files.len());
    for file in response.files {
      files.push(file.try_into_domain()?);
    }
    Ok(RevDiff {
      rendered: response.rendered,
      files,
      truncated: response.truncated,
    })
  }

  /// Line attribution for one path at one commit, with the file's bytes.
  pub async fn blame(&self, commit: &ObjectId, path: &BytePath) -> Result<Blame, GfsError> {
    let request = self.authed(v1::BlameRequest {
      repository_id: self.binding.repository_id.as_str().to_owned(),
      commit_oid: commit.to_qualified(),
      path: path.as_bytes().to_vec(),
      authorization: self.authorization(),
    })?;
    let response = self
      .grpc
      .clone()
      .blame(request)
      .await
      .map_err(|s| convert::from_status(&s))?
      .into_inner();
    let mut hunks = Vec::with_capacity(response.hunks.len());
    for hunk in response.hunks {
      hunks.push(hunk.try_into_domain(self.binding.algorithm)?);
    }
    Ok(Blame {
      hunks,
      content: response.content,
      truncated: response.truncated,
    })
  }

  /// Ask the server to build the pinned commit's search index.
  ///
  /// Nothing else in the client path calls this, which is why it exists: the
  /// server's `Search` never triggers a build, it only reports `SnapshotBuilding`
  /// when the manifest is missing. Without a caller, that condition was
  /// permanent and every search in a freshly mounted workspace failed.
  ///
  /// Returns whether the snapshot is ready *now*. `false` means the build is
  /// still running server-side and a later search may succeed; the distinction
  /// between "building" and "failed" is preserved in the error rather than
  /// folded into the boolean, because one is worth retrying and the other is not.
  pub async fn prepare_snapshot(&self) -> Result<bool, GfsError> {
    let request = self.authed(v1::PrepareSnapshotRequest {
      repository_id: self.binding.repository_id.as_str().to_owned(),
      commit_oid: self.binding.commit.to_qualified(),
      authorization: self.authorization(),
    })?;
    let response = self
      .grpc
      .clone()
      .prepare_snapshot(request)
      .await
      .map_err(|s| convert::from_status(&s))?
      .into_inner();

    match v1::SnapshotState::try_from(response.state) {
      Ok(v1::SnapshotState::Ready) => Ok(true),
      Ok(v1::SnapshotState::Building) => Ok(false),
      Ok(v1::SnapshotState::Failed) => Err(GfsError::new(
        ErrorCode::Internal,
        format!(
          "the server could not build the search index for {}: {}",
          self.binding.commit.to_qualified(),
          response
            .failure_reason
            .unwrap_or_else(|| "no reason given".to_owned())
        ),
      )),
      _ => Err(GfsError::internal(
        "the server reported an unrecognized snapshot state",
      )),
    }
  }

  /// Search the pinned commit.
  ///
  /// Collects the stream into an outcome rather than surfacing it, and the
  /// collection *is* the contract enforcement: a stream that ends without a
  /// terminal completion becomes [`gfs_search::SearchOutcome::FailedBeforeCompletion`],
  /// never an empty result. A caller therefore cannot accidentally treat a
  /// dropped connection as "no matches" -- there is no code path that produces
  /// that value.
  pub async fn search(
    &self,
    query: &gfs_search::Query,
    max_results: u32,
  ) -> Result<gfs_search::SearchOutcome, GfsError> {
    let request = self.authed(v1::SearchRequest {
      repository_id: self.binding.repository_id.as_str().to_owned(),
      commit_oid: self.binding.commit.to_qualified(),
      authorization: self.authorization(),
      pattern: query.pattern.clone(),
      literal: query.literal,
      case_insensitive: query.case_insensitive,
      scope: query.scope.clone(),
      include_globs: query
        .include_globs
        .iter()
        .map(|g| g.as_str().to_owned())
        .collect(),
      exclude_globs: query
        .exclude_globs
        .iter()
        .map(|g| g.as_str().to_owned())
        .collect(),
      context_before: query.context_before as u32,
      context_after: query.context_after as u32,
      start_after_path: Vec::new(),
      max_results,
      max_time_ms: 0,
      max_bytes_read: 0,
      max_candidates: 0,
      // Sent rather than left at the server default, because the local half cuts
      // lines with the same numbers. A merged answer whose two halves truncated
      // at different widths would report one match at two lengths.
      max_line_bytes: query.budget.max_line_bytes as u32,
      max_display_bytes: query.budget.max_display_bytes,
    })?;

    let mut stream = self
      .search_grpc
      .clone()
      .search(request)
      .await
      .map_err(|s| convert::from_status(&s))?
      .into_inner();

    let mut matches = Vec::new();
    let mut completion = None;
    loop {
      match stream.message().await {
        Ok(Some(message)) => match message.message {
          Some(v1::search_response::Message::Match(m)) => matches.push(convert_match(m)),
          Some(v1::search_response::Message::Completion(c)) => completion = Some(c),
          None => {}
        },
        Ok(None) => break,
        Err(status) => {
          // A mid-stream failure. Whatever arrived is unusable as an answer,
          // because the thing that would have said how complete it is never did.
          return Ok(gfs_search::SearchOutcome::FailedBeforeCompletion(
            convert::from_status(&status).message,
          ));
        }
      }
    }

    match completion {
      Some(c) => Ok(gfs_search::SearchOutcome::Completed(
        gfs_search::SearchResult {
          matches,
          completion: convert_completion(c),
        },
      )),
      None => Ok(gfs_search::SearchOutcome::FailedBeforeCompletion(
        "the search stream ended without a completion message".to_owned(),
      )),
    }
  }

  /// Fetch a whole blob over the immutable HTTP endpoint.
  ///
  /// Whole-blob, not ranged: DESIGN.md section 12 fixes whole-blob fetch as the
  /// MVP boundary, and the cache verifies the canonical object hash, which a
  /// partial body cannot satisfy.
  pub async fn read_blob(&self, oid: &ObjectId, ticket: &str) -> Result<Vec<u8>, GfsError> {
    let url = format!(
      "{}/v1/repos/{}/blobs/{}?ticket={}",
      self.http_endpoint,
      self.binding.repository_id.as_str(),
      oid.to_qualified(),
      ticket,
    );
    let uri: http::Uri = url
      .parse()
      .map_err(|_| GfsError::invalid("invalid blob URL"))?;

    let mut builder = http::Request::builder().uri(uri);
    if !self.token.is_empty() {
      builder = builder.header(
        http::header::AUTHORIZATION,
        format!("Bearer {}", self.token),
      );
    }
    let request = builder
      .body(Empty::<Bytes>::new())
      .map_err(|e| GfsError::internal(format!("building blob request: {e}")))?;

    let response = self.http.request(request).await.map_err(|e| {
      GfsError::new(
        ErrorCode::Unavailable,
        format!("blob request did not complete: {e}"),
      )
    })?;
    let status = response.status();
    let body = response
      .into_body()
      .collect()
      .await
      .map_err(|e| {
        GfsError::new(
          ErrorCode::Unavailable,
          format!("blob body did not complete: {e}"),
        )
      })?
      .to_bytes();

    if !status.is_success() {
      return Err(http_error(status, &body));
    }
    Ok(body.to_vec())
  }
}

/// Turn a non-2xx blob response into the same error vocabulary the gRPC path
/// produces, preferring the server's own JSON `code` over the status line.
///
/// The status alone is not enough: 503 is both `UNAVAILABLE` (retry) and
/// `SNAPSHOT_BUILDING` (retry, but a different message), and the retry policy in
/// `ErrorCode::retryable` is written against the codes, not against HTTP.
pub(crate) fn http_error(status: http::StatusCode, body: &[u8]) -> GfsError {
  #[derive(serde::Deserialize)]
  struct Body {
    code: String,
    message: String,
  }
  if let Ok(parsed) = serde_json::from_slice::<Body>(body) {
    if let Some(code) = code_from_wire(&parsed.code) {
      return GfsError::new(code, parsed.message);
    }
  }
  let code = match status.as_u16() {
    400 => ErrorCode::InvalidArgument,
    401 => ErrorCode::Unauthenticated,
    403 => ErrorCode::PermissionDenied,
    404 => ErrorCode::NotFound,
    409 => ErrorCode::Conflict,
    422 => ErrorCode::FailedPrecondition,
    429 => ErrorCode::ResourceLimit,
    503 => ErrorCode::Unavailable,
    504 => ErrorCode::DeadlineExceeded,
    _ => ErrorCode::Internal,
  };
  GfsError::new(code, format!("blob request failed with {status}"))
}

/// The inverse of [`ErrorCode::as_str`].
///
/// Written out rather than derived from `serde` so that an unknown code from a
/// newer server is `None` and falls back to the status mapping, instead of
/// failing to deserialize the whole body and losing the message too.
fn code_from_wire(s: &str) -> Option<ErrorCode> {
  Some(match s {
    "INVALID_ARGUMENT" => ErrorCode::InvalidArgument,
    "NOT_FOUND" => ErrorCode::NotFound,
    "PERMISSION_DENIED" => ErrorCode::PermissionDenied,
    "UNAUTHENTICATED" => ErrorCode::Unauthenticated,
    "EXPIRED" => ErrorCode::Expired,
    "FAILED_PRECONDITION" => ErrorCode::FailedPrecondition,
    "CONFLICT" => ErrorCode::Conflict,
    "RESOURCE_LIMIT" => ErrorCode::ResourceLimit,
    "SNAPSHOT_BUILDING" => ErrorCode::SnapshotBuilding,
    "NOT_INDEXABLE" => ErrorCode::NotIndexable,
    "UNSUPPORTED_REPOSITORY_FORMAT" => ErrorCode::UnsupportedRepositoryFormat,
    "RESERVED_NAMESPACE" => ErrorCode::ReservedNamespace,
    "UNAVAILABLE" => ErrorCode::Unavailable,
    "DEADLINE_EXCEEDED" => ErrorCode::DeadlineExceeded,
    "CANCELLED" => ErrorCode::Cancelled,
    "INTERNAL" => ErrorCode::Internal,
    _ => return None,
  })
}

/// Wire match to domain match.
fn convert_match(m: v1::SearchMatch) -> gfs_search::Match {
  gfs_search::Match {
    path: m.path,
    line: m.line,
    column: m.column,
    matched: m.matched,
    line_text: m.line_text,
    before: m.before,
    after: m.after,
    line_truncated: m.line_truncated,
    blob_oid: m.blob_oid,
  }
}

/// Wire completion to domain completion.
///
/// An unrecognized `execution_status` decodes as `Truncated`, not `Complete`.
/// A client reading a newer server must fail toward "this answer may be
/// incomplete"; the opposite default would make a status the client does not
/// understand look like a clean, exhaustive result.
fn convert_completion(c: v1::SearchCompletion) -> gfs_search::Completion {
  use gfs_search::{Coverage, ExecutionStatus, TruncationReason};
  let coverage = c.coverage.unwrap_or_default();
  gfs_search::Completion {
    execution_status: match v1::ExecutionStatus::try_from(c.execution_status) {
      Ok(v1::ExecutionStatus::Complete) => ExecutionStatus::Complete,
      _ => ExecutionStatus::Truncated,
    },
    truncation: c.truncation_reason.as_deref().map(|name| match name {
      "result_limit" => TruncationReason::ResultLimit,
      "time_budget" => TruncationReason::TimeBudget,
      "bytes_budget" => TruncationReason::BytesBudget,
      "candidate_budget" => TruncationReason::CandidateBudget,
      "display_budget" => TruncationReason::DisplayBudget,
      "no_required_literal" => TruncationReason::NoRequiredLiteral,
      _ => TruncationReason::BackendFailure,
    }),
    coverage: Coverage {
      scope: coverage.scope,
      eligible_paths: coverage.eligible_paths,
      excluded: coverage.excluded.into_iter().collect(),
      declared_exclusions: coverage.declared_exclusions,
    },
    index_generation: c.index_generation,
    commit: c.commit_oid,
    stop_budget: c.stop_budget,
    candidates_considered: c.candidates_considered,
    bytes_read: c.bytes_read,
    elapsed_ms: c.elapsed_ms,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn every_error_code_round_trips_through_the_wire_name() {
    // The client's retry policy reads `ErrorCode`, so a code that does not
    // round-trip silently degrades to the status-line fallback and a retryable
    // failure becomes a permanent one.
    for code in [
      ErrorCode::InvalidArgument,
      ErrorCode::NotFound,
      ErrorCode::PermissionDenied,
      ErrorCode::Unauthenticated,
      ErrorCode::Expired,
      ErrorCode::FailedPrecondition,
      ErrorCode::Conflict,
      ErrorCode::ResourceLimit,
      ErrorCode::SnapshotBuilding,
      ErrorCode::NotIndexable,
      ErrorCode::UnsupportedRepositoryFormat,
      ErrorCode::ReservedNamespace,
      ErrorCode::Unavailable,
      ErrorCode::DeadlineExceeded,
      ErrorCode::Cancelled,
      ErrorCode::Internal,
    ] {
      assert_eq!(code_from_wire(code.as_str()), Some(code), "{code:?}");
    }
  }

  #[test]
  fn an_unknown_code_falls_back_to_the_status_line() {
    let body = br#"{"code":"SOMETHING_NEWER","message":"from a future server"}"#;
    let e = http_error(http::StatusCode::SERVICE_UNAVAILABLE, body);
    assert_eq!(e.code, ErrorCode::Unavailable);
    assert!(e.is_retryable());
  }

  #[test]
  fn a_json_body_wins_over_the_status_code() {
    // 503 is both UNAVAILABLE and SNAPSHOT_BUILDING; only the body distinguishes.
    let body = br#"{"code":"SNAPSHOT_BUILDING","message":"index is building"}"#;
    let e = http_error(http::StatusCode::SERVICE_UNAVAILABLE, body);
    assert_eq!(e.code, ErrorCode::SnapshotBuilding);
  }

  #[test]
  fn a_non_json_body_still_produces_a_typed_error() {
    let e = http_error(http::StatusCode::FORBIDDEN, b"<html>nope</html>");
    assert_eq!(e.code, ErrorCode::PermissionDenied);
  }
}
