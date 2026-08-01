# Server-side LFS expansion (ADR 0012)

## Summary

Implement ADR 0012: the server resolves LFS pointers and the workspace
presents the post-`git lfs pull` state. Entry metadata (`GetEntry`,
`ListDirectory`, `BatchGetEntry`, walks, the seeded index) reports expanded
sizes and `lfs-sha256:{oid}` content keys for LFS entries; the immutable blob
endpoint serves that key form from a new gateway LFS store populated via the
upstream `/info/lfs/objects/batch` API; the mount seeds `filter.lfs.*`
pointing at a daemon-backed shim so stock Git stays truthful (m05d: the
filter is a correctness requirement — without it `git add` corrupts the
branch); `gfs commit` re-cleans edited LFS content into fresh pointers and
`gfs push` uploads new objects before pushing the ref. The projected object
store keeps serving pointer blobs untouched. Search skips LFS entries with
their own coverage reason. Evidence: `spikes/reports/m05d-lfs-expansion.md`.

## Plan

1. **Content-key foundation.** `HashAlgorithm::LfsSha256` in
   `gfs-types/src/oid.rs` (`name = "lfs-sha256"`, raw_len 32). Client
   verification arm in `gfs-mount/src/cache.rs::hash_blob`: sha256 over raw
   bytes (the LFS oid *is* the content hash — no `blob <size>\0` header).
   Flip `sha256_is_refused_rather_than_silently_hashed_as_sha1` and add
   round-trip tests.
2. **Detection in gfs-git.** New `attributes.rs`: `.gitattributes`
   resolution at a revision (read attribute blobs from trees, per-directory
   stacking, last-match-wins) scoped to the `filter` attribute. New
   `lfs.rs`: spec v1 pointer parse (`oid sha256:<64hex>`, `size <n>`,
   ≤ 1024 bytes) → `LfsPointer`. A per-commit cached resolver beside
   `TreeCache`: path + blob → `Option<LfsPointer>`.
3. **Gateway LFS store + blob endpoint.** `gfs-service/src/lfs/store.rs`:
   filesystem CAS under `<state>/lfs/<repository_id>/`, temp + verify-hash +
   fsync + rename (catalog-grade durability: not rebuildable from git).
   Wire into `Server` builder and `gfs-server` state dir. `immutable_blob`
   serves `lfs-sha256:` keys from the store; `BlobTicket` already carries an
   `ObjectId` so ticket issue/verify are unchanged.
4. **Metadata substitution.** Entry producers in `gfs-git/src/libgit2.rs`
   (`entry_info` and callers: entry/batch/list/walk) substitute expanded
   size + `lfs-sha256` oid for LFS entries **iff the store has the object**
   (store presence injected as a small trait, so gfs-git does not depend on
   gfs-service); otherwise the entry degrades to its pointer (MVP behavior).
   `index_for_commit` writes expanded size with the pointer oid — the exact
   post-`lfs pull` index. WebDAV / `file_by_revision` / FUSE getattr expand
   for free.
5. **Batch client + population.** `gfs-service/src/lfs/batch.rs`: derive
   `{upstream}/info/lfs/objects/batch`, POST download request (basic
   transfer), GET objects into the store; credential handling mirrors
   `mirror.rs` (env, never in URL, `env_clear` shape for subprocesses does
   not apply — this is in-process HTTP). Populate at ingest (tip-only
   prefetch, surfaced as an explicit policy knob) and on credential-carrying
   fetches. Unfetchable objects log and degrade.
6. **Workspace filters.** New `gfs-fuse/src/bin/gfs-lfs-filter.rs`
   (clean/smudge subcommands) modeled on `gfs-fsmonitor`; control-protocol
   requests `LfsClean { path }` (daemon answers pointer text for
   base-identical paths from entry metadata; shim drains stdin) and
   `LfsSmudge { oid, size }` (daemon hydrates via the shared `BlobCache` —
   verified, budget-charged — and returns the cache path; shim streams it).
   Genuinely edited content: shim hashes stdin and emits a fresh spec v1
   pointer. `seed_git_dir` writes `[filter "lfs"] clean/smudge/required`
   with the shim's absolute path — hard-required, unlike fsmonitor, because
   a missing filter is a corruption vector, not a degradation.
