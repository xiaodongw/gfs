#!/usr/bin/env bash
# M0.1 repository characterization.
#
# Everything PLAN.md M0.1 asks to record about a repository, measured from the
# bare mirror. Separate from the clone benchmarks because these are properties
# of the repository, not of a workflow, and they change only when the corpus
# does.
set -uo pipefail

CORPUS_DIR="${GFS_CORPUS_DIR:-$HOME/gfs-corpus}"
MIRROR_DIR="$CORPUS_DIR/mirrors"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null

g() { git --git-dir="$1" "${@:2}"; }

for repo in "$@"; do
    bare="$MIRROR_DIR/$repo.git"
    [ -d "$bare" ] || { echo "missing mirror: $bare" >&2; continue; }

    head=$(g "$bare" rev-parse HEAD)
    echo "## $repo"
    echo
    echo "- mirror on disk: $(du -sh "$bare" | cut -f1)"
    echo "- HEAD: \`${head:0:12}\`  ($(g "$bare" log -1 --format=%cd --date=short))"
    echo "- refs: $(g "$bare" for-each-ref --format='%(refname)' | wc -l)" \
         "($(g "$bare" for-each-ref --format='%(refname)' refs/heads | wc -l) branches," \
         "$(g "$bare" for-each-ref --format='%(refname)' refs/tags | wc -l) tags)"
    echo "- commits reachable from HEAD: $(g "$bare" rev-list --count HEAD)"

    # Tip tree shape. One ls-tree pass feeds every count below.
    tmp=$(mktemp)
    g "$bare" ls-tree -r -t -z HEAD | tr '\0' '\n' > "$tmp"
    files=$(awk '$2=="blob"'   "$tmp" | wc -l)
    trees=$(awk '$2=="tree"'   "$tmp" | wc -l)
    links=$(awk '$1=="120000"' "$tmp" | wc -l)
    execs=$(awk '$1=="100755"' "$tmp" | wc -l)
    subs=$(awk  '$1=="160000"' "$tmp" | wc -l)
    uniq_blobs=$(awk '$2=="blob"{print $3}' "$tmp" | sort -u | wc -l)
    echo "- tip files: $files  (unique blobs $uniq_blobs, so $((files - uniq_blobs)) duplicate paths)"
    echo "- tip directories: $trees"
    echo "- symlinks: $links; executables: $execs; submodules: $subs"

    # Non-UTF-8 paths need the NUL-delimited form to be counted at all, and
    # need a real decoder rather than a regex: this number decides whether the
    # byte-path handling in M1.3 is theoretical or load-bearing for this corpus.
    nonutf8=$(g "$bare" ls-tree -r -z --name-only HEAD | python3 -c '
import sys
data = sys.stdin.buffer.read().split(b"\0")
count = 0
for n in data:
    if not n:
        continue
    try:
        n.decode("utf-8")
    except UnicodeDecodeError:
        count += 1
print(count)
')
    echo "- non-UTF-8 path names: $nonutf8"

    # Largest tracked blobs, which drive the whole-blob fetch decision.
    echo "- largest tracked blobs:"
    awk '$2=="blob"{print $3}' "$tmp" | sort -u \
      | g "$bare" cat-file --batch-check='%(objectsize) %(objectname)' 2>/dev/null \
      | sort -rn | head -5 \
      | while read -r size oid; do
            path=$(awk -v o="$oid" '$3==o {sub(/^[^\t]*\t/,""); print; exit}' "$tmp")
            printf '  - %8.1f MiB  %s\n' "$(echo "$size/1048576" | bc -l)" "$path"
        done

    # Total and over-limit content, which is what the search corpus excludes.
    read -r total over8 <<<"$(awk '$2=="blob"{print $3}' "$tmp" | sort -u \
        | g "$bare" cat-file --batch-check='%(objectsize)' 2>/dev/null \
        | awk '{t+=$1; if ($1 > 8*1024*1024) n++} END {print t+0, n+0}')"
    printf -- "- unique tracked content: %.1f MiB; blobs over the 8 MiB search limit: %s\n" \
        "$(echo "$total/1048576" | bc -l)" "$over8"

    # LFS and attributes change what a checkout produces versus what we serve.
    lfs=$(g "$bare" show "HEAD:.gitattributes" 2>/dev/null | grep -c 'filter=lfs' || true)
    attrs=$(g "$bare" show "HEAD:.gitattributes" 2>/dev/null | grep -cE '(^|\s)(text|eol=)' || true)
    echo "- root .gitattributes: $lfs LFS rules, $attrs text/eol conversion rules"

    echo "- top extensions:"
    awk '$2=="blob"{sub(/^[^\t]*\t/,""); n=split($0,p,"."); if (n>1) print p[n]}' "$tmp" \
      | sort | uniq -c | sort -rn | head -6 \
      | while read -r c e; do printf '  - %-8s %s\n' ".$e" "$c"; done

    rm -f "$tmp"
    echo
done
