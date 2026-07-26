# M1 — Repository and snapshot API: completion report

Date: 2026-07-26
Milestone: M1 (PLAN.md section 4)
Status: **Complete.** All five exit criteria met, with two recorded scope reductions.

## The exit gate

PLAN.md section 4 states five criteria. Each is listed with what verifies it, so the
claim can be checked rather than taken.

| # | Criterion | Verified by | Result |
| --- | --- | --- | --- |
| 1 | Resolve a revision, list a million-entry snapshot one directory at a time, fetch one file without cloning | `xvfs-server/tests/exit_criteria.rs::a_million_entry_snapshot_can_be_paged_one_directory_at_a_time` | **Met, measured** |
| 2 | Concurrent ref movement cannot produce mixed-commit responses | `exit_criteria.rs::concurrent_ref_movement_cannot_produce_a_mixed_commit_response`, `::a_mount_is_unaffected_by_ref_movement_after_it_was_created` | Met |
| 3 | A leased commit survives force push, branch deletion, and full `gc`; an expired lease stops protecting it; resolution and lease creation have no race window; a renewed live lease survives past its original TTL | `xvfs-git/tests/repository.rs::a_leased_commit_survives_a_force_push_a_branch_deletion_and_a_full_gc`, `xvfs-server/tests/mounts.rs` (16 tests), `catalog::leases` unit tests | Met |
| 4 | Lease refs absent from all Git advertisements and survive upstream fetch/prune | `exit_criteria.rs::lease_refs_are_absent_from_advertisements_and_survive_a_pruning_fetch`, `mirror::tests` | Met |
| 5 | Unauthorized users cannot infer blob existence through status, timing within a defined tolerance, cache, or error differences | `xvfs-server/tests/authorization.rs` (14 tests), `exit_criteria.rs::a_blob_ticket_cannot_be_used_to_probe_for_blobs_in_another_snapshot` | Met |

### Criterion 1, measured

Machine: the M0.1 profile (WSL2, 32 logical CPUs, 46 GiB RAM).

| Step | Result |
| --- | --- |
| Fixture construction (1000 × 1000 = 1,000,002 entries) | 3.0 s |
| Snapshot size on disk | one blob, 1001 trees |
| Paging the root plus all 1000 directories | 13.2 s |
| Entries returned | 1,000,002 — exactly the expected count, no duplicates |
| Individual file read after paging | 2 bytes |
| Client-side repository | none |

The fixture uses one shared blob for every file. That is deliberate and does not
weaken the criterion: the criterion is about *snapshot* scale — tree entry count and
directory paging — which is unaffected by whether the blobs differ, and a million
distinct blobs would dominate build time and disk for no additional coverage.

The paging test also asserts that both halves of the `pager.h` / `pager/`
sort-key-boundary pair survive, at a scale where a page boundary is guaranteed to
fall somewhere awkward. That is the failure ADR 0005 measured in the M0 spike, where
naive name-based pagination returned 1597 of 1598 entries.

### Criterion 5, and how "timing" is verified

The status and error halves are exact: a denial and an absence produce a
byte-identical `NOT_FOUND` with the same HTTP and gRPC codes, and a quarantined
repository is indistinguishable from one that never existed.

The timing half is verified **structurally rather than by measuring a clock**. The
repository policy is consulted before anything about the repository is looked up, so
an unauthorized caller's cost cannot depend on the repository's existence. The test
points a catalog row at a nonexistent path — so any attempt to open or stat the
repository would produce a different error — and asserts the ordinary masked
`NOT_FOUND`, which proves nothing was touched. `RepositoryPolicy` documents that an
implementation must not query the catalog, for the same reason.

A wall-clock comparison was considered and rejected: on a shared runner it would be
flaky at the tolerances that matter, and it would pass for the wrong reason as soon
as an unrelated cache warmed.

## What M1 changed about the design

Four findings altered something rather than confirming it.

1. **`rustfmt.toml` had a UTF-8 BOM**, so rustfmt silently ignored the whole file
   and reported `tab_spaces = 4`, `edition = 2015`. That is why commit fa7d468 had
   to fix indentation by hand. Removing the BOM makes `cargo fmt --check`
   meaningful for the first time.

2. **`revparse` is too powerful for a service boundary.** The M0 spike passed the
   caller's selector straight to `revparse_single`, which is right for a
   measurement. It accepts an expression language, and `main^{tree}` in particular
   resolves successfully and yields a *tree* OID where every downstream layer
   expects a commit. The grammar is now closed to four shapes before libgit2 sees
   anything.

