# ADR 0001: libgit2 and stock `upload-pack` integration boundary

- Status: Accepted
- Date: 2026-07-26
- Milestone: M0.3
- Supersedes: nothing
- Evidence: `spikes/git-probe`, `spikes/gateway-probe`,
  `spikes/reports/m03-git-integration.md`

## Context

DESIGN.md section 5.1 accepted libgit2 via `git2-rs` for repository access and
stock `git upload-pack` behind a Rust gateway for clone/fetch. M0.3 was tasked
with confirming that the two agree on the repositories XVFS intends to host, and
with establishing the supported-format boundary explicitly rather than by
assumption.

## Decision

Adopt the accepted stack with the following pinned versions and an explicitly
narrower supported-repository boundary than DESIGN.md originally implied.

### Pinned versions

| Component | Version | Notes |
| --- | --- | --- |
| libgit2 | 1.9.6 | vendored via `libgit2-sys`, not the host's |
| `libgit2-sys` | 0.18.7+1.9.6 | `vendored` feature |
| `git2-rs` | 0.20.4 | `default-features = false` + `vendored-libgit2` |
| stock Git | 2.53.0 | server-side `upload-pack` |
| Rust | 1.97.1 | `rust-toolchain.toml` |

libgit2 is **vendored deliberately**. The supported-format boundary below is a
property of the exact build, so the build has to be ours. A host-provided
libgit2 would let a distribution upgrade silently change which repositories the
server accepts.

`default-features = false` drops `git2`'s `https` and `ssh` features. The server
never uses libgit2 as a network client — fetching is stock Git's job and serving
is the gateway's — so linking OpenSSL and libssh2 into the object path would add
attack surface for no capability.

### Licensing and packaging

The crate metadata is **misleading for this stack** and must not be used on its
own. `libgit2-sys` declares `MIT OR Apache-2.0`, but that covers the Rust
wrapper; the vendored C library it compiles and statically links is GPL-2.0.

| Component | License | How it is combined |
| --- | --- | --- |
| **libgit2 1.9.6** (vendored C) | **GPL-2.0-only WITH the libgit2 linking exception** | statically linked into the server binary |
| ├ bundled zlib | Zlib | linked |
| ├ bundled PCRE2 | BSD-3-Clause | linked |
| └ bundled Clar | ISC | test-only, not shipped |
| `libgit2-sys` 0.18.7 | MIT OR Apache-2.0 | Rust wrapper only |
| `git2` 0.20.4 | MIT OR Apache-2.0 | |
| `fuser` 0.18.0 | MIT | |
| `tantivy` 0.25.0 | MIT | |
| **stock Git 2.53.0** | **GPL-2.0** (mixed: GPL-2+, GPL-2, LGPL-2.1+, Expat, ISC) | **separate process, never linked** |
| remaining 215 crates | MIT / Apache-2.0 / Unicode-3.0 / BSD / Zlib / Unlicense | permissive |

Two facts carry the packaging decision:

1. **libgit2's linking exception is what makes static linking safe.** Its
   `COPYING` grants "unlimited permission to link the compiled version of this
   library into combinations with other programs, and to distribute those
   combinations without any restriction coming from the use of this file." So
   linking libgit2 into an XVFS binary under a different license is permitted.
   Modifying libgit2 itself is still governed by GPL-2.0, so **the vendored
   libgit2 must not be patched** without accepting that obligation. If a patch
   ever becomes necessary, that is a licensing decision, not a build decision.

2. **Stock Git is executed, never linked.** DESIGN.md section 7.2's choice to
   run `upload-pack` as a sandboxed child process is, in addition to its
   security rationale, what keeps Git's GPL-2.0 off the XVFS binary: process
   invocation is not a derived work. This is a further reason not to
   reimplement upload-pack by linking Git's internals.

Packaging obligations:

- ship libgit2's `COPYING` (including the linking exception and the bundled
  zlib/PCRE2 notices) with any binary distribution;
- distribute stock Git as its own package or container layer with its own
  license text and a source offer, not vendored into the XVFS binary;
- one crate offers `MIT OR Apache-2.0 OR LGPL-2.1-or-later` — select MIT or
  Apache-2.0 explicitly in the SBOM so no LGPL obligation is inherited by
  default;
- M1.1's license-check and SBOM tooling must assert the libgit2 row above
  directly, because scanning crate metadata alone reports this stack as fully
  permissive and misses the GPL-2.0 C library entirely.

### Supported repository formats

| Format | Verdict | Basis |
| --- | --- | --- |
| `files` ref backend | Supported | measured |
| `reftable` ref backend | **Rejected at ingest** | libgit2 1.9.6: `unsupported extension name extensions.refstorage` |
| SHA-1 objects | Supported | measured |
| SHA-256 objects | **Rejected at ingest** | see ADR consequence below |
| unrecognized `extensions.*` | **Rejected at ingest** | an unknown extension means unknown on-disk meaning |

