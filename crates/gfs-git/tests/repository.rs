//! Conformance tests for the libgit2-backed repository, against the fixture
//! matrix built by stock Git.
//!
//! These are the high-level smoke tests for the crate's entry points, in the sense
//! AGENTS.md asks for: each one covers a behaviour the design or an ADR commits
//! to, rather than an internal detail. Where a claim can be checked against an
//! independent oracle, it is -- `git ls-tree` and `git cat-file` produce the
//! expected values, so a bug shared between the API and its check cannot hide.

use std::sync::Arc;

use gfs_git::{GitRepository, Libgit2Repository};
use gfs_types::error::ErrorCode;
use gfs_types::{limits, mode, BytePath, EntryKind, HashAlgorithm, ObjectId, RevisionSelector};

const TREE_CACHE_BYTES: usize = 4 * 1024 * 1024;

fn open(name: &str) -> Libgit2Repository {
  Libgit2Repository::open(gfs_test::bare(name), 4, TREE_CACHE_BYTES)
    .unwrap_or_else(|e| panic!("opening fixture {name}: {e}"))
}

fn selector(repo: &Libgit2Repository, s: &str) -> RevisionSelector {
  RevisionSelector::parse(s, repo.algorithm()).unwrap()
}

fn head(repo: &Libgit2Repository) -> ObjectId {
  repo.resolve(&selector(repo, "main")).unwrap().commit
}

// ---------------------------------------------------------------------------
// The format gate
// ---------------------------------------------------------------------------

#[test]
fn every_fixture_matches_its_declared_openability() {
  // The matrix-level assertion. `reftable` and `sha256` exist to prove ADR 0001's
  // rejections still fire; the rest must open. Iterating the whole matrix means a
  // newly added fixture cannot quietly go untested.
  for f in gfs_test::FIXTURES {
    let result = Libgit2Repository::open(gfs_test::bare(f.name), 2, TREE_CACHE_BYTES);
    assert_eq!(
      result.is_ok(),
      f.openable,
      "fixture `{}` ({}) openable={} but open() said {:?}",
      f.name,
      f.rationale,
      f.openable,
      result.err().map(|e| e.to_string())
    );
    if let Err(e) = result {
      // A rejection must be the typed format error, not a generic failure. This
      // is what lets an operator see *why* a mirror was refused.
      assert_eq!(
        e.code,
        ErrorCode::UnsupportedRepositoryFormat,
        "fixture `{}` must be rejected as an unsupported format, got {e}",
        f.name
      );
    }
  }
}

#[test]
fn an_empty_repository_opens_and_reports_no_refs() {
  // An unborn HEAD is not an error state.
  let repo = open("empty");
  assert_eq!(repo.visible_refs().unwrap(), vec![]);
  assert_eq!(repo.algorithm(), HashAlgorithm::Sha1);
}

// ---------------------------------------------------------------------------
// Revision resolution
// ---------------------------------------------------------------------------

#[test]
fn resolves_branches_lightweight_tags_annotated_tags_and_oids() {
  let repo = open("basic");
  let expected = gfs_test::git(&gfs_test::bare("basic"), &["rev-parse", "main"])
    .unwrap()
    .trim()
    .to_owned();

  let by_branch = repo.resolve(&selector(&repo, "main")).unwrap();
  assert_eq!(by_branch.commit.to_hex(), expected);
  assert_eq!(by_branch.ref_name.as_deref(), Some("refs/heads/main"));

  // An annotated tag must be peeled to its commit; the tag object's own OID is
  // not a commit and would produce an unreadable snapshot.
  let annotated = repo.resolve(&selector(&repo, "v2.0")).unwrap();
  assert_eq!(annotated.commit.to_hex(), expected);

  let lightweight = repo.resolve(&selector(&repo, "v1.0")).unwrap();
  assert_ne!(
    lightweight.commit.to_hex(),
    expected,
    "v1.0 is the first commit"
  );

  // A full object ID resolves to itself, and the response repeats it.
  let by_oid = repo
    .resolve(&selector(&repo, &by_branch.commit.to_hex()))
    .unwrap();
  assert_eq!(by_oid.commit, by_branch.commit);
  assert_eq!(by_oid.ref_name, None, "an object id names no ref");

  // And the resolved tree matches Git's.
  let expected_tree = gfs_test::git(&gfs_test::bare("basic"), &["rev-parse", "main^{tree}"])
    .unwrap()
    .trim()
    .to_owned();
  assert_eq!(by_branch.tree.to_hex(), expected_tree);
}

#[test]
fn an_abbreviation_resolves_and_an_ambiguous_one_is_reported_not_guessed() {
  let repo = open("basic");
  let full = head(&repo).to_hex();
  let abbrev: String = full.chars().take(10).collect();
  let resolved = repo.resolve(&selector(&repo, &abbrev)).unwrap();
  assert_eq!(resolved.commit.to_hex(), full);

  // A one-character prefix is below MIN_ABBREV_HEX, so the selector grammar
  // classifies it as a name rather than letting it resolve against a huge
  // candidate set. Either way it must not silently pick a commit.
  let short: String = full.chars().take(1).collect();
  let err = RevisionSelector::parse(&short, repo.algorithm())
    .and_then(|s| repo.resolve(&s))
    .unwrap_err();
  assert!(
    matches!(err.code, ErrorCode::NotFound | ErrorCode::InvalidArgument),
    "got {err}"
  );
}

