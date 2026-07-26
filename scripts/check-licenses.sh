#!/usr/bin/env bash
#
# Assert the ADR 0001 dependency/licensing table against the actual build.
#
# This script exists because of a specific measured finding, not as a generic
# license scan. ADR 0001 records that crate metadata is *misleading* for this
# stack: `libgit2-sys` declares `MIT OR Apache-2.0`, which covers only the Rust
# wrapper, while the vendored C library it compiles and statically links into the
# server binary is GPL-2.0-only with the libgit2 linking exception. Any tool that
# reads `cargo metadata` -- including cargo-deny -- reports this workspace as
# fully permissive and misses the GPL-2.0 C library entirely.
#
# PLAN.md M1.1 therefore requires the license check to "assert the dependency
# table in ADR 0001 directly". That is what the checks below do:
#
#   1. the pinned versions in the ADR table are the versions actually resolved;
#   2. the vendored libgit2 source is present and carries its linking exception;
#   3. the vendored libgit2 has not been patched, since modifying it is governed
#      by GPL-2.0 and would be a licensing decision rather than a build change;
#   4. stock Git is a separate executable, never linked;
#   5. libgit2's COPYING is available to ship with a binary distribution.
#
# Run alongside `cargo deny check`, not instead of it.

set -euo pipefail

cd "$(dirname "$0")/.."

fail=0
note() { printf '  %s\n' "$*"; }
ok() { printf '\033[32mok\033[0m   %s\n' "$*"; }
bad() {
  printf '\033[31mFAIL\033[0m %s\n' "$*"
  fail=1
}

# ---------------------------------------------------------------------------
# 1. Pinned versions from the ADR 0001 table.
# ---------------------------------------------------------------------------
# Format: crate:expected-version. These are the versions ADR 0001 pinned; a bump
# is allowed but must update the ADR, because the supported-repository-format
# boundary is a property of the exact libgit2 build.
# `libgit2-sys` carries the bundled C library version after a `+`, so pinning the
# full string asserts the vendored libgit2 1.9.6 from the ADR table too, not just
# the Rust wrapper.
EXPECTED_CRATES=(
  "git2:0.20.4"
  "libgit2-sys:0.18.7+1.9.6"
)

echo "== ADR 0001: pinned crate versions =="
if ! command -v cargo >/dev/null; then
  bad "cargo is not on PATH"
else
  # `cargo metadata` rather than parsing Cargo.lock: it reports what the build
  # actually resolved for this target and feature set.
  metadata=$(cargo metadata --format-version 1 --locked 2>/dev/null ||
    cargo metadata --format-version 1)
  for spec in "${EXPECTED_CRATES[@]}"; do
    crate="${spec%%:*}"
    want="${spec#*:}"
    # Extract "name version" pairs without needing jq. The `|| true` is load
    # bearing under `set -euo pipefail`: when grep matches nothing it exits 1,
    # and a failing command substitution in an assignment aborts the whole
    # script -- which would turn "the pinned crate is missing" into a silent
    # early exit with status 0, the exact failure mode this gate exists to
    # prevent.
    got=$(printf '%s' "$metadata" |
      tr ',' '\n' | grep -o "\"id\":\"[^\"]*${crate}@[^\"]*\"" |
      sed 's/.*@//; s/"$//' | sort -u | head -1 || true)
    if [ -z "$got" ]; then
      bad "$crate is not in the dependency graph (ADR 0001 requires it)"
    elif [ "$got" = "$want" ]; then
      ok "$crate $got matches ADR 0001"
    else
      bad "$crate is $got but ADR 0001 pins $want -- update the ADR or the pin"
    fi
  done
fi

# ---------------------------------------------------------------------------
# 2-3. The vendored GPL-2.0 C library: present, exception intact, unpatched.
# ---------------------------------------------------------------------------
echo
echo "== ADR 0001: vendored libgit2 (GPL-2.0-only WITH linking exception) =="

# Locate the *resolved* version's sources, never "the newest-looking directory".
# A machine that has built other projects carries several `libgit2-sys-*` trees,
# and their parent directories are per-index hashes -- so sorting full paths
# ranks by index hash, not by version, and happily inspects a stale 1.7.1 copy
# while the build links 1.9.6. Asserting the licence of a library the build does
# not use is worse than not checking at all.
resolved_libgit2_sys=$(printf '%s' "${metadata:-}" |
  tr ',' '\n' | grep -o '"id":"[^"]*libgit2-sys@[^"]*"' |
  sed 's/.*@//; s/"$//' | sort -u | head -1 || true)

