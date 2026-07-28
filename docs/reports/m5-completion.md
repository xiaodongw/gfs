# M5 — Git smart HTTP compatibility: completion report

Date: 2026-07-26  
Milestone: M5 (PLAN.md section 8)  
Status: **Complete**, with one recorded gap (the client-version matrix) and one
deliberate omission (a process memory limit) carried forward.

M1–M4 built the GFS-native path: a pinned snapshot API, a lazy mount, a
crash-safe overlay, and revision-aware search. None of it speaks Git's wire
protocol, so a repository GFS serves was reachable only by GFS. M5 adds
`git clone` and `git fetch` over smart HTTP without weakening any boundary the
earlier milestones established.

The implementation is the one ADR 0001 accepted and DESIGN.md section 7.2
describes: a Rust gateway that authenticates, authorizes, limits, and streams to
a sandboxed stock `git upload-pack` child. Git owns the protocol. GFS owns
everything around it.

## The exit gate

PLAN.md section 8 states two criteria.

| # | Criterion | Verified by | Result |
| --- | --- | --- | --- |
| 1 | Stock Git clone/fetch and partial-clone tests pass the declared version/feature matrix | `gfs-server/tests/gateway.rs`, corpus measurement below | **Met for the feature matrix; the *version* row is not covered** — see gaps |
| 2 | Git traffic cannot bypass the same repository authorization used by GFS APIs | `gfs-server/tests/gateway.rs` | **Met** — the gateway calls `Authorizer::authorize_repository`, the same function every other surface calls |

### Criterion 1: measured on the worst case

The protocol matrix runs against fixtures; the numbers below are one release
server against `linux.git` from the M0.1 corpus (2 870 refs, 6.4 GiB of
objects), cloned by stock Git 2.53.0 over protocol v2 on loopback.

| | Wall time | Transferred | Server peak RSS |
| --- | ---: | ---: | ---: |
| v0 advertisement (244 KiB) | 0.4 s | 244 KiB | 13 MiB |
| `--depth 1` | 14.5 s | 292 MiB | 14 MiB |
| `--no-checkout --filter=blob:none` | 161.4 s | 2.0 GiB | 14 MiB |
| full bare clone | 349.9 s | 6.4 GiB | **14 MiB** |

**The RSS column is the result, not the wall times.** The M0.3 probe buffered
whole responses, which is why streaming was M5.1's scope; a server that peaks at
14 MiB while a client pulls 6.4 GiB through it is the evidence that it now
streams. The bound is structural rather than tuned: bytes cross a 16-slot
channel of at most 64 KiB each, so a slow client fills the channel, the pump
stops draining the child's stdout, and the child blocks on its own pipe.

The full clone was verified independently rather than by exit status:
`git fsck --connectivity-only` is clean, `HEAD` equals the bare repository's
`3dab139d`, and the clone holds 939 tags plus one branch — exactly the
heads-and-tags set `git clone --bare` fetches, with none of the mirror's 1 930
`refs/pull/*` and **zero** `refs/gfs/*`.

### Criterion 2: the same function, and the boundary that is not claimed

`gateway::resolve` calls `Authorizer::authenticate`, then
`Authorizer::authorize_repository`, then `Registry::require_servable` — the
identical sequence the snapshot, blob, and search surfaces use. M1.5's "enforce
repository permissions uniformly" is only true if it is literally the same
function, so it is. The consequences are asserted: an outsider with a valid
credential gets `404`, not `403`, because a distinct status would answer the
existence question; an absent repository is indistinguishable from an
unauthorized one; and a missing credential gets `401` with a `WWW-Authenticate`
challenge, so a client with a credential helper can retry.

**Object authorization is not claimed here, deliberately.** ADR 0002 records the
M0.3 measurement: protocol v2 serves any object in a repository's object
database by object ID regardless of `uploadpack.allowAnySHA1InWant`. One bare
repository is one authorization domain. PLAN.md M1.5 says not to write an
acceptance test expecting the Git path to deny it, and none was written.

What *is* enforced is discovery: a live lease anchor is absent from the v0
advertisement and from v2 `ls-refs`, and stays absent when the repository's own
configuration appends `!refs/gfs/` to `transfer.hideRefs`.

## Findings

### 1. `--filter=blob:none` checks out, and checking out defeats the test

The obvious assertion for "the filter worked" is that the clone is missing
blobs. It is not, because `git clone --filter=blob:none` performs a checkout and
checking out a partial clone lazily fetches every blob in the working tree. The
filtered and unfiltered cases end up byte-identical on disk, and the test passes
whether or not the server filtered anything.

