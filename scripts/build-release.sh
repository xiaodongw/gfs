#!/usr/bin/env bash
#
# Reproducible release build.
#
# PLAN.md M1.1 asks for reproducible release builds. The reason is concrete
# rather than aspirational: ADR 0001 makes the supported-repository-format
# boundary a property of the exact vendored libgit2 build, and DESIGN.md section
# 10 requires signed artifacts with dependency provenance. Neither claim means
# much if two builds of the same commit produce different binaries, because then
# nobody can check that a signed artifact came from the source it claims.
#
# The two sources of nondeterminism this addresses:
#
#   * absolute paths embedded in debug info and panic messages, which differ
#     between a developer's home directory and a CI checkout;
#   * build timestamps, which SOURCE_DATE_EPOCH pins to the commit date.
#
# Usage:
#   scripts/build-release.sh            build once
#   scripts/build-release.sh --verify   build twice into separate directories and
#                                       compare digests

set -euo pipefail
cd "$(dirname "$0")/.."

verify=0
[ "${1:-}" = "--verify" ] && verify=1

# The commit date, not the current time, so the same commit always builds the
# same bytes. Falls back to the epoch outside a Git checkout.
SOURCE_DATE_EPOCH=$(git log -1 --pretty=%ct 2>/dev/null || echo 0)
export SOURCE_DATE_EPOCH

# Remap the workspace root and the Cargo registry to fixed names so the paths
# baked into the binary do not depend on where it was built.
remap="--remap-path-prefix=$PWD=/gfs"
remap="$remap --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}/registry=/cargo-registry"

export CARGO_INCREMENTAL=0
export RUSTFLAGS="${RUSTFLAGS:-} $remap"

build_into() {
  local dir="$1"
  printf '\033[1mbuilding into %s\033[0m (SOURCE_DATE_EPOCH=%s)\n' "$dir" "$SOURCE_DATE_EPOCH"
  CARGO_TARGET_DIR="$dir" cargo build --release --workspace --locked
}

digests_of() {
  local dir="$1"
  # Only the shipped artifacts. Intermediate object files are allowed to differ.
  find "$dir/release" -maxdepth 1 -type f -perm -u+x ! -name '*.d' -print0 |
    sort -z | xargs -0 sha256sum | sed "s| $dir/release/| |"
}

if [ "$verify" -eq 1 ]; then
  build_into target/repro-a
  build_into target/repro-b
  a=$(digests_of target/repro-a)
  b=$(digests_of target/repro-b)
  if [ "$a" = "$b" ]; then
    printf '\n\033[32mReproducible.\033[0m Identical digests from two independent builds:\n%s\n' "$a"
  else
    printf '\n\033[31mNOT reproducible.\033[0m Digests differ:\n'
    diff <(printf '%s\n' "$a") <(printf '%s\n' "$b") || true
    exit 1
  fi
else
  build_into target
  printf '\n'
  digests_of target
fi
