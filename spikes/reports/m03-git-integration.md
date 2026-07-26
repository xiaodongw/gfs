# M0.3 — Git integration validation

Milestone exit gate (PLAN.md M0.3):

> libgit2 and `upload-pack` expose the same refs and objects for the fixture
> matrix, and their deployment/runtime boundary is documented.

**Met.** 116/116 fixture checks and 52/52 corpus-mirror checks pass, with four
designed rejections and one documented finding.

Decisions are in [ADR 0001](../../docs/adr/0001-git-integration.md) and
[ADR 0002](../../docs/adr/0002-git-object-authorization-boundary.md).

## How to reproduce

```sh
./spikes/corpus/fetch-corpus.sh              # ~12.5 GB of bare mirrors
cd spikes
cargo test                                    # unit tests
cargo run -p git-probe -- versions
cargo run -p git-probe -- conformance --root "$XVFS_CORPUS_DIR/fixtures"
cargo run -p git-probe -- repo "$XVFS_CORPUS_DIR/mirrors/linux.git"
cargo run -p gateway-probe -- check --root "$XVFS_CORPUS_DIR/fixtures/bare" --repo packed
./git-probe/sha256-support-check.sh
```

Machine: WSL2 (Linux 6.18.33.2), 32 cores, 46 GiB RAM. Versions in ADR 0001.

## Results

### Object layer — libgit2 vs stock Git

Stock Git is the oracle for every check; libgit2 never confirms its own answer.

| Fixture | Purpose | Result |
| --- | --- | --- |
| `empty` | unborn HEAD | pass (checks needing a commit skip) |
| `basic` | trees, branches, lightweight + annotated tags | pass |
| `modes` | exec bit, symlinks, gitlink | pass |
| `bytes` | non-UTF-8, newline, quote, space in names | pass, 2 invalid-UTF-8 paths resolved by exact bytes |
| `content` | empty, CRLF, no final newline, NUL, 4 MiB line, 12 MiB blob | pass |
| `bigdir` | 5000 entries in one tree | pass, 37 pages, no gaps or duplicates |
| `deep` | 40 nested components | pass |
| `packed` | fully packed objects and refs | pass |
| `attrs` | `.gitattributes` + LFS pointer | pass |
| `reftable` | reftable ref backend | **rejected at ingest** (designed) |
| `sha256` | SHA-256 object format | **rejected at ingest** (designed) |

Corpus mirrors, all green:

| Repo | Refs | Tip entries | Largest directory | Symlinks | Gitlinks |
| --- | --- | --- | --- | --- | --- |
| linux | 2 870 | 101 052 | `include/linux`, 1 598 over 12 pages | 99 | 0 |
| rust | 108 962 | 65 969 | `src/tools/clippy/tests/ui`, 2 518 over 19 pages | 5 | 12 |
| vscode | 71 921 | 21 129 | 562 over 5 pages | 1 | 0 |

"Tip entries" is a full recursive comparison of path bytes, file mode, and
object ID against `git ls-tree -r -t -z`. Sizes are compared against
`cat-file -s` and read through `odb.read_header`, which does not inflate the
object — the property the FUSE `getattr` path depends on.

### Protocol layer — the gateway vs real Git clients

| Check | Result |
| --- | --- |
| v0/v1 `# service=` preamble + flush prepended | pass |
| v2 advertisement starts at `version 2`, no preamble | pass |
| `filter` advertised only when policy enables it | pass |
| Content type and `Cache-Control: no-cache` | pass |
| `refs/xvfs/` absent from v0 and v2 advertisements | pass |
| `clone` v0, v2, `--depth 1`, `--filter=blob:none` | pass — `git fsck` clean, HEAD and tree equal to a direct filesystem clone |
| `--filter=` `tree:0`, `blob:limit=1k`, `combine:`, `object:type=` | rejected (designed) |
| gzip request body round-trip; 1029:1 bomb refused | pass |
| Repository-name traversal forms rejected | pass (10 forms) |
| Lease-only commit fetchable by OID over v2 | **finding — see ADR 0002** |

Every clone is verified independently rather than by exit status, because the
gateway's `upload-pack` is the implementation under test.

## Findings that changed the design

### 1. SHA-256 is unreachable through `git2-rs`, not merely experimental

`libgit2-sys` builds with `GIT_EXPERIMENTAL_SHA256`; `git2` 0.20.4 then fails to
compile with 75 errors on `GIT_OID_RAWSZ`/`GIT_OID_HEXSZ`, which `libgit2-sys`
gates out under that feature. The pre-production SHA-256 commitment in
DESIGN.md section 12 depends on `git2-rs`, not only libgit2.
`git-probe/sha256-support-check.sh` fails when this changes.

### 2. Hiding `refs/xvfs/` prevents discovery, not access

Protocol v2 serves any object in the object database by OID regardless of
`uploadpack.allowAnySHA1InWant`; protocol v0 enforces it correctly. This
contradicts DESIGN.md section 7.1's claim that repository access alone does not
grant access to another mount's retained commit. ADR 0002 accepts "one bare
repository is one authorization domain" and re-scopes the claim to the snapshot
API. Reproduced on plain `file://` transport too, so it is stock Git behavior
rather than a gateway defect.

### 3. Directory pagination must use Git's tree ordering, not raw names

Git orders a directory entry as if its name ended in `/`, so `byteorder.h`
(`0x2e`) sorts before `byteorder/` (`0x2f`) while the raw names compare the other
way. A page token that stores the raw name silently skips entries at page
boundaries: `include/linux` returned 1 597 of 1 598. The token must be the
sort key — name plus `/` for trees. M1.3's directory pagination inherits this.

### 4. A tag need not peel to a commit

Linux's `v2.6.11` tag dereferences to a **tree**. `ResolveRevision` must reject
it with a typed error rather than returning a non-commit as a snapshot root.
Two such tags exist in the Linux history; 198 others peel normally.

### 5. Three configuration traps in the upload-pack sandbox

Each fails in a way that does not point at its cause:

- `uploadpackfilter.blob:none.allow` — the subsection name contains a colon.
  `uploadpackfilter.blob.none.allow` is silently ignored and upload-pack then
  rejects `blob:none` as "not supported".
- `GIT_EXEC_PATH` must be set explicitly after `env_clear()`, or upload-pack
  cannot fork `git-pack-objects` — and only once a client is past the
  advertisement.
- `uploadpack.packObjectsHook` must be left unset, not set to `""`; Git treats
  empty as a command and fails with `cannot run :`.

## Limitations

- The protocol matrix runs against fixtures, not the multi-gigabyte mirrors.
  Clone-at-scale, the client version matrix, and other operating systems are
  M5.2's scope.
- Only Git 2.53.0 was tested. ADR 0002's v0/v2 split is version-sensitive and
  must be re-measured on every supported client and server version.
- The probe gateway buffers responses instead of streaming; streaming and
  backpressure are M5.1.
- `byte_paths` skips on all three mirrors — none has a non-UTF-8 path at its
  current tip, so that check rests on the `bytes` fixture alone.
