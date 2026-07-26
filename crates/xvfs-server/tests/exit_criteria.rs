//! M1's exit criteria, asserted directly.
//!
//! PLAN.md section 4 lists five. Each is named below with the criterion it checks,
//! so a reader can match the suite to the plan without inferring it.
//!
//! Two of them are covered elsewhere and are cross-referenced rather than
//! duplicated: lease survival across force push and `gc` lives in
//! `xvfs-git/tests/repository.rs` and `tests/mounts.rs`, and the
//! existence-inference cases live in `tests/authorization.rs`.
//!
//! The million-entry test is `#[ignore]` by default because building the fixture
//! takes tens of seconds. `scripts/check.sh bigtree` runs it, and CI runs that as a
//! separate job -- so it is gated, not skipped.

use std::sync::Arc;

use xvfs_server::auth::{AllowList, CapabilityKey, SnapshotAuthorization, StaticTokens};
use xvfs_server::catalog::repositories::NewRepository;
use xvfs_server::{Catalog, Server};
use xvfs_types::{
  BytePath, DisplayName, HashAlgorithm, LeasePolicy, RepositoryId, RevisionSelector, SubjectId,
};

const TOKEN: &str = "token-owner";

struct Harness {
  server: Arc<Server>,
  repo_id: RepositoryId,
  repo_path: std::path::PathBuf,
  subject: SubjectId,
  _tmp: Option<tempfile::TempDir>,
}

fn harness_for(repo_path: std::path::PathBuf, tmp: Option<tempfile::TempDir>) -> Harness {
  let catalog = Arc::new(Catalog::open_in_memory().unwrap());
  let repo_id = RepositoryId::parse("r-exit").unwrap();
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

  let subject = SubjectId::parse("job-owner").unwrap();
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
    subject,
    _tmp: tmp,
  }
}

fn scratch(fixture: &str) -> Harness {
  let (tmp, path) = xvfs_test::scratch_clone(fixture).unwrap();
  harness_for(path, Some(tmp))
}

// ---------------------------------------------------------------------------
// Criterion 1: resolve, page a million-entry snapshot, fetch one file, no clone
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "builds a million-entry fixture; run with scripts/check.sh bigtree"]
async fn a_million_entry_snapshot_can_be_paged_one_directory_at_a_time() {
  // M1 exit criterion 1, verbatim: "A test can resolve a revision, list a
  // million-entry snapshot one directory at a time, and fetch an individual file
  // without cloning."
  const DIRS: usize = 1000;
  const FILES: usize = 1000;

  let built = std::time::Instant::now();
  let repo_path = xvfs_test::big_tree(DIRS, FILES).unwrap();
  eprintln!("fixture ready in {:?}", built.elapsed());

  let h = harness_for(repo_path, None);
  let repo = h.server.registry.repository(&h.repo_id).unwrap();

  // Resolve.
  let resolved = repo
    .resolve(RevisionSelector::parse("main", HashAlgorithm::Sha1).unwrap())
    .await
    .unwrap();

  // Page the root, one directory at a time. The root holds `DIRS` trees.
  let started = std::time::Instant::now();
  let mut directories = Vec::new();
  let mut token: Option<Vec<u8>> = None;
  loop {
    let page = repo
      .list_directory(
        resolved.commit.clone(),
        BytePath::root(),
        token.clone(),
        500,
      )
      .await
      .unwrap();
    for e in &page.entries {
      if e.kind == xvfs_types::EntryKind::Directory {
        directories.push(e.path.clone());
      }
    }
    match page.next_page_token {
      None => break,
      Some(t) => token = Some(t),
    }
  }
  assert_eq!(
    directories.len(),
    DIRS,
    "every top-level directory must appear"
  );

  // Then page each directory. This is the "one directory at a time" part: the
  // snapshot is never materialized, and no single response is unbounded.
  let mut total = 0usize;
  let mut seen_in_first: std::collections::BTreeSet<Vec<u8>> = Default::default();
  for (i, dir) in directories.iter().enumerate() {
    let mut token: Option<Vec<u8>> = None;
    loop {
      let page = repo
        .list_directory(resolved.commit.clone(), dir.clone(), token.clone(), 1000)
        .await
        .unwrap();
      for e in &page.entries {
        total += 1;
        if i == 0 {
          assert!(
            seen_in_first.insert(e.path.as_bytes().to_vec()),
            "duplicate entry {:?}",
            e.path
          );
        }
      }
      match page.next_page_token {
        None => break,
        Some(t) => token = Some(t),
      }
    }
  }
  let paged = started.elapsed();

  // `d0000` also holds the sort-key boundary pair, so the total is the files plus
  // `pager.h` plus the `pager/` directory entry.
  assert_eq!(
    total,
    DIRS * FILES + 2,
    "paging lost or duplicated entries across {DIRS} directories"
  );
  // Both halves of the boundary pair survived paging at scale.
  assert!(seen_in_first.contains(b"d0000/pager.h".as_slice()));
  assert!(seen_in_first.contains(b"d0000/pager".as_slice()));

  // Fetch one individual file.
  let entry = repo
    .entry(resolved.commit.clone(), BytePath::new("d0500/f000500.txt"))
    .await
    .unwrap()
    .expect("the file must exist");
  let bytes = repo.read_blob(entry.oid.clone()).await.unwrap();
  assert_eq!(bytes, b"x\n");

  // "Without cloning" is the load-bearing half. The client -- this test -- holds no
  // repository of its own: it read one blob and the tree pages it asked for, and
  // nothing transferred the other million entries' content.
  eprintln!(
    "paged {total} entries across {DIRS} directories in {paged:?}, then read one \
     {} byte file",
    bytes.len()
  );
  assert_eq!(
    xvfs_test::expected_entries(DIRS, FILES),
    total,
    "the generator and the reader disagree about the entry count"
  );
}

