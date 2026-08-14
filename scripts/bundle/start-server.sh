#!/usr/bin/env bash
#
# A GFS gateway to poke at by hand, from prebuilt binaries. One line to start,
# Ctrl+C to stop. Standalone copy of the repo's scripts/dev-server.sh, pointed
# at this bundle's bin/ instead of a cargo target directory.
#
# The server runs with an *empty* dev token: anyone who can reach the ports is
# the `dev` subject. It binds to 127.0.0.1 only. Do not point it at anything
# you care about.

set -euo pipefail
cd "$(dirname "$0")"
BIN="$PWD/bin"

LAB="${GFS_LAB:-$HOME/.gfs-lab}"
HTTP_ADDR="${GFS_HTTP_ADDR:-127.0.0.1:8430}"
GRPC_ADDR="${GFS_GRPC_ADDR:-127.0.0.1:8431}"

# A fixed key rather than a generated one: an ephemeral key would invalidate
# every outstanding mount capability on restart.
KEY=$(printf 'gfs-dev-server-local-only-key-not-for-production' \
  | od -An -tx1 | tr -d ' \n' | cut -c1-64)

mkdir -p "$LAB/state" "$LAB/repos"

cleanup() {
  trap - EXIT INT TERM
  shopt -s nullglob
  # Unmount while the gateway is still up, so each daemon releases its lease.
  for state in "$LAB"/*.gfs; do
    "$BIN/gfs" unmount --workspace "${state%.gfs}" >/dev/null 2>&1 || true
  done
  for state in "$LAB"/*.gfs; do
    fusermount3 -u -z "${state%.gfs}" >/dev/null 2>&1 || true
  done
  shopt -u nullglob
  if "$BIN/gfs" daemon status 2>/dev/null | grep -qE '^mounts +0$'; then
    "$BIN/gfs" daemon stop >/dev/null 2>&1 || true
  fi
  if [ -n "${SERVER:-}" ]; then
    kill "$SERVER" 2>/dev/null || true
    wait "$SERVER" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

cat >&2 <<EOF
gfs dev server (bundled binaries)
  lab        $LAB
  http       http://$HTTP_ADDR
  grpc       http://$GRPC_ADDR
  auth       none -- anyone who can reach these ports is 'dev'

In another terminal:

  export PATH="$BIN:\$PATH"
  cd $LAB
  git clone https://github.com/pallets/flask.git

Ctrl+C here stops the server and unmounts anything under $LAB.

EOF

"$BIN/gfs-server" \
  --state-dir "$LAB/state" \
  --repos-root "$LAB/repos" \
  --http-addr "$HTTP_ADDR" \
  --grpc-addr "$GRPC_ADDR" \
  --capability-key "$KEY" \
  --dev-token "" &
SERVER=$!

wait "$SERVER" || true