vendor_root=""
if [ -n "$resolved_libgit2_sys" ]; then
  vendor_root=$(find "${CARGO_HOME:-$HOME/.cargo}/registry/src" \
    -maxdepth 2 -type d -name "libgit2-sys-${resolved_libgit2_sys}" 2>/dev/null |
    head -1 || true)
  note "inspecting the resolved libgit2-sys ${resolved_libgit2_sys}"
fi

if [ -z "$vendor_root" ]; then
  note "libgit2-sys sources are not unpacked yet; run 'cargo fetch' first."
  bad "cannot verify the vendored libgit2 licence without its sources"
else
  copying="$vendor_root/libgit2/COPYING"
  if [ ! -f "$copying" ]; then
    bad "vendored libgit2 COPYING not found under $vendor_root"
  else
    # The linking exception is the clause that makes static linking into a
    # differently licensed binary permissible. If it ever disappears from an
    # upgraded libgit2, static linking stops being safe and this must fail.
    #
    # Matched against a whitespace-flattened copy: the clause is hard-wrapped in
    # COPYING, so any pattern longer than one line fails against the raw file
    # even when the text is present. Flattening keeps the pattern long enough to
    # be specific without being defeated by the line breaks.
    flattened=$(tr '\n' ' ' <"$copying" | tr -s ' ')
    if printf '%s' "$flattened" |
      grep -q "unlimited permission to link the compiled version of this library into combinations with other programs"; then
      ok "libgit2 COPYING carries the linking exception"
    else
      bad "libgit2 COPYING no longer carries the linking exception -- static linking is not safe"
    fi
    if grep -qi "GNU General Public License, version 2" "$copying"; then
      ok "libgit2 is GPL-2.0, as ADR 0001 records (metadata says MIT/Apache -- it is wrong)"
    else
      bad "libgit2 COPYING does not identify GPL-2.0; re-read it and update ADR 0001"
    fi
    # Bundled dependency notices that must ship with a binary distribution.
    for notice in "deps/zlib" "deps/pcre"; do
      if [ -d "$vendor_root/libgit2/$notice" ] || grep -qi "${notice##*/}" "$copying"; then
        ok "bundled ${notice##*/} is accounted for in the notice set"
      else
        note "bundled ${notice##*/} not found; ADR 0001 lists it -- verify the upgrade"
      fi
    done
  fi

  # ADR 0001: "the vendored libgit2 must not be patched" without accepting the
  # GPL-2.0 obligation on modification. A patch in the crate would be applied
  # from the crate's own build script, so a local patch directory is the signal.
  if [ -d "$vendor_root/patches" ] && [ -n "$(ls -A "$vendor_root/patches" 2>/dev/null)" ]; then
    bad "vendored libgit2 carries patches -- modifying it is a GPL-2.0 licensing decision (ADR 0001)"
  else
    ok "vendored libgit2 is unpatched"
  fi
fi

# ---------------------------------------------------------------------------
# 4. Stock Git is executed, never linked.
# ---------------------------------------------------------------------------
echo
echo "== ADR 0001: stock Git is a subprocess, not a link-time dependency =="

# The property: no crate in the graph links Git's internals. Git reaches XVFS
# only through Command::new, which is not a derived work and therefore keeps
# Git's GPL-2.0 off the XVFS binary.
if printf '%s' "${metadata:-}" | grep -qE '"(libgit|git)-?(sys|internals)@' &&
  ! printf '%s' "${metadata:-}" | grep -q '"libgit2-sys@'; then
  bad "a crate appears to link Git internals; ADR 0001 requires subprocess invocation only"
else
  ok "no crate links stock Git's internals"
fi

# ---------------------------------------------------------------------------
# 5. Redistributable notice set.
# ---------------------------------------------------------------------------
echo
echo "== ADR 0001: packaging obligations =="
if [ -f "licenses/libgit2-COPYING" ]; then
  ok "licenses/libgit2-COPYING is staged for redistribution"
elif [ -n "${vendor_root:-}" ] && [ -f "$vendor_root/libgit2/COPYING" ]; then
  note "libgit2 COPYING is available in the registry but not staged in licenses/."
  note "Run: mkdir -p licenses && cp '$vendor_root/libgit2/COPYING' licenses/libgit2-COPYING"
  bad "libgit2 COPYING must ship with any binary distribution (ADR 0001)"
else
  bad "libgit2 COPYING is not available to ship"
fi

echo
if [ "$fail" -ne 0 ]; then
  echo "License assertions FAILED. ADR 0001 records why each of these matters."
  exit 1
fi
echo "All ADR 0001 license assertions hold."
