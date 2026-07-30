#!/usr/bin/env bash
# Measure what raw Git costs against a projected object store.
#
# ADR 0005 chose a synthesized, object-free `.git` plus a shim, and its central
# measurement was that a real `.git` turns `git status` into a full metadata
# sweep of the monorepo: 94 850 first-time FUSE lookups on the Linux kernel.
# `spikes/reports/m05-git-surface.md` states the gap this script closes:
#
#   "`git status` was measured with Git's default settings. `core.untrackedCache`
#    and `core.fsmonitor` can reduce the sweep substantially and were not
#    evaluated; neither is available through a FUSE mount without further work,
#    which is why they do not change the decision."
#
# So the sweep is measured here under three configurations, not one.
#
# The shape under test is stock Git throughout:
#
#   <mount>/tree           the working tree, projected read-only
#   <mount>/objects        the gateway's object database, projected read-only
#   <local>/agent-git      the agent's real git dir, on LOCAL disk:
#                            objects/info/alternates -> <mount>/objects
#                            index                   -> shipped by the gateway
#                            HEAD, refs/, config     -> the pinned ref view
#
# That split is `git worktree`'s own: per-worktree state (`HEAD`, `index`,
# `logs/`) is small and writable, `commondir`'s object store is shared and
# read-only. The alternates pointer is what `git clone --shared` uses.
#
# What is being measured is Git's *demand* on a projection — lookups, reads,
# bytes, bucketed by whether they land on the tree or the object store. One
# lookup here is one snapshot-API round trip or cache hit in a real mount, and a
# byte read under `objects/pack/` is a byte a real gateway must ship. The lower
# directory is materialized on local disk to make the measurement possible; that
# is the instrument, not a claim about how GFS stores anything.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
CORPUS_DIR="${GFS_CORPUS_DIR:-$HOME/gfs-corpus}"
MIRROR_DIR="$CORPUS_DIR/mirrors"
WORK="${GFS_PROJECTION_WORK:-$CORPUS_DIR/projection}"
PROBE="$root/spikes/target/release/git-projection-probe"

repo="${1:-django}"
mirror="$MIRROR_DIR/$repo.git"
[ -d "$mirror" ] || { echo "no mirror at $mirror; run spikes/corpus/fetch-corpus.sh $repo" >&2; exit 1; }
[ -x "$PROBE" ] || { echo "build first: (cd spikes && cargo build --release -p git-projection-probe)" >&2; exit 1; }

stage="$WORK/$repo"
lower="$stage/lower"
mnt="$stage/mnt"
out="$stage/out"
# A fixed snapshot time, the way DESIGN.md section 8.2 requires: identical across
# remounts and hosts, and derived from nothing on the host clock. Both the shipped
# index's stat data and the projection's reported mtime are set to this, which is
# the property `core.checkStat=minimal` needs to hold.
SNAPSHOT_TIME=1600000000

echo "== staging $repo =="
mkdir -p "$out"
if [ ! -f "$stage/.staged" ]; then
    rm -rf "$lower" "$stage/gateway-git"
    mkdir -p "$lower/tree" "$stage/gateway-git"

    # The gateway's object database. Repacked so the object store has the shape a
    # served repository would: a small number of large packs with a bitmap, not
    # thousands of loose objects.
    # Only if the mirror is not already one pack. A fresh `clone --mirror` gets a
    # single server-generated pack, and repacking a monorepo-sized one costs tens
    # of minutes to arrive at the same shape.
    if [ "$(find "$mirror/objects/pack" -name '*.pack' | wc -l)" -gt 1 ]; then
        echo "-- repacking the mirror (once; this is the server's cost, not a job's)"
        git --git-dir="$mirror" -c gc.auto=0 repack -adq --write-bitmap-index || true
    else
        echo "-- mirror is already a single pack; not repacking"
    fi
    # A hardlinked copy, NOT a symlink. A symlink here reads as a symlink through
    # the projection, Git follows it to the absolute path underneath, and every
    # object read bypasses the mount -- which silently reports zero pack traffic
    # for commands that certainly read packs. Hardlinks cost no disk and keep the
    # reads inside the projection where they can be counted.
    cp -al "$mirror/objects" "$lower/objects"

    commit="$(git --git-dir="$mirror" rev-parse HEAD)"
    echo "-- pinned commit $commit"
    echo "$commit" > "$stage/commit"

    # The gateway builds the index and the checkout once. In production this is
    # per commit and shared by every mount of it; here it is what produces both
    # the projected tree and the index that ships with it.
    export GIT_DIR="$stage/gateway-git"
    git init -q --bare "$GIT_DIR"
    mkdir -p "$GIT_DIR/objects/info"
    echo "$mirror/objects" > "$GIT_DIR/objects/info/alternates"
    git --git-dir="$GIT_DIR" config core.bare false
    git --git-dir="$GIT_DIR" config core.autocrlf false
    git --git-dir="$GIT_DIR" config core.worktree "$lower/tree"
    echo "$commit" > "$GIT_DIR/refs/heads/main"
    echo "ref: refs/heads/main" > "$GIT_DIR/HEAD"

    echo "-- checking out the tree (the gateway's one-time cost)"
    git --git-dir="$GIT_DIR" --work-tree="$lower/tree" read-tree "$commit"
    git --git-dir="$GIT_DIR" --work-tree="$lower/tree" checkout-index -a -f

    # Every projected entry reports SNAPSHOT_TIME, so the index the gateway ships
    # has to record SNAPSHOT_TIME too or `status` finds every file stat-dirty and
    # re-hashes the whole tree. Setting it here is what makes one index valid on
    # every host that mounts this commit.
    echo "-- normalizing mtimes to the snapshot time"
    find "$lower/tree" -exec touch -h -d "@$SNAPSHOT_TIME" {} + 2>/dev/null || true
    git --git-dir="$GIT_DIR" --work-tree="$lower/tree" update-index --refresh >/dev/null 2>&1 || true
    unset GIT_DIR
    touch "$stage/.staged"