7. **Write path.** `commit_changes`/`write_tree`: upserted files on LFS
   paths (base-commit attribute stack) that are not already pointers get
   stored into the LFS store and committed as fresh pointers. `push_branch`:
   collect LFS pointers introduced by base..tip, upload store-present
   objects via the batch API with the caller's credential, then push.
8. **Search + oracles + docs.** `lfs` exclusion reason through
   `classify.rs`/coverage/`declared_exclusions` on both manifest build
   paths. Flip `attributes_and_lfs_content_is_served_raw_as_documented` and
   the compat oracle; extend `gfs-test` fixtures with a stocked LFS store.
   Route `Mount::cat` through `BlobCache` (closes the pre-existing
   unverified-cat gap). Update DESIGN.md §12, ADR 0012 → Accepted.

Compile + test after each phase (workspace `cargo build` + targeted crate
tests; full `cargo test` at the end).

**Implemented 2026-07-31** (ADR 0012 → Accepted). All eight phases landed as
planned, with these deltas discovered during implementation:

- The byte-glob moved `gfs-search` → `gfs-types` (re-exported) so `gfs-git`'s
  attribute matcher shares it without dragging in sqlite/regex; `lfs.rs`
  (pointer parse/render) likewise lives in `gfs-types` because the daemon and
  the filter shim both need it, with the `LfsObjectCheck` trait staying in
  `gfs-git`.
- Phase 4's metadata expansion silently broke the search indexer: `build_full`
  seeds its root entries from `list_directory`, which now substitutes
  `lfs-sha256:` keys the blob reader cannot read, while subtree walks stayed
  raw. Fixed in phase 8 by detecting LFS entries once per build
  (`lfs_pointers`, extended to carry the pointer *blob* identity) and keying
  LFS paths by their pointer blobs with an `lfs` flag on `BlobFact`, so the
  registry classifies them `Lfs` without reading and full/incremental builds
  agree byte-for-byte.
- The batch client's network egress is a `curl` subprocess (see Decisions);
  upload implements the `basic` transfer's PUT plus the optional verify
  action, and hands curl the store file path so large objects never pass
  through process memory.
- `gfs cat`'s two unverified paths (daemon `Mount::cat` and the CLI's direct
  HTTP fetch) now verify — the daemon path through the shared `BlobCache`,
  the CLI path by hashing against the entry's content key.
- End-to-end coverage: `crates/gfs-service/tests/lfs.rs` (expansion +
  degradation + ticket + blob endpoint + commit re-clean, against a real
  server on real ports), the gfs-git expansion/index test, and unit suites
  for attributes, pointer parsing, the store, and the batch wire format.

**Live validation 2026-08-01** against `github.com/cbeams/lfs-test` (real
GitHub LFS upstream, git-lfs installed on the host) found and fixed four
bugs the test suite could not see, then passed the whole loop: prefetch from
the real batch API, expanded `ls`/`stat`/read/`gfs cat`, truthful zero-cost
`git status`, `git add` → fresh pointer, `git checkout` → smudge-hydrated
original, revert detection, `gfs switch -c` + `gfs commit` → gateway
re-clean + store population, and the `lfs` search coverage reason.

1. `gfs-proto/src/convert.rs` — client-side entry conversion rejected any
   oid whose algorithm differed from the repository's; `try_content_key`
   now admits `lfs-sha256:` for *entry* oids only (commits/trees stay
   strict). Regression tests added.
2. `gfs-mount/src/cache.rs` — `fetch_and_publish` verified downloads under
   the cache-wide algorithm instead of the key's, so every LFS fetch failed
   verification; it now hashes under the key's own rule.
3. **git-lfs coexistence forced the process protocol.** A host with git-lfs
   installed has `filter.lfs.process = git-lfs filter-process` in its global
   config, Git prefers the process form across scopes, and a set-but-empty
   local `process =` *poisons* the driver rather than falling back
   (measured, Git 2.53). The shim now implements the long-running
   filter-process protocol (pkt-line v2, clean+smudge) and the workspace
   seeds `process = .git/hooks/gfs-lfs-filter process`, which wins the
   precedence. The m05d "escalation if spawn overhead shows up" turned out
   to be a correctness requirement, not a performance option.
