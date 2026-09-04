#!/usr/bin/env bash
#
# Per-lever measurement of the local-mode mount (plans/20260903-1000-fuse-levers.md).
#
#   ./spikes/corpus/benchmark-levers.sh <label> vscode django
#
# One workspace per corpus, mounted with `gfs mount --local` plus whatever
# `GFS_BENCH_MOUNT_FLAGS` holds, and one task: read 2000 files through twice,
# `rg` the tree twice, read the largest blob twice, write 64 MiB in 4 KiB
# pieces, copy a source directory in from the clone, read it back, `git
# status`, commit. Then a per-operation loop in Python on one base file and
# one overlay file. Every number is wall-clock seconds, one run.
#
# Set GFS_BENCH_NATIVE=1 to run the task in a `git worktree add` checkout
# instead (the native column). Set GFS_BENCH_WAIT_PREWARM=1 to wait for `gfs inspect` to report the prewarm
# finished before the task starts (and to record how long that took).

set -uo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"

label="${1:?label}"; shift
CORPUS_DIR="${GFS_CORPUS_DIR:-$HOME/gfs-corpus}"
LOCAL_DIR="${GFS_LOCAL_DIR:-$CORPUS_DIR/local}"
WORK="${GFS_LEVERS_WORK:-$CORPUS_DIR/levers-bench}"
BIN="$root/target/release"
MOUNT_FLAGS=(${GFS_BENCH_MOUNT_FLAGS:-})

RG="${GFS_RG_BINARY:-}"
if [ -z "$RG" ]; then
  for candidate in "$HOME/.cargo/bin/rg" /usr/bin/rg /usr/local/bin/rg; do
    [ -x "$candidate" ] && RG="$candidate" && break
  done
fi
[ -x "$BIN/gfs" ] && [ -x "$BIN/gfs-fuse" ] || { echo "build the release binaries first" >&2; exit 2; }

GITC=(-c user.name=bench -c user.email=bench@example.com
      -c commit.gpgsign=false -c core.autocrlf=false -c gc.auto=0)

now() { date +%s.%N; }
el() { echo "$2 - $1" | bc; }

choose_read_list() { # $1 = clone, $2 = output file; prints the directory
  local dir
  dir=$(git -C "$1" ls-tree -r --name-only HEAD | grep / | cut -d/ -f1 | sort | uniq -c | sort -rn | head -1 | awk '{print $2}')
  git -C "$1" ls-tree -r --name-only HEAD -- "$dir" | head -2000 >"$2"
  echo "$dir"
}

largest_blob() { # $1 = clone; prints "<size> <path>" of the largest regular blob under 512 MiB
  git -C "$1" ls-tree -r -l HEAD | awk '$1 == "100644" && $4 < 536870912 { size=$4; $1=$2=$3=$4=""; sub(/^ +/, ""); print size, $0 }' | sort -rn | head -1
}

read_through() { ( cd "$1" && xargs -d '\n' cat <"$2" >/dev/null 2>&1 ); }

wait_prewarm() { # $1 = workspace
  local i
  for ((i = 0; i < 6000; i++)); do
    if "$BIN/gfs" inspect --workspace "$1" 2>/dev/null | grep -q '^prewarm .*done'; then return 0; fi
    sleep 0.05
  done
  return 1
}

