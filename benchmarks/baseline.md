# M0.1 — Workload and clone baseline

The deliverable PLAN.md M0.1 asks for: what the workflows GFS competes with
cost today, so that "materially lower startup time, network transfer, and local
disk" has a denominator.

## Machine profile

| | |
| --- | --- |
| Host | WSL2, Linux 6.18.33.2-microsoft-standard-WSL2 |
| CPU | 32 logical cores |
| Memory | 46 GiB |
| Disk | 1 TB, ext4 on `/dev/sdd` |
| Git | 2.53.0 |
| ripgrep | 14.1.1 (built from crates.io; see note below) |
| Rust | 1.97.1 |

## Reproducing

```sh
./spikes/corpus/fetch-corpus.sh                       # ~12.5 GiB of mirrors
./spikes/corpus/characterize.sh linux rust vscode
./spikes/corpus/benchmark-clone.sh vscode rust linux
```

Every script reads `spikes/corpus/corpus.conf` and hardcodes no repository
names, so the whole baseline re-runs against the real target monorepos by
editing that one file.

## Corpus

Public stand-ins, chosen to cover the shapes that matter. Replacing them with
the real targets is an open item (ADR 0006, question 2).

| | linux (worst case) | rust | vscode |
| --- | ---: | ---: | ---: |
| Mirror on disk | 8.6 GiB | 1.5 GiB | 2.4 GiB |
| Commits from HEAD | 1 463 690 | 333 794 | 161 743 |
| Refs | 2 870 | 108 962 | 71 921 |
| Tip files | 94 850 | 61 301 | 16 863 |
| Tip directories | 6 202 | 4 656 | 4 266 |
| Unique tracked content | 1 536.8 MiB | 210.6 MiB | 224.8 MiB |
| Duplicate-content paths | 655 | 1 479 | 565 |
| Symlinks | 99 | 5 | 1 |
| Executables | 1 319 | 148 | 52 |
| **Submodules** | 0 | **12** | 0 |
| **Non-UTF-8 path names** | **0** | **0** | **0** |
| Blobs over the 8 MiB search limit | 11 | 1 | 2 |
| Largest tracked blob | 22.9 MiB | 9.6 MiB | 8.5 MiB |
| Root `.gitattributes` LFS rules | 0 | 0 | 0 |
| Root `.gitattributes` text/eol rules | 0 | **2** | **7** |
| Dominant languages | `.c` 36 923, `.h` 26 891 | `.rs` 37 928, `.stderr` 15 575 | `.ts` 11 773, `.json` 1 406 |

Three rows change the compatibility work:

- **Non-UTF-8 paths are absent** from all three tips. Byte-path handling stays
  (it is far cheaper to build in than retrofit, and the `bytes` fixture keeps it
  tested), but it is insurance rather than a daily concern for this corpus.
- **Submodules are present** in rust — 12 gitlinks — so the gitlink
  representation is exercised by a real repository, not only by a fixture.
- **`.gitattributes` text/eol rules are present** in rust and vscode. The mount
  serves raw blob bytes, so those files differ from what `git checkout` would
  produce. This is DESIGN.md section 12's documented divergence, and this corpus
  will trip it.

LFS appears nowhere in the corpus, so the LFS question remains **unanswered**;
an LFS-using target monorepo would be a real gap.

## Clone baselines

Clones run over `file://` against the local mirrors, so times carry no internet
variance. **Wall time is therefore a lower bound**: a real clone adds transfer
time proportional to the "objects" column. `rg s`/`rg hits` are a fixed literal
search (`TODO`) over whatever the workflow materialized.