// ---------------------------------------------------------------------------
// Criterion 2: concurrent ref movement cannot produce mixed-commit responses
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_ref_movement_cannot_produce_a_mixed_commit_response() {
  // M1 exit criterion 2. The property is *snapshot consistency*: a response's
  // content must belong to the commit that response names, even while the branch is
  // being moved underneath.
  //
  // The mechanism is DESIGN.md section 6.2 -- resolve once, then name the commit --
  // so what this test proves is that the mechanism is actually used on the read
  // path, and that a reader mid-sequence is unaffected by a writer.
  let h = scratch("basic");
  let repo = h.server.registry.repository(&h.repo_id).unwrap();

  let tip = xvfs_test::git(&h.repo_path, &["rev-parse", "main"])
    .unwrap()
    .trim()
    .to_owned();
  let older = xvfs_test::git(&h.repo_path, &["rev-parse", "v1.0"])
    .unwrap()
    .trim()
    .to_owned();
  // The two commits differ in a way a mixed response would reveal: `src/new.rs`
  // exists only at the tip, and `docs/guide.md` only at the older commit.
  assert!(xvfs_test::git(
    &h.repo_path,
    &["cat-file", "-e", &format!("{tip}^{{commit}}")]
  )
  .is_ok());

  let writer_path = h.repo_path.clone();
  let writer = std::thread::spawn(move || {
    for i in 0..60 {
      let target = if i % 2 == 0 { &older } else { &tip };
      let _ = xvfs_test::git(&writer_path, &["update-ref", "refs/heads/main", target]);
      std::thread::sleep(std::time::Duration::from_millis(2));
    }
  });

  for _ in 0..60 {
    // Resolve once, then read *that* commit. Every later call names the OID.
    let resolved = repo
      .resolve(RevisionSelector::parse("main", HashAlgorithm::Sha1).unwrap())
      .await
      .unwrap();

    let root = repo
      .list_directory(resolved.commit.clone(), BytePath::root(), None, 100)
      .await
      .unwrap();
    let has_docs = root.entries.iter().any(|e| e.path.as_bytes() == b"docs");
    let new_file = repo
      .entry(resolved.commit.clone(), BytePath::new("src/new.rs"))
      .await
      .unwrap();

    // The invariant: the two observations agree with each other. `docs/` exists in
    // the first commit and `src/new.rs` in the second, and no commit has both --
    // so seeing both would be a response mixed across generations.
    assert!(
      has_docs != new_file.is_some(),
      "mixed-commit response for {}: docs={has_docs}, src/new.rs={}",
      resolved.commit,
      new_file.is_some()
    );

    // And the commit's own tree matches what it reported, which a stale cache
    // keyed on the branch rather than the commit would break.
    let commit = repo.read_commit(resolved.commit.clone()).await.unwrap();
    assert_eq!(commit.tree, resolved.tree);
    assert_eq!(commit.commit, resolved.commit);
  }

  writer.join().unwrap();
}

#[tokio::test]
async fn a_mount_is_unaffected_by_ref_movement_after_it_was_created() {
  // The stronger form of the same criterion, and the one the product depends on:
  // once a mount exists, the branch is irrelevant to it.
  let h = scratch("basic");
  let grant = h
    .server
    .mounts
    .create_mount(
      &h.repo_id,
      RevisionSelector::parse("main", HashAlgorithm::Sha1).unwrap(),
      &h.subject,
      None,
    )
    .await
    .unwrap();

  let older = xvfs_test::git(&h.repo_path, &["rev-parse", "v1.0"])
    .unwrap()
    .trim()
    .to_owned();
  xvfs_test::git(&h.repo_path, &["update-ref", "refs/heads/main", &older]).unwrap();

  // The branch now points elsewhere; the mount does not.
  let repo = h.server.registry.repository(&h.repo_id).unwrap();
  let after = repo
    .resolve(RevisionSelector::parse("main", HashAlgorithm::Sha1).unwrap())
    .await
    .unwrap();
  assert_ne!(after.commit, grant.commit, "the branch must have moved");

  let entry = repo
    .entry(grant.commit.clone(), BytePath::new("src/new.rs"))
    .await
    .unwrap();
  assert!(
    entry.is_some(),
    "the mounted commit must still expose its own tree"
  );
}

