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

use prost::Message;
use xvfs_proto::v1;

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
      (2, "kind", ".xvfs.v1.EntryKind"),
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
      (5, "snapshot_time", ".xvfs.v1.Timestamp"),
    ],
  ),
  (
    "Signature",
    &[
      // Bytes for the same reason as `TreeEntry.path`: Git does not constrain
      // author names to UTF-8.
      (1, "name", "bytes"),
      (2, "email", "bytes"),
      (3, "time", ".xvfs.v1.Timestamp"),
      (4, "tz_offset_minutes", "int32"),
    ],
  ),
  (
    "GetCommitRequest",
    &[
      (1, "repository_id", "string"),
      (2, "commit_oid", "string"),
      (3, "authorization", ".xvfs.v1.SnapshotAuthorization"),
    ],
  ),
  (
    "GetCommitResponse",
    &[
      (1, "commit_oid", "string"),
      (2, "tree_oid", "string"),
      (3, "parent_oids", "string"),
      (4, "author", ".xvfs.v1.Signature"),
      (5, "committer", ".xvfs.v1.Signature"),
      (6, "message", "bytes"),
      (7, "snapshot_time", ".xvfs.v1.Timestamp"),
    ],
  ),
  (
    "GetEntryRequest",
    &[
      (1, "repository_id", "string"),
      (2, "commit_oid", "string"),
      (3, "path", "bytes"),
      (4, "authorization", ".xvfs.v1.SnapshotAuthorization"),
      (5, "want_blob_ticket", "bool"),
    ],
  ),
  (
    "GetEntryResponse",
    &[
      (1, "entry", ".xvfs.v1.TreeEntry"),
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
      (6, "authorization", ".xvfs.v1.SnapshotAuthorization"),
      (7, "want_blob_tickets", "bool"),
    ],
  ),
  (
    "ListDirectoryResponse",
    &[
      (1, "entries", ".xvfs.v1.TreeEntry"),
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
      (4, "authorization", ".xvfs.v1.SnapshotAuthorization"),
      (5, "want_blob_tickets", "bool"),
    ],
  ),
  (
    "EntryResult",
    &[
      (1, "entry", ".xvfs.v1.TreeEntry"),
      (2, "error", ".xvfs.v1.ErrorDetail"),
    ],
  ),
  (
    "BatchGetEntryResponse",
    &[
      (1, "results", ".xvfs.v1.EntryResult"),
      (2, "commit_oid", "string"),
    ],
  ),
  (
    "PrepareSnapshotRequest",
    &[
      (1, "repository_id", "string"),
      (2, "commit_oid", "string"),
      (3, "authorization", ".xvfs.v1.SnapshotAuthorization"),
    ],
  ),
  (
    "PrepareSnapshotResponse",
    &[
      (1, "state", ".xvfs.v1.SnapshotState"),
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
      (5, "snapshot_time", ".xvfs.v1.Timestamp"),
      (6, "mount_capability", "string"),
      (7, "lease_expiry", ".xvfs.v1.Timestamp"),
      (8, "heartbeat_interval_seconds", "uint64"),
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
      (2, "lease_expiry", ".xvfs.v1.Timestamp"),
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
      (3, "authorization", ".xvfs.v1.SnapshotAuthorization"),
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
    ],
  ),
  (
    "SearchResponse",
    &[
      (1, "match", ".xvfs.v1.SearchMatch"),
      (2, "completion", ".xvfs.v1.SearchCompletion"),
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
    ],
  ),
  (
    "Coverage",
    &[
      (1, "scope", "bytes"),
      (2, "eligible_paths", "uint64"),
      (3, "excluded", ".xvfs.v1.Coverage.ExcludedEntry"),
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
      (1, "execution_status", ".xvfs.v1.ExecutionStatus"),
      (2, "truncation_reason", "string"),
      (3, "stop_budget", "string"),
      (4, "coverage", ".xvfs.v1.Coverage"),
      (5, "index_generation", "uint64"),
      (6, "commit_oid", "string"),
      (7, "candidates_considered", "uint64"),
      (8, "bytes_read", "uint64"),
      (9, "elapsed_ms", "uint64"),
    ],
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
    "SearchService/Search(.xvfs.v1.SearchRequest) -> .xvfs.v1.SearchResponse",
    "SnapshotService/BatchGetEntry(.xvfs.v1.BatchGetEntryRequest) -> .xvfs.v1.BatchGetEntryResponse",
    "SnapshotService/CreateMount(.xvfs.v1.CreateMountRequest) -> .xvfs.v1.CreateMountResponse",
    "SnapshotService/GetCommit(.xvfs.v1.GetCommitRequest) -> .xvfs.v1.GetCommitResponse",
    "SnapshotService/GetEntry(.xvfs.v1.GetEntryRequest) -> .xvfs.v1.GetEntryResponse",
    "SnapshotService/ListDirectory(.xvfs.v1.ListDirectoryRequest) -> .xvfs.v1.ListDirectoryResponse",
    "SnapshotService/PrepareSnapshot(.xvfs.v1.PrepareSnapshotRequest) -> .xvfs.v1.PrepareSnapshotResponse",
    "SnapshotService/ReleaseMount(.xvfs.v1.ReleaseMountRequest) -> .xvfs.v1.ReleaseMountResponse",
    "SnapshotService/RenewMount(.xvfs.v1.RenewMountRequest) -> .xvfs.v1.RenewMountResponse",
    "SnapshotService/ResolveRevision(.xvfs.v1.ResolveRevisionRequest) -> .xvfs.v1.ResolveRevisionResponse",
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
