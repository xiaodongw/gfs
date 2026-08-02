//! API compatibility golden tests (PLAN.md M1.4).
//!
//! ADR 0006's versioning policy for gRPC is: additive-only within a major
//! version, new fields optional with safe defaults, **no field renumbering**, no
//! enum value repurposing, and a removed field reserved rather than reused.
//!
//! Prose cannot enforce that. A renumbered field still compiles, still passes
//! every round-trip test in `convert.rs` -- because both sides of a round trip use
//! the same new numbering -- and breaks only against a peer built from the
//! previous schema. The failure appears in production, during a rolling upgrade,
//! as a field that silently reads as its default.
//!
//! So the schema is pinned two ways:
//!
//! * a **field-number table** below, checked against the compiled descriptor. Any
//!   renumber, retype, or removal fails here with the field named;
//! * **wire-encoding goldens** for representative messages, which catch the same
//!   class of change from the other direction and also pin the on-wire layout a
//!   client from a previous release would parse.
//!
//! When this test fails, the question to ask is not "how do I update the golden"
//! but "is this change additive?". If it is, add the new field to the table. If it
//! is not, it needs a `/v2` and a deprecation window.

use gfs_proto::v1;
use prost::Message;

/// One expected field: number, name, and Protobuf type name.
type Field = (i32, &'static str, &'static str);

