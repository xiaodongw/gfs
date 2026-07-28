#!/usr/bin/env bash
#
# The agent edit workflow, end to end, raw Git against GFS.
#
#   acquire a workspace -> read recent history -> start a branch ->
#   find by name -> grep by content -> edit files -> status -> commit
#
# `benchmark-clone.sh` measures the *clone*, which is only the first step. This
# measures the whole task, because the ranking changes once search is in it: the
# clone is where GFS wins and search is where it can lose everything back.
#
# Every step is timed separately and every step's *result* is recorded next to
# its time. A faster search that returns a different answer is not a faster
# search, so the two are never reported apart.
#
#   ./spikes/corpus/benchmark-workflow.sh django
#
# Takes repository ids from corpus.conf and hardcodes no repository names.
# Requires a release build (`scripts/build-release.sh`) and a real ripgrep
# binary; see the note in benchmarks/baseline.md about `rg` being a shell
# function in some environments.

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
# for reasons that have nothing to do with what is being measured.
GITC=(-c user.name=bench -c user.email=bench@example.com
      -c commit.gpgsign=false -c core.autocrlf=false -c gc.auto=0
      -c protocol.version=2)

now() { date +%s.%N; }
el() { echo "$2 - $1" | bc; }
mib() { du -sm --apparent-size "$1" 2>/dev/null | cut -f1; }

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

