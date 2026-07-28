#!/usr/bin/env bash
# Reproducer for the M0.3 SHA-256 finding.
#
# DESIGN.md section 5.1 says libgit2's SHA-256 support "is experimental and
# requires a non-default build". That is true of libgit2 itself but understates
# the situation for GFS, because GFS reaches libgit2 through `git2-rs`. This
# script establishes the two facts separately:
#
#   1. libgit2-sys DOES build with GIT_EXPERIMENTAL_SHA256 enabled.
#   2. git2 (the safe wrapper GFS actually uses) does NOT compile against it.
#
# Re-run this whenever git2/libgit2-sys are bumped. The day it passes is the day
# the SHA-256 pre-production commitment becomes achievable without raw FFI.
set -uo pipefail

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cd "$work"

echo "== 1. libgit2-sys with unstable-sha256 =="
cargo new --lib sys-only -q
cat >>sys-only/Cargo.toml <<'EOF'
libgit2-sys = { version = "0.18", default-features = false, features = ["vendored", "unstable-sha256"] }
EOF
cat >sys-only/src/lib.rs <<'EOF'
pub fn max_oid_size() -> usize { libgit2_sys::GIT_OID_MAX_SIZE }
pub fn sha256_size() -> usize { libgit2_sys::GIT_OID_SHA256_SIZE }
EOF
if (cd sys-only && cargo build -q 2>/dev/null); then
    echo "   RESULT: builds (GIT_OID_MAX_SIZE becomes 32)"
    sys_ok=yes
else
    echo "   RESULT: FAILS to build"
    sys_ok=no
fi

echo
echo "== 2. git2 on top of that same libgit2-sys =="
cargo new --lib with-git2 -q
cat >>with-git2/Cargo.toml <<'EOF'
git2 = { version = "0.20", default-features = false, features = ["vendored-libgit2"] }
libgit2-sys = { version = "0.18", default-features = false, features = ["unstable-sha256"] }
EOF
cat >with-git2/src/lib.rs <<'EOF'
pub fn version() -> String { format!("{:?}", git2::Version::get().libgit2_version()) }
EOF
log=$work/git2.log
if (cd with-git2 && cargo build -q >"$log" 2>&1); then
    echo "   RESULT: builds — SHA-256 is reachable through git2-rs"
    git2_ok=yes
else
    echo "   RESULT: FAILS to build"
    echo "   errors: $(grep -c '^error\[' "$log")"
    echo "   missing symbols: $(grep -oP "cannot find value \`\K[A-Z_0-9]+" "$log" | sort -u | tr '\n' ' ')"
    echo "   cause: libgit2-sys gates these out under #[cfg(not(feature = \"unstable-sha256\"))],"
    echo "          but git2 0.20.4 still references them unconditionally."
    git2_ok=no
fi

echo
echo "== verdict =="
if [ "$sys_ok" = yes ] && [ "$git2_ok" = no ]; then
    echo "UNCHANGED: SHA-256 is unreachable through git2-rs. Reject SHA-256 at ingest."
    exit 0
elif [ "$git2_ok" = yes ]; then
    echo "CHANGED: git2-rs now builds with SHA-256. Revisit docs/adr/0001-git-integration.md."
    exit 0
else
    echo "CHANGED: libgit2-sys itself no longer builds with unstable-sha256. Investigate."
    exit 1
fi