fi

commit="$(cat "$stage/commit")"
files="$(git --git-dir="$stage/gateway-git" --work-tree="$lower/tree" ls-files | wc -l)"
index_bytes="$(stat -c%s "$stage/gateway-git/index")"
pack_bytes="$(du -sbL "$lower/objects" | cut -f1)"
tree_bytes="$(du -sb "$lower/tree" | cut -f1)"

echo
echo "== corpus =="
printf '%-22s %s\n' "repository" "$repo"
printf '%-22s %s\n' "pinned commit" "$commit"
printf '%-22s %s\n' "files at tip" "$files"
printf '%-22s %s MiB\n' "shipped index" "$((index_bytes / 1048576))"
printf '%-22s %s MiB\n' "object store" "$((pack_bytes / 1048576))"
printf '%-22s %s MiB\n' "working tree" "$((tree_bytes / 1048576))"

# The fsmonitor hook. In production this is answered from the overlay journal,
# which is the authoritative modified-path set by construction -- the FUSE server
# sees every mutation, so unlike watchman there is no watcher race. A static
# "nothing changed since the token" is the right stand-in, because the question is
# what Git *does* with the answer, not how GFS computes it.
cat > "$stage/fsmonitor-hook" <<'HOOK'
#!/bin/sh
# fsmonitor v2: print a token, then NUL-separated changed paths. No paths means
# nothing changed, which is what lets Git skip the lstat of every index entry.
printf 'gfs:1:0'
HOOK
chmod +x "$stage/fsmonitor-hook"

# A fresh agent workspace per configuration, so no measurement inherits another's
# refreshed index or primed untracked cache.
new_agent() {
    local name="$1"; shift
    local dir="$stage/agent-$name"
    rm -rf "$dir"
    mkdir -p "$dir/objects/info" "$dir/refs/heads"
    # The object database is the projection, reached the way `git clone --shared`
    # reaches one. Nothing is copied.
    echo "$mnt/objects" > "$dir/objects/info/alternates"
    echo "ref: refs/heads/main" > "$dir/HEAD"
    echo "$commit" > "$dir/refs/heads/main"
    # The pinned ref view: one ref, captured at mount time. A live-projected
    # `refs/heads/main` would move under a workspace whose index still describes
    # the old commit, and Git would report the entire repository as modified.
    printf '[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tautocrlf = false\n\tlogAllRefUpdates = false\n' > "$dir/config"
    # Maintenance off. `git repack -a` without `-l` copies borrowed objects out of
    # the alternate and into a local pack, which is a full hydration of the object
    # database triggered by routine housekeeping. Phase 4 measures that.
    git --git-dir="$dir" config gc.auto 0
    git --git-dir="$dir" config maintenance.auto false
    # The index the gateway shipped, byte for byte.
    cp "$stage/gateway-git/index" "$dir/index"
    for kv in "$@"; do
        git --git-dir="$dir" config "${kv%%=*}" "${kv#*=}"
    done
    echo "$dir"
}

