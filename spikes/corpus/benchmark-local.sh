#!/usr/bin/env bash
#
# Local mode against `git worktree add` and against the server mount.
#
#   ./spikes/corpus/benchmark-local.sh vscode django
#
# The question local mode exists to answer: a developer already has a full
# clone of a monorepo and wants one working tree per change. Today that is
# `git worktree add`, which copies the tree. Local mode mounts the same
# commit lazily from the clone's own object database with no server. This
# harness runs the same task in three working trees of the same clone:
#
#   worktree   git worktree add --detach          (the incumbent)
#   local      gfs mount --local <clone>          (this feature)
#   server     gfs-server over a copy of the mirror, gfs mount --repo
#
# and times acquire, history, ls-files, content search, a read-through of one
# directory (cold and warm), an edit, status (cold and warm), and a commit,
# then checks the three commits produced the same tree. Disk is allocated
# blocks after unmount, as in benchmark-workflow.sh. Search is measured twice
# on the gfs legs: raw ripgrep over the mount, which is what a tool with no
# shim on PATH does, and `gfs rg`, which reads the object store directly.
#
# The clone is `$GFS_LOCAL_DIR/<repo>` (default ~/gfs-corpus/local), made
# from the mirror on first use with LFS smudging disabled -- the mirror has
# no LFS endpoint, and a worktree whose checkout fails halfway is not a
# baseline. Requires a release build and a real ripgrep.

set -uo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"

CORPUS_DIR="${GFS_CORPUS_DIR:-$HOME/gfs-corpus}"
MIRROR_DIR="$CORPUS_DIR/mirrors"
LOCAL_DIR="${GFS_LOCAL_DIR:-$CORPUS_DIR/local}"
WORK="${GFS_LOCAL_WORK:-$CORPUS_DIR/local-bench}"
BIN="$root/target/release"
HTTP_ADDR="${GFS_BENCH_HTTP_ADDR:-127.0.0.1:8630}"
GRPC_ADDR="${GFS_BENCH_GRPC_ADDR:-127.0.0.1:8631}"
TOKEN=local-bench
CAP_KEY="$(printf 'gfs-local-benchmark-key-not-for-production!!!!!' | od -An -tx1 | tr -d ' \n' | cut -c1-64)"
# How many extra workspaces to create for the "one per change" figure.
EXTRA="${GFS_BENCH_EXTRA_WORKSPACES:-4}"

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
    echo "missing $BIN/$tool; run cargo build --release --workspace" >&2
    exit 2
  fi
done

GITC=(-c user.name=bench -c user.email=bench@example.com
      -c commit.gpgsign=false -c core.autocrlf=false -c gc.auto=0
      -c protocol.version=2 -c safe.directory='*')

now() { date +%s.%N; }
el() { echo "$2 - $1" | bc; }
allocated() { du -s --block-size=1 "$1" 2>/dev/null | cut -f1; }
mib_alloc() { echo "scale=1; $(allocated "$1") / 1048576" | bc; }

choose_edit_paths() { # $1 = clone
  git -C "$1" ls-tree -r HEAD |
    awk '$1 == "100644" { $1=$2=$3=""; sub(/^ +/, ""); print }' |
    grep -v $'\t' |
    grep -Ei '\.(py|rs|ts|js|md|rst|txt|c|h|cc|cpp|go|java|rb|sh|toml|cfg|ini)$' |
    sed -n '10p;60p;200p;400p'
}

