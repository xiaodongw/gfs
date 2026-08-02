# GFS: Agent-Oriented Virtual Git Workspace

GFS is a Git-compatible repository service plus a Rust FUSE client, built for
short-lived, hosted coding-agent jobs. A job mounts an immutable Git snapshot,
sees a normal directory tree, and downloads file data only when a process opens
a file. Writes land in a local copy-on-write overlay, and code discovery goes
through a revision-aware server search API — so an agent can search a monorepo
without hydrating the entire working tree.

The system has three entry points:

- `gfs-server` — the repository service. Imports bare Git repositories, resolves
  revisions, serves trees and blobs over HTTP/gRPC, answers search and history
  queries (log, diff, show, blame), resolves Git LFS pointers server-side out
  of its own LFS store (ADR 0012), and exposes a Git smart-HTTP gateway so
  stock `git clone`/`fetch` work against it, plus a read-only WebDAV surface
  (ADR 0010) so a machine without FUSE — Finder, Explorer — can browse a
  branch tip with no client software.
- `gfs-fuse` — the host mount daemon. Mounts a pinned commit as a read-only
  base with a writable overlay, lazily fetching blobs from the server and
  caching them locally. The workspace's `.git` is a real directory passed
  through the mount (ADR 0011) over the projected object store, so stock Git
  answers truthfully; the daemon seeds it with a `filter.lfs` configuration
  and an index carrying expanded LFS sizes, so the tree an agent sees is the
  post-`git lfs pull` state, and with the repository's whole visible ref set as
  `packed-refs` — tags peeled, branches as `refs/remotes/origin/*` — so
  `git describe`, `git log origin/main`, and `git status -sb`'s ahead/behind
  answer without a fetch.
- `gfs` (CLI) — the agent-facing tool. Reading: `resolve`, `ls`, `cat`,
  `search` (and `rg`, an `rg`-flag-compatible spelling), `find` (a
  `find`-compatible subset answered from the index, no tree walk), `diff`,
  `show`, `blame`. Workspace lifecycle: `clone`, `mount`, `switch`, `refresh`,
  `unmount`, `status`, `daemon`, `lease`, `install-shim`. Writing back:
  `commit` (to the gateway's fork of the upstream), `push` (upstream, with
  your credential), and `export` (a bundle with a `git apply`-compatible
  patch).

Supporting libraries live under `crates/` (`gfs-git`, `gfs-search`,
`gfs-overlay`, `gfs-mount`, `gfs-service`, `gfs-proto`, `gfs-types`,
`gfs-test`). The full design rationale is in [docs/DESIGN.md](docs/DESIGN.md);
architecture decisions are in [docs/adr/](docs/adr/).

## Shims: the pre-configured tool surface

Inside an agent image, `gfs install-shim` symlinks `git`, `grep`, `find`, and
`rg` in a directory you prepend to `PATH` (ADR 0007, ADR 0009). The shims are a
cost measure, not a security boundary — a tool that calls a binary by absolute
path bypasses them and still gets correct answers, just the expensive ones:

- **`git`** (`gfs-git-shim`) — passes through to real Git by default. It
  delegates `git clone <url>` to `gfs clone` (falling back to real `clone` if
  the flags do not translate or no gateway is reachable), refuses `gc`,
  `repack`, `prune`, `fsck`, and `maintenance` with the reason (each walks the
  whole object store, which through a projection is a monorepo download), and
  notes on `blame` that `gfs blame` answers server-side.
- **`grep` / `find` / `rg`** (`gfs-scan-shim`) — take the cheap route
  themselves: `rg` and `find` become `gfs rg` and `gfs find` with the same
  argv, recursive `grep` is translated conservatively. An unimplemented flag is
  refused by name at parse time, before any output exists, and the shim runs
  the real tool over the mount instead — unsupported invocations work slowly
  rather than failing.
- **`gfs-lfs-filter`** — the daemon-backed `filter.lfs` clean/smudge/process
  driver, installed into `.git/hooks` and seeded into the workspace config by
  the mount rather than by `install-shim`.
- **`gfs-fsmonitor`** — the `core.fsmonitor` hook, answered from the overlay
  journal: every path the workspace changed this generation, plus the paths
  whose journal rows are gone, because a file created and then deleted leaves
  no row and Git trusts what it is not told about.

Outside a GFS workspace every shim passes through silently, which is what makes
a `PATH`-wide install safe; `GFS_SHIM_BYPASS` disables delegation outright.

## How it compares to raw Git

GFS is not a Git replacement — the server stores real Git repositories and
speaks the real Git protocol. It changes *how an agent consumes* a repository:

| Concern | Raw Git | GFS |
| --- | --- | --- |
| Getting started | `git clone` downloads history and checks out every file | Mount a pinned commit; no clone, no checkout, blobs fetched on first open |
| Large monorepos | Partial clone fetches missing blobs one at a time; sparse checkout needs paths known in advance | Whole tree is visible immediately; only touched files are hydrated, with hydration accounting |
| Code search | `git grep` needs a local checkout; searching means materializing the working set | Server-side revision-aware search (trigram index over the pinned snapshot) — no hydration at all |
| Edits | Working tree is the checkout | Copy-on-write overlay over an immutable base; commit to the gateway, push upstream, or export as a patch |
| History | Local, full fidelity | Server-answered `log`/`diff`/`show`/`blame` against the pinned revision |
| Large files | `git lfs pull` smudges every LFS object into each checkout | Server resolves LFS; the tree is born expanded, objects shared per host, no git-lfs in the image |
| Protocol | — | Smart-HTTP gateway: stock `git clone`/`fetch` work unchanged (pointers, as any Git host serves) |