4. `gfs-overlay` — revert detection hashed workspace content as a git blob
   and compared it to the base entry's `lfs-sha256:` key, so a
   touched-then-restored LFS file stayed "modified" forever and blocked
   `gfs switch`; status now hashes under the base entry's key form
   (`hash.rs` gained the raw-sha256 arm).

## Decisions

- **`lfs-sha256` is a `HashAlgorithm` variant, not a parallel `ContentKey`
  type.** `ObjectId` is the single currency through entry metadata → blob
  ticket → URL → cache shard → verification; a wrapper type would fork
  every signature on that path for no safety gain — the places an LFS key
  must never reach (index writer, overlay hasher, git odb conversions)
  already reject non-SHA-1 algorithms loudly. Revisit if `HashAlgorithm`
  ever grows real git SHA-256 hosting semantics that collide.
- **Substitution is gated on store presence.** "Expanded" is per-entry
  state (ADR consequence): an entry whose object the store lacks serves its
  pointer exactly as the MVP did. This makes blob-endpoint misses
  structurally impossible in the normal flow instead of a 404 surface.
- **Prefetch policy MVP = tip of the default branch at ingest**, an explicit
  knob, because the server can only reach upstream while a caller's
  credential is in hand (the catalog stores credential references, not
  secrets, and `ingest` currently stores none).
- **Clean answers are reconstructed canonically** (`version`/`oid`/`size`
  lines) from entry metadata; a pointer that carried nonstandard extra keys
  would re-clean to a different byte sequence than its original. Accepted
  for MVP; noted in Details.
- **LFS batch egress is a `curl` subprocess**, not an in-process HTTP client:
  `mirror.rs` already establishes that the server links no TLS library and
  network fetching is a subprocess's job (stock Git there). Same discipline —
  cleared environment, and the credential *and* per-object hrefs (signed
  query tokens) travel via curl's config-on-stdin, never argv. `--curl-binary`
  / `GFS_CURL_BINARY` names the binary.
- **Search excludes LFS entries unconditionally** (no policy flag): expanded
  content is unsearchable by nature and pointer text is noise, so a
  configurable exclusion would only manufacture a footgun. The index interns
  LFS paths by their pointer blob (git reality), never the content key.
- **The filter shim is seeded when found, warned about when not** (unlike
  fsmonitor's silent degradation, but short of hard-failing the mount): a
  deployment without a server-side LFS store never expands and is unaffected,
  and failing every mount for a missing binary would break non-LFS setups.
  `filter.lfs.required = true` makes a broken shim fail loudly at use.

## Details

- `gfs status` "which entries degraded" reporting is deferred: it needs a
  per-entry degradation signal in the control protocol; recorded as
  follow-up, not silently dropped.
- Smudge returns a cache-file path over the control socket rather than
  streaming content through it; there is a theoretical eviction race
  between the daemon's reply and the shim's `open`, bounded by LRU
  recency. Fd-passing is the escalation if it ever bites.
- Locally-committed pointers (stock `git commit` through the clean filter)
  reference objects that exist only in the workspace working tree; pushing
  such a commit before `gfs commit`-style adoption uploads nothing for
  them. Known seam, out of MVP scope.
- Coexistence with a real git-lfs in the agent image is untested (m05d
  limitation) and stays an open item.
- The smudge hydration bypasses the per-job hydration budget (it goes through
  the shared verified cache but not `Gfs::open_blob`'s admission check);
  stock `git checkout` across LFS revisions is refused when the pointer names
  an object the pinned revision does not — `gfs switch` is the supported
  path. Both are recorded seams, not silent behavior.
- The prefetch policy knob is `IngestConfig::lfs_prefetch`
  (`Tip` default | `None`); webhook-triggered refresh fetches carry no
  credential, so population happens at ingest and at credential-carrying
  syncs only.
- The batch client requires `curl` on the server host (configurable via
  `--curl-binary` / `GFS_CURL_BINARY`), beside the existing `git` binary
  requirement.
- Not yet exercised against a live LFS upstream (GitHub et al.): the batch
  wire format is unit-tested and everything else is covered end-to-end
  against a local server, but no automated test speaks to a real
  `/info/lfs/objects/batch`. First `gfs clone` of a real LFS repository is
  the manual test to run, and the filter shim's behavior under a real
  `git add`/`git checkout` (m05d used stubs) should be watched then too.
