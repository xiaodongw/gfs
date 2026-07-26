#!/usr/bin/env bash
# M0.5 Git-command surface inside the mount.
#
# Decides what occupies `.git`: the synthesized read-only directory plus a `git`
# shim (DESIGN.md section 8.6's default), or a real shallow blobless partial
# clone whose promisor remote is the XVFS gateway.
#
# DESIGN.md is explicit that this is a measurement, not a preference, because
# the answer changes the milestone graph: if partial clone wins, the minimum
# M5 upload-pack/promisor scope becomes a predecessor of M2.
#
# The number that decides it is `git status` on the worst-case repository,
# because that is the command agents run most and the one whose cost lands
# exactly where XVFS is trying to save.
set -uo pipefail

CORPUS_DIR="${XVFS_CORPUS_DIR:-$HOME/xvfs-corpus}"
MIRROR_DIR="$CORPUS_DIR/mirrors"
REPO="${1:-linux}"
WORK="${XVFS_SURFACE_DIR:-$CORPUS_DIR/surface}"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_TERMINAL_PROMPT=0

mkdir -p "$WORK"
SRC="file://$MIRROR_DIR/$REPO.git"
[ -d "$MIRROR_DIR/$REPO.git" ] || { echo "missing mirror: $REPO" >&2; exit 1; }

now() { date +%s.%N; }
el()  { echo "scale=2; $2 - $1" | bc; }
mib() { echo "scale=1; $1/1048576" | bc; }

echo "# M0.5 measurements on \`$REPO\`"
echo
echo "## Option B: real shallow blobless partial clone"
echo

dst="$WORK/$REPO-partial"
rm -rf "$dst"
t0=$(now)
git -c uploadpack.allowFilter=true clone -q --depth 1 --filter=blob:none \
    "$SRC" "$dst" >"$WORK/clone.log" 2>&1
rc=$?
t1=$(now)
if [ $rc -ne 0 ]; then
    echo "clone FAILED: $(tail -1 "$WORK/clone.log")"
    exit 1
fi

files=$(find "$dst" -path "$dst/.git" -prune -o -type f -print | wc -l)
index_bytes=$(stat -c %s "$dst/.git/index" 2>/dev/null || echo 0)
git_bytes=$(du -sb "$dst/.git" | cut -f1)
work_bytes=$(du -sb "$dst" | cut -f1); work_bytes=$((work_bytes - git_bytes))

echo "| metric | value |"
echo "| --- | ---: |"
echo "| clone wall time (local file://) | $(el "$t0" "$t1") s |"
echo "| working-tree files checked out | $files |"
echo "| working tree | $(mib $work_bytes) MiB |"
echo "| .git total | $(mib $git_bytes) MiB |"
echo "| **.git/index** | **$(mib $index_bytes) MiB** |"
echo

# `git status` is the headline number. Run cold (right after clone, caches warm
# from the checkout but the index untouched) and then repeatedly.
run_status() {
    local s e
    s=$(now); git -C "$dst" "${@:2}" status --porcelain >/dev/null 2>&1; e=$(now)
    printf '| %-34s | %8s s |\n' "$1" "$(el "$s" "$e")"
}

echo "### \`git status\` cost"
echo
echo "| invocation | wall |"
echo "| --- | ---: |"
run_status "first (index freshly written)"
run_status "second"
run_status "third"
# Drop the index stat cache to model a cold job, which is what a fresh mount is.
git -C "$dst" update-index --refresh >/dev/null 2>&1
rm -f "$dst/.git/index.lock"
run_status "after update-index --refresh"

# How much metadata does status actually touch? Every index entry is stat()ed.
echo
if command -v strace >/dev/null 2>&1; then
    syscalls=$(strace -f -c -e trace=lstat,stat,statx,newfstatat \
        git -C "$dst" status --porcelain 2>&1 >/dev/null | tail -3 | head -1)
    echo "stat-family syscalls during \`git status\`: $syscalls"
else
    echo "_strace unavailable; the index entry count below is the proxy for"
    echo "metadata operations, since \`git status\` stats every entry._"
fi
entries=$(git -C "$dst" ls-files | wc -l)
echo
echo "Index entries (each one a \`stat\` on every \`git status\`): **$entries**"

# Diff with a small edit set: the other high-frequency command.
echo
echo "### \`git diff\` with a small edit set"
echo
mapfile -t victims < <(git -C "$dst" ls-files | head -5)
for f in "${victims[@]}"; do
    printf '\n// xvfs probe edit\n' >> "$dst/$f"