# Every phase runs against one agent workspace, so GIT_DIR is per-probe
# environment rather than per-command text.
gitenv() { printf -- "--env\nGIT_DIR=%s\n--env\nGIT_WORK_TREE=%s/tree\n" "$1" "$mnt"; }

echo
echo "== phase 1: does raw Git work through the projection at all =="
a="$(new_agent semantics core.checkStat=minimal core.trustctime=false)"
"$PROBE" --lower "$lower" --mnt "$mnt" --snapshot-time "$SNAPSHOT_TIME" \
    --json "$out/phase1.json" $(gitenv "$a") \
    --run "git rev-parse HEAD" \
    --run "git log --oneline -5 | tail -1" \
    --run "git cat-file -t $commit" \
    --run "git ls-files | wc -l" \
    --run "git show --stat --oneline HEAD | tail -1"

echo
echo "== phase 2: git status, three configurations =="
echo "-- 2a: Git defaults (what ADR 0005 measured)"
a="$(new_agent default)"
"$PROBE" --lower "$lower" --mnt "$mnt" --snapshot-time "$SNAPSHOT_TIME" \
    --json "$out/phase2a.json" $(gitenv "$a") \
    --run "git status --porcelain | wc -l" \
    --run "git status --porcelain | wc -l"

echo "-- 2b: + core.checkStat=minimal, core.trustctime=false"
a="$(new_agent minimal core.checkStat=minimal core.trustctime=false)"
"$PROBE" --lower "$lower" --mnt "$mnt" --snapshot-time "$SNAPSHOT_TIME" \
    --json "$out/phase2b.json" $(gitenv "$a") \
    --run "git status --porcelain | wc -l" \
    --run "git status --porcelain | wc -l"

echo "-- 2c: + core.fsmonitor, core.untrackedCache"
a="$(new_agent fsmonitor core.checkStat=minimal core.trustctime=false \
        "core.fsmonitor=$stage/fsmonitor-hook" core.untrackedCache=true)"
"$PROBE" --lower "$lower" --mnt "$mnt" --snapshot-time "$SNAPSHOT_TIME" \
    --json "$out/phase2c.json" $(gitenv "$a") \
    --run "git status --porcelain | wc -l" \
    --run "git status --porcelain | wc -l" \
    --run "git status --porcelain | wc -l"

echo
echo "== phase 3: the history questions GFS reimplemented as subcommands =="
# Every stage reads to EOF. `head -1` or `awk '{exit}'` would close the pipe
# early, send SIGPIPE to `ls-files`, and abort the script under `pipefail`.
blame_file="$(git --git-dir="$stage/gateway-git" --work-tree="$lower/tree" ls-files \
    | grep -E '\.(c|py|rs|ts|js|go|java)$' | sed -n 1p)"
echo "-- blame target: $blame_file"
a="$(new_agent history core.checkStat=minimal core.trustctime=false \
        "core.fsmonitor=$stage/fsmonitor-hook" core.untrackedCache=true)"
history_cmds=(
    "git log --oneline -20 | wc -l"
    "git log --format=%H -100 | wc -l"
    "git show --stat HEAD | tail -1"
    "git log -10 -p | wc -l"
    "git diff --stat HEAD~5 HEAD | tail -1"
    "git log -20 -- $blame_file | grep -c ^commit"
    "git blame --porcelain $blame_file | grep -c ^author-time"
    "git ls-files '*test*' | wc -l"
)

# One mount per command. Within a single mount the kernel page-caches packfile
# pages across commands under FOPEN_KEEP_CACHE, so a later command can read zero
# bytes purely because an earlier one already paid -- which would understate what
# a gateway has to ship for the *first* command that needs it. A fresh mount is
# the cold number.
echo "-- 3a: cold, one mount per command"
i=0
for cmd in "${history_cmds[@]}"; do
    "$PROBE" --lower "$lower" --mnt "$mnt" --snapshot-time "$SNAPSHOT_TIME" \
        --json "$out/phase3a-$i.json" $(gitenv "$a") --run "$cmd"
    i=$((i + 1))
done

# And the steady state: the same commands in one session, where the mount's cache
# is warm. A job runs many Git commands, so this is the honest second number.
echo "-- 3b: warm, all in one mount"
runs=()
for cmd in "${history_cmds[@]}"; do runs+=(--run "$cmd"); done
"$PROBE" --lower "$lower" --mnt "$mnt" --snapshot-time "$SNAPSHOT_TIME" \
    --json "$out/phase3b.json" $(gitenv "$a") "${runs[@]}"

