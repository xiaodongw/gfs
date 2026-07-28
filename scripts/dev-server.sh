#!/usr/bin/env bash
#
# A gateway to poke at by hand. One line to start, Ctrl+C to stop.
#
#   scripts/dev-server.sh
#
# Then, in another terminal, `gfs` works with no configuration at all:
#
#   gfs clone https://github.com/pallets/flask.git
#   cd flask && gfs status
#
# That is the whole point of this script. It is not `dev-stack.sh`, which seeds
# fixtures and asserts the stack works and is what CI runs; this one exists so a
# person can type `gfs` commands against a real repository without first
# assembling a capability key, a token, and three `--flag` arguments.
#
# # Why no credential is needed
#
# The server runs with an *empty* dev token, which maps a caller presenting no
# credential to the `dev` subject. `gfs` sends no `Authorization` header when it
# has no token, so the two agree and nothing has to be exported. That is
# obviously not a production posture, and it is why this binds to 127.0.0.1 only
# and says so on startup: anyone who can reach the port is `dev`.
#
# # Ctrl+C stops the mounts too
#
# `gfs clone` starts a `gfs-fuse` daemon that outlives this process by design --
# a workspace should survive a gateway restart. That is the wrong default for a
# scratch session, where an orphaned daemon holding a FUSE mount is the thing
# that makes the *next* run fail confusingly. So the exit trap unmounts every
# workspace this session created, and leaves anything it did not create alone.

set -euo pipefail
cd "$(dirname "$0")/.."

LAB="${GFS_LAB:-$HOME/.gfs-lab}"
HTTP_ADDR="${GFS_HTTP_ADDR:-127.0.0.1:8430}"
GRPC_ADDR="${GFS_GRPC_ADDR:-127.0.0.1:8431}"

# A fixed key rather than a generated one. An ephemeral key invalidates every
# outstanding mount capability on restart, so a workspace mounted before a
# restart would start failing its lease renewals for no visible reason.
KEY=$(printf 'gfs-dev-server-local-only-key-not-for-production' \
  | od -An -tx1 | tr -d ' \n' | cut -c1-64)

mkdir -p "$LAB/state" "$LAB/repos"

cleanup() {
  trap - EXIT INT TERM
  shopt -s nullglob
  # Ask each daemon to unmount first, while the gateway is still up, so it can
  # release its lease rather than leaving the gateway holding one until expiry.
  for state in "$LAB"/*.gfs; do
    ./target/debug/gfs unmount --workspace "${state%.gfs}" >/dev/null 2>&1 || true
  done
  # Then force anything that did not go quietly -- a workspace with an open
  # descriptor refuses a clean unmount, and a leftover FUSE mount is what makes
  # the *next* run fail confusingly.
  for gen in "$LAB"/*.gfs/generations/*; do
    fusermount3 -u -z "$gen" >/dev/null 2>&1 || true
  done
  shopt -u nullglob
  if [ -n "${SERVER:-}" ]; then
    kill "$SERVER" 2>/dev/null || true
    wait "$SERVER" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

if [ ! -x ./target/debug/gfs-server ]; then
  printf 'building...\n' >&2
  cargo build -q -p gfs-server -p gfs-fuse -p gfs-cli
fi

cat >&2 <<EOF
gfs dev server
  lab        $LAB
  http       http://$HTTP_ADDR
  grpc       http://$GRPC_ADDR
  auth       none -- anyone who can reach these ports is 'dev'

Put the binaries on PATH, then clone something:

  export PATH="$PWD/target/debug:\$PATH"
  cd $LAB
  gfs clone https://github.com/pallets/flask.git
  cd flask && gfs status

Ctrl+C here stops the server and unmounts anything under $LAB.

EOF

# Backgrounded and waited on, deliberately **not** `exec`. `exec` replaces this
# shell with the server, which discards the trap -- so Ctrl+C would stop the
# gateway and leave every `gfs-fuse` daemon still holding its mount, which is the
# exact mess the trap exists to prevent.
./target/debug/gfs-server \
  --state-dir "$LAB/state" \
  --repos-root "$LAB/repos" \
  --http-addr "$HTTP_ADDR" \
  --grpc-addr "$GRPC_ADDR" \
  --capability-key "$KEY" \
  --dev-token "" &
SERVER=$!

# `wait` returns 128+signal when a trap fires, which is not a failure here.
wait "$SERVER" || true