// ---------------------------------------------------------------------------
// Criterion 4: lease refs are absent from advertisements and survive prune
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lease_refs_are_absent_from_advertisements_and_survive_a_pruning_fetch() {
  // M1 exit criterion 4. The advertisement half needs the protected upload-pack
  // configuration; the prune half needs the explicit refspecs.
  let h = scratch("basic");
  let grant = h
    .server
    .mounts
    .create_mount(
      &h.repo_id,
      RevisionSelector::parse("main", HashAlgorithm::Sha1).unwrap(),
      &h.subject,
      None,
    )
    .await
    .unwrap();
  let anchor = xvfs_types::revision::lease_anchor_ref(grant.mount_id.as_str());

  // Absent from the API's ref enumeration. No `use GitRepository` needed: the
  // registry hands back an `Arc<dyn GitRepository>`, and a trait object's methods
  // resolve without the trait in scope.
  let repo = h.server.registry.blocking_repository(&h.repo_id).unwrap();
  assert!(repo
    .visible_refs()
    .unwrap()
    .iter()
    .all(|(n, _)| !n.starts_with("refs/xvfs/")));

  // Absent from a Git advertisement under the protected configuration.
  let advertised = xvfs_test::git(
    &h.repo_path,
    &[
      "ls-remote",
      "--refs",
      "--upload-pack",
      "git -c uploadpack.hideRefs=refs/xvfs/ upload-pack",
      h.repo_path.to_str().unwrap(),
    ],
  )
  .unwrap();
  assert!(!advertised.contains("refs/xvfs/"), "{advertised}");

  // Survives a pruning upstream fetch with the explicit refspecs.
  let (_up_tmp, upstream) = xvfs_test::scratch_clone("basic").unwrap();
  xvfs_test::git(&upstream, &["update-ref", "-d", "refs/heads/feature"]).unwrap();
  xvfs_server::mirror::fetch(
    &h.repo_path,
    upstream.to_str().unwrap(),
    None,
    std::path::Path::new("git"),
  )
  .unwrap();

  h.server.registry.evict(&h.repo_id);
  let repo = h.server.registry.blocking_repository(&h.repo_id).unwrap();
  assert_eq!(
    repo.read_lease_anchor(&anchor).unwrap(),
    Some(grant.commit),
    "a pruning fetch must not remove the anchor"
  );
  // The prune did run, so the assertion above is not vacuous.
  assert!(repo
    .visible_refs()
    .unwrap()
    .iter()
    .all(|(n, _)| n != "refs/heads/feature"));
}

// ---------------------------------------------------------------------------
// Criterion 5: no existence inference, including through the cache
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_blob_ticket_cannot_be_used_to_probe_for_blobs_in_another_snapshot() {
  // The cache half of criterion 5. A ticket is bound to a specific blob, so holding
  // one does not let a caller enumerate or confirm other object IDs -- which a
  // repository-scoped grant would.
  let h = scratch("basic");
  let repo = h.server.registry.repository(&h.repo_id).unwrap();
  let resolved = repo
    .resolve(RevisionSelector::parse("main", HashAlgorithm::Sha1).unwrap())
    .await
    .unwrap();
  let access = h
    .server
    .authz
    .authorize_commit(
      &h.subject,
      &h.repo_id,
      &resolved.commit,
      &SnapshotAuthorization::default(),
    )
    .await
    .unwrap();

  let readme = repo
    .entry(resolved.commit.clone(), BytePath::new("README.md"))
    .await
    .unwrap()
    .unwrap();
  let other = repo
    .entry(resolved.commit.clone(), BytePath::new("src/main.rs"))
    .await
    .unwrap()
    .unwrap();

  let ticket =
    h.server
      .authz
      .issue_blob_ticket(&access, &readme.oid, std::time::Duration::from_secs(60));

  // The ticket works for its own blob.
  h.server
    .authz
    .verify_blob_ticket(&h.subject, &h.repo_id, &readme.oid, &ticket)
    .unwrap();

  // And gives the same answer for a blob that exists and one that does not, so it
  // cannot be used as an existence oracle.
  let absent = xvfs_types::ObjectId::from_hex(HashAlgorithm::Sha1, &"ab".repeat(20)).unwrap();
  let for_existing = h
    .server
    .authz
    .verify_blob_ticket(&h.subject, &h.repo_id, &other.oid, &ticket)
    .unwrap_err();
  let for_absent = h
    .server
    .authz
    .verify_blob_ticket(&h.subject, &h.repo_id, &absent, &ticket)
    .unwrap_err();
  assert_eq!(for_existing, for_absent);
}