#[test]
fn a_tag_that_peels_to_a_tree_is_rejected_with_a_typed_error() {
  // M0.3 found this in the wild: the Linux kernel's `v2.6.11` tag peels to a
  // tree. ADR 0006 rejects it rather than resolving it, because a tree OID where
  // every layer expects a commit produces a snapshot nobody can read -- and the
  // failure would surface far from its cause.
  let repo = open("basic");
  let err = repo.resolve(&selector(&repo, "tree-tag")).unwrap_err();
  assert_eq!(err.code, ErrorCode::InvalidArgument);
  assert!(
    err.message.contains("tree"),
    "the error must name what it found: {err}"
  );
}

#[test]
fn a_missing_ref_is_not_found_rather_than_an_internal_error() {
  let repo = open("basic");
  let err = repo
    .resolve(&selector(&repo, "no-such-branch"))
    .unwrap_err();
  assert_eq!(err.code, ErrorCode::NotFound);
}

#[test]
fn resolution_refuses_the_reserved_namespace_even_when_the_ref_exists() {
  // Defence in depth. The selector grammar already rejects both spellings, and
  // this is the layer that would actually hand out a lease anchor if that ever
  // regressed -- so it checks the *resolved* ref name too.
  let (_tmp, path) = gfs_test::scratch_clone("basic").unwrap();
  let repo = Libgit2Repository::open(&path, 2, TREE_CACHE_BYTES).unwrap();
  let commit = head(&repo);
  let anchor = gfs_types::revision::lease_anchor_ref("m-test");
  repo.create_lease_anchor(&anchor, &commit).unwrap();

  // The parser rejects it, which is the first line of defence.
  assert_eq!(
    RevisionSelector::parse(&anchor, repo.algorithm())
      .unwrap_err()
      .code,
    ErrorCode::ReservedNamespace
  );
  assert_eq!(
    RevisionSelector::parse("gfs/mounts/m-test", repo.algorithm())
      .unwrap_err()
      .code,
    ErrorCode::ReservedNamespace
  );

  // And the anchor is absent from ref enumeration, matching the upload-pack
  // hideRefs policy. ADR 0002: this prevents discovery, not access.
  let names: Vec<String> = repo
    .visible_refs()
    .unwrap()
    .into_iter()
    .map(|(n, _)| n)
    .collect();
  assert!(
    !names.iter().any(|n| n.starts_with("refs/gfs/")),
    "lease anchors must not appear in ref enumeration: {names:?}"
  );
}

// ---------------------------------------------------------------------------
// Tree traversal
// ---------------------------------------------------------------------------

#[test]
fn entry_lookup_matches_git_ls_tree_for_every_mode() {
  let repo = open("modes");
  let bare = gfs_test::bare("modes");
  let commit = head(&repo);

  // The oracle: stock Git's own view of the tree.
  let listing = gfs_test::git(&bare, &["ls-tree", "-r", "-t", "HEAD"]).unwrap();
  let mut checked = 0;
  for line in listing.lines() {
    // `<mode> <type> <oid>\t<path>`
    let (meta, path) = line.split_once('\t').unwrap();
    let parts: Vec<&str> = meta.split_whitespace().collect();
    let (git_mode, git_oid) = (parts[0], parts[2]);

    let entry = repo
      .entry(&commit, &BytePath::new(path))
      .unwrap()
      .unwrap_or_else(|| panic!("{path} is in ls-tree but absent from the API"));

    assert_eq!(
      format!("{:06o}", entry.mode),
      format!("{:0>6}", git_mode),
      "mode mismatch for {path}"
    );
    assert_eq!(entry.oid.to_hex(), git_oid, "oid mismatch for {path}");
    checked += 1;
  }
  assert!(checked >= 8, "expected the modes fixture to have entries");

  // The specific representations the design commits to.
  let script = repo
    .entry(&commit, &BytePath::new("script.sh"))
    .unwrap()
    .unwrap();
  assert_eq!(script.kind, EntryKind::Executable);

  let link = repo
    .entry(&commit, &BytePath::new("rel-link"))
    .unwrap()
    .unwrap();
  assert_eq!(link.kind, EntryKind::Symlink);
  // The target is read eagerly so `readlink` never needs a blob fetch.
  assert_eq!(link.symlink_target.as_deref(), Some(&b"plain.txt"[..]));

  // An absolute and an escaping link are *stored* faithfully. Whether to follow
  // them is a FUSE-layer policy decision (DESIGN.md section 10 item 10), not
  // something this layer may normalize away.
  let abs = repo
    .entry(&commit, &BytePath::new("abs-link"))
    .unwrap()
    .unwrap();
  assert_eq!(abs.symlink_target.as_deref(), Some(&b"/etc/passwd"[..]));
  let escape = repo
    .entry(&commit, &BytePath::new("escape-link"))
    .unwrap()
    .unwrap();
  assert_eq!(
    escape.symlink_target.as_deref(),
    Some(&b"../../../etc/shadow"[..])
  );
}

