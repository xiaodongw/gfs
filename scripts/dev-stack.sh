#!/usr/bin/env bash
#
# The one-command local development stack.
#
# PLAN.md M1.1 calls this "a deliverable rather than a convenience", because every
# later milestone depends on being able to bring up a server with seeded
# repositories on a laptop without hosted infrastructure. It builds the workspace,
# seeds fixture repositories of several sizes, starts the server, imports them, and
# then *demonstrates* the API rather than only claiming it is up -- a stack that
# starts but cannot answer a request is not a working stack.
#
#   scripts/dev-stack.sh            seed, start, demo, then keep running
#   scripts/dev-stack.sh --smoke    seed, start, demo, then exit (used by CI)
#   scripts/dev-stack.sh --big      also seed the million-entry snapshot
#
# The libgit2 and stock Git versions ADR 0001 pins are asserted at startup, so a
# local environment cannot silently differ from CI.

set -euo pipefail
cd "$(dirname "$0")/.."

SMOKE=0
BIG=0
for arg in "$@"; do
  case "$arg" in
    --smoke) SMOKE=1 ;;
    --big) BIG=1 ;;
    *)
      printf 'unknown option: %s\n' "$arg" >&2
      exit 2
      ;;
  esac
done

STATE_DIR="${XVFS_DEV_STATE:-target/dev-stack}"
HTTP_ADDR="${XVFS_HTTP_ADDR:-127.0.0.1:8430}"
GRPC_ADDR="${XVFS_GRPC_ADDR:-127.0.0.1:8431}"
TOKEN="dev-token"
# A fixed key so a restart does not invalidate every capability issued before it.
# Fine for local development, and exactly what a real deployment must not do with a
# value committed to a repository.
CAP_KEY="$(printf 'xvfs-local-development-key-do-not-use-in-production' | od -An -tx1 | tr -d ' \n' | cut -c1-64)"

say() { printf '\033[1m==\033[0m %s\n' "$*"; }

# ---------------------------------------------------------------------------
say "building"
cargo build --workspace --quiet

# ---------------------------------------------------------------------------
say "seeding fixture repositories"
# Built by the same code the test suite uses, so the stack and the suite cannot
# drift into seeing different repositories. Running the matrix test is what
# materializes every fixture into the shared cache under `target/`.
FIXTURES="basic modes bytes content bigdir deep packed attrs"
FIXTURE_ROOT="target/xvfs-fixtures/v1/bare"
cargo test -p xvfs-git --test repository --quiet every_fixture >/dev/null
mkdir -p "$STATE_DIR"

# Copied into the stack's own state directory, never imported in place.
#
# This is not tidiness. The server writes `refs/xvfs/mounts/*` lease anchors into
# whatever repository it serves, and the demo below creates a mount. Importing the
# shared fixture cache directly would leave an anchor in `basic.git`, which then keeps
# the tip reachable in every later `scratch_clone` -- and silently breaks the test
# that asserts `gc --prune=now` reclaims an unleased commit. Test fixtures are inputs
# and must stay immutable.
REPO_DIR="$STATE_DIR/repos"
rm -rf "$REPO_DIR"
mkdir -p "$REPO_DIR"

IMPORTS=()
for name in $FIXTURES; do
  src="$FIXTURE_ROOT/$name.git"
  if [ -d "$src" ]; then
    cp -a "$src" "$REPO_DIR/$name.git"
    IMPORTS+=(--import "$name=$REPO_DIR/$name.git")
    printf '  %-10s %s\n' "$name" "$REPO_DIR/$name.git"
  fi
done

if [ "$BIG" -eq 1 ]; then
  say "seeding the million-entry snapshot (this takes a few seconds)"
  cargo test -p xvfs-server --test exit_criteria --quiet -- --ignored --nocapture
  big="$FIXTURE_ROOT/bigtree-1000x1000.git"
  if [ -d "$big" ]; then
    cp -a "$big" "$REPO_DIR/bigtree.git"
    IMPORTS+=(--import "bigtree=$REPO_DIR/bigtree.git")
    printf '  %-10s %s\n' "bigtree" "$REPO_DIR/bigtree.git"
  fi
