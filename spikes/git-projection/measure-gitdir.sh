#!/usr/bin/env bash
# Measure what putting the agent's `.git` itself behind FUSE costs.
#
# The single-mount workspace proposal (successor to the two-mount layout ADR
# 0009 shipped) overmounts the workspace directory and serves the real `.git`
# through the same FUSE filesystem as the tree, so every index read, lockfile,
# ref update, and loose-object write becomes FUSE traffic. m05b measured Git's
# *read* demand on a projected object store; this closes the remaining gap by
# measuring Git's demand on its *own directory*, write path included.
#
# Isolation: both arms run the identical command list against the identical
# git dir contents and the identical on-disk working tree. The only variable is
# whether GIT_DIR is the real directory (today's layout) or a writable FUSE
# passthrough of it (the proposal). objects/info/alternates points at the
# staged object store on local disk in both arms, so pack traffic — measured
# by m05b, unchanged by this proposal's delta — drops out of the comparison.
#
# Usage: measure-gitdir.sh [repo]     (default django; linux is the worst case)
# Requires the staging measure.sh builds: run `measure.sh <repo>` once first.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
CORPUS_DIR="${GFS_CORPUS_DIR:-$HOME/gfs-corpus}"
WORK="${GFS_PROJECTION_WORK:-$CORPUS_DIR/projection}"
PROBE="$root/spikes/target/release/git-projection-probe"

repo="${1:-django}"
stage="$WORK/$repo"
lower="$stage/lower"
out="$stage/out"
[ -f "$stage/.staged" ] || { echo "stage $repo first: measure.sh $repo" >&2; exit 1; }
[ -x "$PROBE" ] || { echo "build first: (cd spikes && cargo build --release -p git-projection-probe)" >&2; exit 1; }

# Each command runs twice; the second run is the warm number. `read-tree HEAD`
# is the deliberate heavyweight: it rewrites the whole index (11 MB on linux)
# through whatever filesystem holds it. It also discards the index's stat
# data, which is why every status precedes it.
RUNS=(
  --run 'git rev-parse HEAD'
  --run 'git rev-parse HEAD'
  --run 'git status --porcelain | wc -l'
  --run 'git status --porcelain | wc -l'
  --run 'git log --oneline -1000 | wc -l'
  --run 'git log --oneline -1000 | wc -l'
  --run 'git read-tree HEAD'
  --run 'git read-tree HEAD'
  --run 'git commit --allow-empty -m spike-one'
  --run 'git commit --allow-empty -m spike-two'
  --run 'git update-ref refs/heads/spike HEAD'
  --run 'git update-ref refs/heads/spike2 HEAD'
)

prepare_gitdir() { # $1 = destination
  rm -rf "$1"
  cp -a "$stage/agent-default" "$1"
  # Straight at the staged object store on disk, in both arms: alternates
  # normally points at the object projection, and leaving it there would mix
  # m05b's already-measured cost into this measurement.
  echo "$lower/objects" > "$1/objects/info/alternates"
}

echo "== $repo: .git on local disk (today's layout) =="
prepare_gitdir "$stage/gitdir-local"
mkdir -p "$stage/gitdir-empty" "$stage/mnt-gitdir"
# The probe requires a mount; the local arm mounts an empty directory it never
# touches, so both arms pay the identical harness overhead per command.
"$PROBE" \
  --lower "$stage/gitdir-empty" --mnt "$stage/mnt-gitdir" \
  --env GIT_DIR="$stage/gitdir-local" --env GIT_WORK_TREE="$lower/tree" \
  --json "$out/gitdir-local.json" \
  "${RUNS[@]}"

for ttl in 1 60; do
  echo "== $repo: .git behind FUSE, ttl ${ttl}s (the single-mount proposal) =="
  prepare_gitdir "$stage/gitdir-fuse"
  "$PROBE" --rw --ttl "$ttl" \
    --lower "$stage/gitdir-fuse" --mnt "$stage/mnt-gitdir" \
    --env GIT_DIR="$stage/mnt-gitdir" --env GIT_WORK_TREE="$lower/tree" \
    --json "$out/gitdir-fuse-ttl$ttl.json" \
    "${RUNS[@]}"
done

echo "== $repo: .git behind FUSE + negative-dentry caching (the mitigation) =="
prepare_gitdir "$stage/gitdir-fuse"
"$PROBE" --rw --ttl 60 --negative-ttl 60 \
  --lower "$stage/gitdir-fuse" --mnt "$stage/mnt-gitdir" \
  --env GIT_DIR="$stage/mnt-gitdir" --env GIT_WORK_TREE="$lower/tree" \
  --json "$out/gitdir-fuse-neg.json" \
  "${RUNS[@]}"

echo
echo "== warm wall-clock, ms (second run of each command) =="
python3 - "$out" <<'EOF'
import json, sys, pathlib
out = pathlib.Path(sys.argv[1])
arms = ["gitdir-local", "gitdir-fuse-ttl1", "gitdir-fuse-ttl60", "gitdir-fuse-neg"]
reports = {a: json.loads((out / f"{a}.json").read_text()) for a in arms}
runs = reports[arms[0]]["runs"]
print(f"{'command':<42}" + "".join(f"{a.removeprefix('gitdir-'):>14}" for a in arms))
for i in range(1, len(runs), 2):  # odd indices: the warm repeat
    cmd = runs[i]["command"]
    row = f"{cmd[:40]:<42}"
    for a in arms:
        row += f"{reports[a]['runs'][i]['wall_ms']:>14}"
    print(row)
EOF
