#!/usr/bin/env bash
#
# POSIX conformance: pjdfstest against an XVFS mount, with an ext4 control.
#
# PLAN.md M2.4 asks for a relevant subset of pjdfstest and xfstests. M2 and M3
# both recorded it as not run. This runs it.
#
#   ./spikes/conformance/pjdfstest.sh <mounted-workspace>
#
# ---------------------------------------------------------------------------
# Why there is a control run
# ---------------------------------------------------------------------------
#
# pjdfstest's README says "You must be root when running these testcases", and
# root is not available here -- nor would it help, because a root process cannot
# enter a uid-1000 FUSE mount without `user_allow_other` in /etc/fuse.conf, which
# ADR 0003 treats as a privileged host action.
#
# Running as an ordinary user instead makes 76 of 238 test files fail on **ext4**,
# for reasons that have nothing to do with XVFS. So the suite is run twice, as
# the same user, against ext4 and against the mount, and only the difference is
# reported. ext4 is the oracle, the same way the raw tree is M2's oracle for the
# mount and ripgrep is M4's for search: a suite that cannot be its own baseline
# gets one.
#
# ---------------------------------------------------------------------------
# Why it is built by hand
# ---------------------------------------------------------------------------
#
# pjdfstest is autotools and autoconf/automake are not installed here, but it is
# a single C file. `config.h` is written rather than probed; every HAVE_* it sets
# is a POSIX.1-2008 call glibc has, and the BSD-only ones are left unset.
#
# The clone forces `core.autocrlf=false`. With the global `true` in this
# environment the .t scripts arrive with CRLF endings and every one of them fails
# with ": not found" on its odd-numbered lines -- which looks like a broken test
# suite rather than a checkout problem. The M3 completion report records the same
# setting corrupting `git apply`.

set -uo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

WORKSPACE="${1:-}"
if [ -z "$WORKSPACE" ] || [ ! -d "$WORKSPACE" ]; then
  echo "usage: $0 <mounted-xvfs-workspace>" >&2
  exit 2
fi

WORK="${XVFS_CONFORMANCE_DIR:-$HOME/xvfs-conformance}"
SUITE="$WORK/pjdfstest"
mkdir -p "$WORK"

# ---------------------------------------------------------------------------
if [ ! -x "$SUITE/pjdfstest" ]; then
  echo "== fetching and building pjdfstest"
  rm -rf "$SUITE"
  git -c core.autocrlf=false clone -q --depth 1 \
    https://github.com/pjd/pjdfstest.git "$SUITE" || exit 1
  cat >"$SUITE/config.h" <<'EOF'
/* Hand-written for Linux/glibc in place of autoconf; see pjdfstest.sh. */
#define HAVE_FCHMODAT 1
#define HAVE_FCHOWNAT 1
#define HAVE_FSTATAT 1
#define HAVE_LINKAT 1
#define HAVE_MKDIRAT 1
#define HAVE_MKFIFOAT 1
#define HAVE_MKNODAT 1
#define HAVE_OPENAT 1
#define HAVE_RENAMEAT 1
#define HAVE_SYMLINKAT 1
#define HAVE_UNLINKAT 1
#define HAVE_UTIMENSAT 1
#define HAVE_POSIX_FALLOCATE 1
#define HAVE_SYS_SYSMACROS_H 1
#define HAVE_STRUCT_STAT_ST_ATIM 1
#define HAVE_STRUCT_STAT_ST_CTIM 1
#define HAVE_STRUCT_STAT_ST_MTIM 1
EOF
  (cd "$SUITE" && gcc -Wall -D_GNU_SOURCE -I. -o pjdfstest pjdfstest.c) || exit 1
fi

# ---------------------------------------------------------------------------
# One run. Per test file rather than one `prove -r`, because the triage needs the
# individual assertions and a summary line cannot be taken apart afterwards.
run_suite() { # $1 = directory to run in, $2 = output directory
  local target="$1" out="$2"
  rm -rf "$out"; mkdir -p "$out"
  ( cd "$target" || exit 2
    for t in "$SUITE"/tests/*/*.t; do
      local rel name
      rel="${t#"$SUITE"/tests/}"
      name="${rel//\//_}"
      timeout 120 env dir="$(dirname "$t")" sh "$t" >"$out/${name}.tap" 2>&1
    done )
}

# Files with at least one assertion and no failing one.
clean_files() { # $1 = output directory
  local f ok no
  for f in "$1"/*.tap; do
    ok=$(grep -c '^ok ' "$f"); no=$(grep -c '^not ok ' "$f")
    [ $((ok + no)) -gt 0 ] && [ "$no" -eq 0 ] && basename "$f" .tap
  done | sort
}

CONTROL="$WORK/ext4-control"
SCRATCH="$WORKSPACE/pjdfstest-scratch"
rm -rf "$CONTROL" "$SCRATCH"; mkdir -p "$CONTROL" "$SCRATCH"

echo "== control run on $(df -PT "$CONTROL" | tail -1 | awk '{print $2}')"
run_suite "$CONTROL" "$WORK/out-ext4"
echo "== run on $(df -PT "$SCRATCH" | tail -1 | awk '{print $2}')"
run_suite "$SCRATCH" "$WORK/out-xvfs"

clean_files "$WORK/out-ext4" >"$WORK/ext4.clean"
clean_files "$WORK/out-xvfs" >"$WORK/xvfs.clean"

echo
printf 'test files:            %s\n' "$(ls "$SUITE"/tests/*/*.t | wc -l)"
printf 'clean on ext4:         %s\n' "$(wc -l <"$WORK/ext4.clean")"
printf 'clean on XVFS:         %s\n' "$(wc -l <"$WORK/xvfs.clean")"
comm -23 "$WORK/ext4.clean" "$WORK/xvfs.clean" >"$WORK/xvfs-only-failures.txt"
printf 'XVFS-only failures:    %s\n' "$(wc -l <"$WORK/xvfs-only-failures.txt")"
printf 'XVFS-only passes:      %s  (ext4 fails, XVFS does not)\n' \
  "$(comm -13 "$WORK/ext4.clean" "$WORK/xvfs.clean" | wc -l)"
echo
echo "== XVFS-only failures, with the suite's own description"
while read -r n; do
  src="$SUITE/tests/$(echo "${n%.t}" | sed 's/_/\//').t"
  desc=$(grep -m1 '^desc=' "$src" 2>/dev/null | sed 's/desc=//;s/"//g')
  f="$WORK/out-xvfs/$n.tap"
  nf=$(grep -c '^not ok ' "$f")
  tot=$(( $(grep -c '^ok ' "$f") + nf ))
  printf '  %-16s %2s/%-3s  %s\n' "${n%.t}" "$nf" "$tot" "$desc"
done <"$WORK/xvfs-only-failures.txt"
echo
echo "raw TAP: $WORK/out-xvfs and $WORK/out-ext4"