/// The pinned schema.
///
/// Deliberately verbose rather than generated from the descriptor: a table
/// derived from the thing it checks would agree with any change, which is the one
/// property it must not have.
const PINNED: &[(&str, &[Field])] = &[
  ("Timestamp", &[(1, "secs", "int64"), (2, "nanos", "uint32")]),
  (
    "SnapshotAuthorization",
    &[(1, "mount_capability", "string")],
  ),
  (
    "ErrorDetail",
    &[(1, "code", "string"), (2, "message", "string")],
  ),
  (
    "TreeEntry",
    &[
      // `path` is `bytes`, not `string`. A change to `string` would make
      // non-UTF-8 Git paths unrepresentable, and Protobuf runtimes would reject
      // or corrupt them below the application layer.
      (1, "path", "bytes"),
      (2, "kind", ".gfs.v1.EntryKind"),
      (3, "mode", "uint32"),
      (4, "oid", "string"),
      (5, "size", "uint64"),
      (6, "symlink_target", "bytes"),
      (7, "blob_ticket", "string"),
    ],
  ),
  (
    "ResolveRevisionRequest",
    &[
      (1, "repository_id", "string"),
      (2, "revision_selector", "string"),
    ],
  ),
  (
    "ResolveRevisionResponse",
    &[
      (1, "commit_oid", "string"),
      (2, "tree_oid", "string"),
      (3, "ref_name", "string"),
      (4, "ref_version", "uint64"),
      (5, "snapshot_time", ".gfs.v1.Timestamp"),
    ],
  ),
  ("ListRefsRequest", &[(1, "repository_id", "string")]),
  (
    "Ref",
    &[
      (1, "name", "string"),
      (2, "target_oid", "string"),
      (3, "peeled_oid", "string"),
    ],
  ),
  ("ListRefsResponse", &[(1, "refs", ".gfs.v1.Ref")]),
  (
    "Signature",
    &[
      // Bytes for the same reason as `TreeEntry.path`: Git does not constrain
      // author names to UTF-8.
      (1, "name", "bytes"),
      (2, "email", "bytes"),
      (3, "time", ".gfs.v1.Timestamp"),
      (4, "tz_offset_minutes", "int32"),
    ],
  ),
  (
    "GetCommitRequest",
    &[
      (1, "repository_id", "string"),
      (2, "commit_oid", "string"),
      (3, "authorization", ".gfs.v1.SnapshotAuthorization"),
    ],
  ),
  (
    "GetCommitResponse",
    &[
      (1, "commit_oid", "string"),
      (2, "tree_oid", "string"),
      (3, "parent_oids", "string"),
      (4, "author", ".gfs.v1.Signature"),
      (5, "committer", ".gfs.v1.Signature"),
      (6, "message", "bytes"),
      (7, "snapshot_time", ".gfs.v1.Timestamp"),
    ],
  ),
  (
    "GetEntryRequest",
    &[
      (1, "repository_id", "string"),
      (2, "commit_oid", "string"),
      (3, "path", "bytes"),
      (4, "authorization", ".gfs.v1.SnapshotAuthorization"),
      (5, "want_blob_ticket", "bool"),
    ],
  ),
  (
    "GetEntryResponse",
    &[
      (1, "entry", ".gfs.v1.TreeEntry"),
      (2, "commit_oid", "string"),
    ],
  ),
  (
    "ListDirectoryRequest",
    &[
      (1, "repository_id", "string"),
      (2, "commit_oid", "string"),
      (3, "path", "bytes"),
      // Bytes because the token *is* the Git tree sort key, which appends `/`
      // for directories and is not guaranteed UTF-8.
      (4, "page_token", "bytes"),
      (5, "page_size", "uint32"),
      (6, "authorization", ".gfs.v1.SnapshotAuthorization"),
      (7, "want_blob_tickets", "bool"),
    ],
  ),
  (
    "ListDirectoryResponse",
    &[
      (1, "entries", ".gfs.v1.TreeEntry"),
      (2, "next_page_token", "bytes"),
      (3, "commit_oid", "string"),
    ],
  ),
  (
    "BatchGetEntryRequest",
    &[
      (1, "repository_id", "string"),
      (2, "commit_oid", "string"),
      (3, "paths", "bytes"),
      (4, "authorization", ".gfs.v1.SnapshotAuthorization"),
      (5, "want_blob_tickets", "bool"),
    ],
  ),
  (
    "EntryResult",
    &[
      (1, "entry", ".gfs.v1.TreeEntry"),
      (2, "error", ".gfs.v1.ErrorDetail"),
    ],
  ),
  (
    "BatchGetEntryResponse",
    &[
      (1, "results", ".gfs.v1.EntryResult"),
      (2, "commit_oid", "string"),
    ],
  ),
  (
    "LogRequest",
    &[
      (1, "repository_id", "string"),
      (2, "commit_oid", "string"),
      (3, "authorization", ".gfs.v1.SnapshotAuthorization"),
      (4, "skip", "uint32"),
      (5, "limit", "uint32"),
      (6, "first_parent", "bool"),
      (7, "paths", "bytes"),
    ],
  ),
  (
    "LogCommit",
    &[
      (1, "commit_oid", "string"),
      (2, "parent_oids", "string"),
      (3, "author", ".gfs.v1.Signature"),
      (4, "committer", ".gfs.v1.Signature"),
      (5, "message", "bytes"),
      (6, "tree_oid", "string"),
    ],
  ),
  (
    "DiffCommitsRequest",
    &[
      (1, "repository_id", "string"),
      (2, "base_commit_oid", "string"),
      (3, "commit_oid", "string"),
      (4, "authorization", ".gfs.v1.SnapshotAuthorization"),
      (5, "paths", "bytes"),
      (6, "format", ".gfs.v1.DiffFormat"),
      (7, "context_lines", "uint32"),
      (8, "max_bytes", "uint64"),
      (9, "zero_context", "bool"),
    ],
  ),
  (
    "DiffFileChange",
    &[
      (1, "path", "bytes"),
      (2, "old_path", "bytes"),
      (3, "status", ".gfs.v1.ChangeStatus"),
      (4, "additions", "uint32"),
      (5, "deletions", "uint32"),
      (6, "binary", "bool"),
      (7, "old_mode", "uint32"),
      (8, "new_mode", "uint32"),
    ],
  ),
  (
    "DiffCommitsResponse",
    &[
      (1, "rendered", "bytes"),
      (2, "files", ".gfs.v1.DiffFileChange"),
      (3, "truncated", "bool"),
      (4, "base_commit_oid", "string"),
      (5, "commit_oid", "string"),
    ],
  ),
  (
    "BlameRequest",
    &[
      (1, "repository_id", "string"),
      (2, "commit_oid", "string"),
      (3, "path", "bytes"),
      (4, "authorization", ".gfs.v1.SnapshotAuthorization"),
    ],
  ),
  (
    "BlameHunk",
    &[
      (1, "final_start_line", "uint32"),
      (2, "lines", "uint32"),
      (3, "commit_oid", "string"),
      (4, "orig_path", "bytes"),
      (5, "orig_start_line", "uint32"),
      (6, "author", ".gfs.v1.Signature"),
      (7, "boundary", "bool"),
    ],
  ),
  (
    "BlameResponse",
    &[
      (1, "hunks", ".gfs.v1.BlameHunk"),
      (2, "content", "bytes"),
      (3, "truncated", "bool"),
      (4, "commit_oid", "string"),
    ],
  ),
  (
    "LogResponse",
    &[(1, "commits", ".gfs.v1.LogCommit"), (2, "has_more", "bool")],
  ),
  (
    "FoundPath",
    &[
      (1, "path", "bytes"),
      (2, "kind", ".gfs.v1.EntryKind"),
      (3, "mode", "uint32"),
    ],
  ),
  (
    "PrepareSnapshotRequest",
    &[
      (1, "repository_id", "string"),
      (2, "commit_oid", "string"),
      (3, "authorization", ".gfs.v1.SnapshotAuthorization"),
    ],
  ),
  (
    "PrepareSnapshotResponse",
    &[
      (1, "state", ".gfs.v1.SnapshotState"),
      (2, "operation_id", "string"),
      (3, "failure_reason", "string"),
      (4, "commit_oid", "string"),
    ],
  ),
  (
    "CreateMountRequest",
    &[
      (1, "repository_id", "string"),
      (2, "revision_selector", "string"),
      (3, "requested_ttl_seconds", "uint64"),
    ],
  ),
  (
    "CreateMountResponse",
    &[
      (1, "mount_id", "string"),
      (2, "commit_oid", "string"),
      (3, "tree_oid", "string"),
      (4, "ref_name", "string"),
      (5, "snapshot_time", ".gfs.v1.Timestamp"),
      (6, "mount_capability", "string"),
      (7, "lease_expiry", ".gfs.v1.Timestamp"),
      (8, "heartbeat_interval_seconds", "uint64"),
      (9, "work_ref_root", "string"),
    ],
  ),
  (
    "RenewMountRequest",
    &[(1, "mount_id", "string"), (2, "mount_capability", "string")],
  ),
  (
    "RenewMountResponse",
    &[
      (1, "mount_capability", "string"),
      (2, "lease_expiry", ".gfs.v1.Timestamp"),
    ],
  ),
  (
    "ReleaseMountRequest",
    &[(1, "mount_id", "string"), (2, "mount_capability", "string")],
  ),
  ("ReleaseMountResponse", &[]),
  // --- Search (M4.3) ---------------------------------------------------------
  (
    "SearchRequest",
    &[
      (1, "repository_id", "string"),
      (2, "commit_oid", "string"),
      (3, "authorization", ".gfs.v1.SnapshotAuthorization"),
      (4, "pattern", "string"),
      (5, "literal", "bool"),
      (6, "case_insensitive", "bool"),
      // `bytes`, for the same reason as `TreeEntry.path`: a path scope need not
      // be UTF-8.
      (7, "scope", "bytes"),
      (8, "include_globs", "string"),
      (9, "exclude_globs", "string"),
      (10, "context_before", "uint32"),
      (11, "context_after", "uint32"),
      (12, "start_after_path", "bytes"),
      (13, "max_results", "uint32"),
      (14, "max_time_ms", "uint64"),
      (15, "max_bytes_read", "uint64"),
      (16, "max_candidates", "uint64"),
      (17, "max_line_bytes", "uint32"),
      (18, "max_display_bytes", "uint64"),
    ],
  ),
  (
    "SearchResponse",
    &[
      (1, "match", ".gfs.v1.SearchMatch"),
      (2, "completion", ".gfs.v1.SearchCompletion"),
    ],
  ),
  (
    "SearchMatch",
    &[
      (1, "path", "bytes"),
      (2, "line", "uint64"),
      (3, "column", "uint64"),
      (4, "matched", "bytes"),
      (5, "line_text", "bytes"),
      (6, "before", "bytes"),
      (7, "after", "bytes"),
      (8, "blob_oid", "string"),
      (9, "line_truncated", "bool"),
    ],
  ),
  (
    "Coverage",
    &[
      (1, "scope", "bytes"),
      (2, "eligible_paths", "uint64"),
      (3, "excluded", ".gfs.v1.Coverage.ExcludedEntry"),
      (4, "declared_exclusions", "string"),
    ],
  ),
  (
    "SearchCompletion",
    &[
      // The two dimensions ADR 0004 froze. `execution_status` is whether the
      // query finished; `coverage` is what was outside the corpus. Collapsing
      // them into one field is the change this pin exists to catch, because an
      // agent that cannot tell them apart concludes a symbol does not exist.
      (1, "execution_status", ".gfs.v1.ExecutionStatus"),
      (2, "truncation_reason", "string"),
      (3, "stop_budget", "string"),
      (4, "coverage", ".gfs.v1.Coverage"),
      (5, "index_generation", "uint64"),
      (6, "commit_oid", "string"),
      (7, "candidates_considered", "uint64"),
      (8, "bytes_read", "uint64"),
      (9, "elapsed_ms", "uint64"),
    ],
  ),
  // RepositoryService: the write half. Added whole, so nothing here can have
  // been renumbered yet -- but pinning it now is what makes that true later.
  (
    "CloneRepositoryRequest",
    &[(1, "upstream_url", "string"), (2, "credential", "string")],
  ),
  (
    "CloneRepositoryResponse",
    &[
      (1, "repository_id", "string"),
      (2, "created", "bool"),
      (3, "default_branch", "string"),
      (4, "directory", "string"),
      (5, "summary", "string"),
    ],
  ),
  (
    "CreateBranchRequest",
    &[
      (1, "repository_id", "string"),
      (2, "branch", "string"),
      (3, "start_point", "string"),
      (4, "authorization", ".gfs.v1.SnapshotAuthorization"),
    ],
  ),
  (
    "CreateBranchResponse",
    &[(1, "ref_name", "string"), (2, "commit_oid", "string")],
  ),
  (
    "FileChange",
    &[
      (1, "path", "bytes"),
      (2, "kind", ".gfs.v1.ChangeKind"),
      (3, "mode", "uint32"),
      (4, "content", "bytes"),
    ],
  ),
  (
    "CommitChangesRequest",
    &[
      (1, "repository_id", "string"),
      (2, "base_commit_oid", "string"),
      (3, "branch", "string"),
      (4, "message", "string"),
      (5, "author_name", "string"),
      (6, "author_email", "string"),
      (7, "changes", ".gfs.v1.FileChange"),
      (8, "authorization", ".gfs.v1.SnapshotAuthorization"),
      (9, "deleted_directories", "bytes"),
    ],
  ),
  (
    "CommitChangesResponse",
    &[
      (1, "commit_oid", "string"),
      (2, "tree_oid", "string"),
      (3, "ref_name", "string"),
    ],
  ),
  (
    "PushBranchRequest",
    &[
      (1, "repository_id", "string"),
      (2, "branch", "string"),
      (3, "remote_branch", "string"),
      (4, "credential", "string"),
      (5, "force", "bool"),
      (6, "authorization", ".gfs.v1.SnapshotAuthorization"),
    ],
  ),
  (
    "PushBranchResponse",
    &[(1, "summary", "string"), (2, "remote_ref", "string")],
  ),
];