Deliberate divergences from a real checkout (see DESIGN.md section 12):

- **No content filters other than LFS.** The mount serves raw blob bytes:
  `.gitattributes` `text`/`eol`, `core.autocrlf`, and custom clean/smudge
  filters are not applied, so a repository that relies on them presents
  different bytes than `git checkout` would. LFS is the one exception, and it
  is resolved server-side rather than by the mount running a filter.
- **LFS files appear expanded** (ADR 0012), resolved by the server rather than
  by a client-side `git lfs pull`: entry metadata reports the expanded size and
  an `lfs-sha256:{oid}` content key served from the gateway's LFS store, so the
  mount, `gfs cat`, and WebDAV expand without knowing they did. "Expanded" is
  per-entry state — an object the store cannot fetch upstream degrades that one
  entry to its pointer file. Search skips LFS entries with their own `lfs`
  coverage reason. First `open()` of a large LFS file blocks for a whole-object
  fetch and verify; range fetch is not implemented.
- **SHA-1 repositories with the `files` ref backend only**; `reftable` and
  SHA-256 repositories are rejected, not degraded.
- **Linux + FUSE3 only**, case-sensitive paths, whole-blob fetch, single-node
  server storage.

The closest existing options — Git partial clone, sparse checkout/Scalar,
VFS for Git, EdenFS — each solve part of this problem; DESIGN.md section 1
explains why an agent-specific combination (pinned snapshot + COW overlay +
remote revision-correct search + hydration accounting) is the point of the
project.

## Development setup

Prerequisites:

- Rust, pinned by `rust-toolchain.toml` (currently 1.97.1; `rustup` installs it
  automatically)
- `fuse3` and `/dev/fuse` (required for the mount tests and the FUSE half of
  the dev stack)
- Stock Git 2.53.0 (pinned by ADR 0001; the gateway executes it as
  `upload-pack`. A different version produces a warning, not a failure)
- `curl` on the server host, which `gfs-server` runs for LFS batch-API
  transfers (`--curl-binary` / `GFS_CURL_BINARY`)
- Optional: `cargo-deny` and `cargo-cyclonedx` for the license/SBOM check
  stages (skipped with a notice if absent)

`libgit2`, `protoc`, and SQLite are vendored through Cargo, so no host packages
are needed for them.

## Build

```sh
cargo build --workspace          # debug
cargo build --workspace --release
```

Binaries land in `target/debug/` (or `target/release/`): `gfs`, `gfs-server`,
`gfs-fuse`, plus the mount-support helpers `gfs-git-shim`, `gfs-scan-shim`,
`gfs-lfs-filter`, and `gfs-fsmonitor`. The two shims are also built under the
names they answer to — `git`, `grep`, `find`, `rg` — so putting `target/debug`
on `PATH` *is* the pre-configured environment, with no `gfs install-shim` step.
(For the same reason, do not `cargo install --path gfs-fuse`: it would drop a
binary named `git` into `~/.cargo/bin` and shadow the real tool everywhere.)

## Test

```sh
scripts/check.sh                 # the full local gate, same as CI
scripts/check.sh fmt clippy test # or individual stages
```

Stages: `versions`, `fmt`, `clippy`, `test`, `doc`, `deny`, `licenses`, `sbom`,
`secrets`, plus two opt-in ones: `bigtree` (million-entry snapshot test) and
`devstack` (end-to-end stack smoke test). A stage whose tool is missing is
reported as skipped, never as passed.

Plain `cargo test --workspace --all-features` runs the test suite directly.

## Try it locally

```sh
scripts/dev-stack.sh             # seed fixtures, start the server, demo the API
scripts/dev-stack.sh --smoke     # same, then exit (what CI runs)
scripts/dev-stack.sh --big       # also seed the million-entry snapshot
```

The stack builds the workspace, seeds fixture repositories, starts `gfs-server`
on `127.0.0.1:8430` (HTTP) / `127.0.0.1:8431` (gRPC), and demonstrates the main
flows: resolving a revision, reading files without cloning, retention leases,
and — where FUSE is available — mounting a pinned commit and reading through
the mount. With the stack running:

```sh
export GFS_ENDPOINT=http://127.0.0.1:8431 GFS_TOKEN=dev-token
./target/debug/gfs resolve --repo basic main
./target/debug/gfs cat --repo basic --rev main README.md
./target/debug/gfs mount --repo basic --rev main --workspace /tmp/ws
```

To drive a real repository by hand instead — one command for a gateway, then
`git clone` a URL and work the tree with the commands you already know —
follow [docs/manual-test.md](docs/manual-test.md). It walks the whole loop
(clone, search, commit, push), the LFS walkthrough against
`github.com/cbeams/lfs-test`, the WebDAV surface, and the traps that produced
plausible-looking wrong answers during development.

## License

Apache-2.0. Note that the vendored libgit2 is GPL-2.0-only with a linking
exception, statically linked into the binaries — see `licenses/` and
`scripts/check-licenses.sh` for what must ship with a binary distribution.
