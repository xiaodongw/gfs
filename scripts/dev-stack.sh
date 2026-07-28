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

STATE_DIR="${GFS_DEV_STATE:-target/dev-stack}"
HTTP_ADDR="${GFS_HTTP_ADDR:-127.0.0.1:8430}"
GRPC_ADDR="${GFS_GRPC_ADDR:-127.0.0.1:8431}"
TOKEN="dev-token"
# A fixed key so a restart does not invalidate every capability issued before it.
# Fine for local development, and exactly what a real deployment must not do with a
# value committed to a repository.
CAP_KEY="$(printf 'gfs-local-development-key-do-not-use-in-production' | od -An -tx1 | tr -d ' \n' | cut -c1-64)"

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
FIXTURE_ROOT="target/gfs-fixtures/v1/bare"
cargo test -p gfs-git --test repository --quiet every_fixture >/dev/null
mkdir -p "$STATE_DIR"

# Copied into the stack's own state directory, never imported in place.
#
# This is not tidiness. The server writes `refs/gfs/mounts/*` lease anchors into
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
  cargo test -p gfs-server --test exit_criteria --quiet -- --ignored --nocapture
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
./target/debug/gfs-server \
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

export GFS_ENDPOINT="http://$GRPC_ADDR"
export GFS_HTTP_ENDPOINT="http://$HTTP_ADDR"
export GFS_TOKEN="$TOKEN"
GFS=./target/debug/gfs

# ---------------------------------------------------------------------------
# The demonstration. Each step is one of M1's deliverables, so a failure here is a
# real regression rather than a scripting problem.
say "resolve a revision"
$GFS resolve --repo basic main

say "list a directory"
$GFS ls --repo basic --rev main src

say "read a file without cloning"
$GFS cat --repo basic --rev main README.md

say "raw bytes, with no .gitattributes conversion (DESIGN.md section 12)"
# `attrs` declares `*.txt text eol=crlf`, so a real checkout would emit CRLF and
# GFS emits the stored LF. Shown because it is a documented divergence an agent
# would otherwise discover by surprise.
$GFS cat --repo attrs --rev main converted.txt | od -c | head -2

say "a non-UTF-8 path is addressable"
$GFS ls --repo bytes --rev main | head -8

say "create, renew, and release a retention lease by hand"
MOUNT_OUT=$($GFS lease create --repo basic --rev main)
printf '%s\n' "$MOUNT_OUT"
MOUNT_ID=$(printf '%s\n' "$MOUNT_OUT" | awk '$1=="mount"{print $2}')
CAP=$(printf '%s\n' "$MOUNT_OUT" | awk '$1=="capability"{print $2}')
$GFS lease renew --mount-id "$MOUNT_ID" --capability "$CAP" >/dev/null
say "  lease renewed; the anchor is verified under the repository lock"
$GFS lease release --mount-id "$MOUNT_ID" --capability "$CAP"

