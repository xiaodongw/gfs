# M0 — Architecture review and go/no-go

Date: 2026-07-26
Milestone: M0 (PLAN.md section 3)
Recommendation: **Go, conditionally** — two conditions, both cheap, neither
architectural.

## The gate

PLAN.md states it precisely:

> proceed only if lazy mount works on the target platform, search index storage
> is viable at steady state, a Git-command surface that the pilot's tooling
> accepts has been identified and costed, and projected task savings are
> meaningful over partial clone.

| Condition | Verdict | Evidence |
| --- | --- | --- |
| Lazy mount works on the target platform | **Met for WSL2 + Docker; unmeasured for Kubernetes** | ADR 0003 |
| Search index storage viable at steady state | **Met, decisively** | ADR 0004 |
| A Git-command surface the tooling accepts, costed | **Met** | ADR 0005 |
| Projected savings meaningful over partial clone | **Met** | below |

## Savings over the best existing alternative

Linux kernel, the worst case: 94 850 tip files, 1 463 690 commits, 1537 MiB of
unique tracked content, 8.6 GiB of history.

| Workflow | Startup | Disk (`.git` + tree) | Searchable without more transfer |
| --- | ---: | ---: | :---: |
| Full clone | 383 s | 8 087 MiB | yes |
| Blobless (`--filter=blob:none`) | 181 s | 3 854 MiB | yes |
| Shallow (`--depth 1`) | 15 s | 1 832 MiB | yes |
| **Shallow + blobless** (best) | **19.5 s** | **1 839 MiB** | yes |
| **GFS target** | **< 2 s** | **< 10 MiB + overlay** | yes, server-side |

Against the strongest existing option, the projected improvement is roughly
**10× on startup and ~180× on local disk**, and the disk figure is the more
important one: a shallow blobless clone still materializes the entire 1540 MiB
working tree, because a checkout hydrates every blob it writes. That is the cost
GFS removes by not materializing files nobody opens.

Two honesty notes on these numbers:

- **Clone times are local `file://` and exclude network transfer entirely.**
  A real shallow+blobless clone also moves 289 MiB over the network. The gap in
  a hosted environment is therefore *wider* than shown, not narrower.
- **The GFS column is a target, not a measurement.** The probe mounted in
  18.7 ms and served files lazily, but against a synthetic tree, not the Linux
  kernel. M2 is where this becomes a measurement.

## What M0 changed about the design

Seven findings altered the plan rather than confirming it. They are the reason
the milestone was worth running.

1. **SHA-256 is unreachable, not just experimental** (ADR 0001). `git2-rs`
   fails to compile against a SHA-256 libgit2. The pre-production commitment
   cannot be met by libgit2 maturing alone, so SHA-256 moves out of scope.

2. **Hiding `refs/gfs/` prevents discovery, not access** (ADR 0002). Protocol
   v2 serves any object in the ODB by OID regardless of
   `uploadpack.allowAnySHA1InWant`; v0 enforces it. A documented security claim
   in DESIGN.md section 7.1 is false for the Git path and is re-scoped to the
   snapshot API. One bare repository is now explicitly one authorization domain.

3. **Blocking a FUSE callback serializes the whole mount** (ADR 0003).
   Quantified at 1321 ms versus 123 ms for the same work. The design asserted
   this; M0 priced it and adopted both remedies.

4. **`allow_other` is a host-level prerequisite** (ADR 0003), and it fails in a
   place nobody would look: the Docker daemon cannot even prepare a bind mount
   whose source is a uid-1000 FUSE mount without it.

5. **Manifest storage is a non-issue** (ADR 0004). 1.99 MiB per snapshot for the
   Linux kernel, 0.39 GiB for 200 retained. On-demand arbitrary-commit search
   needs no rationing — the fallback of restricting search to branch tips is
   not needed.

6. **The synthesized `.git` spec was incomplete and partly wrong** (ADR 0005).
   Four files are not a repository; `objects/` and `refs/` are also required.
   And `ls-files` and `diff` return **empty with exit 0** rather than failing
   visibly, which promotes the shim from a convenience to a correctness
   requirement.

7. **`git status` on a partial clone is a full metadata sweep** (ADR 0005):
   101 180 stat calls over 94 850 index entries. This is what settles the `.git`
   decision, and it keeps M5 off the critical path.

Findings 2, 6, and 7 each contradicted something the design asserted. That is
the expected yield from a feasibility milestone, and it is why the two remaining
conditions below are worth insisting on rather than waiving.

## Conditions on the "go"

### 1. Re-run the FUSE deployment matrix on the real hosted runner

The M0.2 exit gate says "the actual hosted environment". Kubernetes/CSI was
unmeasured because no cluster was reachable. The measurements transfer *by
argument* — a CSI node plugin is a privileged host component publishing a mount
into an unprivileged pod, which is the model that was measured — but an argument
is not a measurement, and this is the milestone's highest-risk assumption.

`spikes/fuse-probe/deployment-matrix.sh` runs in minutes. It must pass on the
real runner before M6, and ideally before M2 commits to the host-daemon skeleton.

> **Superseded, 2026-07-26 (after M1).** The "ideally before M2" half is dropped.
> The condition is now deliberately deferred until the prototype mounts and serves a
> workspace locally, because the script has already run on this machine — the gap is
> Kubernetes and the real runner, neither reachable — and the unmeasured leg
> constrains how a mount is *published* to a job (M6.1, M7.4) rather than anything
> M2 builds. In exchange, M2 keeps mount publication behind a single seam. The
> "before M6" half stands. Reasoning and the trigger:
> [ADR 0003 amendment](../../docs/adr/0003-fuse-deployment-model.md).

### 2. Confirm the corpus

Every number here comes from public stand-ins. The harness is parameterized by
`spikes/corpus/corpus.conf` and re-runs unchanged against real repositories, but
"materially lower cost" is only as representative as the corpus behind it. The
LFS question in particular is **unanswered** — no corpus repository uses it — and
LFS in a target monorepo would be a real compatibility gap.

Two product questions also remain open and are recorded in ADR 0006: which
workloads define success, and whether patch export suffices for the first
integration. Neither blocks starting M1.

## What M0 did not measure

Stated so no one mistakes silence for coverage:

- Kubernetes and CSI (condition 1).
- FUSE behaviour at monorepo scale — the probe served a synthetic tree, so
  metadata cost for a million-entry snapshot is projected, not measured.
- `mmap`, writable `MAP_SHARED`, and the writeback-cache question.
- The clone/fetch protocol matrix at scale and across Git client versions (M5.2).
- Warm end-to-end search latency, and incremental manifest construction.
- Overlay behaviour of any kind: M0 built no overlay.
- Real agent task correctness. No agent task was run against a mount.

That last one deserves emphasis. M0 established that the *mechanisms* work and
what they cost. It established nothing about whether an agent completes real
tasks correctly on a mounted workspace. That is M6's job, and it is the question
the product ultimately turns on.

## Recommendation

**Proceed to M1**, with the milestone graph unchanged: M0 → M1 → M2 → M3 → M6,
M5 parallel to M3/M4. The `.git` decision kept the promisor path off the
critical path, so PLAN.md section 1's 14–18 week parallel estimate remains
achievable on its own terms.

Carry both conditions above as tracked items rather than blockers, and re-run
the deployment matrix on the real runner at the first opportunity.