Two flags are load-bearing as a result: `--no-checkout` on the clone, and
`GIT_NO_LAZY_FETCH=1` on the check, or the question fetches its own answer.

### 2. Withdrawing a capability is advice; the client degrades silently

With `uploadpack.allowFilter=false` the advertisement carries no `filter`
capability — and a stock client responds by cloning in full, exiting 0, and
still writing `remote.origin.partialclonefilter=blob:none` into its own config
from the command line. Neither the exit code nor that config says what the
server did.

So the advertisement is not the enforcement point. The gateway validates the
exact filter spec out of the request body before spawning a child, and a
hand-crafted request carrying `filter blob:none` against a filter-disabled
server is refused `403` even though nothing advertised it. Both halves are
tested; neither is redundant.

### 3. `RLIMIT_AS` would break the repositories GFS exists to serve

PLAN.md M5.3 lists a memory limit. There is none, and the reason is measured:
`upload-pack` mmaps the repository's packfiles and mapped pack bytes count
against `RLIMIT_AS`. `linux.git`'s pack is 4.5 GiB, so any address-space limit
small enough to be a backstop is small enough to break a clone of the worst case
in the corpus.

Memory is bounded by the container's cgroup — the mechanism ADR 0003's
deployment model already relies on. `RLIMIT_CPU` *is* applied, through `prlimit`,
because `pre_exec` is unavailable under this workspace's `unsafe_code = "deny"`
and a helper that execs the target directly is not a shell. When `prlimit` is
absent the invocation proceeds without it: the wall-clock and inactivity
deadlines still bound the child, and refusing to serve Git because a util-linux
binary is missing would be the worse failure.

### 4. A blocking Git client on a current-thread runtime is a clean hang

Every test in the suite drives a real `git` process while the server runs in the
same process. On tokio's default current-thread runtime, `Command::output()`
parks the only executor thread, the server can never answer the request the
client is waiting for, and the test hangs with no output and no failure. Every
test is therefore `multi_thread`, and the reason is written at the top of the
file rather than left to be rediscovered.

### 5. The advertisement scanner cannot run on the RPC response

`transfer.hideRefs` is a list, and a repository's own configuration can append a
negating `!` entry. Command-line `-c` wins, and a test proves it — but a
configuration-only defence for "another job's retained commit must not be
discoverable" is thin, so `pkt::AdvertisementScanner` re-checks the outgoing
bytes and aborts the response if a reserved ref appears.

It runs on `GET /info/refs` and on nothing else. A `POST /git-upload-pack`
response carries a **packfile**, whose bytes are arbitrary repository content: a
blob containing the ASCII `refs/gfs/` is ordinary — the gateway's own source
files contain it — and a scanner over pack bytes would abort legitimate clones.
The advertisement is pkt-lines of ref names and capabilities and never carries
object content, which is what makes the check safe there and unsafe elsewhere.

## What the sandbox is, concretely

Everything the gateway may decide about the child is decided in
`gateway::upload_pack` and returned as data, so a test asserts on it rather than
on behaviour that could regress silently.

| Control | Setting | Why it is not left to Git |
| --- | --- | --- |
| Executable | `git`, absolute argv, no shell | argv is built from constants and a catalog path; nothing from the request reaches it |
| Working directory | `/` | a relative path anywhere in a request cannot resolve to something useful |
| Environment | `env_clear` plus a 7-entry allow-list | an inherited `GIT_CONFIG_*` or `GIT_ALTERNATE_OBJECT_DIRECTORIES` would defeat the sandbox |
| `GIT_PROTOCOL` | reconstructed from a recognized `version=2`, never forwarded | the header is attacker-controlled and colon-separated |
| Filters | `uploadpackfilter.allow=false` plus one explicit allow, **and** gateway validation of the exact spec | Git's per-family granularity cannot express "`blob:none` but not `blob:limit`" |
| Unadvertised wants | all three `allow*SHA1InWant` false | v0 honours them; v2 does not (ADR 0002), and they are set for the Git that fixes it |
| Hidden refs | `transfer.hideRefs` plus the outgoing scanner | repository config can append a negating entry |
| Hooks | `core.hooksPath=/dev/null`; `packObjectsHook` left **unset** | an empty value is treated as a command and fails with `cannot run :` |
| Repository selection | a parsed `RepositoryId` looked up in the catalog | no request component is ever joined onto a filesystem path, so there is no traversal to normalize |
| Concurrency | a process-wide semaphore, `try_acquire` | an unbounded clone count is an unbounded `pack-objects` count |
| Time | wall-clock and inactivity deadlines | a large clone is slow but never silent |
| Bytes | request cap, gzip output cap, gzip ratio cap, output cap | neither compressed nor decompressed bound implies the other |
| stderr | drained concurrently, capped, redacted | an unread stderr pipe fills at 64 KiB and blocks the child forever |

