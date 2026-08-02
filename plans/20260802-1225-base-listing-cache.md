# A daemon-side listing cache for the pinned tree

## Summary

Live measurement against `~/.gfs-lab/flask`: every *warm* `git status` costs
~15 server round trips (+6 `list_directory`, +9 `get_entry` NOT_FOUND per run,
recurring every prompt redraw, forever). The daemon caches blob *content* only;
all metadata questions lean on the kernel cache, which has two structural
holes: FUSE never caches readdir unless the filesystem opts in (every
`opendir` → daemon → server), and negative dentries expire after 1 s (every
`.gitignore` probe repeats). Yet the pinned commit is immutable: one complete
listing of a directory decides every question about that directory — children,
attributes, and *definitive* negatives — for the life of the pin.

## Plan

**Phase 1 — the cache** (`crates/gfs-mount/src/listing.rs`, new)
* `Listing`: a complete directory's `TreeEntryInfo`s plus a by-name index.
* `ListingCache`: `dir path → Arc<Listing>`, LRU-capped
  (`FsConfig::listing_cache_dirs`, default 4096 directories) so a monorepo
  walk cannot pin the whole tree's metadata — the concern that rejected
  manifest pinning in ADR 0009.
* The cache lives **inside `Pinned`**, not beside it. A repin swaps `Pinned`
  wholesale, so the cache is born empty with the new commit and a fetch
  against the old client can never insert into the new generation — the
  stale-insert race is structurally impossible rather than locked away.

**Phase 2 — rewiring** (`crates/gfs-mount/src/fs.rs`)
* One method, `base_listing(pinned, dir)`: cache hit or page-to-completion
  fetch (existing retry + `directory_pages` accounting), then insert.
* `resolve_path`'s `Resolution::Base` arm answers from the parent's listing —
  positives and negatives both, zero server calls warm. The root (no parent)
  keeps `get_entry`.
* `base_child_names`, `fill_directory`, `base_descendants` all read the
  listing. `DirState` loses its paging fields (`next_page_token`,
  `base_done`); the listing is complete by construction.
* `open_blob` keeps `get_entry(want_ticket = true)`: a blob ticket is
  short-lived server state, not metadata.
* New counter `FsStats::listing_hits`.
* Kernel TTLs unchanged: the 1 s negative TTL still guards overlay-created
  names; what changes is that the re-ask is now answered locally.

**Phase 3 — verification** (`crates/gfs-mount/tests/`)
* `a_warm_metadata_walk_never_reaches_the_server`: real FUSE mount, full
  recursive walk + probes of absent names, snapshot `FsStats`, walk again
  with *fresh* absent names (fresh so the kernel's negative cache cannot mask
  the daemon's), assert `metadata_requests` and `directory_pages` deltas are
  exactly zero.
* Overlay-over-cache precedence and repin freshness ride on the existing
  suites (mutations, switch): the overlay is consulted before the base in
  every rewired path, and the cache cannot survive a repin by construction.
* Build, `cargo test --workspace --all-features`.

**Phase 4 — docs.** DESIGN.md section 8.2 (the kernel-cache paragraph now
states why it is not enough and what the daemon adds); `docs/manual-test.md`
gains the warm-status-is-silent check, observable both from the server's
`gfs_requests_total` counters and from `gfs inspect`'s new `metadata` line
(README has no client-cache section to amend). `gfs inspect` previously
printed none of the `FsStats` counters — the gap that forced the original
diagnosis through server metrics — so `print_report` now shows
`metadata_requests`, `directory_pages`, and `listing_hits`.

Built as planned. `a_warm_metadata_walk_never_reaches_the_server` was checked
to fail against a cache whose `get` always misses (the pre-change behaviour:
warm `directory_pages` kept climbing). Full workspace suite green.

## Decisions

* **Cache listings, not synthesized attributes.** Inode numbers are assigned
  by the never-pruned `by_path` table and must stay stable per path for the
  life of the mount (git's index records them; a changed ino means a re-hash
  and a hydration). The cache therefore stores raw `TreeEntryInfo` and lets
  the existing lookup path assign numbers — serving from cache is
  indistinguishable from serving from the server. (`core.checkStat = minimal`
  is the second, independent guard.)
* **Negatives are answered from a complete listing, not from a TTL.** Against
  an immutable pin, "name ∉ listing" is permanently correct; the overlay is
  consulted first in every path, so names the overlay creates are never
  shadowed by the base's answer.
* **Lookup fetches the whole parent listing rather than one `get_entry`.**
  A cold deep lookup pays one listing per ancestor — the same count the
  kernel's component walk already forces — and every later question about
  those directories is free. Git workloads readdir every directory anyway.
* **No single-flight.** Two concurrent misses on one directory fetch twice
  and the second insert wins; both results are identical against an immutable
  pin. A dedup layer is complexity without a correctness payoff at prototype
  stage.
* **`ls`-style eviction, O(cap) scan.** At 4096 slots an occasional linear
  scan for the least-recently-used slot is noise; no new dependency.