/// The pinned enum values. Repurposing one is the change this catches.
const PINNED_ENUMS: &[(&str, &[(i32, &str)])] = &[
  (
    "EntryKind",
    &[
      (0, "ENTRY_KIND_UNSPECIFIED"),
      (1, "ENTRY_KIND_REGULAR"),
      (2, "ENTRY_KIND_EXECUTABLE"),
      (3, "ENTRY_KIND_SYMLINK"),
      (4, "ENTRY_KIND_DIRECTORY"),
      (5, "ENTRY_KIND_GITLINK"),
      (6, "ENTRY_KIND_UNSUPPORTED"),
    ],
  ),
  (
    // DESIGN.md section 7.3: exactly three states plus the unspecified zero.
    // `NOT_INDEXABLE` and `RESOURCE_LIMIT` are request errors and must never
    // appear here -- folding an error into a state is how a caller ends up
    // treating "this will never work" as "try again shortly".
    "SnapshotState",
    &[
      (0, "SNAPSHOT_STATE_UNSPECIFIED"),
      (1, "SNAPSHOT_STATE_READY"),
      (2, "SNAPSHOT_STATE_BUILDING"),
      (3, "SNAPSHOT_STATE_FAILED"),
    ],
  ),
  (
    // Execution status is one of the two independent dimensions of the
    // completion contract. It must never gain a value that folds coverage into
    // it -- "complete but some files were skipped" is a coverage fact, not an
    // execution one, and merging them is what makes an empty answer ambiguous.
    "ExecutionStatus",
    &[
      (0, "EXECUTION_STATUS_UNSPECIFIED"),
      (1, "EXECUTION_STATUS_COMPLETE"),
      (2, "EXECUTION_STATUS_TRUNCATED"),
    ],
  ),
  (
    // Mirrors `gfs_overlay::ChangeKind`. Renumbering one of these would make a
    // client's "deleted" arrive as the server's "modified", which is a data-loss
    // bug rather than a compatibility inconvenience.
    "ChangeKind",
    &[
      (0, "CHANGE_KIND_UNSPECIFIED"),
      (1, "CHANGE_KIND_ADDED"),
      (2, "CHANGE_KIND_MODIFIED"),
      (3, "CHANGE_KIND_DELETED"),
    ],
  ),
  (
    // A *rendering* choice, so unlike `SnapshotState` the unspecified zero has a
    // safe meaning -- the fullest output -- and must keep it. Renumbering would
    // turn one client's request for a patch into another's request for a list of
    // names, which reads as a mysteriously empty diff rather than as an error.
    "DiffFormat",
    &[
      (0, "DIFF_FORMAT_UNSPECIFIED"),
      (1, "DIFF_FORMAT_PATCH"),
      (2, "DIFF_FORMAT_STAT"),
      (3, "DIFF_FORMAT_NAME_STATUS"),
      (4, "DIFF_FORMAT_NAME_ONLY"),
    ],
  ),
  (
    // Deliberately *not* `ChangeKind`, and not renumbered to line up with it.
    // The two answer different questions -- what a workspace changed against its
    // base, and what one commit did against another -- and this one has Git's
    // five statuses because a reader needs them.
    "ChangeStatus",
    &[
      (0, "CHANGE_STATUS_UNSPECIFIED"),
      (1, "CHANGE_STATUS_ADDED"),
      (2, "CHANGE_STATUS_MODIFIED"),
      (3, "CHANGE_STATUS_DELETED"),
      (4, "CHANGE_STATUS_RENAMED"),
      (5, "CHANGE_STATUS_TYPE_CHANGED"),
    ],
  ),
];