per_op() { # $1 = workspace, $2 = base file relative path
  python3 - "$1" "$2" <<'PY'
import os, sys, time
ws, base = sys.argv[1], sys.argv[2]
N = 2000
p = os.path.join(ws, base)
def bench(fn, n=N):
    t0 = time.perf_counter()
    for _ in range(n): fn()
    return (time.perf_counter() - t0) / n * 1e6
def oc():
    fd = os.open(p, os.O_RDONLY); os.close(fd)
def orc():
    fd = os.open(p, os.O_RDONLY); os.read(fd, 65536); os.close(fd)
def st():
    os.stat(p)
oc(); orc()
r = {}
r["open+close, base blob"] = bench(oc)
r["open+read+close, base blob"] = bench(orc)
r["stat, cached"] = bench(st)
o = os.path.join(ws, "perop.bin")
fd = os.open(o, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
buf = b"x" * 4096
t0 = time.perf_counter()
for i in range(N): os.pwrite(fd, buf, i * 4096)
os.close(fd)
r["write 4 KiB, overlay file"] = (time.perf_counter() - t0) / N * 1e6
def ocw():
    fd = os.open(o, os.O_RDONLY); os.close(fd)
def orcw():
    fd = os.open(o, os.O_RDONLY); os.read(fd, 65536); os.close(fd)
r["open+close, overlay file"] = bench(ocw)
r["open+read+close, overlay file"] = bench(orcw)
for k, v in r.items(): print(f"| {k} | {v:.1f} µs |")
PY
}

run_one() { # $1 = repo
  local repo="$1" clone="$LOCAL_DIR/$repo"
  [ -d "$clone/.git" ] || { echo "[$repo] no clone at $clone; run benchmark-local.sh once" >&2; return 1; }
  rm -rf "${WORK:?}/$repo"; mkdir -p "$WORK/$repo"
  export GFS_HOST_SOCKET="$WORK/$repo/host.sock"
  export XDG_CACHE_HOME="$WORK/$repo/xdg-cache"
  local list="$WORK/$repo/read-list.txt"
  local read_dir; read_dir=$(choose_read_list "$clone" "$list")
  local big; big=$(largest_blob "$clone")
  local big_size="${big%% *}" big_path="${big#* }"
  local ws="$WORK/$repo/ws"
  local t0 t1

  local native="${GFS_BENCH_NATIVE:-0}"
  if [ "$native" = 1 ]; then
    t0=$(now); git -C "$clone" "${GITC[@]}" worktree add -q --detach "$ws" HEAD >"$WORK/$repo/mount.log" 2>&1; t1=$(now)
  else
    t0=$(now); "$BIN/gfs" mount --local "$clone" --rev HEAD --workspace "$ws" "${MOUNT_FLAGS[@]}" >"$WORK/$repo/mount.log" 2>&1; t1=$(now)
  fi
  local r_mount; r_mount=$(el "$t0" "$t1")
  if [ "$native" != 1 ] && ! grep -q '^origin' "$WORK/$repo/mount.log"; then echo "[$repo] mount failed:" >&2; cat "$WORK/$repo/mount.log" >&2; return 1; fi
  local r_prewarm="–"
  if [ "${GFS_BENCH_WAIT_PREWARM:-0}" = 1 ]; then
    t0=$(now); wait_prewarm "$ws" || echo "[$repo] prewarm did not finish" >&2; t1=$(now)
    r_prewarm="$(printf '%.3f s' "$(el "$t0" "$t1")")"
  fi

  t0=$(now); read_through "$ws" "$list"; t1=$(now); local r_read=$(el "$t0" "$t1")
  t0=$(now); read_through "$ws" "$list"; t1=$(now); local r_read2=$(el "$t0" "$t1")
  t0=$(now); local n_rg; n_rg=$("$RG" -F TODO "$ws" 2>/dev/null | wc -l); t1=$(now); local r_rg=$(el "$t0" "$t1")
  t0=$(now); "$RG" -F TODO "$ws" >/dev/null 2>&1; t1=$(now); local r_rg2=$(el "$t0" "$t1")
  t0=$(now); cat "$ws/$big_path" >/dev/null; t1=$(now); local r_big=$(el "$t0" "$t1")
  t0=$(now); cat "$ws/$big_path" >/dev/null; t1=$(now); local r_big2=$(el "$t0" "$t1")
  t0=$(now); dd if=/dev/zero of="$ws/bench.bin" bs=4k count=16384 status=none; t1=$(now); local r_dd=$(el "$t0" "$t1")
  t0=$(now); cp -r "$clone/$read_dir" "$ws/bench-copy"; t1=$(now); local r_cp=$(el "$t0" "$t1")
  local n_cp; n_cp=$(find "$clone/$read_dir" -type f | wc -l)
  t0=$(now); cat "$ws/bench.bin" >/dev/null; t1=$(now); local r_ddread=$(el "$t0" "$t1")
  t0=$(now); ( cd "$ws/bench-copy" && find . -type f -print0 | xargs -0 cat >/dev/null ); t1=$(now); local r_cpread=$(el "$t0" "$t1")
  t0=$(now); git -C "$ws" "${GITC[@]}" status --porcelain >/dev/null; t1=$(now); local r_stat=$(el "$t0" "$t1")
  t0=$(now); git -C "$ws" "${GITC[@]}" add -A >/dev/null 2>&1; git -C "$ws" "${GITC[@]}" commit -q -m bench >/dev/null 2>&1; t1=$(now); local r_cmt=$(el "$t0" "$t1")
  local perop; perop=$(per_op "$ws" "$(head -1 "$list")")
  if [ "$native" = 1 ]; then
    : >"$WORK/$repo/inspect.txt"
    git -C "$clone" worktree remove --force "$ws" >/dev/null 2>&1; git -C "$clone" worktree prune >/dev/null 2>&1
  else
    "$BIN/gfs" inspect --workspace "$ws" >"$WORK/$repo/inspect.txt" 2>&1
    "$BIN/gfs" unmount --workspace "$ws" >/dev/null 2>&1
  fi
  local anchors; anchors=$(git -C "$clone" for-each-ref refs/gfs/ | wc -l)

  echo "## $repo — $label"
  echo "mount flags: $([ "$native" = 1 ] && echo "native worktree" || echo "${MOUNT_FLAGS[*]:-(none)}")   read-through: $(wc -l <"$list") files under $read_dir/   largest blob: $big_path ($big_size bytes)   copy: $n_cp files"
  echo
  printf '| step | %s |\n| --- | ---: |\n' "$label"
  printf '| mount | %.3f s |\n' "$r_mount"
  printf '| prewarm (waited) | %s |\n' "$r_prewarm"
  printf '| read 2000 files, cold | %.3f s |\n' "$r_read"
  printf '| read again, warm | %.3f s |\n' "$r_read2"
  printf '| `rg -F TODO`, first (%s lines) | %.3f s |\n' "$n_rg" "$r_rg"
  printf '| `rg -F TODO`, second | %.3f s |\n' "$r_rg2"
  printf '| read largest blob, cold | %.3f s |\n' "$r_big"
  printf '| read largest blob, warm | %.3f s |\n' "$r_big2"
  printf '| write 64 MiB, 4 KiB `dd` | %.3f s |\n' "$r_dd"
  printf '| `cp -r` %s files in | %.3f s |\n' "$n_cp" "$r_cp"
  printf '| read the 64 MiB back | %.3f s |\n' "$r_ddread"
  printf '| read the copied files back | %.3f s |\n' "$r_cpread"
  printf '| `git status` after the writes | %.3f s |\n' "$r_stat"
  printf '| `git add -A` + commit | %.3f s |\n' "$r_cmt"
  echo "$perop"
  echo
  grep -E '^(kernel|prewarm|opens|reads|writes)' "$WORK/$repo/inspect.txt" | sed 's/^/    /'
  echo "anchors left: $anchors"
  echo
}

for repo in "$@"; do run_one "$repo"; done