3. **Hiding `refs/xvfs/` needs two spellings, not one.** Git resolves a short name
   by trying `refs/<name>`, so the selector `xvfs/mounts/<id>` reaches
   `refs/xvfs/mounts/<id>` — a live lease anchor — while looking nothing like the
   reserved prefix. Both spellings are rejected, and the check also runs against the
   *resolved* ref name so it holds for a spelling the parser did not anticipate.

4. **`is_protecting` needs the lease policy, not just the clock.** The first
   implementation compared against `terminal_at`, so an expired lease stopped
   protecting its commit the instant it expired — which defeats the entire purpose
   of ADR 0006's 24-hour prune delay, since a mistaken expiry would be
   unrecoverable by definition. Found by the test that separates the three expiry
   stages.

## Recorded scope reductions

Both are deliberate, and neither is a criterion.

### OIDC is a seam, not an implementation

PLAN.md M1.5 says "integrate the chosen OIDC/workload identity". No provider has
been chosen — ADR 0006 leaves it open — and none is reachable from this
environment. `Authenticator` is therefore a trait with a `StaticTokens` development
verifier that reports `is_production_safe() == false`, and the server fails closed
with `DenyAll` when no authenticator is configured.

Everything M1.5 actually gates on is provider-independent and implemented: uniform
repository authorization, the mount capability, object authorization for unreachable
commits, blob tickets, audit records, and the confused-deputy, traversal,
unauthorized-OID, and stale-credential cases.

**What M2 or M6 must do:** supply a real verifier behind the trait. Nothing else
changes.

### `PrepareSnapshot` reports `READY` unconditionally

M1 has no search index, so a snapshot is ready as soon as its commit is readable —
which is the truthful answer to "can I read this snapshot" from the snapshot APIs.
The three-state vocabulary is already fixed and the RPC already exists, so M4.2
replacing this with real manifest state is additive.

## Carried forward from M0

Both M0 conditions remain open and are **not** M1's to close:

1. **Re-run the FUSE deployment matrix on the real hosted runner.** Kubernetes and
   CSI are still unmeasured. M2 commits to the host-daemon skeleton, so this is
   best done before then.
2. **Confirm the corpus.** Every number in `benchmarks/baseline.md` and every
   measurement above uses public stand-ins. The LFS question in particular is
   unanswered.

The two ADR 0006 product questions — which workloads define success, and whether
patch export suffices for the first integration — also remain open.

## What M1 did not build

Stated so nobody mistakes silence for coverage.

- **No FUSE client.** M1 is a server milestone; nothing has been mounted.
- **No Git smart-HTTP gateway.** M5. The `refs/xvfs/` hiding configuration is
  tested with a directly-invoked `upload-pack`, not through a gateway.
- **No search.** M4. `PrepareSnapshot` exists; no manifest or index does.
- **No overlay.** M3.
- **Single-node only.** The repository lock is in-process, and the catalog is
  SQLite. M7.1 partitions repositories to owner nodes precisely so that lock stays
  local; a distributed lock on the mount path would put a network round trip inside
  every mount creation.
- **No credential-helper wiring for authenticated upstream fetch.** The refspec and
  sandbox behaviour is tested against local `file://` upstreams, which need no
  credential. M6.1 wires the secret store.
- **Performance is measured only where a criterion required it.** The M2 targets in
  ADR 0006 — cold mount under 2 s, cached `getattr` p95 under 1 ms — are client
  properties and are M2's to measure.

## Test inventory

240 tests, all passing, plus the gated large-snapshot test.

| Crate | Tests | Covers |
| --- | ---: | --- |
| `xvfs-types` | 52 | OIDs, byte paths, the closed selector grammar, error codes, ADR 0006's lease and timestamp policies |
| `xvfs-proto` | 17 | wire conversions and the schema compatibility goldens |
| `xvfs-git` | 51 | the format gate, tree ordering and paging, blob verification, lease anchors, `gc` survival |
| `xvfs-server` | 120 | catalog and lease state machines, mirroring refspecs, capabilities, authorization, both API surfaces, the exit criteria |

The gate is one script, `scripts/check.sh`, shared with CI: version pinning, format,
Clippy with `-D warnings`, tests, docs, `cargo-deny`, the ADR 0001 license
assertions, SBOM with its addendum, and a secret scan. Two stages are gated into
separate CI jobs rather than skipped — `bigtree` for the million-entry criterion and
`devstack` for the local stack.

## Recommendation

**Proceed to M2.** The milestone graph is unchanged: M0 → M1 → M2 → M3 → M6, with
M5 parallel to M3/M4.

Before M2 commits to the host-daemon skeleton, re-run
`spikes/fuse-probe/deployment-matrix.sh` on the real hosted runner. That is M0's
first carried condition and M2.1's most expensive assumption.