| repo | variant | wall s | .git MiB | work MiB | objects MiB | files | rg s | rg hits |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| vscode | full | 57.0 | 1376.8 | 226.6 | 1374.0 | 16862 | 0.04 | 901 |
| vscode | shallow `--depth 1` | 3.8 | 51.7 | 226.6 | 49.4 | 16862 | 0.04 | 901 |
| vscode | blobless | 25.8 | 369.9 | 226.6 | 367.1 | 16862 | 0.04 | 901 |
| vscode | shallow + blobless | 4.3 | 53.2 | 226.6 | 50.9 | 16862 | 0.04 | 901 |
| vscode | sparse `src/vs/editor` | 20.9 | 322.6 | 12.3 | 319.8 | 1330 | 0.01 | 108 |
| vscode | bare full | 53.6 | 1374.4 | 0 | 1374.0 | 0 | – | – |
| rust | full | 76.9 | 1039.9 | 212.1 | 1032.8 | 61296 | 0.07 | 207 |
| rust | shallow `--depth 1` | 7.7 | 55.9 | 212.1 | 48.7 | 61296 | 0.07 | 207 |
| rust | blobless | 50.7 | 488.0 | 212.1 | 480.9 | 61296 | 0.08 | 207 |
| rust | shallow + blobless | 9.5 | 60.9 | 212.1 | 53.8 | 61296 | 0.06 | 207 |
| rust | sparse `compiler` | 47.4 | 445.2 | 34.2 | 438.0 | 2901 | 0.01 | 0 |
| rust | bare full | 80.6 | 1032.8 | 0 | 1032.8 | 0 | – | – |
| **linux** | **full** | **383.1** | **6546.9** | **1540.4** | 6537.1 | 94751 | 0.13 | 3155 |
| **linux** | **shallow `--depth 1`** | **14.9** | **291.2** | **1540.4** | 281.5 | 94751 | 0.13 | 3155 |
| **linux** | **blobless** | **181.4** | **2313.3** | **1540.4** | 2303.4 | 94751 | 0.13 | 3155 |
| **linux** | **shallow + blobless** | **19.5** | **298.7** | **1540.4** | 288.9 | 94751 | 0.14 | 3155 |
| **linux** | **sparse `drivers/net`** | **167.7** | **2060.7** | **144.8** | 2050.7 | 6813 | 0.03 | 401 |
| **linux** | **bare full** | **373.7** | **6537.2** | **0** | 6537.1 | 0 | – | – |

`--filter=tree:0` failed against the mirrors, correctly: the corpus mirrors
carry GFS's own filter policy, which permits only `blob:none`. The variant is
kept in the harness so the denial is visible rather than absent.

## What the numbers say

**Partial clone does not avoid materializing the working tree.** Every blobless
variant still checked out the full tree — 1540 MiB for linux — because a
checkout hydrates every blob it writes. `--filter=blob:none` saves *history*
transfer, not working-set cost. This is the gap GFS targets, and it is the
single most important line in this document.

**Shallow is the cheapest existing option, and it is not cheap.** Linux
`--depth 1` costs 15 s and 1832 MiB (291 MiB `.git` + 1540 MiB tree) before an
agent has read one file. Combining shallow with blobless barely changes it —
19.5 s and 1839 MiB — because the tree dominates.

**Sparse checkout wins only when the needed paths are known in advance.**
`drivers/net` cut the tree from 1540 MiB to 145 MiB, but at 168 s (it still
fetches history), and it cut `rg` hits from 3155 to 401 — the agent can no
longer find code outside the cone. That is precisely the trade DESIGN.md
section 1 describes: it helps when paths are known, and agents usually need
repository-wide discovery before they know them.

**Search over a materialized tree is fast** (0.13 s for linux) once you have
paid to materialize it. GFS's server-side search has to beat the *total*, not
the `rg` time alone.

### The comparison the go/no-go turns on

| Workflow | Startup | Disk before any file is read |
| --- | ---: | ---: |
| linux full clone | 383 s | 8 087 MiB |
| linux shallow + blobless (best existing) | 19.5 s | 1 839 MiB |
| GFS target | < 2 s | < 10 MiB + overlay |

Roughly **10× on startup and ~180× on disk** against the strongest existing
option — and wider in a hosted environment, since these clone times exclude
network transfer of 289 MiB.

## Caveats

- **The GFS column is a target, not a measurement.** M0 built no mount capable
  of serving the Linux tree; M2 turns it into a measurement.
- Clone wall times exclude network transfer entirely.
- The 20–50 replayable agent tasks PLAN.md M0.1 also asks for were **not**
  assembled — that needs the real workload corpus (ADR 0006, question 2). Task
  correctness is measured in M6, and nothing here speaks to it.
- One measurement bug is worth recording: `rg` in this environment is a shell
  *function* wrapping another tool, not a binary, so the first run of this
  harness reported 0 hits in 0.05 s for every variant — a plausible-looking
  number rather than an obvious failure. The script now resolves a real ripgrep
  binary explicitly and refuses to run without one.
