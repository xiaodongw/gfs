# m05d: is the post-`git lfs pull` state safe to synthesize, without git-lfs?

Date: 2026-07-31
Instrument: `spikes/lfs-expansion/measure-lfs-state.sh` — self-contained, no
git-lfs, no network. Stock Git 2.53.0, WSL2 kernel 6.18.

## Question

Server-side LFS expansion (proposed as ADR 0012) presents a working tree whose
LFS files hold expanded content while the projected object store and the index
keep the pointer blobs — byte-for-byte the state a real `git lfs pull` leaves
behind. ADR 0009's guarantee is that stock Git answers truthfully against the
workspace, so the question is: **under what conditions does stock Git stay
truthful in that state, what does each condition cost, and what happens when
one is missing?**

## Method

An upstream bare repository holds 400 small source files plus two
spec-conformant LFS pointers (fabricated; the pointer format is three text
lines) whose objects — 64 MiB and 8 MiB of `/dev/urandom` — live in a
directory keyed by sha256, standing in for the gateway's LFS store. A
workspace is staged the ADR 0009 way: real `.git`, `objects/info/alternates`
to the upstream store, `core.checkStat=minimal`, `checkout-index`, then the
pointer files are replaced with expanded content and every working file is
stamped with a past mtime — the mount's sanitized snapshot time (ADR 0006).

Four arms:

- **A** — no filter configuration: the state with nothing reconciling it;
- **B** — `filter.lfs.clean` is a stub that sha256s the streamed content and
  emits the pointer (what real git-lfs does);
- **C** — the clean stub instead answers the pointer from per-path metadata
  and drains stdin without hashing (what a daemon-backed gfs filter can do,
  because the mount knows the path is unmodified base content);
- **D** — stock `git checkout` onto a branch whose LFS pointer differs, with a
  smudge stub that hydrates from the store by oid.

Filter invocations are counted from the stubs' own log. Two consecutive runs
agreed on every number below.

## Results (warm wall-clock, ms; filter runs per command)

| command | A: no filter | B: hashing clean | C: metadata clean |
|---|---|---|---|
| `git status`, cold | 122 — **2 files lie as modified** | 273 (2 runs, clean) | 65 (2 runs, clean) |
| `git status`, second | — | 6 (0 runs) | 6 (0 runs) |
| `git status`, stat coherent | — | 6 (0 runs) | 6 (0 runs) |
| `git diff` | — | 6 (0 runs) | — |
| `git status` after `touch` on the 64 MiB file | — | 207 (1 run, clean) | — |
| `git add` the 64 MiB file | **stores the expanded blob** | stores the pointer | — |
| edit an LFS file, `add`, `commit` | — | commits a fresh, correct pointer | — |

Arm D: `git checkout v2` takes 48 ms, runs the smudge once, and the working
file's bytes verify against the v2 pointer's oid. The immediately following
`status` re-cleans (4 runs, ~100 ms with the metadata stub) because
checkout-written files carry a current mtime — see counters, point 3.

## What the counters say

1. **The filter carries correctness; the stat cache carries cost.** Without a
   filter (A), `git status` reports every LFS file modified and `git add`
   writes the 64 MiB expanded blob into the object store — the branch-corrupting
   move. With any clean filter configured, every command told the truth in
   every run: status and diff clean, `add` and `commit` producing pointers,
   including a *fresh, correct* pointer for genuinely edited content. No
   correctness result depended on timing.
2. **Steady state is free, and GFS can make it free deterministically.** Once
   index stat data matches the working tree, `status` is 6 ms with zero filter
   invocations. Whether git *persists* that refreshed stat opportunistically
   turned on the racily-clean guard: in early runs where expanded files
   carried current mtimes, every status re-cleaned forever (~250 ms each, four
   consecutive statuses). Stamping the working tree with the sanitized
   snapshot time — in the past by construction, which is what the mount serves
   anyway (ADR 0006) — made persistence deterministic: one cold reconcile,
   then 0 runs. A mount that seeds the index with matching stat data skips
   even the cold reconcile.
3. **Reconciliation cost is the filter's read strategy.** The hashing clean
   (git-lfs equivalent) pays sha256 over the full content: 273 ms cold for
   72 MiB, 207 ms to re-verify one touched 64 MiB file. The metadata-answering
   clean pays only the pipe drain: 65 ms cold, 4× cheaper, without reading the
   file at all — the daemon already knows the answer. Git may invoke the clean
   more than once per command for a stat-dirty path (arm D's status ran it 4
   times across 2 files), which multiplies whichever cost is chosen.
4. **Stock branch switching works with a hydrating smudge.** Checkout invoked
   the smudge once for the one changed LFS file and wrote verified content.
   The one seam: checkout writes with current mtimes, so the *next* status
   re-cleans those paths until an index write at a later second clears the
   raciness — bounded, and only drain-cost with the metadata clean.

## Conclusion

Go. The synthesized post-`lfs pull` state is exactly as trustworthy as its
filter configuration: with `filter.lfs.*` present, stock Git is truthful on
every path exercised, including the write path; with it absent, `git add` is
a corruption vector, so seeding the config is a correctness requirement, not
an optimization. A daemon-backed clean filter beats the git-lfs-equivalent by
4× on reconciliation and needs no content hashing for base-identical paths,
and snapshot-time mtimes plus a mount-seeded index make the steady state
zero-filter-traffic by construction rather than by luck.

## Limitations

Real git-lfs was not installed, so coexistence (an image that ships git-lfs
whose config fights the seeded one, `git lfs status` against this state) is
untested. Filters ran as per-file subprocess spawns; the long-running
`filter.<driver>.process` protocol would only lower the per-invocation floor.
Content lived on local disk, not behind FUSE — this spike isolates the git
contract; m05b/m05c cover the projection and `.git` traffic costs it composes
with.
