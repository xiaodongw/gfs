# ADR 0002: The Git gateway's object-authorization boundary

- Status: Accepted
- Date: 2026-07-26
- Milestone: M0.3
- Amends: DESIGN.md sections 7.1 and 7.3
- Evidence: `spikes/gateway-probe` check `unadvertised_want`

## Context

DESIGN.md section 7.1 states, of retention leases:

> Snapshot calls for a commit that is no longer reachable from a public ref must
> present that capability; repository access alone does not grant access to
> every commit retained for another mount.

The retention-lease design anchors a mounted commit under `refs/xvfs/mounts/*`,
hides that namespace from `upload-pack` with `transfer.hideRefs`, and disables
`uploadpack.allowAnySHA1InWant`. The intent is that a repository reader cannot
reach another mount's retained commit.

M0.3 tested whether that holds. It does not.

## Measurement

With `transfer.hideRefs=refs/xvfs/`, `uploadpack.allowAnySHA1InWant=false`,
`uploadpack.allowReachableSHA1InWant=false`, and
`uploadpack.allowTipSHA1InWant=false` on Git 2.53.0:

| Object | protocol v0 | protocol v2 |
| --- | --- | --- |
| reachable only from a hidden `refs/xvfs/` ref | refused | **served** |
| reachable from no ref at all (dangling) | refused | **served** |

Control, varying only the configuration, with a dangling commit:

| `uploadpack.allowAnySHA1InWant` | v0 | v2 |
| --- | --- | --- |
| unset (Git default) | refused | **served** |
| `true` | served | served |
| `false` | refused | **served** |

The setting is honoured in protocol v0 and is not consulted in protocol v2. In
v2, any object present in the repository's object database is retrievable by a
client that knows or guesses its object ID, and the fetched object is fully
readable — the probe reads the file content back out of the client's clone.

Hiding a ref prevents **discovery**. It does not prevent **access**.

This is not specific to the XVFS gateway: it reproduces identically over plain
`file://` transport with stock Git on both ends.

## Decision

**One bare repository is one authorization domain.** Any subject authorized to
read a repository is authorized to read every object in that repository's object
database, including objects that are unreachable, force-pushed away, or retained
only by another subject's mount lease.

Three things follow.

1. **DESIGN.md's claim is corrected, not implemented.** The sentence quoted
   above is true of the snapshot/search API, where XVFS controls authorization,
   and false of the Git gateway path, where stock `upload-pack` does. It must be
   restated with that scope.

2. **Mount capabilities remain required on the snapshot API.** They still do
   real work: they stop the *XVFS* API from serving an unreachable commit to a
   caller who has not been issued a lease, and they are what binds a mount to a
   subject, repository, commit, and expiry. They are simply not a defence
   against a Git client on the same repository.

3. **Multi-tenancy is drawn at the repository, never inside one.** Two subjects
   with different object-visibility rights must not share a bare repository.
   This rules out a design where one mirror holds several tenants' refs.

## Alternatives considered

**Reachability-check every `want` in the gateway.** The gateway would parse the
v2 `fetch` request and reject any object ID not reachable from a ref the caller
is authorized to see. This closes the hole with stock upload-pack, but a
reachability check per want on a repository the size of the Linux history is
expensive on the negotiation path, and it must be correct against shallow,
partial, and `deepen-*` requests or it becomes a denial-of-service on ordinary
clones. Rejected for the MVP; recorded as available hardening if a deployment
ever needs object isolation inside one repository.

**Refuse protocol v2 at the gateway.** Forcing v0 restores enforcement, and v0
is measurably the safer protocol here. Rejected: it gives up `ls-refs`,
efficient ref filtering, and partial clone — the last of which M0.5 may make
load-bearing — to defend a boundary this ADR concludes is not needed inside a
single-tenant repository. Reconsider only if per-object isolation becomes a
requirement.

**Keep lease anchors out of the shared object database.** For example, copy the
retained commit into a per-mount repository. This defeats the point of the
lease, which is to keep the *existing* objects reachable without duplicating a
monorepo's worth of data.

## Consequences

- The threat model gains an explicit, tested statement: repository read access
  implies object-database read access, regardless of reachability.
- M1.5's requirement that "another repository reader must not gain access merely
  because an internal lease retains the object" is **not achievable** through the
  Git path and must be re-scoped to the snapshot/search API.
- The practical exposure is bounded and worth stating plainly: what leaks is a
  commit that was, until recently, on a branch of the same repository the caller
  can already read. It is the same exposure ordinary Git has between a force
  push and the next `gc`. It is a real change to a documented guarantee, not a
  practical escalation for the pilot's single-tenant repositories.
- `spikes/gateway-probe` reports this as `FINDING`, not `FAIL`, so a genuine
  regression elsewhere is not buried under a known result. If the outcome ever
  changes — Git enforcing the setting in v2 — the check fails loudly with
  `unexpected leak profile`, which is the correct time to revisit this ADR.