#[test]
fn a_gitlink_is_an_empty_readonly_directory_rather_than_an_error() {
  // DESIGN.md section 8.2. ADR 0006 confirms submodules are present in the real
  // corpus, so this is a live case.
  let repo = open("modes");
  let commit = head(&repo);
  let entry = repo
    .entry(&commit, &BytePath::new("vendor/submodule"))
    .unwrap()
    .unwrap();
  assert_eq!(entry.kind, EntryKind::Gitlink);
  assert_eq!(entry.mode, mode::GITLINK);

  // Listing it yields an empty page, not an error: the submodule's contents live
  // in another repository, so there is genuinely nothing to list.
  let page = repo
    .list_directory(&commit, &BytePath::new("vendor/submodule"), None, 100)
    .unwrap();
  assert!(page.entries.is_empty());
  assert_eq!(page.next_page_token, None);

  // And a path *inside* a gitlink is absent rather than an error.
  assert!(repo
    .entry(&commit, &BytePath::new("vendor/submodule/anything"))
    .unwrap()
    .is_none());
}

#[test]
fn non_utf8_paths_round_trip_through_lookup_and_listing() {
  let repo = open("bytes");
  let commit = head(&repo);

  let listed = repo
    .list_directory(&commit, &BytePath::root(), None, 100)
    .unwrap();
  let names: Vec<Vec<u8>> = listed
    .entries
    .iter()
    .map(|e| e.path.as_bytes().to_vec())
    .collect();

  for expected in [
    &b"latin1-\xff-name.txt"[..],
    b"latin1-caf\xe9.txt",
    b"with space.txt",
    b"with\"quote.txt",
    b"with\nnewline.txt",
    b"back\\slash.txt",
  ] {
    assert!(
      names.iter().any(|n| n == expected),
      "missing {:?} from listing",
      String::from_utf8_lossy(expected)
    );
    // And each is individually addressable by its exact bytes.
    assert!(
      repo
        .entry(&commit, &BytePath::new(expected.to_vec()))
        .unwrap()
        .is_some(),
      "cannot look up {:?}",
      String::from_utf8_lossy(expected)
    );
  }
}

#[test]
fn deep_paths_traverse_every_component() {
  let repo = open("deep");
  let commit = head(&repo);
  let mut rel = String::new();
  for i in 0..40 {
    rel.push_str(&format!("d{i:02}/"));
  }
  rel.push_str("leaf.txt");
  let entry = repo.entry(&commit, &BytePath::new(rel)).unwrap().unwrap();
  assert_eq!(entry.kind, EntryKind::Regular);
  assert_eq!(entry.size, 5);
}

#[test]
fn a_path_whose_parent_is_a_file_is_absent_rather_than_an_error() {
  let repo = open("basic");
  let commit = head(&repo);
  assert!(repo
    .entry(&commit, &BytePath::new("README.md/nested"))
    .unwrap()
    .is_none());
}

#[test]
fn the_root_is_reported_as_a_directory_whose_oid_is_the_commit_tree() {
  let repo = open("basic");
  let resolved = repo.resolve(&selector(&repo, "main")).unwrap();
  let root = repo
    .entry(&resolved.commit, &BytePath::root())
    .unwrap()
    .unwrap();
  assert_eq!(root.kind, EntryKind::Directory);
  assert_eq!(root.oid, resolved.tree);
}

// ---------------------------------------------------------------------------
// Pagination
// ---------------------------------------------------------------------------

#[test]
fn paging_a_large_directory_returns_every_entry_exactly_once() {
  // The regression ADR 0005 measured: paginating on raw names instead of Git's
  // sort keys returned 1597 of 1598 entries in the Linux kernel's include/linux,
  // because the page boundary fell between `byteorder.h` and `byteorder/`. The
  // `bigdir` fixture carries the same `pager.h`/`pager/` pair.
  let repo = open("bigdir");
  let commit = head(&repo);
  let bare = gfs_test::bare("bigdir");

  // The oracle: how many entries Git says are in that directory.
  let expected: usize = gfs_test::git(&bare, &["ls-tree", "HEAD:many"])
    .unwrap()
    .lines()
    .count();
  assert!(expected > 5000, "fixture should be large, got {expected}");

  let mut seen: Vec<Vec<u8>> = Vec::new();
  let mut token: Option<Vec<u8>> = None;
  let mut pages = 0;
  loop {
    let page = repo
      .list_directory(&commit, &BytePath::new("many"), token.as_deref(), 100)
      .unwrap();
    pages += 1;
    assert!(pages < 200, "pagination did not terminate");
    for e in &page.entries {
      seen.push(e.path.as_bytes().to_vec());
    }
    match page.next_page_token {
      None => break,
      Some(t) => token = Some(t),
    }
  }

  assert_eq!(
    seen.len(),
    expected,
    "pagination lost or duplicated entries"
  );
  let unique: std::collections::BTreeSet<&Vec<u8>> = seen.iter().collect();
  assert_eq!(unique.len(), seen.len(), "pagination returned a duplicate");

  // Both halves of the sort-key pair must be present.
  assert!(seen.iter().any(|p| p == b"many/pager.h"));
  assert!(seen.iter().any(|p| p == b"many/pager"));
}

