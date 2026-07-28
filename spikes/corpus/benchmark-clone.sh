#!/usr/bin/env bash
# M0.1 clone baselines.
#
# Establishes what the workflows GFS competes with actually cost today, so
# "materially lower startup time, network transfer, and local disk" has a
# denominator. Without these numbers the M0 go/no-go gate cannot be evaluated.
#
# Clones run over file:// against the local mirrors rather than over the
# network, so the numbers are free of internet variance. The consequence is that
# wall time is a lower bound -- a real clone adds transfer time proportional to
# the pack bytes reported here -- and that is stated wherever the numbers are.
#
# Each clone is measured and then deleted, so peak disk stays at roughly one
# clone rather than the sum of all variants.
set -uo pipefail

CORPUS_DIR="${GFS_CORPUS_DIR:-$HOME/gfs-corpus}"
MIRROR_DIR="$CORPUS_DIR/mirrors"
WORK="${GFS_BENCH_DIR:-$CORPUS_DIR/bench}"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_TERMINAL_PROMPT=0

mkdir -p "$WORK"

# A real ripgrep binary, resolved explicitly.
#
# `rg` is a shell function in some interactive environments (Claude Code ships
# one), and a function is invisible to a script. An unresolved `rg` silently
# produced 0 hits in 0.05 s for every variant here, which reads as a plausible
# measurement rather than as a missing tool. Resolve it or refuse to guess.
RG="${GFS_RG:-$(command -v rg || echo "$HOME/.cargo/bin/rg")}"
if [ ! -x "$RG" ]; then
    echo "no ripgrep binary found; set GFS_RG or run: cargo install ripgrep" >&2
    exit 1
fi
echo "<!-- ripgrep: $("$RG" --version | head -1) -->"

# The corpus mirrors carry GFS's own narrow filter policy. These baselines
# measure what *Git* can do, so filtering is opened up for the duration and the
# policy is restored afterwards.
relax_filters() {
    git --git-dir="$1" config uploadpackfilter.allow true
}
restore_filters() {
    git --git-dir="$1" config uploadpackfilter.allow false
}

# A representative subtree per repository for the sparse-checkout variant.
sparse_path() {
    case "$1" in
        linux)  echo "drivers/net" ;;
        rust)   echo "compiler" ;;
        vscode) echo "src/vs/editor" ;;
        *)      echo "src" ;;
    esac
}

# A literal that exists in every corpus repository, for the search timing.
search_pattern() { echo "TODO"; }

bytes() { du -sb "$1" 2>/dev/null | cut -f1; }
mib() { echo "scale=1; $1/1048576" | bc; }

measure() {
    local repo="$1" variant="$2"; shift 2
    local src="file://$MIRROR_DIR/$repo.git"
    local dst="$WORK/$repo-$variant"
    rm -rf "$dst"

    local start end rc
    start=$(date +%s.%N)
    # Filters must be permitted by the source for the partial-clone variants.
    # These baselines measure what Git can do, deliberately unconstrained by
    # GFS's own narrower filter policy.
    git -c uploadpack.allowFilter=true \
        -c uploadpackfilter.allow=true \
        clone -q "$@" "$src" "$dst" >"$WORK/$repo-$variant.log" 2>&1
    rc=$?
    end=$(date +%s.%N)

    if [ $rc -ne 0 ]; then
        printf '| %-8s | %-22s | %s |\n' "$repo" "$variant" \
            "FAILED: $(tail -1 "$WORK/$repo-$variant.log" | cut -c1-60)"
        rm -rf "$dst"
        return
    fi

    local wall gitb workb totalb files packb gitdir
    wall=$(echo "$end - $start" | bc)
    totalb=$(bytes "$dst")
    # A bare clone has no .git subdirectory: the repository *is* the directory,
    # and there is no working tree at all.
    if [ -d "$dst/.git" ]; then
        gitdir="$dst/.git"
        gitb=$(bytes "$gitdir")
        workb=$((totalb - gitb))
        files=$(find "$dst" -path "$dst/.git" -prune -o -type f -print 2>/dev/null | wc -l)
    else
        gitdir="$dst"
        gitb=$totalb
        workb=0
        files=0
    fi
    packb=$(bytes "$gitdir/objects")

    # Search cost over whatever the workflow actually materialized. A variant
    # that checked out nothing searches nothing, which is the point.
    local rgtime rghits
    local rgstart rgend
    rgstart=$(date +%s.%N)
    rghits=$("$RG" --no-messages -c "$(search_pattern)" "$dst" 2>/dev/null | wc -l)
    rgend=$(date +%s.%N)
    rgtime=$(echo "$rgend - $rgstart" | bc)

    printf '| %-8s | %-22s | %7.1f | %9s | %9s | %9s | %8s | %6.2f | %6s |\n' \
        "$repo" "$variant" "$wall" \
        "$(mib "$gitb")" "$(mib "$workb")" "$(mib "$packb")" \
        "$files" "$rgtime" "$rghits"

    rm -rf "$dst"
}