# The gateway's lever. Without a commit-graph, walking history means reading commit
# objects out of the pack, and a path-limited `log` re-reads trees across every
# commit it considers. A commit-graph answers the walk from a purpose-built file,
# and `--changed-paths` adds the Bloom filters that exist precisely so
# `log -- <path>` can skip commits without loading their trees. It is written once
# per repository on the gateway and shared by every mount, so if it moves these
# numbers it is free.
echo "-- 3c: cold, with a commit-graph (--changed-paths)"
if [ ! -f "$lower/objects/info/commit-graph" ]; then
    git --git-dir="$mirror" commit-graph write --reachable --changed-paths 2>/dev/null
    cp -f "$mirror/objects/info/commit-graph" "$lower/objects/info/commit-graph"
fi
printf '%-22s %s MiB\n' "commit-graph" \
    "$(( $(stat -c%s "$lower/objects/info/commit-graph") / 1048576 ))"
i=0
for cmd in "${history_cmds[@]}"; do
    "$PROBE" --lower "$lower" --mnt "$mnt" --snapshot-time "$SNAPSHOT_TIME" \
        --json "$out/phase3c-$i.json" $(gitenv "$a") --run "$cmd"
    i=$((i + 1))
done

echo
echo "== phase 5: pin the shared metadata, project only pack data =="
# Phases 3a/3c show the dominant term is not object data but the pack's lookup
# structures -- `.idx`, `.bitmap`, `.rev` -- plus the commit-graph. Those files are
# byte-identical for every mount of the repository and immutable by name, so a host
# can hold one copy and every mount can share it, exactly as ADR 0008 already does
# with one blob cache per repository instead of one per mount.
#
# This measures that design rather than asserting it: the lookup structures are
# local files, and only `*.pack` is reached through the projection, by symlink from
# the same directory Git expects to find both in.
nodecache="$stage/nodecache"
if [ ! -d "$nodecache" ]; then
    mkdir -p "$nodecache/objects/pack" "$nodecache/objects/info"
    for f in "$lower"/objects/pack/*.idx "$lower"/objects/pack/*.bitmap "$lower"/objects/pack/*.rev; do
        [ -e "$f" ] && cp -l "$f" "$nodecache/objects/pack/" 2>/dev/null || true
    done
    [ -f "$lower/objects/info/commit-graph" ] && \
        cp -l "$lower/objects/info/commit-graph" "$nodecache/objects/info/commit-graph"
    # The one file still served by the projection. Git resolves the symlink and
    # every read of it is counted, while `.idx` reads are not because they never
    # cross the mount.
    for f in "$lower"/objects/pack/*.pack; do
        ln -sf "$mnt/objects/pack/$(basename "$f")" "$nodecache/objects/pack/$(basename "$f")"
    done
fi
printf '%-22s %s MiB\n' "pinned per node" \
    "$(( $(du -sbL --exclude='*.pack' "$nodecache/objects" | cut -f1) / 1048576 ))"
a="$(new_agent nodecache core.checkStat=minimal core.trustctime=false \
        "core.fsmonitor=$stage/fsmonitor-hook" core.untrackedCache=true)"
echo "$nodecache/objects" > "$a/objects/info/alternates"
i=0
for cmd in "${history_cmds[@]}"; do
    "$PROBE" --lower "$lower" --mnt "$mnt" --snapshot-time "$SNAPSHOT_TIME" \
        --json "$out/phase5-$i.json" $(gitenv "$a") --run "$cmd"
    i=$((i + 1))
done

echo
echo "== phase 4: the footguns =="
# Skippable because it repacks the whole object store: on a monorepo that is
# minutes of CPU and a second copy of the pack on disk, and the result does not
# change between runs.
if [ "${GFS_SKIP_FOOTGUN:-0}" = "1" ]; then
    echo "(skipped: GFS_SKIP_FOOTGUN=1)"
    echo; echo "JSON in $out"; exit 0
fi
# `repack -a` without `-l` is the one that matters: it pulls every borrowed object
# out of the alternate and writes it into a local pack. If routine maintenance can
# hydrate the whole object database, the projection has to disable it explicitly.
a="$(new_agent footgun core.checkStat=minimal core.trustctime=false)"
"$PROBE" --lower "$lower" --mnt "$mnt" --snapshot-time "$SNAPSHOT_TIME" \
    --json "$out/phase4.json" $(gitenv "$a") \
    --run "git count-objects -v | tail -2" \
    --run "git repack -a -d -q 2>&1 | tail -1; du -sb $a/objects/pack 2>/dev/null | cut -f1"

echo
echo "JSON in $out"