fn descriptors() -> prost_types::FileDescriptorSet {
  prost_types::FileDescriptorSet::decode(v1::FILE_DESCRIPTOR_SET)
    .expect("the build script's descriptor set must decode")
}

#[test]
fn field_numbers_and_types_match_the_pinned_schema() {
  let set = descriptors();
  let mut seen = std::collections::BTreeMap::new();
  for file in &set.file {
    for msg in &file.message_type {
      seen.insert(msg.name().to_owned(), msg.clone());
    }
  }

  for (msg_name, expected) in PINNED {
    let msg = seen
      .get(*msg_name)
      .unwrap_or_else(|| panic!("message `{msg_name}` was removed from the schema"));

    let actual: Vec<(i32, String, String)> = msg
      .field
      .iter()
      .map(|f| {
        // For a message or enum field the type name is authoritative; for a
        // scalar it is empty and the type enum carries the information.
        let type_name = if f.type_name().is_empty() {
          scalar_type_name(f.r#type()).to_owned()
        } else {
          f.type_name().to_owned()
        };
        (f.number(), f.name().to_owned(), type_name)
      })
      .collect();

    let expected: Vec<(i32, String, String)> = expected
      .iter()
      .map(|(n, name, ty)| (*n, (*name).to_owned(), (*ty).to_owned()))
      .collect();

    assert_eq!(
      actual, expected,
      "\n`{msg_name}` no longer matches the pinned schema.\n\
       ADR 0006 allows only additive changes within v1: appending a new optional \n\
       field is fine (add it to PINNED), but renumbering, retyping, or removing \n\
       one breaks every peer built from the previous schema and needs a /v2.\n"
    );
  }

  // A message added to the schema but not to the table would go unchecked, so
  // the table has to be exhaustive rather than a subset.
  let pinned_names: std::collections::BTreeSet<&str> = PINNED.iter().map(|(n, _)| *n).collect();
  let missing: Vec<&String> = seen
    .keys()
    .filter(|n| !pinned_names.contains(n.as_str()))
    .collect();
  assert!(
    missing.is_empty(),
    "these messages exist in the schema but are not pinned in PINNED: {missing:?}"
  );
}

#[test]
fn enum_values_are_not_repurposed() {
  let set = descriptors();
  let mut seen = std::collections::BTreeMap::new();
  for file in &set.file {
    for e in &file.enum_type {
      seen.insert(e.name().to_owned(), e.clone());
    }
  }

  for (enum_name, expected) in PINNED_ENUMS {
    let e = seen
      .get(*enum_name)
      .unwrap_or_else(|| panic!("enum `{enum_name}` was removed from the schema"));
    let actual: Vec<(i32, String)> = e
      .value
      .iter()
      .map(|v| (v.number(), v.name().to_owned()))
      .collect();
    let expected: Vec<(i32, String)> = expected
      .iter()
      .map(|(n, name)| (*n, (*name).to_owned()))
      .collect();
    assert_eq!(
      actual, expected,
      "\n`{enum_name}` no longer matches the pinned values. Reusing a number for \n\
       a different meaning silently changes what an existing peer understands.\n"
    );
  }
}

#[test]
fn service_methods_are_stable() {
  let set = descriptors();
  let mut methods = Vec::new();
  for file in &set.file {
    for svc in &file.service {
      for m in &svc.method {
        methods.push(format!(
          "{}/{}({}) -> {}",
          svc.name(),
          m.name(),
          m.input_type(),
          m.output_type()
        ));
      }
    }
  }
  methods.sort();

  // Removing or renaming a method breaks a deployed client outright; the gRPC
  // path is built from the service and method names.
  let expected = [
    "RepositoryService/CloneRepository(.gfs.v1.CloneRepositoryRequest) -> .gfs.v1.CloneRepositoryResponse",
    "RepositoryService/CommitChanges(.gfs.v1.CommitChangesRequest) -> .gfs.v1.CommitChangesResponse",
    "RepositoryService/CreateBranch(.gfs.v1.CreateBranchRequest) -> .gfs.v1.CreateBranchResponse",
    "RepositoryService/PushBranch(.gfs.v1.PushBranchRequest) -> .gfs.v1.PushBranchResponse",
    "SearchService/Search(.gfs.v1.SearchRequest) -> .gfs.v1.SearchResponse",
    "SnapshotService/BatchGetEntry(.gfs.v1.BatchGetEntryRequest) -> .gfs.v1.BatchGetEntryResponse",
    "SnapshotService/Blame(.gfs.v1.BlameRequest) -> .gfs.v1.BlameResponse",
    "SnapshotService/CreateMount(.gfs.v1.CreateMountRequest) -> .gfs.v1.CreateMountResponse",
    "SnapshotService/DiffCommits(.gfs.v1.DiffCommitsRequest) -> .gfs.v1.DiffCommitsResponse",
    "SnapshotService/GetCommit(.gfs.v1.GetCommitRequest) -> .gfs.v1.GetCommitResponse",
    "SnapshotService/GetEntry(.gfs.v1.GetEntryRequest) -> .gfs.v1.GetEntryResponse",
    "SnapshotService/ListDirectory(.gfs.v1.ListDirectoryRequest) -> .gfs.v1.ListDirectoryResponse",
    "SnapshotService/ListRefs(.gfs.v1.ListRefsRequest) -> .gfs.v1.ListRefsResponse",
    "SnapshotService/Log(.gfs.v1.LogRequest) -> .gfs.v1.LogResponse",
    "SnapshotService/PrepareSnapshot(.gfs.v1.PrepareSnapshotRequest) -> .gfs.v1.PrepareSnapshotResponse",
    "SnapshotService/ReleaseMount(.gfs.v1.ReleaseMountRequest) -> .gfs.v1.ReleaseMountResponse",
    "SnapshotService/RenewMount(.gfs.v1.RenewMountRequest) -> .gfs.v1.RenewMountResponse",
    "SnapshotService/ResolveRevision(.gfs.v1.ResolveRevisionRequest) -> .gfs.v1.ResolveRevisionResponse",
  ];
  assert_eq!(methods, expected);
}

#[test]
fn wire_encoding_of_a_populated_entry_is_stable() {
  // The encoding golden. Pins the on-wire layout a peer from a previous release
  // would parse, from the opposite direction to the descriptor check.
  let entry = v1::TreeEntry {
    path: b"a/\xffb".to_vec(),
    kind: v1::EntryKind::Symlink as i32,
    mode: 0o120000,
    oid: "sha1:0123456789abcdef0123456789abcdef01234567".to_owned(),
    size: 7,
    symlink_target: Some(b"../t".to_vec()),
    blob_ticket: Some("tkt".to_owned()),
  };
  let hex = hex_of(&entry.encode_to_vec());
  assert_eq!(
    hex,
    concat!(
      "0a04612fff62", // 1: path = "a/\xffb"
      "1003",         // 2: kind = 3 (SYMLINK)
      "1880c002",     // 3: mode = 0o120000 = 40960
      "222d",         // 4: oid, 45 bytes
      "736861313a30313233343536373839616263646566",
      "30313233343536373839616263646566",
      "3031323334353637",
      "2807",         // 5: size = 7
      "32042e2e2f74", // 6: symlink_target = "../t"
      "3a03746b74",   // 7: blob_ticket = "tkt"
    ),
    "TreeEntry wire layout changed; a peer built from the previous schema would \
     misparse this message"
  );

  // And it round-trips back to the same value.
  assert_eq!(
    v1::TreeEntry::decode(entry.encode_to_vec().as_slice()).unwrap(),
    entry
  );
}

#[test]
fn absent_optional_fields_encode_to_nothing() {
  // Explicit presence matters here: `symlink_target: None` must not encode as an
  // empty byte string, or a client cannot distinguish "not a symlink" from "a
  // symlink whose target is empty".
  let entry = v1::TreeEntry {
    path: b"f".to_vec(),
    kind: v1::EntryKind::Regular as i32,
    mode: 0o100644,
    oid: String::new(),
    size: 0,
    symlink_target: None,
    blob_ticket: None,
  };
  let encoded = entry.encode_to_vec();
  // Field 6 has tag byte 0x32 and field 7 has 0x3a; neither may appear.
  assert!(
    !encoded.contains(&0x32u8),
    "absent symlink_target was encoded"
  );
  assert!(!encoded.contains(&0x3au8), "absent blob_ticket was encoded");

  let with_empty = v1::TreeEntry {
    symlink_target: Some(Vec::new()),
    ..entry.clone()
  };
  assert_ne!(
    with_empty.encode_to_vec(),
    encoded,
    "an empty symlink target must be distinguishable from an absent one"
  );
}

fn scalar_type_name(t: prost_types::field_descriptor_proto::Type) -> &'static str {
  use prost_types::field_descriptor_proto::Type;
  match t {
    Type::Double => "double",
    Type::Float => "float",
    Type::Int64 => "int64",
    Type::Uint64 => "uint64",
    Type::Int32 => "int32",
    Type::Fixed64 => "fixed64",
    Type::Fixed32 => "fixed32",
    Type::Bool => "bool",
    Type::String => "string",
    Type::Group => "group",
    Type::Message => "message",
    Type::Bytes => "bytes",
    Type::Uint32 => "uint32",
    Type::Enum => "enum",
    Type::Sfixed32 => "sfixed32",
    Type::Sfixed64 => "sfixed64",
    Type::Sint32 => "sint32",
    Type::Sint64 => "sint64",
  }
}

fn hex_of(bytes: &[u8]) -> String {
  bytes.iter().map(|b| format!("{b:02x}")).collect()
}