#[test]
fn a_page_size_above_the_limit_is_clamped_rather_than_honoured() {
  let repo = open("bigdir");
  let commit = head(&repo);
  let page = repo
    .list_directory(&commit, &BytePath::new("many"), None, usize::MAX)
    .unwrap();
  assert!(page.entries.len() <= limits::MAX_DIRECTORY_PAGE_SIZE);
}

#[test]
fn listing_a_file_is_invalid_and_listing_a_missing_path_is_not_found() {
  let repo = open("basic");
  let commit = head(&repo);
  assert_eq!(
    repo
      .list_directory(&commit, &BytePath::new("README.md"), None, 10)
      .unwrap_err()
      .code,
    ErrorCode::InvalidArgument
  );
  assert_eq!(
    repo
      .list_directory(&commit, &BytePath::new("no/such/dir"), None, 10)
      .unwrap_err()
      .code,
    ErrorCode::NotFound
  );
}

// ---------------------------------------------------------------------------
// Blobs
// ---------------------------------------------------------------------------

#[test]
fn blob_bytes_match_git_cat_file_exactly_including_crlf_and_no_final_newline() {
  // DESIGN.md section 12: the mount serves *raw* blob bytes with no
  // `.gitattributes` conversion. `cat-file` is the right oracle precisely because
  // it also bypasses checkout-time filters -- comparing against a working-tree
  // file would compare against the conversion the product does not perform.
  let repo = open("content");
  let bare = gfs_test::bare("content");
  let commit = head(&repo);

  for name in [
    "empty.txt",
    "crlf.txt",
    "no-final-newline.txt",
    "binary.bin",
    "utf16.txt",
  ] {
    let entry = repo
      .entry(&commit, &BytePath::new(name))
      .unwrap()
      .unwrap_or_else(|| panic!("{name} missing"));
    let ours = repo.read_blob(&entry.oid).unwrap();

    let expected = std::process::Command::new("git")
      .current_dir(&bare)
      .args(["cat-file", "blob", &entry.oid.to_hex()])
      .output()
      .unwrap()
      .stdout;

    assert_eq!(ours, expected, "blob bytes differ for {name}");
    assert_eq!(entry.size, expected.len() as u64, "size differs for {name}");
  }
}

#[test]
fn attributes_and_lfs_content_is_served_raw_as_documented() {
  // The divergence DESIGN.md section 12 documents, asserted as expected behaviour
  // rather than left to be discovered: `.gitattributes` says `*.txt text
  // eol=crlf`, a real checkout would emit CRLF, and the mount emits the stored LF.
  // ADR 0006 confirms such rules are present in the real corpus.
  let repo = open("attrs");
  let commit = head(&repo);

  let txt = repo
    .entry(&commit, &BytePath::new("converted.txt"))
    .unwrap()
    .unwrap();
  let bytes = repo.read_blob(&txt.oid).unwrap();
  assert_eq!(
    bytes, b"alpha\nbeta\n",
    "raw stored bytes, not the CRLF checkout"
  );
  assert!(!bytes.windows(2).any(|w| w == b"\r\n"));

  // An LFS pointer is served as the pointer file. Resolving LFS is out of MVP
  // scope, and a pointer is what the object database actually contains.
  let psd = repo
    .entry(&commit, &BytePath::new("asset.psd"))
    .unwrap()
    .unwrap();
  let pointer = repo.read_blob(&psd.oid).unwrap();
  assert!(pointer.starts_with(b"version https://git-lfs.github.com/spec/v1"));
}

#[test]
fn a_large_blob_is_sized_without_being_inflated() {
  // `blob_size` reads the object header only. On a 12 MiB blob the difference
  // between a header read and a full inflate is the difference between a stat and
  // a read, and `getattr` is on the hot path.
  let repo = open("content");
  let commit = head(&repo);
  let entry = repo
    .entry(&commit, &BytePath::new("large-blob.bin"))
    .unwrap()
    .unwrap();
  assert_eq!(entry.size, 12 * 1024 * 1024);
  assert_eq!(repo.blob_size(&entry.oid).unwrap(), 12 * 1024 * 1024);
}

#[test]
fn reading_a_blob_verifies_its_object_id() {
  // DESIGN.md section 10 item 7. Verifying here means the client cache, the blob
  // endpoint, and the FUSE read path all inherit the guarantee.
  let repo = open("basic");
  let commit = head(&repo);
  let entry = repo
    .entry(&commit, &BytePath::new("README.md"))
    .unwrap()
    .unwrap();
  let bytes = repo.read_blob(&entry.oid).unwrap();
  assert_eq!(bytes, b"# basic\n");

  // A tree OID is not a blob, and asking for it as one must fail rather than
  // return the tree's serialized bytes.
  let root = repo.resolve(&selector(&repo, "main")).unwrap().tree;
  assert!(repo.read_blob(&root).is_err());
}

