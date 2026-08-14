#!/usr/bin/env bash
#
# Build a relocatable test bundle for machines that cannot build GFS
# themselves: prebuilt binaries + a standalone dev server script + a test
# guide, packed as a tarball.
#
#   scripts/build-bundle.sh [output.tar.gz]
#
# The binaries are compiled inside an Ubuntu 20.04 container so they run on
# any distro with glibc 2.30 or newer — a host-native release build links
# against the host's glibc and fails to load on older targets, which is the
# mistake this script exists to prevent. Requires docker; the repo's pinned
# toolchain (rust-toolchain.toml) is installed inside the container, so the
# host needs no Rust at all.

set -euo pipefail
cd "$(dirname "$0")/.."

OUT="${1:-$HOME/gfs-bundle.tar.gz}"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

docker run --rm -v "$PWD":/src -w /src \
  -e CARGO_TARGET_DIR=/src/target-focal -e DEBIAN_FRONTEND=noninteractive \
  ubuntu:20.04 bash -c "
    set -euo pipefail
    fail_bootstrap() {
      cat /tmp/gfs-bootstrap.log >&2
      exit 1
    }
    apt-get update -qq >/tmp/gfs-bootstrap.log 2>&1 || fail_bootstrap
    apt-get install -y -qq curl build-essential pkg-config zlib1g-dev cmake \
      ca-certificates >>/tmp/gfs-bootstrap.log 2>&1 || fail_bootstrap
    curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none \
      --profile minimal >>/tmp/gfs-bootstrap.log 2>&1 || fail_bootstrap
    export PATH=\"/root/.cargo/bin:\$PATH\"
    # Resolve the pinned override and its components while bootstrap output is
    # captured. If this fails, replay the log; compiler diagnostics stay live.
    cargo --version >>/tmp/gfs-bootstrap.log 2>&1 || fail_bootstrap
    cargo build --release -p gfs-cli -p gfs-fuse -p gfs-server
    chown -R $(id -u):$(id -g) /src/target-focal
  "

mkdir -p "$STAGE/gfs-bundle/bin"
for bin in \
  gfs gfs-server gfs-fuse \
  gfs-git-shim gfs-scan-shim git grep find rg \
  gfs-fsmonitor gfs-lfs-filter
do
  cp "target-focal/release/$bin" "$STAGE/gfs-bundle/bin/"
done
cp scripts/bundle/start-server.sh scripts/bundle/TESTING.md "$STAGE/gfs-bundle/"
chmod +x "$STAGE/gfs-bundle/start-server.sh" "$STAGE/gfs-bundle/bin/"*

# The floor the build actually produced, verified rather than assumed.
floor=$(objdump -T "$STAGE/gfs-bundle/bin/gfs-server" \
  | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -1)
echo "glibc floor: $floor" >&2

tar czf "$OUT" -C "$STAGE" gfs-bundle
echo "$OUT" >&2
