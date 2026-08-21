#!/usr/bin/env bash
#
# The agent edit workflow, end to end, raw Git against GFS.
#
#   acquire a workspace -> read recent history -> find by name ->
#   grep by content -> edit files -> status -> commit
#
# `benchmark-clone.sh` measures the *clone*, which is only the first step. This
# measures the whole task, because the ranking changes once search is in it: the
# clone is where GFS wins and search is where it can lose everything back.
#
# Every step is timed separately and every step's *result* is recorded next to
# its time. A faster search that returns a different answer is not a faster
# search, so the two are never reported apart.
#
#   ./spikes/corpus/benchmark-workflow.sh vscode
#
# Takes repository ids from corpus.conf and hardcodes no repository names.
# Requires a release build (`scripts/build-release.sh`) and a real ripgrep
# binary; see the note in benchmarks/baseline.md about `rg` being a shell
# function in some environments.
#
# Both flows run **stock Git inside their own working tree** -- ADR 0009 put a
# real object database behind the mount, so `log`, `ls-files`, `status` and
# `commit` are the same commands on both sides. Only content search differs,
# because search is the one question GFS deliberately answers somewhere else.
# The earlier version of this harness predates that ADR: it drove `gfs log`,
# which no longer exists, and measured a sibling `<workspace>.gfs` state
# directory, which ADR 0011 moved inside the workspace.
#
# Three measurements exist because a single number hid something real:
#
#   * `status` is timed cold *and* warm. The first one in a fresh workspace
#     populates the untracked cache, which reads every directory once; on vscode
#     that is 5 328 uncached listings and 555 s, against 1.75 s warm.
#   * search is timed twice and its stderr is kept. An index that is not ready
#     answers SNAPSHOT_BUILDING, and a harness that discards stderr turns that
#     error into what looks like an empty result.
#   * disk is measured in allocated blocks, not apparent size: the odb block
#     cache is sparse, so `du --apparent-size` reports a pack's whole span
#     rather than the bytes actually fetched.

set -uo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"

CORPUS_DIR="${GFS_CORPUS_DIR:-$HOME/gfs-corpus}"
MIRROR_DIR="$CORPUS_DIR/mirrors"
WORK="${GFS_WORKFLOW_WORK:-$CORPUS_DIR/workflow}"
BIN="$root/target/release"
HTTP_ADDR="${GFS_BENCH_HTTP_ADDR:-127.0.0.1:8630}"
GRPC_ADDR="${GFS_BENCH_GRPC_ADDR:-127.0.0.1:8631}"
TOKEN=workflow-bench
CAP_KEY="$(printf 'gfs-workflow-benchmark-key-not-for-production!!' | od -An -tx1 | tr -d ' \n' | cut -c1-64)"

# Never `rg`: in at least one developer environment that is a shell function
# wrapping another tool, which made an earlier harness report 0 hits in 0.05 s
# for every variant -- a plausible-looking number rather than an obvious failure.
RG="${GFS_RG_BINARY:-}"
if [ -z "$RG" ]; then
  for candidate in "$HOME/.cargo/bin/rg" /usr/bin/rg /usr/local/bin/rg; do
    [ -x "$candidate" ] && RG="$candidate" && break
  done
fi
if [ -z "$RG" ] || ! "$RG" --version >/dev/null 2>&1; then
  echo "no ripgrep binary found; set GFS_RG_BINARY" >&2
  exit 2
fi

for tool in gfs gfs-fuse gfs-server; do
  if [ ! -x "$BIN/$tool" ]; then
    echo "missing $BIN/$tool; run scripts/build-release.sh" >&2
    exit 2
  fi
done

# Identity and config that must not vary between the flows, or the commits differ
# for reasons that have nothing to do with what is being measured. `safe.directory`
# because the mount may be owned by the daemon rather than by the job UID
# (DESIGN.md section 8.6).
GITC=(-c user.name=bench -c user.email=bench@example.com
      -c commit.gpgsign=false -c core.autocrlf=false -c gc.auto=0
      -c protocol.version=2 -c safe.directory='*')

now() { date +%s.%N; }
el() { echo "$2 - $1" | bc; }
mib() { du -sm --apparent-size "$1" 2>/dev/null | cut -f1; }
allocated() { du -s --block-size=1 "$1" 2>/dev/null | cut -f1; }

