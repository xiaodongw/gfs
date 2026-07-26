#!/usr/bin/env bash
# M0.2 deployment matrix.
#
# The in-process `measure` command answers "how does the filesystem behave".
# This answers the question the milestone actually gates on: *where* can it be
# mounted, at what privilege, and what happens when things go wrong. Those need
# separate processes and separate containers, so they live here rather than in
# the probe binary.
#
# Kubernetes/CSI is not covered: no cluster is reachable from this machine. That
# is a recorded gap in the M0.2 exit gate, not a silent omission.
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="${FUSE_PROBE_BIN:-$here/../target/debug/fuse-probe}"
WORK="$(mktemp -d)"
# Needs fuse3 installed: without fusermount3 a non-CAP_SYS_ADMIN mount fails
# with ENOENT, which reads exactly like a permission failure and is not one.
IMAGE="${FUSE_PROBE_IMAGE:-xvfs-fuse-probe:latest}"
trap 'cleanup' EXIT

results=()
record() { results+=("$1|$2|$3"); printf '%-34s %-9s %s\n' "$1" "$2" "$3"; }

cleanup() {
    for m in "$WORK"/mnt-*; do
        [ -d "$m" ] && fusermount3 -u "$m" 2>/dev/null
    done
    pkill -f "fuse-probe mount" 2>/dev/null
    rm -rf "$WORK" 2>/dev/null
}

[ -x "$BIN" ] || { echo "build first: cargo build -p fuse-probe"; exit 1; }

echo "== host =="
echo "kernel:  $(uname -r)"
echo "uid:     $(id -u)"
echo "docker:  $(docker --version 2>/dev/null || echo 'not available')"
echo

# ---------------------------------------------------------------------------
echo "== 1. host mount, unprivileged user =="
# ---------------------------------------------------------------------------
mnt="$WORK/mnt-host"; mkdir -p "$mnt"
"$BIN" mount --dir "$mnt" --files 8 >"$WORK/host.log" 2>&1 &
host_pid=$!
for _ in $(seq 50); do mountpoint -q "$mnt" && break; sleep 0.1; done
if mountpoint -q "$mnt"; then
    n=$(ls "$mnt" | wc -l)
    record host_mount_unprivileged PASS "mounted as uid $(id -u), $n entries, no root and no CAP_SYS_ADMIN"
else
    record host_mount_unprivileged FAIL "$(tail -1 "$WORK/host.log")"
fi

# ---------------------------------------------------------------------------
echo
echo "== 2. failure modes on that mount =="
# ---------------------------------------------------------------------------
# An open file descriptor across daemon death. This is what an agent's compiler
# experiences when xvfsd is OOM-killed mid-build, and the errno it sees decides
# whether the build fails loudly or silently reads short.
exec 9<"$mnt/file-0000" 2>/dev/null
if [ -e /proc/self/fd/9 ]; then
    kill -9 $host_pid 2>/dev/null; wait $host_pid 2>/dev/null
    sleep 0.5
    err=$(dd if=/proc/self/fd/9 bs=4096 count=1 of=/dev/null 2>&1 >/dev/null | tail -1)
    exec 9<&-
    record daemon_death_open_fd INFO "read after SIGKILL: ${err:-succeeded from page cache}"
else
    record daemon_death_open_fd SKIP "could not open a file descriptor"
fi
# The mount point is now stale: the kernel keeps the superblock until unmounted.
stale=$(ls "$mnt" 2>&1 >/dev/null | tail -1)
record stale_mount_after_death INFO "ls on the orphaned mount: ${stale:-succeeded}"
fusermount3 -u "$mnt" 2>/dev/null
record stale_mount_cleanup INFO "fusermount3 -u exit=$?  (an orphaned mount needs explicit cleanup)"

