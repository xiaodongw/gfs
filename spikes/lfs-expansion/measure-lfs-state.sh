#!/usr/bin/env bash
# m05d: is the post-`git lfs pull` state safe to synthesize, without git-lfs?
#
# Server-side LFS expansion presents a working tree whose LFS files hold
# expanded content while the object store and index keep the pointer blobs.
# That is byte-for-byte the state real `git lfs pull` leaves behind. This spike
# stages that state against stock Git (no git-lfs installed) and measures:
#
#   A  no filter config       -> how exactly does the state lie to Git?
#   B  stub clean/smudge      -> does the standard filter contract reconcile it,
#                                what does reconciliation cost, and is the
#                                write path (add/commit) pointer-safe?
#   C  daemon-style filter    -> a clean filter that answers from metadata
#                                without hashing: what does skipping the
#                                sha256 buy on a 64 MiB file?
#   D  branch switch          -> does a hydrating smudge filter make stock
#                                `git checkout` work across LFS revisions?
#
# Self-contained: fabricates spec-conformant pointers, needs no git-lfs and no
# network. Layout mirrors ADR 0009: a real .git whose objects/info/alternates
# points at a shared bare store, core.checkStat=minimal.

set -euo pipefail

OUT="${1:-$(mktemp -d)}"
LARGE_MB=64 MEDIUM_MB=8 SMALL_FILES=400
mkdir -p "$OUT"
echo "workdir: $OUT"

export GIT_CONFIG_GLOBAL="$OUT/gitconfig" GIT_CONFIG_SYSTEM=/dev/null
git config --file "$GIT_CONFIG_GLOBAL" user.name spike
git config --file "$GIT_CONFIG_GLOBAL" user.email spike@invalid
git config --file "$GIT_CONFIG_GLOBAL" init.defaultBranch main

STORE="$OUT/lfs-store"          # the gateway's LFS CAS, keyed by sha256
FILTER_LOG="$OUT/filter.log"    # every stub invocation appends one line
mkdir -p "$STORE"

now_ms() { date +%s%N | cut -c1-13; }
timed() { # timed <label> <cmd...> -> prints ms, logs to timings
  local label="$1"; shift
  local t0 t1; t0=$(now_ms); "$@" >"$OUT/last-cmd.out" 2>&1; t1=$(now_ms)
  printf '%-34s %6d ms\n' "$label" $((t1 - t0)) | tee -a "$OUT/timings.txt"
}
filter_calls() { grep -c "^$1" "$FILTER_LOG" 2>/dev/null || true; }
reset_filter_log() { : >"$FILTER_LOG"; }

# --- fabricate content, pointers, and the upstream repository ----------------

make_object() { # make_object <name> <mb> -> stores content, echoes "oid size"
  local f="$OUT/content-$1.bin"
  head -c "$(($2 * 1024 * 1024))" /dev/urandom >"$f"
  local oid size
  oid=$(sha256sum "$f" | cut -d' ' -f1); size=$(stat -c%s "$f")
  cp "$f" "$STORE/$oid"
  echo "$oid $size"
}
pointer() { # pointer <oid> <size>
  printf 'version https://git-lfs.github.com/spec/v1\noid sha256:%s\nsize %s\n' "$1" "$2"
}

read -r LARGE_OID LARGE_SIZE <<<"$(make_object large "$LARGE_MB")"
read -r MEDIUM_OID MEDIUM_SIZE <<<"$(make_object medium "$MEDIUM_MB")"
read -r LARGE2_OID LARGE2_SIZE <<<"$(make_object large2 $((LARGE_MB / 2)))"

UPSTREAM="$OUT/upstream.git"
SEED="$OUT/seed"
git init -q "$SEED"
mkdir -p "$SEED/bin" "$SEED/src"
printf '*.bin filter=lfs diff=lfs merge=lfs -text\n' >"$SEED/.gitattributes"
pointer "$LARGE_OID" "$LARGE_SIZE"  >"$SEED/bin/large.bin"
pointer "$MEDIUM_OID" "$MEDIUM_SIZE" >"$SEED/bin/medium.bin"
for i in $(seq 1 "$SMALL_FILES"); do
  printf 'fn f%d() -> u32 { %d }\n' "$i" "$i" >"$SEED/src/f$i.rs"