done
s=$(now); git -C "$dst" diff --stat >/dev/null 2>&1; e=$(now)
echo "| operation | wall |"
echo "| --- | ---: |"
echo "| \`git diff --stat\` over ${#victims[@]} edited files | $(el "$s" "$e") s |"
s=$(now); git -C "$dst" status --porcelain >/dev/null 2>&1; e=$(now)
echo "| \`git status\` with ${#victims[@]} edits | $(el "$s" "$e") s |"
for f in "${victims[@]}"; do git -C "$dst" checkout -- "$f" 2>/dev/null; done

# What a checkout would hydrate. Not run to completion on the worst case: the
# point is the size of the bill, not paying it.
echo
echo "### What bulk commands would hydrate"
echo
missing=$(git -C "$dst" rev-list --objects --all --missing=print 2>/dev/null | grep -c '^?')
missing=${missing:-0}
echo "- objects the promisor still owes this clone: **$missing**"
echo "- \`git checkout <other-rev>\` or \`git reset --hard\` would demand-fetch"
echo "  every blob it touches, one round trip per object unless batched."
echo

echo "## Option A: synthesized read-only .git"
echo
syn="$WORK/$REPO-synth"
rm -rf "$syn"; mkdir -p "$syn/.git"
# Git's repository detection requires `objects/` and `refs/` to exist in
# addition to HEAD and config. DESIGN.md section 8.6 lists only HEAD,
# packed-refs, config, and xvfs.json; with just those, every command below
# fails with "not a git repository" and the synthesized surface satisfies
# nothing at all. Measured, not assumed.
mkdir -p "$syn/.git/objects" "$syn/.git/refs"
head_oid=$(git --git-dir="$MIRROR_DIR/$REPO.git" rev-parse HEAD)
branch=$(git --git-dir="$MIRROR_DIR/$REPO.git" symbolic-ref --short HEAD 2>/dev/null || echo main)

# Exactly what DESIGN.md section 8.6 specifies: HEAD, a packed-refs entry for
# the pinned revision, a minimal config, and xvfs.json. No object database and
# no index, deliberately.
printf 'ref: refs/heads/%s\n' "$branch" > "$syn/.git/HEAD"
printf '# pack-refs with: peeled fully-peeled sorted \n%s refs/heads/%s\n' \
    "$head_oid" "$branch" > "$syn/.git/packed-refs"
cat > "$syn/.git/config" <<CFG
[core]
	repositoryformatversion = 0
	filemode = true
	bare = false
	logallrefupdates = false
CFG
cat > "$syn/.git/xvfs.json" <<JSON
{
  "repository": "$REPO",
  "commit": "$head_oid",
  "branch": "$branch",
  "api": "https://xvfs.invalid/v1",
  "surface": "synthesized-readonly"
}
JSON
mkdir -p "$syn/src" && echo 'fn main() {}' > "$syn/src/main.rs"

echo "| command | result |"
echo "| --- | --- |"
probe() {
    local desc="$1"; shift
    local out rc
    out=$(git -C "$syn" "$@" 2>&1); rc=$?
    out=$(echo "$out" | head -1 | cut -c1-72)
    if [ $rc -eq 0 ]; then
        printf '| `git %s` | works: `%s` |\n' "$*" "$out"
    else
        printf '| `git %s` | **fails** (exit %d): `%s` |\n' "$*" "$rc" "$out"
    fi
}
probe "" rev-parse --show-toplevel
probe "" rev-parse --git-dir
probe "" rev-parse HEAD
probe "" rev-parse --abbrev-ref HEAD
probe "" symbolic-ref --short HEAD
probe "" status --porcelain
probe "" log -1 --format=%H
probe "" ls-files
probe "" diff --stat
probe "" show HEAD:src/main.rs
probe "" cat-file -t HEAD

echo
echo "### Ownership and safe.directory under a bind mount"
echo
owner_uid=$(stat -c %u "$syn")
echo "- mount owned by uid $owner_uid, current uid $(id -u)"
out=$(git -C "$syn" rev-parse --show-toplevel 2>&1)
if echo "$out" | grep -q "dubious ownership"; then
    echo "- **rejected**: \`$out\`"
    echo "- the job UID must own the mount, or \`safe.directory\` must be set"
else
    echo "- accepted (same-UID case); a host-daemon mount owned by a different"
    echo "  UID triggers Git's \`dubious ownership\` check and needs"
    echo "  \`safe.directory\`, which is why M2.1 lists it"
fi

rm -rf "$dst" "$syn"