# ---------------------------------------------------------------------------
# The M2 half: a real FUSE mount, driven through the daemon.
#
# Skipped rather than failed where FUSE is unavailable. ADR 0003's prerequisites
# are /dev/fuse and fuse3; a container without them is a legitimate place to run
# the rest of this stack, and a skip that says so is more useful than a failure
# that looks like a regression.
if [ -c /dev/fuse ] && command -v fusermount3 >/dev/null 2>&1; then
  WS="$STATE_DIR/workspace"

  # Clean up after an earlier run, without ever recursing into a live mount.
  # `rm -rf` over a mounted workspace is both wrong and slow: the base is
  # read-only, so every removal fails, and ADR 0003 measured that a mount point
  # outlives its daemon and answers ENOTCONN until something unmounts it.
  $GFS unmount --workspace "$WS" >/dev/null 2>&1 || true
  for gen in "$WS.gfs"/generations/*; do
    [ -d "$gen" ] && fusermount3 -u -z "$gen" >/dev/null 2>&1 || true
  done
  rm -f "$WS"
  rm -rf "$WS.gfs"
  # Whatever happens below, do not leave a daemon behind.
  # shellcheck disable=SC2317
  cleanup_mount() { $GFS unmount --workspace "$WS" >/dev/null 2>&1 || true; }
  trap 'cleanup_mount; cleanup' EXIT

  say "mount a pinned commit as a workspace"
  $GFS mount --repo basic --rev main --workspace "$WS" --cache-dir "$STATE_DIR/cache"

  say "read a file through the mount, with no repository on the client"
  cat "$WS/README.md"

  say "the synthesized .git surface (ADR 0005: six entries, not four)"
  ls -A "$WS/.git"
  printf '  HEAD is: '
  cat "$WS/.git/HEAD"

  say "git finds the repository root through the synthesized surface"
  ( cd "$WS" && git rev-parse --show-toplevel && git rev-parse --abbrev-ref HEAD )

  say "the git shim answers what the raw surface gets wrong (ADR 0005)"
  SHIM_BIN=$($GFS install-shim --workspace "$WS" 2>/dev/null)
  printf '  stock git ls-files:  %s entries, exit 0 -- silently wrong\n' \
    "$( (cd "$WS" && git ls-files | wc -l) )"
  printf '  shim  git ls-files:  %s entries\n' \
    "$( (cd "$WS" && PATH="$SHIM_BIN:$PATH" git ls-files | wc -l) )"
  printf '  shim  git log -1:    %s\n' \
    "$( (cd "$WS" && PATH="$SHIM_BIN:$PATH" git log -1 --format='%h %s') )"
  if (cd "$WS" && PATH="$SHIM_BIN:$PATH" git checkout main) 2>/dev/null; then
    echo "FAILED: the shim accepted an unsupported subcommand" >&2
    exit 1
  fi
  echo "  unsupported subcommands are refused, not approximated"

  say "hydration accounting"
  $GFS inspect --workspace "$WS" | grep -E 'hydration|generation|lease|overlay'

  say "the workspace is writable, and the overlay records what changed"
  echo "a new file" >"$WS/added.txt"
  printf '\nan appended line\n' >>"$WS/README.md"
  rm -f "$WS/src/lib/util.rs"
  mv "$WS/src/main.rs" "$WS/src/entry.rs"
  $GFS status --workspace "$WS"

  say "the same change set as a patch, from the journal (no tree scan)"
  $GFS diff --workspace "$WS" | head -20

  say "the shim answers status and diff from the overlay too"
  (cd "$WS" && PATH="$SHIM_BIN:$PATH" git status --porcelain)

  say "export is atomic and checksummed"
  $GFS export --workspace "$WS" --bundle "$STATE_DIR/export"
  ls -A "$STATE_DIR/export"

  say "refresh refuses a dirty workspace (three-way refresh is out of scope)"
  if $GFS refresh --workspace "$WS" 2>/dev/null; then
    echo "FAILED: refresh accepted a workspace with local changes" >&2
    exit 1
  fi
  echo "  refused, as PLAN.md M2.1 requires"

  say "the overlay survives a daemon restart: the job's edits are still there"
  $GFS unmount --workspace "$WS" >/dev/null
  $GFS mount --repo basic --rev main --workspace "$WS" >/dev/null
  $GFS status --workspace "$WS" | tail -n +2

  say "discard the workspace, and refresh publishes a new generation atomically"
  $GFS unmount --workspace "$WS" >/dev/null
  rm -rf "$WS.gfs"
  $GFS mount --repo basic --rev main --workspace "$WS" >/dev/null
  $GFS refresh --workspace "$WS"

  say "health"
  $GFS health --workspace "$WS"

  say "unmount"
  $GFS unmount --workspace "$WS"
else
  say "FUSE is unavailable (need /dev/fuse and fuse3); the mount demo is SKIPPED"
fi

say "stock git clones the same repository over smart HTTP"
# M5: the gateway is on the HTTP listener, so a clone URL is the repository's
# API path. Verified against real `git`, because the gateway's protocol engine
# *is* stock git and cannot be its own oracle.
CLONE_DIR="$STATE_DIR/clone"
rm -rf "$CLONE_DIR"
if git -c "http.extraHeader=Authorization: Bearer $TOKEN" \
  -c protocol.version=2 \
  clone -q "http://$HTTP_ADDR/v1/repos/basic" "$CLONE_DIR" 2>&1 | sed 's/^/  /'; then
  echo "  cloned $(git -C "$CLONE_DIR" rev-parse --short HEAD), $(git -C "$CLONE_DIR" ls-files | wc -l) files"
  git -C "$CLONE_DIR" fsck --no-progress >/dev/null 2>&1 && echo "  git fsck clean"
else
  echo "FAILED: stock git could not clone through the gateway" >&2
  exit 1
fi

say "the internal lease namespace is absent from the advertisement"
if git -c "http.extraHeader=Authorization: Bearer $TOKEN" \
  ls-remote "http://$HTTP_ADDR/v1/repos/basic" 2>/dev/null | grep -q 'refs/gfs/'; then
  echo "FAILED: refs/gfs/ was advertised" >&2
  exit 1
fi
echo "  ok: no refs/gfs/ in ls-remote"

say "a revision expression is refused, not interpreted"
if $GFS resolve --repo basic 'main^{tree}' 2>/dev/null; then
  echo "FAILED: a revision expression was accepted" >&2
  exit 1
fi
echo "  ok: main^{tree} rejected"

say "metrics"
curl -fsS "http://$HTTP_ADDR/metrics" | grep -E '^gfs_requests_total' | head -5

if [ "$SMOKE" -eq 1 ]; then
  say "smoke run complete"
  exit 0
fi

cat <<EOF

$(say "stack is up")

  gRPC     $GFS_ENDPOINT
  HTTP     $GFS_HTTP_ENDPOINT
  git      $GFS_HTTP_ENDPOINT/v1/repos/<id>
  token    $TOKEN
  state    $STATE_DIR

Try:
  export GFS_ENDPOINT=$GFS_ENDPOINT GFS_HTTP_ENDPOINT=$GFS_HTTP_ENDPOINT GFS_TOKEN=$TOKEN
  ./target/debug/gfs ls --repo bigdir --rev main many | head
  ./target/debug/gfs mount --repo basic --rev main
  git -c "http.extraHeader=Authorization: Bearer $TOKEN" clone $GFS_HTTP_ENDPOINT/v1/repos/basic

Ctrl-C to stop.
EOF

wait "$SERVER_PID"