fi

if [ ${#IMPORTS[@]} -eq 0 ]; then
  echo "no fixtures were built; cannot start a useful stack" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
say "starting the server"
rm -f "$STATE_DIR/catalog.sqlite"*
./target/debug/xvfsd-server \
  --state-dir "$STATE_DIR" \
  --http-addr "$HTTP_ADDR" \
  --grpc-addr "$GRPC_ADDR" \
  --capability-key "$CAP_KEY" \
  --dev-token "$TOKEN" \
  "${IMPORTS[@]}" &
SERVER_PID=$!
# shellcheck disable=SC2317
cleanup() {
  if kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

for _ in $(seq 1 100); do
  if curl -fsS "http://$HTTP_ADDR/readyz" >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done
if ! curl -fsS "http://$HTTP_ADDR/readyz" >/dev/null 2>&1; then
  echo "server did not become ready" >&2
  exit 1
fi

export XVFS_ENDPOINT="http://$GRPC_ADDR"
export XVFS_HTTP_ENDPOINT="http://$HTTP_ADDR"
export XVFS_TOKEN="$TOKEN"
XVFS=./target/debug/xvfs

# ---------------------------------------------------------------------------
# The demonstration. Each step is one of M1's deliverables, so a failure here is a
# real regression rather than a scripting problem.
say "resolve a revision"
$XVFS resolve --repo basic main

say "list a directory"
$XVFS ls --repo basic --rev main src

say "read a file without cloning"
$XVFS cat --repo basic --rev main README.md

say "raw bytes, with no .gitattributes conversion (DESIGN.md section 12)"
# `attrs` declares `*.txt text eol=crlf`, so a real checkout would emit CRLF and
# XVFS emits the stored LF. Shown because it is a documented divergence an agent
# would otherwise discover by surprise.
$XVFS cat --repo attrs --rev main converted.txt | od -c | head -2

say "a non-UTF-8 path is addressable"
$XVFS ls --repo bytes --rev main | head -8

say "create, renew, and release a mount lease"
MOUNT_OUT=$($XVFS mount --repo basic --rev main)
printf '%s\n' "$MOUNT_OUT"
MOUNT_ID=$(printf '%s\n' "$MOUNT_OUT" | awk '$1=="mount"{print $2}')
CAP=$(printf '%s\n' "$MOUNT_OUT" | awk '$1=="capability"{print $2}')
$XVFS renew --mount-id "$MOUNT_ID" --capability "$CAP" >/dev/null
say "  lease renewed; the anchor is verified under the repository lock"
$XVFS release --mount-id "$MOUNT_ID" --capability "$CAP"

say "a revision expression is refused, not interpreted"
if $XVFS resolve --repo basic 'main^{tree}' 2>/dev/null; then
  echo "FAILED: a revision expression was accepted" >&2
  exit 1
fi
echo "  ok: main^{tree} rejected"

say "metrics"
curl -fsS "http://$HTTP_ADDR/metrics" | grep -E '^xvfs_requests_total' | head -5

if [ "$SMOKE" -eq 1 ]; then
  say "smoke run complete"
  exit 0
fi

cat <<EOF

$(say "stack is up")

  gRPC     $XVFS_ENDPOINT
  HTTP     $XVFS_HTTP_ENDPOINT
  token    $TOKEN
  state    $STATE_DIR

Try:
  export XVFS_ENDPOINT=$XVFS_ENDPOINT XVFS_HTTP_ENDPOINT=$XVFS_HTTP_ENDPOINT XVFS_TOKEN=$TOKEN
  ./target/debug/xvfs ls --repo bigdir --rev main many | head
  ./target/debug/xvfs mount --repo basic --rev main

Ctrl-C to stop.
EOF

wait "$SERVER_PID"
