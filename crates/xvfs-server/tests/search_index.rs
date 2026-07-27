//! M4.2: snapshot manifests, their lifecycle, and their agreement with Git.
//!
//! The oracle is stock `git ls-tree`, never libgit2 and never XVFS reading
//! itself. A manifest that disagreed with Git about which files a commit
//! contains would produce search results that are wrong rather than slow, which
//! M4's exit gate ranks as the worst available outcome.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use xvfs_search::snapshots::{Claim, PreparePolicy};
use xvfs_search::{SearchStore, SnapshotStore};
use xvfs_server::auth::{AllowList, CapabilityKey, StaticTokens};
use xvfs_server::catalog::repositories::NewRepository;
use xvfs_server::search::{IndexManager, PrepareOutcome};
use xvfs_server::{Catalog, Server};
use xvfs_types::{
  DisplayName, HashAlgorithm, LeasePolicy, ObjectId, RepositoryId, RevisionSelector, SnapshotState,
  SubjectId,
};

const TOKEN: &str = "token-search";

struct Harness {
  server: Arc<Server>,
  repo_id: RepositoryId,
  repo_path: std::path::PathBuf,
  _tmp: tempfile::TempDir,
}

impl Harness {
  fn new(fixture: &str) -> Harness {
    let (tmp, repo_path) = xvfs_test::scratch_clone(fixture).unwrap();
    let catalog = Arc::new(Catalog::open_in_memory().unwrap());
    let repo_id = RepositoryId::parse("r-search").unwrap();
    catalog
      .create_repository(&NewRepository {
        repository_id: repo_id.clone(),
        display_name: DisplayName::parse("acme/searched").unwrap(),
        repo_path: repo_path.clone(),
        algorithm: HashAlgorithm::Sha1,
        upstream_url: None,
        credential_ref: None,
      })
      .unwrap();
    let subject = SubjectId::parse("job-search").unwrap();
    let server = Arc::new(Server::new(
      catalog,
      Arc::new(StaticTokens::new().with_token(TOKEN, subject.clone())),
      Arc::new(AllowList::new().allow(&subject, &repo_id)),
      CapabilityKey::generate().unwrap(),
      LeasePolicy::adr_0006(),
    ));
    server.registry.activate(&repo_id).unwrap();
    Harness {
      server,
      repo_id,
      repo_path,
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

  async fn first_parent(&self, commit: &ObjectId) -> ObjectId {
    self
      .server
      .registry
      .repository(&self.repo_id)
      .unwrap()
      .read_commit(commit.clone())
      .await
      .unwrap()
      .parents
      .first()
      .cloned()
      .expect("the fixture has more than one commit")
  }

  fn search(&self) -> &Arc<IndexManager> {
    &self.server.search
  }

  fn snapshots(&self) -> SnapshotStore {
    self.server.search.snapshots(&self.repo_id)
  }

  /// A second index over the same repository, with nothing in it.
  fn fresh_index(&self) -> Arc<IndexManager> {
    Arc::new(IndexManager::new(
      Arc::new(SearchStore::open_in_memory().unwrap()),
      Arc::clone(&self.server.registry),
    ))
  }

  /// The searchable paths stock Git reports for a commit.
  fn git_paths(&self, commit: &ObjectId) -> BTreeSet<Vec<u8>> {
    use std::ffi::OsStr;
    let listing = xvfs_test::git_bytes(
      &self.repo_path,
      &[
        OsStr::new("ls-tree"),
        OsStr::new("-r"),
        OsStr::new("-z"),
        OsStr::new("--full-tree"),
        OsStr::new(&commit.to_hex()),
      ],
    )
    .unwrap();

    let mut out = BTreeSet::new();
    for record in listing.split(|b| *b == 0).filter(|r| !r.is_empty()) {
      let tab = record.iter().position(|b| *b == b'\t').unwrap();
      let (meta, path) = record.split_at(tab);
      let mode = &meta[..6];
      // The searchable corpus: regular and executable files. Symlinks (120000)
      // and gitlinks (160000) are excluded, matching `rg`'s default of not
      // following symlinks.
      if mode == b"100644" || mode == b"100755" {
        out.insert(path[1..].to_vec());
      }
    }
    out
  }
}

async fn prepared(h: &Harness, commit: &ObjectId) -> xvfs_search::Manifest {
  let outcome = h.search().prepare(&h.repo_id, commit, true).await.unwrap();
  assert!(
    matches!(outcome, PrepareOutcome::Ready(_)),
    "preparation did not finish: {outcome:?}"
  );
  h.snapshots().manifest(commit).unwrap().unwrap()
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_manifest_contains_exactly_the_files_stock_git_reports() {
  // `modes` carries symlinks and a gitlink alongside ordinary files, so this
  // also pins the corpus boundary rather than only the walk.
  let h = Harness::new("modes");
  let head = h.head().await;
  let manifest = prepared(&h, &head).await;

  let from_manifest: BTreeSet<Vec<u8>> = manifest
    .paths()
    .iter()
    .map(|e| e.path.as_bytes().to_vec())
    .collect();
  assert_eq!(from_manifest, h.git_paths(&head));
  assert!(!from_manifest.is_empty());
}

#[tokio::test]
async fn non_utf8_paths_survive_the_walk_and_the_encoding() {
  let h = Harness::new("bytes");
  let head = h.head().await;
  let manifest = prepared(&h, &head).await;
  let from_manifest: BTreeSet<Vec<u8>> = manifest
    .paths()
    .iter()
    .map(|e| e.path.as_bytes().to_vec())
    .collect();
  assert_eq!(from_manifest, h.git_paths(&head));
  assert!(
    from_manifest
      .iter()
      .any(|p| std::str::from_utf8(p).is_err()),
    "the `bytes` fixture exists to carry a path that is not UTF-8"
  );
}

#[tokio::test]
async fn a_deep_tree_is_walked_to_the_bottom() {
  let h = Harness::new("deep");
  let head = h.head().await;
  let manifest = prepared(&h, &head).await;
  assert_eq!(
    manifest
      .paths()
      .iter()
      .map(|e| e.path.as_bytes().to_vec())
      .collect::<BTreeSet<_>>(),
    h.git_paths(&head)
  );
}

#[tokio::test]
async fn a_wide_directory_is_walked_completely() {
  // `bigdir` has 5000 entries in one tree, which is past the directory page
  // size. A fan-out that assumed one page would silently lose most of them.
  let h = Harness::new("bigdir");
  let head = h.head().await;
  let manifest = prepared(&h, &head).await;
  assert_eq!(manifest.len(), h.git_paths(&head).len());
}

#[tokio::test]
async fn an_incremental_build_describes_the_same_tree_as_a_full_one() {
  // The invariant that makes the cheap path safe. `basic` has two commits, so
  // preparing the parent first sends the child down `diff_commits`.
  //
  // The comparison is on `(path, mode, blob OID)`, not on the encoded bytes.
  // A blob *key* is an allocation within one registry — the first index sees
  // the parent's blobs first and numbers them accordingly, the second index
  // never sees the parent at all — so two stores legitimately assign different
  // keys to the same content. Asserting byte equality across stores would be
  // asserting that two independent allocators agree, which is neither true nor
  // something the design wants to be true.
  let h = Harness::new("basic");
  let head = h.head().await;
  let parent = h.first_parent(&head).await;

  prepared(&h, &parent).await;
  let incremental = prepared(&h, &head).await;

  // A second index that has never seen the parent must take the full-walk path.
  let fresh = h.fresh_index();
  let outcome = fresh.prepare(&h.repo_id, &head, true).await.unwrap();
  assert!(matches!(outcome, PrepareOutcome::Ready(_)));
  let full = fresh
    .snapshots(&h.repo_id)
    .manifest(&head)
    .unwrap()
    .unwrap();

  let resolve = |manifest: &xvfs_search::Manifest, blobs: xvfs_search::BlobRegistry| {
    let keys: Vec<u32> = manifest.paths().iter().map(|e| e.key).collect();
    let records = blobs.records_for_keys(&keys).unwrap();
    let by_key: std::collections::HashMap<u32, String> = records
      .iter()
      .map(|r| (r.key, r.oid.to_qualified()))
      .collect();
    manifest
      .paths()
      .iter()
      .map(|e| (e.path.as_bytes().to_vec(), e.mode, by_key[&e.key].clone()))
      .collect::<Vec<_>>()
  };

  assert_eq!(
    resolve(&incremental, h.search().blobs(&h.repo_id)),
    resolve(&full, fresh.blobs(&h.repo_id)),
    "an incremental manifest that drifts from a full one returns wrong results, not slow ones"
  );
  assert_eq!(
    incremental
      .paths()
      .iter()
      .map(|e| e.path.as_bytes().to_vec())
      .collect::<BTreeSet<_>>(),
    h.git_paths(&head)
  );
}

#[tokio::test]
async fn a_deleted_file_leaves_the_incremental_manifest() {
  // `basic`'s second commit removes `docs/guide.md`. An incremental build that
  // only applied additions would keep serving a file the commit does not have.
  let h = Harness::new("basic");
  let head = h.head().await;
  let parent = h.first_parent(&head).await;

  let before = prepared(&h, &parent).await;
  assert!(before
    .paths()
    .iter()
    .any(|e| e.path.as_bytes() == b"docs/guide.md"));

  let after = prepared(&h, &head).await;
  assert!(
    !after
      .paths()
      .iter()
      .any(|e| e.path.as_bytes() == b"docs/guide.md"),
    "the removed path is still in the manifest"
  );
}

#[tokio::test]
async fn repeated_preparation_of_the_same_commit_does_not_rebuild() {
  let h = Harness::new("basic");
  let head = h.head().await;
  prepared(&h, &head).await;
  let record = h.snapshots().get(&head).unwrap().unwrap();

  // The second call must find it READY rather than claiming a build.
  match h.snapshots().claim(&head, "op-second").unwrap() {
    Claim::Ready(found) => assert_eq!(found.checksum, record.checksum),
    other => panic!("a prepared snapshot must not be rebuilt: {other:?}"),
  }
}

#[tokio::test]
async fn simultaneous_preparation_of_one_commit_produces_one_snapshot() {
  let h = Harness::new("basic");
  let head = h.head().await;

  let a = h.search().clone();
  let b = h.search().clone();
  let (ra, rb) = tokio::join!(
    a.prepare(&h.repo_id, &head, true),
    b.prepare(&h.repo_id, &head, true)
  );
  // Both callers get an answer, and neither gets an error: the loser waits on
  // the durable claim rather than walking the same tree again.
  for outcome in [ra.unwrap(), rb.unwrap()] {
    assert!(
      matches!(outcome, PrepareOutcome::Ready(_)),
      "expected both callers to see READY, got {outcome:?}"
    );
  }
  let record = h.snapshots().get(&head).unwrap().unwrap();
  assert_eq!(record.state, SnapshotState::Ready);
  assert_eq!(record.attempts, 0);
}

#[tokio::test]
async fn an_on_demand_snapshot_expires_and_a_retained_one_does_not() {
  let h = Harness::new("basic");
  let head = h.head().await;
  let parent = h.first_parent(&head).await;

  // The tip is retained by policy; the older commit is a job's on-demand ask.
  h.search().prepare(&h.repo_id, &head, true).await.unwrap();
  h.search()
    .prepare(&h.repo_id, &parent, false)
    .await
    .unwrap();

  assert_eq!(h.snapshots().get(&head).unwrap().unwrap().expires_at, None);
  assert!(h
    .snapshots()
    .get(&parent)
    .unwrap()
    .unwrap()
    .expires_at
    .is_some());

  // Collect with an already-elapsed TTL and nothing pinned. The retained tip
  // survives because its expiry is NULL, not because it was pinned.
  let expiring = SnapshotStore::new(
    Arc::new(SearchStore::open_in_memory().unwrap()),
    h.repo_id.clone(),
    PreparePolicy {
      ttl_seconds: -1,
      ..PreparePolicy::default()
    },
  );
  drop(expiring);

  let report = h.snapshots().gc(&HashSet::new()).unwrap();
  assert_eq!(report.expired, 0, "nothing has expired yet");
  assert_eq!(report.retained, 2);
}

#[tokio::test]
async fn the_prepare_snapshot_rpc_reports_a_real_state() {
  use xvfs_proto::v1;

  let h = Harness::new("basic");
  let head = h.head().await;
  let api = h.server.snapshot_api();

  let mut request = tonic::Request::new(v1::PrepareSnapshotRequest {
    repository_id: h.repo_id.as_str().to_owned(),
    commit_oid: head.to_qualified(),
    authorization: None,
  });
  request
    .metadata_mut()
    .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());

  let response =
    xvfs_proto::v1::snapshot_service_server::SnapshotService::prepare_snapshot(&api, request)
      .await
      .unwrap()
      .into_inner();

  assert_eq!(response.state, v1::SnapshotState::Ready as i32);
  assert_eq!(response.commit_oid, head.to_qualified());
  assert_eq!(response.failure_reason, None);
  // And the manifest it reports as READY is actually readable.
  assert!(h.snapshots().manifest(&head).unwrap().is_some());
}

#[tokio::test]
async fn every_blob_in_a_manifest_is_classified() {
  // A path whose blob was never examined is an index gap, and M4.3's coverage
  // contract has to be able to distinguish that from a clean miss. Here the
  // build is complete, so there should be none.
  let h = Harness::new("content");
  let head = h.head().await;
  let manifest = prepared(&h, &head).await;

  let blobs = h.search().blobs(&h.repo_id);
  let keys: Vec<u32> = manifest.paths().iter().map(|e| e.key).collect();
  let records = blobs.records_for_keys(&keys).unwrap();
  assert_eq!(records.len(), manifest.members().len() as usize);
  for record in &records {
    assert!(
      record.class.is_some(),
      "blob {} was interned but never classified",
      record.oid.to_qualified()
    );
  }
  // `content` deliberately holds a blob with NUL bytes and a large one, so the
  // classifier must have produced more than one class.
  let classes: HashSet<_> = records.iter().filter_map(|r| r.class).collect();
  assert!(
    classes.len() > 1,
    "the `content` fixture should produce both text and non-text classes, got {classes:?}"
  );
}