run_one() { # $1 = repository id
  local repo="$1"
  local mirror="$MIRROR_DIR/$repo.git"
  if [ ! -d "$mirror" ]; then
    echo "[$repo] no mirror; run fetch-corpus.sh" >&2
    return 1
  fi

  rm -rf "$WORK/$repo"; mkdir -p "$WORK/$repo"
  local served="$WORK/$repo/served.git"
  # Copied, never served in place: the server writes `refs/gfs/*` lease anchors
  # into whatever repository it serves, and the mirror is an input.
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
  echo "find glob: $find_glob   grep pattern: $grep_pat"
  echo "edits: modify $m1, modify $m2, delete $del, rename $ren"
  echo
  printf '| step | raw git full | raw git --depth 10 | GFS | raw result | GFS result |\n'
  printf '| --- | ---: | ---: | ---: | --- | --- |\n'

  # ---- raw git ----
  local d="$WORK/$repo/raw" d10="$WORK/$repo/raw10" t0 t1
  t0=$(now); git "${GITC[@]}" clone -q "file://$served" "$d" >/dev/null 2>&1; t1=$(now)
  local a_acq; a_acq=$(el "$t0" "$t1")
  t0=$(now); git "${GITC[@]}" clone -q --depth 10 "file://$served" "$d10" >/dev/null 2>&1; t1=$(now)
  local a10_acq; a10_acq=$(el "$t0" "$t1")

  t0=$(now); local a_log; a_log=$(git -C "$d" log -10 --oneline | wc -l); t1=$(now)
  local a_log_s; a_log_s=$(el "$t0" "$t1")
  t0=$(now); local a_find; a_find=$(git -C "$d" ls-files "$find_glob" | wc -l); t1=$(now)
  local a_find_s; a_find_s=$(el "$t0" "$t1")
  t0=$(now); local a_grep; a_grep=$("$RG" -F "$grep_pat" "$d" 2>/dev/null | wc -l); t1=$(now)
  local a_grep_s; a_grep_s=$(el "$t0" "$t1")
  t0=$(now); apply_edits "$d" "$m1" "$m2" "$del" "$ren"; t1=$(now)
  local a_edit; a_edit=$(el "$t0" "$t1")
  t0=$(now); git -C "$d" "${GITC[@]}" status --porcelain >/dev/null; t1=$(now)
  local a_stat; a_stat=$(el "$t0" "$t1")
  t0=$(now)
  git -C "$d" "${GITC[@]}" add -A >/dev/null 2>&1
  git -C "$d" "${GITC[@]}" commit -q -m "bench: agent edit" >/dev/null 2>&1
  t1=$(now); local a_cmt; a_cmt=$(el "$t0" "$t1")
  local a_tree; a_tree=$(git -C "$d" rev-parse HEAD^{tree})

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
  # Generous: the first import reconciles every ref, which on a 29 000-ref
  # repository is the better part of a minute.
  local ready=0 i
  for ((i = 0; i < 6000; i++)); do
    if curl -fsS "http://$HTTP_ADDR/readyz" >/dev/null 2>&1; then ready=1; break; fi
    sleep 0.1
  done
  if [ "$ready" -eq 0 ]; then echo "[$repo] server did not become ready" >&2; return 1; fi

  export GFS_ENDPOINT="http://$GRPC_ADDR" GFS_HTTP_ENDPOINT="http://$HTTP_ADDR" GFS_TOKEN="$TOKEN"
  local ws="$WORK/$repo/ws"
  t0=$(now); "$BIN/gfs" mount --repo "$repo" --rev HEAD --workspace "$ws" \
    --cache-dir "$WORK/$repo/cache" >/dev/null 2>&1; t1=$(now)
  local c_acq; c_acq=$(el "$t0" "$t1")

  t0=$(now); local c_log; c_log=$(cd "$ws" && "$BIN/gfs" log -10 --oneline 2>/dev/null | wc -l); t1=$(now)
  local c_log_s; c_log_s=$(el "$t0" "$t1")
  t0=$(now); local c_find; c_find=$(cd "$ws" && "$BIN/gfs" find "$find_glob" --max-results "$max_results" 2>/dev/null | wc -l); t1=$(now)
  local c_find_s; c_find_s=$(el "$t0" "$t1")
  t0=$(now); local c_grep; c_grep=$(cd "$ws" && "$BIN/gfs" rg -F "$grep_pat" -m "$max_results" 2>/dev/null | wc -l); t1=$(now)
  local c_grep_s; c_grep_s=$(el "$t0" "$t1")
  t0=$(now); apply_edits "$ws" "$m1" "$m2" "$del" "$ren"; t1=$(now)
  local c_edit; c_edit=$(el "$t0" "$t1")
  t0=$(now); "$BIN/gfs" status --workspace "$ws" >/dev/null 2>&1; t1=$(now)
  local c_stat; c_stat=$(el "$t0" "$t1")

  t0=$(now); "$BIN/gfs" export --workspace "$ws" --bundle "$WORK/$repo/bundle" >/dev/null 2>&1; t1=$(now)
  local c_exp; c_exp=$(el "$t0" "$t1")
  # The commit lands where the objects are. No worktree: a temporary index and
  # `commit-tree` is what a server would do, and it is the honest cost -- an
  # earlier harness cloned the repository here and charged GFS for it.
  t0=$(now)
  local idx="$WORK/$repo/index"
  GIT_INDEX_FILE="$idx" git --git-dir="$served" "${GITC[@]}" read-tree "$base"
  GIT_INDEX_FILE="$idx" git --git-dir="$served" "${GITC[@]}" apply --cached \
    --whitespace=nowarn "$WORK/$repo/bundle/changes.patch"
  local c_tree
  c_tree=$(GIT_INDEX_FILE="$idx" git --git-dir="$served" "${GITC[@]}" write-tree)
  t1=$(now); local c_cmt; c_cmt=$(el "$t0" "$t1")

  local hydration
  hydration=$("$BIN/gfs" inspect --workspace "$ws" | grep hydration | sed 's/^hydration *//')

  printf '| acquire | %.3f s | %.3f s | %.3f s | clone | mount |\n' "$a_acq" "$a10_acq" "$c_acq"
  printf '| `log -10` | %.3f s | %.3f s | %.3f s | %s commits | %s commits |\n' \
    "$a_log_s" "$a_log_s" "$c_log_s" "$a_log" "$c_log"
  printf '| find | %.3f s | %.3f s | %.3f s | %s files | %s files |\n' \
    "$a_find_s" "$a_find_s" "$c_find_s" "$a_find" "$c_find"
  printf '| grep | %.3f s | %.3f s | %.3f s | %s lines | %s lines |\n' \
    "$a_grep_s" "$a_grep_s" "$c_grep_s" "$a_grep" "$c_grep"
  printf '| edit | %.3f s | %.3f s | %.3f s | | |\n' "$a_edit" "$a_edit" "$c_edit"
  printf '| status | %.3f s | %.3f s | %.3f s | | |\n' "$a_stat" "$a_stat" "$c_stat"
  printf '| commit | %.3f s | %.3f s | %.3f s | | export + apply |\n' \
    "$a_cmt" "$a_cmt" "$(echo "$c_exp + $c_cmt" | bc)"
  printf '| **total** | **%.3f s** | **%.3f s** | **%.3f s** | | |\n' \
    "$(echo "$a_acq+$a_log_s+$a_find_s+$a_grep_s+$a_edit+$a_stat+$a_cmt" | bc)" \
    "$(echo "$a10_acq+$a_log_s+$a_find_s+$a_grep_s+$a_edit+$a_stat+$a_cmt" | bc)" \
    "$(echo "$c_acq+$c_log_s+$c_find_s+$c_grep_s+$c_edit+$c_stat+$c_exp+$c_cmt" | bc)"
  echo
  printf 'disk: raw full %s MiB, raw --depth 10 %s MiB, GFS %s bytes state + %s bytes cache\n' \
    "$(mib "$d")" "$(mib "$d10")" \
    "$(du -sb --exclude=generations "$ws.gfs" | cut -f1)" \
    "$(du -sb "$WORK/$repo/cache" | cut -f1)"
  printf 'hydration: %s\n' "$hydration"
  printf 'tree(raw)  %s\ntree(GFS) %s\n' "$a_tree" "$c_tree"
  if [ "$a_tree" = "$c_tree" ]; then
    printf 'COMMIT CORRECTNESS: PASS -- the two flows produced the same tree\n\n'
  else
    printf 'COMMIT CORRECTNESS: **FAIL** -- the two flows disagree\n\n'
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