#[test]
fn an_oid_from_the_wrong_algorithm_is_refused_before_a_lookup() {
  let repo = open("basic");
  let sha256 = ObjectId::from_hex(HashAlgorithm::Sha256, &"ab".repeat(32)).unwrap();
  let err = repo.read_blob(&sha256).unwrap_err();
  assert_eq!(err.code, ErrorCode::InvalidArgument);
  assert!(err.message.contains("sha256"), "{err}");
}

// ---------------------------------------------------------------------------
// Batch reads and the tree cache
// ---------------------------------------------------------------------------

#[test]
fn a_batch_reports_per_path_results_and_one_bad_path_does_not_fail_the_rest() {
  let repo = open("basic");
  let commit = head(&repo);
  let paths = vec![
    BytePath::new("README.md"),
    BytePath::new("no/such/file"),
    BytePath::new("src/main.rs"),
  ];
  let results = repo.batch_entries(&commit, &paths);
  assert_eq!(results.len(), 3);
  assert!(results[0].as_ref().unwrap().is_some());
  // An absent path is `Ok(None)`, an ordinary negative result.
  assert!(results[1].as_ref().unwrap().is_none());
  assert!(results[2].as_ref().unwrap().is_some());
}

#[test]
fn the_tree_cache_serves_repeated_traversals_of_a_shared_prefix() {
  // The reason a batch call exists: the paths in one batch share directory
  // prefixes, and after the first walk those trees come from the cache instead of
  // libgit2. Asserted through the counters rather than by timing, which would be
  // flaky.
  let repo = open("deep");
  let commit = head(&repo);
  let mut prefix = String::new();
  let mut paths = Vec::new();
  for i in 0..40 {
    prefix.push_str(&format!("d{i:02}/"));
    paths.push(BytePath::new(format!("{prefix}leaf.txt")));
  }

  let before = repo.tree_cache_stats();
  let _ = repo.batch_entries(&commit, &paths);
  let after = repo.tree_cache_stats();

  assert!(
    after.hits > before.hits,
    "repeated prefixes must hit the cache: {before:?} -> {after:?}"
  );
  assert!(after.trees > 0);
}

// ---------------------------------------------------------------------------
// Lease anchors and visibility
// ---------------------------------------------------------------------------

#[test]
fn a_lease_anchor_outside_the_reserved_namespace_is_refused() {
  // A caller's bug must not be able to create a publicly advertised ref, which
  // upstream fetch would then prune.
  let (_tmp, path) = gfs_test::scratch_clone("basic").unwrap();
  let repo = Libgit2Repository::open(&path, 2, TREE_CACHE_BYTES).unwrap();
  let commit = head(&repo);
  for bad in ["refs/heads/sneaky", "refs/tags/v9", "refs/mounts/x"] {
    assert_eq!(
      repo.create_lease_anchor(bad, &commit).unwrap_err().code,
      ErrorCode::InvalidArgument,
      "{bad} must be refused as a lease anchor"
    );
  }
}

#[test]
fn creating_an_anchor_is_idempotent_for_the_same_commit_and_conflicts_otherwise() {
  let (_tmp, path) = gfs_test::scratch_clone("basic").unwrap();
  let repo = Libgit2Repository::open(&path, 2, TREE_CACHE_BYTES).unwrap();
  let main = head(&repo);
  let older = repo.resolve(&selector(&repo, "v1.0")).unwrap().commit;
  let anchor = gfs_types::revision::lease_anchor_ref("m-1");

  repo.create_lease_anchor(&anchor, &main).unwrap();
  // Idempotent, so a retried CreateMount after an ambiguous failure is safe.
  repo.create_lease_anchor(&anchor, &main).unwrap();
  assert_eq!(repo.read_lease_anchor(&anchor).unwrap(), Some(main.clone()));

  // A different commit under the same mount id is a collision and must not
  // silently re-anchor another mount's lease.
  assert_eq!(
    repo.create_lease_anchor(&anchor, &older).unwrap_err().code,
    ErrorCode::Conflict
  );
  assert_eq!(repo.read_lease_anchor(&anchor).unwrap(), Some(main));

  // Deletion is idempotent too, so release and restart reconciliation need no
  // coordination.
  repo.delete_lease_anchor(&anchor).unwrap();
  repo.delete_lease_anchor(&anchor).unwrap();
  assert_eq!(repo.read_lease_anchor(&anchor).unwrap(), None);
}

#[test]
fn an_anchor_for_a_nonexistent_commit_is_refused() {
  // A dangling anchor keeps nothing reachable while looking like it does, which
  // is the worst possible failure mode for a retention lease.
  let (_tmp, path) = gfs_test::scratch_clone("basic").unwrap();
  let repo = Libgit2Repository::open(&path, 2, TREE_CACHE_BYTES).unwrap();
  let absent = ObjectId::from_hex(HashAlgorithm::Sha1, &"ab".repeat(20)).unwrap();
  let anchor = gfs_types::revision::lease_anchor_ref("m-dangling");
  assert_eq!(
    repo.create_lease_anchor(&anchor, &absent).unwrap_err().code,
    ErrorCode::NotFound
  );
}

