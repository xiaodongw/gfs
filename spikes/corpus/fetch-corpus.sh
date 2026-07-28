#!/usr/bin/env bash
# Fetch the M0 benchmark corpus as bare mirrors.
#
# Mirrors are the stand-in for the GFS server's authoritative bare repository,
# so they are configured the way section 7.2 of DESIGN.md requires: the `files`
# ref backend, filtering enabled but restricted to the allowed families, and no
# hooks. Every later spike serves out of these mirrors over file:// or the
# gateway, which keeps the measurements off the public network.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CORPUS_DIR="${GFS_CORPUS_DIR:-$HOME/gfs-corpus}"
MIRROR_DIR="$CORPUS_DIR/mirrors"
LOG_DIR="$CORPUS_DIR/logs"
mkdir -p "$MIRROR_DIR" "$LOG_DIR"

fetch_one() {
    local id="$1" url="$2"
    local dest="$MIRROR_DIR/$id.git"

    if [ -d "$dest" ]; then
        echo "[$id] mirror exists, fetching updates"
        git --git-dir="$dest" fetch --prune origin >>"$LOG_DIR/$id.fetch.log" 2>&1
    else
        echo "[$id] cloning $url"
        # --mirror gives the full ref namespace and history, which is what the
        # worst-case baseline needs. ref-format=files is forced because libgit2
        # cannot read reftable (DESIGN.md section 5.1).
        git clone --mirror --ref-format=files "$url" "$dest" \
            >>"$LOG_DIR/$id.clone.log" 2>&1
    fi

    # Server-side policy the gateway will later set explicitly. Set here so
    # local file:// benchmarks exercise the same filter policy as the gateway.
    # Deny by default, then allow exactly the one filter GFS policy permits.
    # Key names are Git's config names per filter family: `blob:none` and
    # `blob:limit` keep their colons, but the `tree:<depth>` family is `tree`.
    git --git-dir="$dest" config uploadpack.allowFilter true
    git --git-dir="$dest" config uploadpackfilter.allow false
    git --git-dir="$dest" config 'uploadpackfilter.blob:none.allow' true
    git --git-dir="$dest" config 'uploadpackfilter.blob:limit.allow' false
    git --git-dir="$dest" config uploadpackfilter.tree.allow false
    git --git-dir="$dest" config 'uploadpackfilter.sparse:oid.allow' false
    git --git-dir="$dest" config uploadpack.allowAnySHA1InWant false
    git --git-dir="$dest" config uploadpack.allowReachableSHA1InWant false
    git --git-dir="$dest" config core.logAllRefUpdates false
    echo "[$id] ready: $(du -sh "$dest" | cut -f1)"
}

targets=("$@")
while read -r id role url; do
    case "$id" in ''|'#'*) continue ;; esac
    if [ ${#targets[@]} -gt 0 ]; then
        case " ${targets[*]} " in *" $id "*) ;; *) continue ;; esac
    fi
    fetch_one "$id" "$url"
done <"$here/corpus.conf"

echo "corpus ready in $MIRROR_DIR"
