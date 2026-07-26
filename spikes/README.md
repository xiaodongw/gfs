# M0 spikes

Measurement code for the M0 feasibility milestone in [PLAN.md](../docs/PLAN.md).

These crates exist to produce numbers and are expected to be **deleted**, not
maintained. They are deliberately kept out of the `xvfs-*` production workspace
that M1.1 creates: spike code is written to be thrown away, production code is
written to be lived with, and mixing the two makes the second one worse.

| Crate | Milestone | Answers |
| --- | --- | --- |
| `git-probe` | M0.3 | Does libgit2 agree with stock Git on the repositories XVFS will host, and where is the supported-format boundary? |
| `gateway-probe` | M0.3 | Can the smart-HTTP subprocess contract with stock `upload-pack` be reproduced exactly, and does the sandbox hold? |
| `fuse-probe` | M0.2 | Can we mount in the target hosted environment, and at what privilege? |
| `search-probe` | M0.4 | Does the blob-key + trigram + snapshot-bitmap representation fit on disk at steady state? |

## Corpus

Every script reads [`corpus/corpus.conf`](corpus/corpus.conf) and hardcodes no
repository names, so the real target monorepos can be swapped in by editing that
one file. Mirrors land in `$XVFS_CORPUS_DIR` (default `~/xvfs-corpus`), outside
the repository, because they are tens of gigabytes.

```sh
./corpus/fetch-corpus.sh            # all repositories
./corpus/fetch-corpus.sh linux      # just one
```

The mirrors are configured the way DESIGN.md section 7.2 requires a server-side
repository to be — `files` ref backend, restricted filter policy, no reflogs —
so local `file://` measurements exercise the real policy rather than Git's
defaults.

## Reports

Findings live in [`reports/`](reports/); accepted decisions become ADRs under
[`../docs/adr/`](../docs/adr/). A report states what was measured and on what
machine; an ADR states what was decided and what was rejected.