#[test]
fn visibility_tracks_reachability_from_visible_refs_only() {
  let (_tmp, path) = gfs_test::scratch_clone("basic").unwrap();
  let repo = Libgit2Repository::open(&path, 2, TREE_CACHE_BYTES).unwrap();

  let tip = head(&repo);
  let first = repo.resolve(&selector(&repo, "v1.0")).unwrap().commit;

  // A tip is visible, and so is its ancestor.
  assert!(repo.is_visible(&tip).unwrap());
  assert!(repo.is_visible(&first).unwrap());

  // Now make the tip unreachable the way a force push does, and remove every
  // other ref that still reaches it.
  gfs_test::git(&path, &["update-ref", "refs/heads/main", &first.to_hex()]).unwrap();
  gfs_test::git(&path, &["update-ref", "-d", "refs/heads/feature"]).unwrap();
  gfs_test::git(&path, &["tag", "-d", "v2.0"]).unwrap();
  gfs_test::git(&path, &["tag", "-d", "tree-tag"]).unwrap();

  let repo = Libgit2Repository::open(&path, 2, TREE_CACHE_BYTES).unwrap();
  assert!(
    !repo.is_visible(&tip).unwrap(),
    "the force-pushed-away commit must no longer be visible"
  );
  assert!(repo.is_visible(&first).unwrap());

  // A lease anchor keeps the object *readable* but must not make it *visible*:
  // that distinction is exactly what M1.5's mount-capability rule turns on.
  let anchor = gfs_types::revision::lease_anchor_ref("m-vis");
  repo.create_lease_anchor(&anchor, &tip).unwrap();
  assert!(
    !repo.is_visible(&tip).unwrap(),
    "a lease anchor must not make a commit publicly visible"
  );
  assert!(repo.read_commit(&tip).is_ok(), "but it stays readable");
}

#[test]
fn a_leased_commit_survives_a_force_push_a_branch_deletion_and_a_full_gc() {
  // M1's headline exit criterion, and the reason retention leases are in M1
  // rather than M7: without them a routine upstream force push during a pilot job
  // prunes objects out from under a live mount and every uncached read fails
  // permanently mid-task.
  let (_tmp, path) = gfs_test::scratch_clone("basic").unwrap();
  let repo = Libgit2Repository::open(&path, 2, TREE_CACHE_BYTES).unwrap();

  let leased = head(&repo);
  let older = repo.resolve(&selector(&repo, "v1.0")).unwrap().commit;
  let blob = repo
    .entry(&leased, &BytePath::new("src/new.rs"))
    .unwrap()
    .expect("src/new.rs exists only in the second commit")
    .oid;

  let anchor = gfs_types::revision::lease_anchor_ref("m-gc");
  repo.create_lease_anchor(&anchor, &leased).unwrap();

  // Everything that could make the commit unreachable.
  gfs_test::git(&path, &["update-ref", "refs/heads/main", &older.to_hex()]).unwrap();
  gfs_test::git(&path, &["update-ref", "-d", "refs/heads/feature"]).unwrap();
  gfs_test::git(&path, &["tag", "-d", "v2.0"]).unwrap();
  gfs_test::git(&path, &["tag", "-d", "tree-tag"]).unwrap();
  // A full, immediate garbage collection with no grace period.
  gfs_test::git(
    &path,
    &[
      "-c",
      "gc.reflogExpire=now",
      "gc",
      "-q",
      "--prune=now",
      "--aggressive",
    ],
  )
  .unwrap();

  // Reopen so nothing is served from an in-process cache: the objects must still
  // be on disk.
  let repo = Libgit2Repository::open(&path, 2, TREE_CACHE_BYTES).unwrap();
  assert!(
    repo.read_commit(&leased).is_ok(),
    "the leased commit must survive gc --prune=now"
  );
  assert_eq!(
    repo.read_blob(&blob).unwrap(),
    b"pub fn added() {}\n",
    "and so must its tree and blobs"
  );
  assert_eq!(
    repo.read_lease_anchor(&anchor).unwrap(),
    Some(leased.clone())
  );

  // The negative half: once the lease is released, gc reclaims the commit. An
  // anchor that could never be released would be a leak, not a lease.
  repo.delete_lease_anchor(&anchor).unwrap();
  gfs_test::git(
    &path,
    &["-c", "gc.reflogExpire=now", "gc", "-q", "--prune=now"],
  )
  .unwrap();
  let repo = Libgit2Repository::open(&path, 2, TREE_CACHE_BYTES).unwrap();
  assert!(
    repo.read_commit(&leased).is_err(),
    "an expired lease must stop protecting its commit"
  );
}