# The edit set: one of each change kind the overlay models, applied by the same
# function in every flow so a difference in the result is a difference in the
# system and not in the script.
#
# The four paths are chosen once, from `git ls-tree` on the served repository,
# and passed to both flows. Deriving them per-flow with `find` was wrong in a way
# that looked plausible: `find` walks in filesystem order, which is ext4's hash
# order in a clone and Git's tree order in the mount, so the two flows edited
# *different files* and the trees disagreed for a reason that had nothing to do
# with GFS. The comparison's whole value is that both sides did the same work.
#
# Regular files only (mode 100644): a delete or rename targeting a symlink would
# still be a valid test, but not the one the other flow ran.
# Text files only, by extension. Not fastidiousness: an earlier run picked
# `.mo` locale catalogs, and `git apply` cannot apply a binary patch without a
# full index line, so the export failed for a reason the M3 report already
# records as a known limitation rather than anything this benchmark is measuring.
choose_edit_paths() { # $1 = served bare repo
  git --git-dir="$1" ls-tree -r HEAD |
    awk '$1 == "100644" { $1=$2=$3=""; sub(/^ +/, ""); print }' |
    grep -v $'\t' |
    grep -Ei '\.(py|rs|ts|js|md|rst|txt|c|h|cc|cpp|go|java|rb|sh|toml|cfg|ini)$' |
    sed -n '10p;60p;200p;400p'
}

apply_edits() { # $1 = worktree, $2..$5 = the four chosen paths
  local w="$1" m1="$2" m2="$3" del="$4" ren="$5"
  [ -n "$m1" ] && printf '\n# benchmark: appended by the agent\n' >>"$w/$m1"
  [ -n "$m2" ] && printf '\nBENCH_MARKER = "gfs-benchmark"\n' >>"$w/$m2"
  printf 'agent scratch notes\nline two\n' >"$w/AGENT_NOTES.md"
  [ -n "$del" ] && rm -f "$w/$del"
  [ -n "$ren" ] && mv "$w/$ren" "$w/$ren.renamed"
  return 0
}

# One raw-git leg: clone with the given flags, then run the task in the clone.
# Results land in R_* for the caller to snapshot, because bash has no better way
# to return fifteen values.
raw_leg() { # $1 = label, $2 = destination, $3.. = clone flags
  local label="$1" d="$2"; shift 2
  local t0 t1
  t0=$(now); git "${GITC[@]}" clone -q "$@" "file://$served" "$d" >/dev/null 2>&1; t1=$(now)
  R_ACQ=$(el "$t0" "$t1")
  t0=$(now); R_LOG=$(git -C "$d" "${GITC[@]}" log -10 --oneline | wc -l); t1=$(now)
  R_LOG_S=$(el "$t0" "$t1")
  t0=$(now); R_FIND=$(git -C "$d" "${GITC[@]}" ls-files "$find_glob" | wc -l); t1=$(now)
  R_FIND_S=$(el "$t0" "$t1")
  t0=$(now); R_GREP=$("$RG" -F "$grep_pat" "$d" 2>/dev/null | wc -l); t1=$(now)
  R_GREP_S=$(el "$t0" "$t1")
  t0=$(now); apply_edits "$d" "$m1" "$m2" "$del" "$ren"; t1=$(now)
  R_EDIT=$(el "$t0" "$t1")
  t0=$(now); git -C "$d" "${GITC[@]}" status --porcelain >/dev/null; t1=$(now)
  R_STAT=$(el "$t0" "$t1")
  t0=$(now); git -C "$d" "${GITC[@]}" status --porcelain >/dev/null; t1=$(now)
  R_STAT2=$(el "$t0" "$t1")
  t0=$(now)
  git -C "$d" "${GITC[@]}" add -A >/dev/null 2>&1
  git -C "$d" "${GITC[@]}" commit -q -m "bench: agent edit" >/dev/null 2>&1
  t1=$(now); R_CMT=$(el "$t0" "$t1")
  R_TREE=$(git -C "$d" rev-parse HEAD^{tree} 2>/dev/null)
  R_DISK=$(mib "$d")
  R_TOTAL=$(echo "$R_ACQ+$R_LOG_S+$R_FIND_S+$R_GREP_S+$R_EDIT+$R_STAT+$R_CMT" | bc)
}