done
git -C "$SEED" add -A
git -C "$SEED" commit -qm v1
pointer "$LARGE2_OID" "$LARGE2_SIZE" >"$SEED/bin/large.bin"
git -C "$SEED" commit -qam v2
git -C "$SEED" branch -f v2
git -C "$SEED" checkout -q main~0 2>/dev/null || true
git -C "$SEED" update-ref refs/heads/main main~1 2>/dev/null || true
git clone -q --bare "$SEED" "$UPSTREAM"
V1=$(git -C "$UPSTREAM" rev-parse main) V2=$(git -C "$UPSTREAM" rev-parse v2)
POINTER_BLOB_LARGE=$(git -C "$UPSTREAM" rev-parse "$V1:bin/large.bin")

# --- the filter stubs --------------------------------------------------------

BIN="$OUT/bin"; mkdir -p "$BIN"
cat >"$BIN/clean-hash" <<EOF
#!/usr/bin/env bash
# git-lfs-equivalent clean: sha256 the streamed content, emit the pointer.
echo "clean-hash \$1" >>"$FILTER_LOG"
tmp=\$(mktemp); cat >"\$tmp"
oid=\$(sha256sum "\$tmp" | cut -d' ' -f1); size=\$(stat -c%s "\$tmp"); rm -f "\$tmp"
printf 'version https://git-lfs.github.com/spec/v1\noid sha256:%s\nsize %s\n' "\$oid" "\$size"
EOF
cat >"$BIN/clean-daemon" <<EOF
#!/usr/bin/env bash
# daemon-style clean: the mount knows the path is unmodified base content, so
# answer the pointer from metadata; drain stdin without hashing.
echo "clean-daemon \$1" >>"$FILTER_LOG"
cat >/dev/null
cat "$OUT/pointers/\$1"
EOF
cat >"$BIN/smudge-store" <<EOF
#!/usr/bin/env bash
# hydrating smudge: parse the pointer, serve bytes from the CAS.
echo "smudge \$1" >>"$FILTER_LOG"
oid=\$(grep '^oid sha256:' | cut -d: -f2)
cat "$STORE/\$oid"
EOF
chmod +x "$BIN"/*
mkdir -p "$OUT/pointers/bin"
pointer "$LARGE_OID" "$LARGE_SIZE"  >"$OUT/pointers/bin/large.bin"
pointer "$MEDIUM_OID" "$MEDIUM_SIZE" >"$OUT/pointers/bin/medium.bin"

# --- stage an ADR 0009-shaped workspace at v1 with expanded LFS content ------

stage() { # stage <dir>
  local ws="$1"
  rm -rf "$ws"; git init -q "$ws"
  echo "$UPSTREAM/objects" >"$ws/.git/objects/info/alternates"
  git -C "$ws" config core.checkStat minimal
  git -C "$ws" update-ref refs/heads/main "$V1"
  git -C "$ws" update-ref refs/heads/v2 "$V2"
  git -C "$ws" symbolic-ref HEAD refs/heads/main
  git -C "$ws" read-tree HEAD
  git -C "$ws" checkout-index -qa   # pointers land on disk, stat cache matches
  # ...and this is the mount's projection: expanded bytes behind those paths,
  # every base file stamped with the sanitized snapshot time (ADR 0006) — in
  # the past by construction, so no index entry can ever be racily clean.
  cp "$OUT/content-large.bin"  "$ws/bin/large.bin"
  cp "$OUT/content-medium.bin" "$ws/bin/medium.bin"
  find "$ws" -name .git -prune -o -type f -exec touch -d '5 minutes ago' {} +
}
config_filters() { # config_filters <ws> <clean-cmd>
  git -C "$1" config filter.lfs.clean "$2 %f"
  git -C "$1" config filter.lfs.smudge "$BIN/smudge-store %f"
  git -C "$1" config filter.lfs.required true
}

echo; echo "== A: expanded working tree, pointer index, NO filter config =="
WS="$OUT/ws-a"; stage "$WS"
timed "A git status (cold)" git -C "$WS" status --porcelain
grep -c '^ M' "$OUT/last-cmd.out" | xargs printf '  files reported modified: %s\n'
git -C "$WS" add bin/large.bin
ADDED=$(git -C "$WS" rev-parse ":bin/large.bin")
printf '  after add, index holds: %s\n' \
  "$([ "$ADDED" = "$POINTER_BLOB_LARGE" ] && echo "the pointer (safe)" \
     || echo "a $(git -C "$WS" cat-file -s "$ADDED")-byte expanded blob (CORRUPTION)")"

echo; echo "== B: filter.lfs = hashing stub (git-lfs equivalent) =="
WS="$OUT/ws-b"; stage "$WS"; config_filters "$WS" "$BIN/clean-hash"
reset_filter_log
timed "B git status (cold reconcile)" git -C "$WS" status --porcelain
printf '  clean-filter runs: %s, modified reported: %s\n' \
  "$(filter_calls clean-hash)" "$(grep -c '^ M' "$OUT/last-cmd.out" || true)"
reset_filter_log
timed "B git status (2nd)" git -C "$WS" status --porcelain
printf '  clean-filter runs: %s\n' "$(filter_calls clean-hash)"
# The mount-time equivalent: GFS seeds index stat data matching the projection.
timed "B git update-index --refresh" git -C "$WS" update-index --refresh
reset_filter_log
timed "B git status (stat coherent)" git -C "$WS" status --porcelain
printf '  clean-filter runs: %s\n' "$(filter_calls clean-hash)"
timed "B git diff (stat coherent)" git -C "$WS" diff --exit-code
touch "$WS/bin/large.bin"
reset_filter_log
timed "B git status after touch" git -C "$WS" status --porcelain
printf '  clean-filter runs: %s, modified reported: %s\n' \
  "$(filter_calls clean-hash)" "$(grep -c '^ M' "$OUT/last-cmd.out" || true)"
# The agent-edit path: genuinely change an LFS file, add, commit.
head -c 1048576 /dev/urandom >>"$WS/bin/medium.bin"
NEW_OID=$(sha256sum "$WS/bin/medium.bin" | cut -d' ' -f1)
NEW_SIZE=$(stat -c%s "$WS/bin/medium.bin")
git -C "$WS" add bin/medium.bin
git -C "$WS" commit -qm probe
COMMITTED=$(git -C "$WS" cat-file -p "HEAD:bin/medium.bin")
printf '  edited LFS file commits as: %s\n' \
  "$([ "$COMMITTED" = "$(pointer "$NEW_OID" "$NEW_SIZE")" ] \
     && echo "a fresh pointer for the new content (safe)" || echo "CORRUPTION")"
git -C "$WS" reset -q --hard "$V1"

echo; echo "== C: filter.lfs = daemon-style stub (no hashing) =="
WS="$OUT/ws-c"; stage "$WS"; config_filters "$WS" "$BIN/clean-daemon"
reset_filter_log
timed "C git status (cold reconcile)" git -C "$WS" status --porcelain
printf '  clean-filter runs: %s, modified reported: %s\n' \
  "$(filter_calls clean-daemon)" "$(grep -c '^ M' "$OUT/last-cmd.out" || true)"
reset_filter_log
timed "C git status (2nd)" git -C "$WS" status --porcelain
printf '  clean-filter runs: %s\n' "$(filter_calls clean-daemon)"
timed "C git update-index --refresh" git -C "$WS" update-index --refresh
reset_filter_log
timed "C git status (stat coherent)" git -C "$WS" status --porcelain
printf '  clean-filter runs: %s\n' "$(filter_calls clean-daemon)"

echo; echo "== D: stock checkout across LFS revisions, hydrating smudge =="
WS="$OUT/ws-c"
reset_filter_log
timed "D git checkout v2" git -C "$WS" checkout -q v2
printf '  smudge runs: %s\n' "$(filter_calls smudge)"
GOT=$(sha256sum "$WS/bin/large.bin" | cut -d' ' -f1)
printf '  working file now: %s\n' \
  "$([ "$GOT" = "$LARGE2_OID" ] && echo "v2 expanded content, verified by oid" || echo "WRONG CONTENT")"
reset_filter_log
timed "D git status after checkout" git -C "$WS" status --porcelain
printf '  clean-filter runs: %s\n' "$(filter_calls clean-daemon)"

echo; echo "done. timings in $OUT/timings.txt"