#[test]
fn upstream_fetch_and_prune_cannot_remove_a_lease_anchor() {
  // ADR 0006: `refs/gfs/` is excluded from every upstream fetch and prune
  // refspec, and unrestricted mirror pruning must not be used over internal refs.
  // `--prune --mirror` is precisely the command that would delete an anchor, so
  // the safe form is what gets tested.
  let (_upstream_tmp, upstream) = gfs_test::scratch_clone("basic").unwrap();
  let (_tmp, path) = gfs_test::scratch_clone("basic").unwrap();
  let repo = Libgit2Repository::open(&path, 2, TREE_CACHE_BYTES).unwrap();

  let leased = head(&repo);
  let anchor = gfs_types::revision::lease_anchor_ref("m-prune");
  repo.create_lease_anchor(&anchor, &leased).unwrap();

  // The upstream drops a branch and a tag, so a pruning fetch has real work.
  gfs_test::git(&upstream, &["update-ref", "-d", "refs/heads/feature"]).unwrap();
  gfs_test::git(&upstream, &["tag", "-d", "v2.0"]).unwrap();

  let upstream_url = upstream.to_string_lossy().into_owned();
  gfs_test::git(
    &path,
    &[
      "fetch",
      "--prune",
      "--prune-tags",
      &upstream_url,
      // Explicit refspecs, never `--mirror`: the namespaces GFS mirrors are
      // named, so pruning cannot reach anything else.
      "+refs/heads/*:refs/heads/*",
      "+refs/tags/*:refs/tags/*",
    ],
  )
  .unwrap();

  let repo = Libgit2Repository::open(&path, 2, TREE_CACHE_BYTES).unwrap();
  assert_eq!(
    repo.read_lease_anchor(&anchor).unwrap(),
    Some(leased),
    "a pruning upstream fetch must not remove the lease anchor"
  );
  // And the prune did do its job on the mirrored namespaces, so the test is not
  // passing because the fetch was a no-op.
  let names: Vec<String> = repo
    .visible_refs()
    .unwrap()
    .into_iter()
    .map(|(n, _)| n)
    .collect();
  assert!(
    !names.iter().any(|n| n == "refs/heads/feature"),
    "the prune should have removed the deleted branch: {names:?}"
  );
}