Rejection happens at mirror creation, in `gitrepo::verdict`, and is validated
against stock Git's `rev-parse --show-ref-format` / `--show-object-format`. The
gate reads `config` directly rather than relying on libgit2 to refuse, because
it must produce a verdict even for repositories libgit2 cannot open at all.

### SHA-256 is further away than DESIGN.md states

DESIGN.md section 5.1 says libgit2's SHA-256 support "is experimental and
requires a non-default build". Measurement shows the situation is worse for
XVFS, because XVFS reaches libgit2 through `git2-rs`:

- `libgit2-sys` 0.18.7 **does** build with `unstable-sha256`
  (`GIT_EXPERIMENTAL_SHA256`), and `GIT_OID_MAX_SIZE` becomes 32.
- `git2` 0.20.4 **does not compile** against that build: 75 errors, every one a
  reference to `GIT_OID_RAWSZ` or `GIT_OID_HEXSZ`, which `libgit2-sys` gates out
  under `#[cfg(not(feature = "unstable-sha256"))]`.

So SHA-256 is not merely experimental — it is **unreachable through the safe
wrapper**, and would require raw FFI against a second, ABI-incompatible build.
`spikes/git-probe/sha256-support-check.sh` is the reproducer and exits non-zero
if the situation changes; run it on every `git2`/`libgit2-sys` bump.

**Consequence for the pre-production SHA-256 commitment** (DESIGN.md section
12): it is not achievable by "libgit2 support maturing" alone. `git2-rs` must
also gain support. Until both land, SHA-256 hosting is declared out of scope
rather than silently unsupported.

### Handle and blocking model

`git2::Repository` is `Send` but not `Sync`, and every libgit2 call is
synchronous and can block on a cold pack read. The model is therefore:

- a **bounded pool** of repository handles per repository (`RepoPool`);
- one handle **checked out for the life of one request**, never shared;
- the bound is admission control, not just reuse: when every handle is busy a
  caller waits rather than adding another concurrent pack read;
- all libgit2 borrows end in an inner scope before the handle returns to the
  pool, which the FFI lifetimes enforce at compile time.

### Partial-clone filter policy (frozen)

Exactly `blob:none`. Enforced in two independent places because they have
different granularity:

1. **Git configuration** — `uploadpack.allowFilter=true`,
   `uploadpackfilter.allow=false` as the default, then
   `uploadpackfilter.blob:none.allow=true`, with `blob:limit`, `tree:depth`,
   `sparse:oid`, `object:type`, and `combine` each denied by name.
2. **Gateway request validation** — the `filter` pkt-line is parsed and compared
   to the exact string `blob:none`.

The second exists because the first cannot express the policy: the subsection
name granularity means allowing `blob:none` is as fine as Git gets, and
`blob:limit=<n>` is a separate key that a future config change could flip.

Note the subsection name contains a colon: `uploadpackfilter.blob:none.allow`.
Writing `uploadpackfilter.blob.none.allow` parses as subsection `blob`, key
`none.allow`, which Git ignores silently; the only symptom is upload-pack
rejecting `blob:none` as "not supported" while the configuration reads
correctly. This cost real debugging time and is recorded so it costs none again.

### Subprocess sandbox

- absolute executable, no shell, no user-controlled path, argument, or cwd;
- `env_clear()` then an allow-listed environment;
- `Git-Protocol` is parsed and **reconstructed**, never forwarded verbatim;
- `GIT_EXEC_PATH` must be set explicitly — with a cleared environment,
  upload-pack cannot fork `git-pack-objects` and fails only once a real client
  gets past the advertisement;
- `uploadpack.packObjectsHook` must be left **unset**, not set to the empty
  string: Git treats empty as a command and fails with `cannot run :`.

## Alternatives considered

**Reimplement upload-pack on libgit2.** Rejected, as DESIGN.md 7.2 argued:
capability advertisement, negotiation, filtering, reachability security, and
sideband streaming are a large compatibility surface, and getting reachability
security subtly wrong is a data-leak bug rather than a compatibility bug.

**Use the host's libgit2.** Rejected: makes the supported-format boundary a
property of the deployment environment.

**Convert `reftable` mirrors to `files` on ingest.** Deferred, not rejected.
Cheap to do with stock Git, but it makes XVFS's mirror diverge from upstream in
a way that has to be maintained on every fetch. Revisit if a target repository
actually uses `reftable`; DESIGN.md open question 9 tracks it.

## Consequences

- The `GitRepository` trait is a thin wrapper, not a compatibility project:
  libgit2 agreed with stock Git on every check across the fixture matrix and all
  three corpus mirrors, including 101,052 Linux tree entries compared for path
  bytes, mode, and object ID.
- SHA-256 must be removed from the pre-production commitment or re-scoped to
  depend on `git2-rs`, not just libgit2.
- The object-authorization boundary needs its own decision; see ADR 0002.