`git config uploadpack.packObjectsHook` set in the served repository is ignored
by Git itself — it documents this as a safety measure against fetching from
untrusted repositories — and the suite asserts it rather than trusting it.

## Test inventory

35 tests are M5's: 15 unit tests in `crates/gfs-service/src/gateway/` and 20 protocol
tests in `gfs-server/tests/gateway.rs`. Every test that transfers objects
verifies the result against a **direct filesystem clone** of the same bare
repository and runs `git fsck` on it, because the gateway's protocol engine is
stock `upload-pack` and cannot be its own oracle.

| Area | Cases | Covers |
| --- | ---: | --- |
| `gateway::pkt` | 5 | framing, malformed lengths, chunk-boundary independence at every split point, a leaked anchor in a ref line and in a capability |
| `gateway::upload_pack` | 8 | protocol-header allow-listing, filter policy, the protected configuration line by line, the environment allow-list, gzip bomb by ratio, request validation, argv shape, stderr redaction |
| `gateway` HTTP | 2 | both credential schemes Git can send; padded base64 against the url alphabet |
| Framing | 2 | byte-exact v0 preamble, v2 starting at `version 2`, content types, cache headers, `git-receive-pack` refused, dumb HTTP refused |
| Transfer | 3 | clone over v0 and v2, shallow, `blob:none`, fetch after the branch moves, tree equality against a direct clone |
| Authorization | 1 | missing credential challenged, outsider and absent repository both masked as `404`, uncredentialed clone refused |
| Filters | 2 | advertised exactly when enabled, degradation when withdrawn, `403` on an unadvertised filter line, four denied filter families |
| Repository shapes | 4 | packed, 16 MiB content, 5 000-entry directory, non-UTF-8 paths, 40-deep paths, empty/unborn HEAD, alternates-based, corrupt object |
| Isolation | 3 | reserved namespace under v0 and v2 with a live lease, hostile repository config, formats ADR 0001 refuses at ingest |
| Limits and abuse | 4 | gzip round-trip and bomb, a fixed malformed-input corpus over bodies/names/headers, admission refusal, disconnect mid-transfer |
| Version matrix | 1 | every binary named by `GFS_GIT_CLIENTS`; **skips loudly when unset** |

The malformed-input sweep is a fixed corpus rather than a random one: a
randomized sweep that fails is not reproducible, and every shape in it — bad
pkt-line lengths, truncated payloads, NUL runs, traversal repository names,
oversized and hostile `Git-Protocol` values — is one a real client or a real
attacker produces. It asserts no `500`, and then clones successfully to prove
the server survived.

## Recorded gaps

**The client-version matrix is not covered.** PLAN.md M5.2 asks for multiple
maintained Git client versions on Linux and at least one other OS. Only the
pinned 2.53.0 is installed here. `every_configured_git_client_version_clones`
runs whatever `GFS_GIT_CLIENTS` names and prints a skip notice otherwise, so
the absence is visible in test output rather than implied by a passing run. This
matters more than it looks: ADR 0002's v0/v2 split is version-sensitive and the
ADR already says it must be re-measured per client and server version. **Close
before M6.1.**

**Ref-in-want is not implemented or tested.** PLAN.md M5.2 lists it as "if
selected"; it is not selected. `want-ref` naming the reserved namespace is
rejected by name, which is the only part that is a policy question.

**The promisor-remote row does not apply.** M0.5 selected the synthesized
`.git`, so the mount does not need the gateway as a promisor remote. The
`blob:none` clone test does exercise the gateway in that role for an ordinary
Git client, which is the part that generalizes.

**Maintenance coordination is deferred to M7.2.** PLAN.md M5.3 asks that active
libgit2 readers and upload-pack processes never observe partially replaced
packs. Nothing in this milestone runs `git gc` concurrently with a transfer;
`RepositoryLocks` exists and M7.2 owns the coordination across nodes, where the
problem actually lives. Recorded rather than silently skipped.

**A memory limit is absent by decision, not oversight.** See finding 3.

**The corpus is still the public stand-in set.** Every number above moves when
`spikes/corpus/corpus.conf` is pointed at the real monorepos. Open since M0.1.