#[test]
fn lease_anchors_are_absent_from_stock_git_advertisements() {
  // ADR 0002 is explicit that hiding prevents *discovery*, not access, so this
  // asserts only the discovery half -- which is what `visible_refs` and the
  // upload-pack hideRefs configuration are for.
  let (_tmp, path) = gfs_test::scratch_clone("basic").unwrap();
  let repo = Libgit2Repository::open(&path, 2, TREE_CACHE_BYTES).unwrap();
  let commit = head(&repo);
  let anchor = gfs_types::revision::lease_anchor_ref("m-adv");
  repo.create_lease_anchor(&anchor, &commit).unwrap();

  // Without the hiding configuration stock Git *does* advertise it, which is why
  // the configuration is load-bearing rather than cosmetic.
  let bare_advertised =
    gfs_test::git(&path, &["ls-remote", "--refs", path.to_str().unwrap()]).unwrap();
  assert!(
    bare_advertised.contains("refs/gfs/"),
    "sanity: an unconfigured upload-pack advertises the anchor, so the test below is meaningful"
  );

  // With the protected configuration M5.3 will apply, it is gone.
  //
  // The config has to be given to the *server* side. Passing `-c` to the client
  // `ls-remote` does nothing, because `hideRefs` is read by the upload-pack
  // process, and for a local transport that is a separate child. Injecting it
  // through `--upload-pack` is also the shape M5.3 requires: the protected
  // configuration is built on the command line, independently of whatever the
  // repository's own config says, so repository config cannot re-enable hidden
  // refs.
  let hidden = gfs_test::git(
    &path,
    &[
      "ls-remote",
      "--refs",
      "--upload-pack",
      "git -c uploadpack.hideRefs=refs/gfs/ upload-pack",
      path.to_str().unwrap(),
    ],
  )
  .unwrap();
  assert!(
    !hidden.contains("refs/gfs/"),
    "hideRefs must remove the anchor from the advertisement: {hidden}"
  );
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

#[test]
fn concurrent_readers_share_the_bounded_pool_without_mixing_results() {
  // The handle model from ADR 0001: handles are never shared, and the bound is
  // admission control. Two handles serving eight threads must still give every
  // thread the correct answer.
  let repo = Arc::new(open("basic"));
  let commit = head(&repo);
  let expected = repo
    .entry(&commit, &BytePath::new("README.md"))
    .unwrap()
    .unwrap()
    .oid;

  let mut handles = Vec::new();
  for _ in 0..8 {
    let repo = Arc::clone(&repo);
    let commit = commit.clone();
    let expected = expected.clone();
    handles.push(std::thread::spawn(move || {
      for _ in 0..25 {
        let e = repo
          .entry(&commit, &BytePath::new("README.md"))
          .unwrap()
          .unwrap();
        assert_eq!(e.oid, expected);
        assert_eq!(repo.read_blob(&e.oid).unwrap(), b"# basic\n");
      }
    }));
  }
  for h in handles {
    h.join().unwrap();
  }
}

#[tokio::test]
async fn the_async_wrapper_bounds_concurrency_and_returns_correct_results() {
  use gfs_git::AsyncRepository;

  let repo = open("basic");
  let algorithm = repo.algorithm();
  let async_repo = AsyncRepository::new(Arc::new(repo), 2);

  let resolved = async_repo
    .resolve(RevisionSelector::parse("main", algorithm).unwrap())
    .await
    .unwrap();

  // Many more concurrent requests than permits: all must succeed, and the
  // semaphore -- not the blocking pool -- is what queues them.
  let mut set = tokio::task::JoinSet::new();
  for _ in 0..32 {
    let r = async_repo.clone();
    let commit = resolved.commit.clone();
    set.spawn(async move {
      let e = r.entry(commit, BytePath::new("README.md")).await.unwrap();
      e.unwrap().oid
    });
  }
  let mut count = 0;
  while let Some(res) = set.join_next().await {
    let oid = res.unwrap();
    assert_eq!(oid.algorithm(), algorithm);
    count += 1;
  }
  assert_eq!(count, 32);
}

// ---------------------------------------------------------------------------
// Ancestry, for `gfs log`
// ---------------------------------------------------------------------------

#[test]
fn a_log_walks_ancestry_newest_first_and_pages_without_gaps() {
  let repo = open("basic");
  let head = head(&repo);

  let (all, has_more) = repo.log(&head, 0, 100).unwrap();
  assert!(!has_more, "the whole history fits in one page of 100");
  assert!(all.len() >= 2, "the basic fixture has at least two commits");
  assert_eq!(
    all[0].commit, head,
    "the walk starts at the commit asked for"
  );
  // Newest first: each commit lists the next as a parent. This holds for the
  // fixture because its history is linear with distinct commit times; the walk
  // is ordered by time, not topologically, so a repository with equal timestamps
  // or clock skew can legitimately interleave branches. See `log`'s comment on
  // why topological order is not used.
  for pair in all.windows(2) {
    assert!(
      pair[0].parents.contains(&pair[1].commit),
      "{} does not descend from {}",
      pair[0].commit,
      pair[1].commit
    );
  }

  // A page short of the history reports that more remains, and `skip` resumes
  // exactly where it stopped — no gap, no repeat.
  let (first, more) = repo.log(&head, 0, 1).unwrap();
  assert_eq!(first.len(), 1);
  assert!(more, "one commit of a multi-commit history leaves more");
  let (second, _) = repo.log(&head, 1, 1).unwrap();
  assert_eq!(second[0].commit, all[1].commit);
}

#[test]
fn a_log_limit_equal_to_the_history_does_not_claim_more() {
  // The off-by-one that a naive implementation gets wrong: a page whose size is
  // exactly the remaining history is complete, not truncated, and reporting
  // otherwise sends a caller into a page that is always empty.
  let repo = open("basic");
  let head = head(&repo);
  let (all, _) = repo.log(&head, 0, 100).unwrap();

  let (exact, has_more) = repo.log(&head, 0, all.len()).unwrap();
  assert_eq!(exact.len(), all.len());
  assert!(
    !has_more,
    "a page that covered the history must not claim more"
  );
}

// ---------------------------------------------------------------------------
// Name walks, for `gfs find`
// ---------------------------------------------------------------------------

#[test]
fn a_path_walk_reports_the_entries_the_searchable_walk_drops() {
  // The distinction `gfs find` depends on. `walk_tree`'s corpus is the
  // searchable one and omits symlinks and gitlinks to agree with `rg`; answering
  // a filename query from it silently loses every symlink in the repository.
  let repo = open("modes");
  let head = head(&repo);

  let mut searchable = Vec::new();
  repo
    .walk_tree(&head, &BytePath::new(""), &mut |e| {
      searchable.push(e.path.as_bytes().to_vec());
      Ok(())
    })
    .unwrap();

  let mut named = Vec::new();
  repo
    .walk_paths(&head, &BytePath::new(""), &mut |path, mode| {
      named.push((path.as_bytes().to_vec(), mode));
      Ok(())
    })
    .unwrap();

  let named_paths: Vec<Vec<u8>> = named.iter().map(|(p, _)| p.clone()).collect();
  for link in [b"rel-link".as_slice(), b"abs-link", b"loop-a"] {
    assert!(
      named_paths.iter().any(|p| p == link),
      "walk_paths lost the symlink {}",
      String::from_utf8_lossy(link)
    );
    assert!(
      !searchable.iter().any(|p| p == link),
      "walk_tree is expected to drop symlinks; this test's premise is gone"
    );
  }
  assert!(
    named_paths.iter().any(|p| p == b"vendor/submodule"),
    "walk_paths lost the gitlink"
  );

  // Directories are recursed into, never emitted: the set is `git ls-files`'s.
  assert!(
    named.iter().all(|(_, mode)| *mode != mode::DIRECTORY),
    "a directory was emitted as a named entry"
  );
  // And it is a superset of the searchable corpus, not a different one.
  for path in &searchable {
    assert!(
      named_paths.contains(path),
      "walk_paths is missing a searchable file: {}",
      String::from_utf8_lossy(path)
    );
  }
}

#[test]
fn a_path_walk_of_a_missing_directory_is_an_error_not_an_empty_result() {
  // Same rule `walk_tree` states: a mistyped scope that returned nothing would
  // be indistinguishable from a directory that is genuinely empty.
  let repo = open("basic");
  let head = head(&repo);
  let err = repo
    .walk_paths(&head, &BytePath::new("no/such/dir"), &mut |_, _| Ok(()))
    .unwrap_err();
  assert_eq!(err.code, gfs_types::error::ErrorCode::NotFound);
}