measure_sparse() {
    local repo="$1"
    local src="file://$MIRROR_DIR/$repo.git"
    local dst="$WORK/$repo-sparse"
    local path; path=$(sparse_path "$repo")
    rm -rf "$dst"

    local start end
    start=$(date +%s.%N)
    git -c uploadpack.allowFilter=true -c uploadpackfilter.allow=true \
        clone -q --filter=blob:none --no-checkout "$src" "$dst" \
        >"$WORK/$repo-sparse.log" 2>&1 \
      && git -C "$dst" sparse-checkout set --cone "$path" >>"$WORK/$repo-sparse.log" 2>&1 \
      && git -C "$dst" checkout -q >>"$WORK/$repo-sparse.log" 2>&1
    local rc=$?
    end=$(date +%s.%N)
    if [ $rc -ne 0 ]; then
        printf '| %-8s | %-22s | %s |\n' "$repo" "sparse($path)" \
            "FAILED: $(tail -1 "$WORK/$repo-sparse.log" | cut -c1-60)"
        rm -rf "$dst"; return
    fi

    local wall gitb workb totalb files packb rgs rge
    wall=$(echo "$end - $start" | bc)
    gitb=$(bytes "$dst/.git"); totalb=$(bytes "$dst"); workb=$((totalb - gitb))
    packb=$(bytes "$dst/.git/objects")
    files=$(find "$dst" -path "$dst/.git" -prune -o -type f -print 2>/dev/null | wc -l)
    rgs=$(date +%s.%N)
    local rghits; rghits=$("$RG" --no-messages -c "$(search_pattern)" "$dst" 2>/dev/null | wc -l)
    rge=$(date +%s.%N)

    printf '| %-8s | %-22s | %7.1f | %9s | %9s | %9s | %8s | %6.2f | %6s |\n' \
        "$repo" "sparse:$path" "$wall" \
        "$(mib "$gitb")" "$(mib "$workb")" "$(mib "$packb")" \
        "$files" "$(echo "$rge - $rgs" | bc)" "$rghits"
    rm -rf "$dst"
}

echo "| repo | variant | wall s | .git MiB | work MiB | objects MiB | files | rg s | rg hits |"
echo "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"

for repo in "$@"; do
    [ -d "$MIRROR_DIR/$repo.git" ] || { echo "missing mirror: $repo" >&2; continue; }
    relax_filters "$MIRROR_DIR/$repo.git"
    measure "$repo" full
    measure "$repo" shallow-depth1        --depth 1
    measure "$repo" blobless              --filter=blob:none
    measure "$repo" treeless              --filter=tree:0
    measure "$repo" shallow+blobless      --depth 1 --filter=blob:none
    measure_sparse "$repo"
    measure "$repo" bare-full             --bare
    restore_filters "$MIRROR_DIR/$repo.git"
done