run_one() { # $1 = repository id
  local repo="$1"
  local mirror="$MIRROR_DIR/$repo.git"
  if [ ! -d "$mirror" ]; then
    echo "[$repo] no mirror; run fetch-corpus.sh" >&2
    return 1
  fi

  rm -rf "${WORK:?}/$repo"; mkdir -p "$WORK/$repo"
  # Copied, never served in place: the server writes `refs/gfs/*` lease anchors
  # into whatever repository it serves, and the mirror is an input.
  local served="$WORK/$repo/served.git"
  cp -a "$mirror" "$served"
  local base; base=$(git --git-dir="$served" rev-parse HEAD)

  # A glob and a literal. Both default to something present in any source tree
  # rather than being derived from the repository's own content: an earlier
  # version picked the pattern from a path and landed on the project's own name,
  # which matched 26 779 lines -- past `gfs rg`'s result cap, so the two sides
  # were measuring different questions.
  local find_glob="${GFS_BENCH_FIND_GLOB:-*test*}"
  local grep_pat="${GFS_BENCH_GREP_PATTERN:-TODO}"
  # Raised well above the expected hit count on both sides. Left at the default,
  # a truncated GFS answer would be compared against an untruncated `rg` one and
  # the difference read as a correctness failure.
  local max_results="${GFS_BENCH_MAX_RESULTS:-200000}"

  local m1 m2 del ren
  { read -r m1; read -r m2; read -r del; read -r ren; } < <(choose_edit_paths "$served")

  echo "## $repo"
  echo "base commit: $base"
  echo "tip files: $(git --git-dir="$served" ls-tree -r HEAD | wc -l)   refs: $(git --git-dir="$served" for-each-ref | wc -l)   mirror: $(mib "$mirror") MiB"
  echo "find glob: $find_glob   grep pattern: $grep_pat"
  echo "edits: modify $m1, modify $m2, delete $del, rename $ren"
  echo

  # ---- raw git, twice: everything, and the cheapest clone that still works ----
  local a_acq a_log a_log_s a_find a_find_s a_grep a_grep_s a_edit a_stat a_stat2 a_cmt a_total a_disk a_tree
  raw_leg "full" "$WORK/$repo/raw"
  a_acq=$R_ACQ a_log=$R_LOG a_log_s=$R_LOG_S a_find=$R_FIND a_find_s=$R_FIND_S
  a_grep=$R_GREP a_grep_s=$R_GREP_S a_edit=$R_EDIT a_stat=$R_STAT a_stat2=$R_STAT2
  a_cmt=$R_CMT a_total=$R_TOTAL a_disk=$R_DISK a_tree=$R_TREE

  local b_acq b_log b_log_s b_find_s b_grep_s b_edit b_stat b_stat2 b_cmt b_total b_disk b_tree
  raw_leg "shallow+blobless" "$WORK/$repo/raw_sb" --depth 1 --filter=blob:none --no-single-branch
  b_acq=$R_ACQ b_log=$R_LOG b_log_s=$R_LOG_S b_find_s=$R_FIND_S
  b_grep_s=$R_GREP_S b_edit=$R_EDIT b_stat=$R_STAT b_stat2=$R_STAT2
  b_cmt=$R_CMT b_total=$R_TOTAL b_disk=$R_DISK b_tree=$R_TREE

  # ---- GFS ----
  local state="$WORK/$repo/server-state"; mkdir -p "$state"
  "$BIN/gfs-server" --state-dir "$state" --http-addr "$HTTP_ADDR" --grpc-addr "$GRPC_ADDR" \
    --capability-key "$CAP_KEY" --dev-token "$TOKEN" --import "$repo=$served" \
    >"$WORK/$repo/server.log" 2>&1 &
  local server_pid=$!
  # shellcheck disable=SC2317
  stop_server() {
    "$BIN/gfs" unmount --workspace "$WORK/$repo/ws" >/dev/null 2>&1
    kill "$server_pid" 2>/dev/null; wait "$server_pid" 2>/dev/null
  }
  trap stop_server RETURN

  # The server reconciles every ref into its catalog before it binds a listener,
  # so this is the repository's one-time import cost and belongs in the report.
  local ready=0 i t0 t1
  t0=$(now)
  for ((i = 0; i < 12000; i++)); do
    if curl -fsS "http://$HTTP_ADDR/readyz" >/dev/null 2>&1; then ready=1; break; fi
    sleep 0.1
  done
  t1=$(now)
  if [ "$ready" -eq 0 ]; then echo "[$repo] server did not become ready" >&2; return 1; fi
  local import_s; import_s=$(el "$t0" "$t1")

  export GFS_ENDPOINT="http://$GRPC_ADDR" GFS_HTTP_ENDPOINT="http://$HTTP_ADDR" GFS_TOKEN="$TOKEN"
  local ws="$WORK/$repo/ws" cache="$WORK/$repo/cache"

  t0=$(now); "$BIN/gfs" mount --repo "$repo" --rev HEAD --workspace "$ws" \
    --cache-dir "$cache" >"$WORK/$repo/mount.log" 2>&1; t1=$(now)
  local c_acq; c_acq=$(el "$t0" "$t1")

  local c_log c_log_s c_find c_find_s
  t0=$(now); c_log=$(git -C "$ws" "${GITC[@]}" log -10 --oneline | wc -l); t1=$(now)
  c_log_s=$(el "$t0" "$t1")
  t0=$(now); c_find=$(git -C "$ws" "${GITC[@]}" ls-files "$find_glob" | wc -l); t1=$(now)
  c_find_s=$(el "$t0" "$t1")

  # Hydration around the search, which is the claim worth proving: a
  # repository-wide content search must move no file bytes to the client.
  "$BIN/gfs" inspect --workspace "$ws" >"$WORK/$repo/inspect-before-search.txt" 2>&1
  local c_grep c_grep_s c_grep_rc c_grep2 c_grep2_s
  t0=$(now)
  ( cd "$ws" && "$BIN/gfs" rg -F "$grep_pat" -m "$max_results" \
      >"$WORK/$repo/search-cold.out" 2>"$WORK/$repo/search-cold.err" )
  c_grep_rc=$?; t1=$(now)
  c_grep_s=$(el "$t0" "$t1"); c_grep=$(wc -l <"$WORK/$repo/search-cold.out")
  # The warm query must measure a *ready* index, not the tail of the build. The
  # 2026-08-21 run recorded 2.385 s here on vscode without this wait, against
  # 0.245 s for the same query once the build had finished -- a number that
  # reads as a search regression and is not one. The cold step above keeps its
  # own timing and its `SNAPSHOT_BUILDING` error, which is the property being
  # measured there.
  # Exit 2 is ADR 0004's "the search did not complete", which is what an
  # unbuilt index answers; every other code means the index replied. Not
  # `&& break`: `-m 1` truncates, and truncation is exit 3.
  for _ in $(seq 1 600); do
    ( cd "$ws" && "$BIN/gfs" rg -F "$grep_pat" -m 1 >/dev/null 2>&1 )
    [ $? -ne 2 ] && break
    sleep 0.5
  done
  t0=$(now)
  ( cd "$ws" && "$BIN/gfs" rg -F "$grep_pat" -m "$max_results" \
      >"$WORK/$repo/search-warm.out" 2>"$WORK/$repo/search-warm.err" )
  t1=$(now)
  c_grep2_s=$(el "$t0" "$t1"); c_grep2=$(wc -l <"$WORK/$repo/search-warm.out")
  "$BIN/gfs" inspect --workspace "$ws" >"$WORK/$repo/inspect-after-search.txt" 2>&1

  local c_edit c_stat c_stat2 c_gfsstat c_cmt c_tree
  t0=$(now); apply_edits "$ws" "$m1" "$m2" "$del" "$ren"; t1=$(now)
  c_edit=$(el "$t0" "$t1")
  t0=$(now); git -C "$ws" "${GITC[@]}" status --porcelain >/dev/null; t1=$(now)
  c_stat=$(el "$t0" "$t1")
  t0=$(now); git -C "$ws" "${GITC[@]}" status --porcelain >/dev/null; t1=$(now)
  c_stat2=$(el "$t0" "$t1")
  t0=$(now); "$BIN/gfs" status --workspace "$ws" >/dev/null 2>&1; t1=$(now)
  c_gfsstat=$(el "$t0" "$t1")
  t0=$(now)
  git -C "$ws" "${GITC[@]}" add -A >/dev/null 2>&1
  git -C "$ws" "${GITC[@]}" commit -q -m "bench: agent edit" >/dev/null 2>&1
  t1=$(now); c_cmt=$(el "$t0" "$t1")
  c_tree=$(git -C "$ws" "${GITC[@]}" rev-parse HEAD^{tree} 2>/dev/null)

  "$BIN/gfs" inspect --workspace "$ws" >"$WORK/$repo/inspect-final.txt" 2>&1
  local hydration odb
  hydration=$(grep -E '^hydration' "$WORK/$repo/inspect-final.txt" | sed 's/^hydration *//')
  odb=$(grep -E '^odb' "$WORK/$repo/inspect-final.txt" | sed 's/^odb *//')

  local c_total; c_total=$(echo "$c_acq+$c_log_s+$c_find_s+$c_grep2_s+$c_edit+$c_stat+$c_cmt" | bc)

  # Disk after unmount: while mounted the odb projection advertises pack sizes it
  # does not hold, so measuring the live mount reports bytes nobody stored.
  "$BIN/gfs" unmount --workspace "$ws" >/dev/null 2>&1
  local c_state c_cache
  c_state=$(allocated "$ws"); c_cache=$(allocated "$cache")

  printf '| step | raw git full | raw git shallow+blobless | GFS | raw result | GFS result |\n'
  printf '| --- | ---: | ---: | ---: | --- | --- |\n'
  printf '| acquire | %.3f s | %.3f s | %.3f s | clone | mount |\n' "$a_acq" "$b_acq" "$c_acq"
  printf '| `log -10` | %.3f s | %.3f s | %.3f s | %s commits | %s commits |\n' \
    "$a_log_s" "$b_log_s" "$c_log_s" "$a_log" "$c_log"
  printf '| `ls-files` | %.3f s | %.3f s | %.3f s | %s files | %s files |\n' \
    "$a_find_s" "$b_find_s" "$c_find_s" "$a_find" "$c_find"
  printf '| grep, cold index | %.3f s | %.3f s | %.3f s | %s lines | %s lines (exit %s) |\n' \
    "$a_grep_s" "$b_grep_s" "$c_grep_s" "$a_grep" "$c_grep" "$c_grep_rc"
  printf '| grep, warm | %.3f s | %.3f s | %.3f s | %s lines | %s lines |\n' \
    "$a_grep_s" "$b_grep_s" "$c_grep2_s" "$a_grep" "$c_grep2"
  printf '| edit | %.3f s | %.3f s | %.3f s | | |\n' "$a_edit" "$b_edit" "$c_edit"
  printf '| `git status`, cold | %.3f s | %.3f s | %.3f s | | |\n' "$a_stat" "$b_stat" "$c_stat"
  printf '| `git status`, warm | %.3f s | %.3f s | %.3f s | | |\n' "$a_stat2" "$b_stat2" "$c_stat2"
  printf '| `gfs status` | – | – | %.3f s | | journal |\n' "$c_gfsstat"
  printf '| commit | %.3f s | %.3f s | %.3f s | | |\n' "$a_cmt" "$b_cmt" "$c_cmt"
  printf '| **total** | **%.3f s** | **%.3f s** | **%.3f s** | | |\n' "$a_total" "$b_total" "$c_total"
  echo
  printf 'server import (one time, blocks the listeners): %.3f s\n' "$import_s"
  printf 'disk: raw full %s MiB, raw shallow+blobless %s MiB, GFS %s bytes workspace + %s bytes host cache (allocated)\n' \
    "$a_disk" "$b_disk" "$c_state" "$c_cache"
  printf 'hydration: %s\nodb: %s\n' "$hydration" "$odb"
  printf 'search coverage: %s\n' "$(head -c 300 "$WORK/$repo/search-warm.err")"
  printf 'tree(raw full) %s\ntree(raw s+b)  %s\ntree(GFS)      %s\n' "$a_tree" "$b_tree" "$c_tree"
  # The clone is not automatically the reference answer: where a repository uses
  # LFS and no LFS endpoint is reachable, the smudge filter leaves files missing
  # and `git add -A` commits them as deletions. Print both diffs against the base
  # so a disagreement says which side wandered rather than only that one did.
  if [ "$a_tree" = "$c_tree" ]; then
    printf 'COMMIT CORRECTNESS: PASS -- the two flows produced the same tree\n\n'
  else
    printf 'COMMIT CORRECTNESS: **FAIL** -- the two flows disagree\n'
    printf 'raw changed %s paths, GFS changed %s paths, against base tree %s\n\n' \
      "$(git -C "$WORK/$repo/raw" diff --name-status "$base^{tree}" "$a_tree" | wc -l)" \
      "$(GIT_ALTERNATE_OBJECT_DIRECTORIES="$ws/.git/objects" git --git-dir="$served" \
           diff --name-status "$base^{tree}" "$c_tree" 2>/dev/null | wc -l)" \
      "$(git --git-dir="$served" rev-parse "$base^{tree}")"
  fi
}

targets=("$@")
while read -r id role url; do
  case "$id" in ''|'#'*) continue ;; esac
  : "$role" "$url"
  if [ ${#targets[@]} -gt 0 ]; then
    case " ${targets[*]} " in *" $id "*) ;; *) continue ;; esac
  fi
  run_one "$id"
done <"$here/corpus.conf"
