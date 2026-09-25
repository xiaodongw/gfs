# Phase A Corrections: Local-Mode Index Cache

## Summary

Phase 2 of the local-mode effort implemented per-commit index caching with design issues:
writes to the clone's `.git/gfs/indices/` directory (wrong for bare/linked repos),
unbounded cache growth (violates the bound that exists to prevent), and no crash safety
(temp file writes without atomic rename or verification). Phase A implements the corrections:
move the cache to `~/.cache/gfs/indices/<repo-id>/`, implement LRU eviction (8 entries max),
crash-safe writes with SHA-1 verification, hardlink-or-copy into workspace `.git/index`,
and parallel index build to reduce cold mount time.

## Plan

Completed implementation includes:
1. Index cache moved from `~/.clone/.git/gfs/indices/` to `~/.cache/gfs/indices/<repo-id>/`
   - Correct for all repository types (bare, linked, regular clones)
   - Survives clones copied to other machines (OID space immutable)

2. LRU eviction: keeps 8 most recent indices per repository by mtime, touches on cache hit

3. Crash-safe writes:
   - Temp file + fsync + atomic rename on cache write
   - SHA-1 trailer verification on read (git index files end with 20-byte SHA-1)
   - Cache misses on verification failure (corrupted files removed)

4. File installation:
   - `link_or_copy_index()` tries hardlink first (same filesystem), falls back to copy
   - Handles both same-filesystem and cross-filesystem cases

5. Parallel index build:
   - Serial tree descent to maintain Git's index order (byte-wise by full path)
   - Deferred header reads collected during descent
   - Parallelized via `std::thread::scope` across `min(threads, DEFAULT_REPO_HANDLES)` threads
   - Byte-identical output verified on universe

## Decisions

* **Cache location: `~/.cache/gfs/indices/<repo-id>/`** follows XDG Base Directory spec
  (with XDG_CACHE_HOME override). Per-repository subdirectories prevent collisions.
  
* **LRU bound at 8 entries**: balances overhead (8 × ~200 MiB for universe = ~1.6 GiB)
  with reuse frequency. Eviction by mtime on put, not on hit.

* **Crash-safe writes mandatory**: temp file + fsync + rename prevents partial writes
  and mount-time corruption. SHA-1 verification treats failures as cache miss.

* **Hardlink-or-copy strategy**: hardlink is atomic within a filesystem but fails
  cross-filesystem; copy is slower but always works. No performance optimization
  since index install is negligible next to tree descent.

* **Parallel index build stays serial-descent-first**: maintains Git's index order,
  which is cryptographically important (index hash depends on order). Only header
  reads are parallelized; LFS checks remain sequential (rare case).

## Details

* Index cache directory structure: `~/.cache/gfs/indices/local-<hex>/<commit-oid>.index`
* Each index file ends with a 20-byte SHA-1 hash of preceding content (Git format)
* Cache entry keyed by `(repository_id, commit_oid)` in `BTreeMap` for LRU tracking
* Parallel build uses `std::thread::scope` directly within libgit2 sync context
  (PooledRepo keeps odb alive for header reads)

## Testing

* All unit tests pass (113 in gfs-service, 41 in gfs-mount, 7 in gfs-git)
* Local integration tests pass (4 tests in mount/tests/local.rs)
* Cache structures verified in `~/.cache/gfs/indices/` after test runs
* Index files are small (433 bytes for test repos, ~200 MiB for universe)
* No regression in other mount modes (overlay, server)

## Future work

- Measure cold/warm create on universe with full dataset
- Verify byte-identical indices across multiple cold runs
- Consider per-workspace index copies for seeding FSMN/UNTR fsmonitor ident
- Profile parallel build speedup on universe (current: deferred but not measured)
