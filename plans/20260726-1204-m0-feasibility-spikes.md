# M0 — Feasibility and Architecture Spikes

## Summary

M0 is the go/no-go milestone of `docs/PLAN.md`. It is a measurement milestone, not
a delivery milestone: nothing in it ships, and its only product is enough evidence
to decide whether XVFS is worth building and what shape it takes. Four decisions
come out of it and constrain every later milestone:

1. **Can we mount at all** in the target hosted environment, and with what privilege
   (M0.2). If this fails, the product does not exist in its designed form.
2. **Can libgit2 and stock `upload-pack` agree** on the repositories we intend to
   host, and where is the supported-format boundary (M0.3). This decides whether the
   `GitRepository` trait is a thin wrapper or a compatibility project.
3. **Does the blob-key + trigram + snapshot-bitmap representation fit on disk** at
   steady state, over realistic commit churn (M0.4). The plan is explicit that
   *manifest storage per retained snapshot*, not index build time, is the number
   that decides whether arbitrary-commit search is affordable.
4. **What occupies `.git`** inside the mount (M0.5). The synthesized-`.git` default
   and the shallow-blobless-partial-clone alternative have different milestone
   graphs: if partial clone wins, a minimum M5 upload-pack/promisor scope becomes a
   predecessor of M2 and the "parallel" 14–18 week estimate is off the table.

Everything else in M0 is the baseline needed to make those four decisions honest:
M0.1 measures what a clone costs today so "materially lower" has a denominator, and
M0.6 writes down the answers plus the policies (retention lease, timestamps,
versioning, threat model) that M1 immediately depends on.

Three inputs were settled with the user before starting:

- **Corpus**: public stand-ins, parameterized so real monorepos can be swapped in.
  `torvalds/linux` is the worst case, `microsoft/vscode` the primary
  language-mix representative, `rust-lang/rust` the submodule/mixed-build shape.
- **FUSE target**: WSL2 host plus Docker. Kubernetes/CSI gets a documented design,
  not a measurement, because no cluster is reachable from this machine. This is a
  recorded gap in the M0.2 exit gate, not a silent omission.
- **Toolchain**: latest stable Rust (1.97.1), pinned in `rust-toolchain.toml` as
  the MSRV baseline that M1.1 inherits.

Spike code lives in `spikes/`, deliberately outside the `xvfs-*` workspace that
M1.1 creates. Spike code is written to be measured and thrown away; production code
is written to be maintained, and mixing them makes the second one worse.

## Plan

### Phase 0 — Scaffold and corpus (task #1)

- `spikes/` Cargo workspace, `rust-toolchain.toml` pinned to stable 1.97.1.
- `spikes/corpus/corpus.conf` as the single source of repository truth; every
  script reads it and hardcodes no repository names.
- Bare mirrors fetched with `--ref-format=files` and the restricted upload-pack
  filter policy from DESIGN.md section 7.2 already applied, so local `file://`
  benchmarks exercise the real policy.

### Phase 1 — M0.3 Git integration validation (task #2)

- `git2-rs` conformance probe over the fixture matrix: bare repo open, OID parsing,
  ref resolution, byte (non-UTF-8) paths, tree traversal, tree diff, object
  creation, transactions.
- Repository format boundary: prove libgit2 rejects `reftable`, determine the
  actual state of SHA-256 support in the pinned build, and implement the ingest
  detection that fails a mirror at creation rather than serving a partial view.
- `GitRepository` trait plus a libgit2-backed proof of concept; blocking-worker and
  request-local handle model.
- Smart-HTTP gateway prototype proxying to sandboxed `git upload-pack`; verify the
  v0/v1 `# service=` preamble is added and the v2 advertisement is preamble-free,
  then clone/fetch/shallow/partial smoke tests against it.
- Freeze the partial-clone filter policy and confirm disallowed filters fail closed.
- Pin libgit2 / `git2-rs` / stock Git versions with licenses.
- ADR.

### Phase 2 — M0.2 FUSE deployment spike (task #3)

- Minimal read-only `fuser` filesystem, one remote-backed file, real network fetch.
- Matrix: WSL2 host direct mount; Docker with `--device /dev/fuse` and varying
  capabilities; Docker unprivileged with a host-daemon bind mount.
- Measure privilege floor, unmount and daemon-death behavior, kernel attribute and
  entry caching, request concurrency, container teardown with a live mount.
- Decide direct mount vs host daemon vs CSI, and verify the bind-mount into an
  unprivileged job with the ownership/`safe.directory` question from M0.5.

### Phase 3 — M0.1 Workload baseline (task #4)

- Characterize each corpus repo: history bytes, tip file/tree/blob counts, language
  mix, large files, submodules, LFS, non-UTF-8 paths, symlinks.
- Benchmark full / shallow / `blob:none` / `tree:0` / sparse / warm-cache clones for
  startup time, bytes transferred, disk used, and search time.
- `benchmarks/baseline.md` with machine profile and reproducible scripts.

### Phase 4 — M0.4 Search representation spike (task #5)

- Blob-key assignment, trigram postings, snapshot Roaring bitmap, reverse path table
  over the largest snapshot; compare against per-snapshot Tantivy.
- Correctness against `rg` over a raw-tree materialization for literals, Unicode,
  regex with and without required literals, repeated blobs, binary detection.
- **Steady-state manifest storage** under plausible branch and arbitrary-commit
  churn — the number the exit gate actually turns on.
- Excluded (binary/oversized) fraction, so the coverage contract has real numbers.
- Two-dimensional terminal result prototype: execution status vs scoped coverage.

### Phase 5 — M0.5 Git-command surface (task #6)

- Inventory real `git` invocations from the corpus repos' build and CI config.
- Freeze the exact shim grammar (subcommands, flags, pathspecs, output, exit codes).
- Measure first and subsequent `git status` and `git diff` on a shallow blobless
  partial clone of the worst-case repo: wall time, metadata ops, bytes, index size.
- Prototype the synthesized `.git`; record which tools it satisfies and confirm the
  rest fail visibly rather than reporting a wrong tree state.
- Decide, ADR, and if partial clone wins, record the M2→M5 dependency inversion.

### Phase 6 — M0.6 Product decisions and go/no-go (task #7)

- Answer the DESIGN.md section 14 open questions with the measurements above.
- Freeze MVP compatibility boundaries, performance gates, API versioning, repository
  path semantics.
- Threat-model the host cache and the FUSE privilege boundary.
- Fix the retention-lease policy (lifetime, orphan expiry, heartbeat, GC interaction,
  `refs/xvfs/` hiding) and the timestamp policy, since M1 implements both directly.
- Write the go/no-go architecture review against the four gate conditions.

## Decisions

_Filled in as the spikes produce evidence._

## Details

- Corpus mirrors live outside the repository at `$XVFS_CORPUS_DIR`
  (default `~/xvfs-corpus`) so tens of gigabytes of benchmark data never enter git.
- Spike crates are excluded from the future `xvfs-*` production workspace on purpose.
- The Kubernetes/CSI leg of M0.2 is unmeasured on this machine. Any M0 exit claim
  about hosted deployment is scoped to WSL2 and Docker.