# The directory read through: the top-level directory with the most files,
# capped at 2000 of them, the same list for every leg.
choose_read_list() { # $1 = clone, $2 = output file
  local dir
  dir=$(git -C "$1" ls-tree -r --name-only HEAD | grep / | cut -d/ -f1 | sort | uniq -c | sort -rn | head -1 | awk '{print $2}')
  git -C "$1" ls-tree -r --name-only HEAD -- "$dir" | head -2000 >"$2"
  echo "$dir"
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

read_through() { # $1 = worktree, $2 = list file
  ( cd "$1" && xargs -d '\n' cat <"$2" >/dev/null 2>&1 )
}

ensure_clone() { # $1 = repo
  local repo="$1" clone="$LOCAL_DIR/$repo" mirror="$MIRROR_DIR/$repo.git"
  [ -d "$clone/.git" ] && return 0
  [ -d "$mirror" ] || { echo "[$repo] no mirror at $mirror; run fetch-corpus.sh" >&2; return 1; }
  mkdir -p "$LOCAL_DIR"
  echo "[$repo] cloning $mirror into $clone (LFS smudge disabled)" >&2
  git "${GITC[@]}" -c filter.lfs.smudge='git-lfs smudge --skip -- %f' \
    -c filter.lfs.process='git-lfs filter-process --skip' -c filter.lfs.required=false \
    clone -q "$mirror" "$clone" || return 1
  git -C "$clone" config filter.lfs.smudge 'git-lfs smudge --skip -- %f'
  git -C "$clone" config filter.lfs.process 'git-lfs filter-process --skip'
  git -C "$clone" config filter.lfs.required false
  git -C "$clone" config gc.auto 0
}

# The task, in a working tree that already exists. Results in R_*.
task() { # $1 = worktree, $2 = read list, $3.. = the four paths
  local d="$1" list="$2"; shift 2
  local t0 t1
  t0=$(now); R_LOG=$(git -C "$d" "${GITC[@]}" log -10 --oneline | wc -l); t1=$(now)
  R_LOG_S=$(el "$t0" "$t1")
  t0=$(now); R_FIND=$(git -C "$d" "${GITC[@]}" ls-files "$find_glob" | wc -l); t1=$(now)
  R_FIND_S=$(el "$t0" "$t1")
  t0=$(now); R_GREP=$("$RG" -F "$grep_pat" "$d" 2>/dev/null | wc -l); t1=$(now)
  R_GREP_S=$(el "$t0" "$t1")
  t0=$(now); read_through "$d" "$list"; t1=$(now)
  R_READ_S=$(el "$t0" "$t1")
  t0=$(now); read_through "$d" "$list"; t1=$(now)
  R_READ2_S=$(el "$t0" "$t1")
  t0=$(now); apply_edits "$d" "$@"; t1=$(now)
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
}

gfs_task_extras() { # $1 = workspace; gfs rg and gfs status
  local ws="$1" t0 t1
  t0=$(now)
  ( cd "$ws" && "$BIN/gfs" rg -F "$grep_pat" -m "$max_results" >"$ws.search.out" 2>"$ws.search.err" )
  R_GFSRG_RC=$?; t1=$(now)
  R_GFSRG_S=$(el "$t0" "$t1"); R_GFSRG=$(wc -l <"$ws.search.out")
  t0=$(now); "$BIN/gfs" status --workspace "$ws" >/dev/null 2>&1; t1=$(now)
  R_GFSSTAT=$(el "$t0" "$t1")
}

run_one() { # $1 = repository id
  local repo="$1"
  ensure_clone "$repo" || return 1
  local clone="$LOCAL_DIR/$repo"
  local base; base=$(git -C "$clone" rev-parse HEAD)

  rm -rf "${WORK:?}/$repo"; mkdir -p "$WORK/$repo"
  # A private host, so the benchmark never attaches to a developer's session.
  export GFS_HOST_SOCKET="$WORK/$repo/host.sock"
  export XDG_CACHE_HOME="$WORK/$repo/xdg-cache"

  local find_glob="${GFS_BENCH_FIND_GLOB:-*test*}"
  local grep_pat="${GFS_BENCH_GREP_PATTERN:-TODO}"
  local max_results="${GFS_BENCH_MAX_RESULTS:-200000}"
  local m1 m2 del ren
  { read -r m1; read -r m2; read -r del; read -r ren; } < <(choose_edit_paths "$clone")
  local list="$WORK/$repo/read-list.txt"
  local read_dir; read_dir=$(choose_read_list "$clone" "$list")

  echo "## $repo"
  echo "clone: $clone   base commit: $base"
  echo "tip files: $(git -C "$clone" ls-tree -r HEAD | wc -l)   clone on disk: $(mib_alloc "$clone") MiB"
  echo "find glob: $find_glob   grep pattern: $grep_pat   read-through: $(wc -l <"$list") files under $read_dir/"
  echo "edits: modify $m1, modify $m2, delete $del, rename $ren"
  echo

  # ---- worktree ----
  local wt="$WORK/$repo/wt"
  local t0 t1
  t0=$(now); git -C "$clone" "${GITC[@]}" worktree add -q --detach "$wt" HEAD >/dev/null 2>&1; t1=$(now)
  local a_acq; a_acq=$(el "$t0" "$t1")
  task "$wt" "$list" "$m1" "$m2" "$del" "$ren"
  local a_log=$R_LOG a_log_s=$R_LOG_S a_find=$R_FIND a_find_s=$R_FIND_S a_grep=$R_GREP a_grep_s=$R_GREP_S
  local a_read_s=$R_READ_S a_read2_s=$R_READ2_S a_edit=$R_EDIT a_stat=$R_STAT a_stat2=$R_STAT2 a_cmt=$R_CMT a_tree=$R_TREE
  local a_disk; a_disk=$(mib_alloc "$wt")
  # Several more, for the "one per change" figure.
  t0=$(now)
  for ((i = 0; i < EXTRA; i++)); do
    git -C "$clone" "${GITC[@]}" worktree add -q --detach "$WORK/$repo/wt-$i" HEAD >/dev/null 2>&1
  done
  t1=$(now); local a_extra_s; a_extra_s=$(el "$t0" "$t1")
  local a_extra_disk=0
  for ((i = 0; i < EXTRA; i++)); do a_extra_disk=$(echo "$a_extra_disk + $(mib_alloc "$WORK/$repo/wt-$i")" | bc); done
  for ((i = 0; i < EXTRA; i++)); do git -C "$clone" worktree remove --force "$WORK/$repo/wt-$i" >/dev/null 2>&1; done
  git -C "$clone" worktree remove --force "$wt" >/dev/null 2>&1
  git -C "$clone" worktree prune >/dev/null 2>&1

  # ---- local mode ----
  local ws="$WORK/$repo/local"
  t0=$(now); "$BIN/gfs" mount --local "$clone" --rev HEAD --workspace "$ws" >"$WORK/$repo/local-mount.log" 2>&1; t1=$(now)
  local b_acq; b_acq=$(el "$t0" "$t1")
  if ! grep -q '^origin' "$WORK/$repo/local-mount.log"; then
    echo "[$repo] local mount failed:" >&2; cat "$WORK/$repo/local-mount.log" >&2; return 1
  fi
  task "$ws" "$list" "$m1" "$m2" "$del" "$ren"
  local b_log=$R_LOG b_log_s=$R_LOG_S b_find=$R_FIND b_find_s=$R_FIND_S b_grep=$R_GREP b_grep_s=$R_GREP_S
  local b_read_s=$R_READ_S b_read2_s=$R_READ2_S b_edit=$R_EDIT b_stat=$R_STAT b_stat2=$R_STAT2 b_cmt=$R_CMT b_tree=$R_TREE
  gfs_task_extras "$ws"
  local b_gfsrg=$R_GFSRG b_gfsrg_s=$R_GFSRG_S b_gfsrg_rc=$R_GFSRG_RC b_gfsstat=$R_GFSSTAT
  "$BIN/gfs" inspect --workspace "$ws" >"$WORK/$repo/local-inspect.txt" 2>&1
  t0=$(now)
  for ((i = 0; i < EXTRA; i++)); do
    "$BIN/gfs" mount --local "$clone" --rev HEAD --workspace "$WORK/$repo/local-$i" >/dev/null 2>&1
  done
  t1=$(now); local b_extra_s; b_extra_s=$(el "$t0" "$t1")
  for ((i = 0; i < EXTRA; i++)); do "$BIN/gfs" unmount --workspace "$WORK/$repo/local-$i" >/dev/null 2>&1; done
  local b_extra_disk=0
  for ((i = 0; i < EXTRA; i++)); do b_extra_disk=$(echo "$b_extra_disk + $(mib_alloc "$WORK/$repo/local-$i")" | bc); done
  "$BIN/gfs" unmount --workspace "$ws" >/dev/null 2>&1
  local b_disk; b_disk=$(mib_alloc "$ws")
  local b_cache; b_cache=$(mib_alloc "$XDG_CACHE_HOME")
  local b_anchors; b_anchors=$(git -C "$clone" for-each-ref refs/gfs/ | wc -l)

  # ---- server ----
  local served="$WORK/$repo/served.git"
  cp -a "$MIRROR_DIR/$repo.git" "$served"
  local state="$WORK/$repo/server-state"; mkdir -p "$state"
  "$BIN/gfs-server" --state-dir "$state" --http-addr "$HTTP_ADDR" --grpc-addr "$GRPC_ADDR" \
    --capability-key "$CAP_KEY" --dev-token "$TOKEN" --import "$repo=$served" \
    >"$WORK/$repo/server.log" 2>&1 &
  local server_pid=$!
  # shellcheck disable=SC2317
  stop_all() {
    "$BIN/gfs" unmount --workspace "$WORK/$repo/server-ws" >/dev/null 2>&1
    "$BIN/gfs" daemon stop >/dev/null 2>&1
    kill "$server_pid" 2>/dev/null; wait "$server_pid" 2>/dev/null
  }
  trap stop_all RETURN
  local ready=0 i
  t0=$(now)
  for ((i = 0; i < 12000; i++)); do
    if curl -fsS "http://$HTTP_ADDR/readyz" >/dev/null 2>&1; then ready=1; break; fi
    sleep 0.1
  done
  t1=$(now)
  if [ "$ready" -eq 0 ]; then echo "[$repo] server did not become ready" >&2; return 1; fi
  local import_s; import_s=$(el "$t0" "$t1")
  export GFS_ENDPOINT="http://$GRPC_ADDR" GFS_HTTP_ENDPOINT="http://$HTTP_ADDR" GFS_TOKEN="$TOKEN"
  local sws="$WORK/$repo/server-ws"
  t0=$(now); "$BIN/gfs" mount --repo "$repo" --rev HEAD --workspace "$sws" >"$WORK/$repo/server-mount.log" 2>&1; t1=$(now)
  local c_acq; c_acq=$(el "$t0" "$t1")
  task "$sws" "$list" "$m1" "$m2" "$del" "$ren"
  local c_log=$R_LOG c_log_s=$R_LOG_S c_find=$R_FIND c_find_s=$R_FIND_S c_grep=$R_GREP c_grep_s=$R_GREP_S
  local c_read_s=$R_READ_S c_read2_s=$R_READ2_S c_edit=$R_EDIT c_stat=$R_STAT c_stat2=$R_STAT2 c_cmt=$R_CMT c_tree=$R_TREE
  # The server leg's search needs a built index; wait for it as the workflow
  # benchmark does, so the number is a warm query and not the build.
  for _ in $(seq 1 600); do
    ( cd "$sws" && "$BIN/gfs" rg -F "$grep_pat" -m 1 >/dev/null 2>&1 )
    [ $? -ne 2 ] && break
    sleep 0.5
  done
  gfs_task_extras "$sws"
  local c_gfsrg=$R_GFSRG c_gfsrg_s=$R_GFSRG_S c_gfsrg_rc=$R_GFSRG_RC c_gfsstat=$R_GFSSTAT
  "$BIN/gfs" inspect --workspace "$sws" >"$WORK/$repo/server-inspect.txt" 2>&1
  t0=$(now)
  for ((i = 0; i < EXTRA; i++)); do
    "$BIN/gfs" mount --repo "$repo" --rev HEAD --workspace "$WORK/$repo/server-$i" >/dev/null 2>&1
  done
  t1=$(now); local c_extra_s; c_extra_s=$(el "$t0" "$t1")
  for ((i = 0; i < EXTRA; i++)); do "$BIN/gfs" unmount --workspace "$WORK/$repo/server-$i" >/dev/null 2>&1; done
  "$BIN/gfs" unmount --workspace "$sws" >/dev/null 2>&1
  local c_disk; c_disk=$(mib_alloc "$sws")
  local c_cache; c_cache=$(mib_alloc "$XDG_CACHE_HOME/gfs/$repo")
  unset GFS_ENDPOINT GFS_HTTP_ENDPOINT GFS_TOKEN

  local n=$((EXTRA + 1))
  printf '| step | git worktree | gfs local | gfs server | worktree result | local result | server result |\n'
  printf '| --- | ---: | ---: | ---: | --- | --- | --- |\n'
  printf '| acquire one workspace | %.3f s | %.3f s | %.3f s | `worktree add` | `mount --local` | `mount --repo` |\n' "$a_acq" "$b_acq" "$c_acq"
  printf '| acquire %s more | %.3f s | %.3f s | %.3f s | | | |\n' "$EXTRA" "$a_extra_s" "$b_extra_s" "$c_extra_s"
  printf '| `log -10` | %.3f s | %.3f s | %.3f s | %s | %s | %s commits |\n' "$a_log_s" "$b_log_s" "$c_log_s" "$a_log" "$b_log" "$c_log"
  printf '| `ls-files %s` | %.3f s | %.3f s | %.3f s | %s | %s | %s files |\n' "$find_glob" "$a_find_s" "$b_find_s" "$c_find_s" "$a_find" "$b_find" "$c_find"
  printf '| `rg -F %s` over the tree | %.3f s | %.3f s | %.3f s | %s | %s | %s lines |\n' "$grep_pat" "$a_grep_s" "$b_grep_s" "$c_grep_s" "$a_grep" "$b_grep" "$c_grep"
  printf '| `gfs rg -F %s` | – | %.3f s | %.3f s | | %s lines (exit %s) | %s lines (exit %s) |\n' "$grep_pat" "$b_gfsrg_s" "$c_gfsrg_s" "$b_gfsrg" "$b_gfsrg_rc" "$c_gfsrg" "$c_gfsrg_rc"
  printf '| read %s files, cold | %.3f s | %.3f s | %.3f s | | | |\n' "$(wc -l <"$list")" "$a_read_s" "$b_read_s" "$c_read_s"
  printf '| read again, warm | %.3f s | %.3f s | %.3f s | | | |\n' "$a_read2_s" "$b_read2_s" "$c_read2_s"
  printf '| edit | %.3f s | %.3f s | %.3f s | | | |\n' "$a_edit" "$b_edit" "$c_edit"
  printf '| `git status`, cold | %.3f s | %.3f s | %.3f s | | | |\n' "$a_stat" "$b_stat" "$c_stat"
  printf '| `git status`, warm | %.3f s | %.3f s | %.3f s | | | |\n' "$a_stat2" "$b_stat2" "$c_stat2"
  printf '| `gfs status` | – | %.3f s | %.3f s | | journal | journal |\n' "$b_gfsstat" "$c_gfsstat"
  printf '| commit | %.3f s | %.3f s | %.3f s | | | |\n' "$a_cmt" "$b_cmt" "$c_cmt"
  echo
  printf 'disk, one workspace (allocated): worktree %s MiB, local %s MiB workspace + %s MiB cache, server %s MiB workspace + %s MiB cache\n' \
    "$a_disk" "$b_disk" "$b_cache" "$c_disk" "$c_cache"
  printf 'disk, %s workspaces: worktree %s MiB, local %s MiB\n' "$n" "$(echo "$a_disk + $a_extra_disk" | bc)" "$(echo "$b_disk + $b_extra_disk" | bc)"
  printf 'server import (one time): %.3f s\n' "$import_s"
  printf 'anchors left in the clone after unmount: %s\n' "$b_anchors"
  printf 'local search coverage: %s\n' "$(head -c 300 "$WORK/$repo/local.search.err")"
  printf 'tree(worktree) %s\ntree(local)    %s\ntree(server)   %s\n' "$a_tree" "$b_tree" "$c_tree"
  if [ "$a_tree" = "$b_tree" ]; then
    printf 'COMMIT CORRECTNESS: PASS -- worktree and local mode produced the same tree\n'
  else
    printf 'COMMIT CORRECTNESS: **FAIL** -- worktree and local mode disagree\n'
  fi
  if [ "$a_tree" = "$c_tree" ]; then
    printf 'server: same tree as the worktree\n\n'
  else
    printf 'server: a different tree from the worktree (see benchmarks/agent-workflow.md for the LFS reason)\n\n'
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
