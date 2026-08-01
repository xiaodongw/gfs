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
  queries (log, diff, show, blame), and exposes a Git smart-HTTP gateway so
  stock `git clone`/`fetch` work against it, plus a read-only WebDAV surface
  (ADR 0010) so a machine without FUSE — Finder, Explorer — can browse a
  branch tip with no client software.
- `gfs-fuse` — the host mount daemon. Mounts a pinned commit as a read-only
  base with a writable overlay, lazily fetching blobs from the server and
  caching them locally. It synthesizes a minimal read-only `.git` surface so
  tools that probe for a repository root still work.
- `gfs` (CLI) — the agent-facing tool. Reading: `resolve`, `ls`, `cat`,
  `search` (and `rg`, an `rg`-flag-compatible spelling), `diff`, `show`,
  `blame`. Workspace lifecycle: `clone`, `mount`, `switch`, `refresh`,
  `unmount`, `status`, `daemon`, `lease`, `install-shim`. Writing back:
  `commit` (to the gateway's fork of the upstream), `push` (upstream, with
  your credential), and `export` (a bundle with a `git apply`-compatible
  patch).

Supporting libraries live under `crates/` (`gfs-git`, `gfs-search`,
`gfs-overlay`, `gfs-mount`, `gfs-service`, `gfs-proto`, `gfs-types`,
`gfs-test`). The full design rationale is in [docs/DESIGN.md](docs/DESIGN.md);
architecture decisions are in [docs/adr/](docs/adr/).

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
| Protocol | — | Smart-HTTP gateway: stock `git clone`/`fetch` work unchanged |

Deliberate divergences from a real checkout (see DESIGN.md section 12):

- **No content filters.** The mount serves raw blob bytes; `.gitattributes`
  `text`/`eol`, `core.autocrlf`, and clean/smudge filters are not applied.
- **LFS files appear as pointer files** — LFS content is not resolved.
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
and `gfs-fsmonitor`.

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

## License

Apache-2.0. Note that the vendored libgit2 is GPL-2.0-only with a linking
exception, statically linked into the binaries — see `licenses/` and
`scripts/check-licenses.sh` for what must ship with a binary distribution.