# ---------------------------------------------------------------------------
echo
echo "== 3. unmount while a file is open =="
# ---------------------------------------------------------------------------
mnt2="$WORK/mnt-busy"; mkdir -p "$mnt2"
"$BIN" mount --dir "$mnt2" --files 8 >"$WORK/busy.log" 2>&1 &
busy_pid=$!
for _ in $(seq 50); do mountpoint -q "$mnt2" && break; sleep 0.1; done
exec 8<"$mnt2/file-0000"
out=$(fusermount3 -u "$mnt2" 2>&1); rc=$?
exec 8<&-
if [ $rc -ne 0 ]; then
    record unmount_with_open_file INFO "refused while busy: $out"
    fusermount3 -u "$mnt2" 2>/dev/null
else
    record unmount_with_open_file INFO "unmounted despite an open fd (lazy semantics)"
fi
kill -9 $busy_pid 2>/dev/null; wait $busy_pid 2>/dev/null

# ---------------------------------------------------------------------------
echo
echo "== 4. containers =="
# ---------------------------------------------------------------------------
if ! docker info >/dev/null 2>&1; then
    record docker SKIP "docker unavailable"
else
    if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
        echo "  image $IMAGE missing; build it from fuse-probe/Dockerfile" >&2
    fi
    binpath="$(readlink -f "$BIN")"

    run_in_container() {
        local label="$1"; shift
        local dockerargs=("$@")
        docker run --rm \
            -v "$binpath:/fuse-probe:ro" \
            "${dockerargs[@]}" \
            "$IMAGE" \
            /bin/sh -c 'mkdir -p /mnt/x && /fuse-probe capabilities 2>&1 | grep -E "^(plain|allow_other|auto_unmount|multi)"' \
            2>&1
    }

    # 4a. The permissive configuration everyone reaches for first.
    out=$(run_in_container privileged --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined)
    if echo "$out" | grep -q "^plain  *mounted ok"; then
        record container_dev_fuse_sysadmin PASS "mounts with --device /dev/fuse --cap-add SYS_ADMIN"
    else
        record container_dev_fuse_sysadmin FAIL "$(echo "$out" | grep -i 'fail\|error' | head -1)"
    fi

    # 4b. The question that decides the deployment model: is CAP_SYS_ADMIN
    # actually required, or is /dev/fuse enough?
    out=$(run_in_container devonly --device /dev/fuse)
    plain_line=$(echo "$out" | grep "^plain" | head -1 | sed 's/^plain *//; s/^FAILED: *//')
    if echo "$plain_line" | grep -q "mounted ok"; then
        record container_dev_fuse_only PASS "mounts with only --device /dev/fuse (no CAP_SYS_ADMIN)"
    else
        # Not a missing binary: fuse3 is installed in the image. Docker's
        # default seccomp/AppArmor profile denies the mount(2) that fusermount3
        # performs, so /dev/fuse alone is not sufficient.
        record container_dev_fuse_only EXPECTED \
            "CAP_SYS_ADMIN required; with /dev/fuse alone: $plain_line"
    fi

    # 4c. The unprivileged agent container: no /dev/fuse, no capabilities.
    out=$(run_in_container unprivileged --cap-drop ALL)
    if echo "$out" | grep -q "^plain  *mounted ok"; then
        record container_unprivileged UNEXPECTED "mounted with no /dev/fuse and no capabilities"
    else
        record container_unprivileged EXPECTED "cannot mount: $(echo "$out" | grep -io 'no such file[^\"]*' | head -1)"
    fi

    # 4c-2. Does allow_other actually let a different UID read the mount? This
    # is the mechanism the whole host-daemon model rests on, so it is measured
    # rather than assumed. Run inside the container because enabling
    # user_allow_other is a privileged host action.
    out=$(docker run --rm --device /dev/fuse --cap-add SYS_ADMIN \
            --security-opt apparmor=unconfined \
            -v "$binpath:/fuse-probe:ro" "$IMAGE" /bin/sh -c '
              mkdir -p /mnt/x
              /fuse-probe mount --dir /mnt/x --files 4 --allow-other >/tmp/m.log 2>&1 &
              for i in $(seq 50); do mountpoint -q /mnt/x && break; sleep 0.1; done
              mountpoint -q /mnt/x || { echo "MOUNT_FAILED"; tail -2 /tmp/m.log; exit 1; }
              su agent -c "ls /mnt/x | wc -l" 2>&1
            ' 2>&1)
    if echo "$out" | tail -1 | grep -qE '^[1-9]'; then
        record allow_other_cross_uid PASS "a different UID reads the mount when allow_other is set"
    else
        record allow_other_cross_uid FAIL "$(echo "$out" | tail -2 | tr '\n' ' ')"
    fi

    # 4c-3. The same thing without allow_other, which is the default.
    out=$(docker run --rm --device /dev/fuse --cap-add SYS_ADMIN \
            --security-opt apparmor=unconfined \
            -v "$binpath:/fuse-probe:ro" "$IMAGE" /bin/sh -c '
              mkdir -p /mnt/x
              /fuse-probe mount --dir /mnt/x --files 4 >/tmp/m.log 2>&1 &
              for i in $(seq 50); do mountpoint -q /mnt/x && break; sleep 0.1; done
              mountpoint -q /mnt/x || { echo "MOUNT_FAILED"; exit 1; }
              su agent -c "ls /mnt/x" 2>&1 | head -1
            ' 2>&1)
    if echo "$out" | grep -qi "permission denied"; then
        record no_allow_other_cross_uid EXPECTED "without allow_other a different UID gets EACCES"
    else
        record no_allow_other_cross_uid INFO "$(echo "$out" | tail -1)"
    fi

    # 4d. The host-daemon model XVFS actually proposes: the daemon owns the
    # mount on the host, the job container only receives a bind mount.
    # Docker refuses to bind-mount a path it cannot stat, so the mount point is
    # created before the daemon claims it and Docker is pointed at it directly.
    mnt3="$WORK/mnt-bind"; mkdir -p "$mnt3"
    "$BIN" mount --dir "$mnt3" --files 8 >"$WORK/bind.log" 2>&1 &
    bind_pid=$!
    for _ in $(seq 50); do mountpoint -q "$mnt3" && break; sleep 0.1; done
    if mountpoint -q "$mnt3"; then
        out=$(docker run --rm --cap-drop ALL -v "$mnt3:/work:ro" "$IMAGE" \
              /bin/sh -c 'ls /work | wc -l; cat /work/file-0000 2>&1 | head -c 40' 2>&1)
        if echo "$out" | head -1 | grep -qE '^[1-9]'; then
            record container_bind_mount_from_host PASS \
                "unprivileged container reads the host FUSE mount ($(echo "$out" | head -1) entries)"
        elif echo "$out" | grep -q "mount source path"; then
            # The Docker daemon runs as root and cannot traverse a FUSE mount
            # owned by uid 1000 without allow_other, so it cannot even prepare
            # the bind source. This is the same allow_other requirement proven
            # in 4c-2, showing up one layer earlier.
            record container_bind_mount_from_host BLOCKED \
                "dockerd (root) cannot stat the uid-$(id -u) mount without allow_other; \
enabling it needs user_allow_other in the host /etc/fuse.conf (no root here)"
        else
            record container_bind_mount_from_host FAIL "$(echo "$out" | head -2 | tr '\n' ' ')"
        fi
    else
        record container_bind_mount_from_host FAIL "host mount did not come up"
    fi
    kill -9 $bind_pid 2>/dev/null; wait $bind_pid 2>/dev/null
    fusermount3 -u "$mnt3" 2>/dev/null
fi

echo
echo "== summary =="
printf '%s\n' "${results[@]}" | awk -F'|' '{printf "%-34s %-11s %s\n", $1, $2, $3}'
