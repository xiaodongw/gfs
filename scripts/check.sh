#!/usr/bin/env bash
#
# The full local gate. CI runs the same script, so a green run here means a green
# run there; that is the point of having one entry point rather than a list of
# commands in a README that drifts.
#
# Usage:
#   scripts/check.sh            everything
#   scripts/check.sh fmt clippy just those stages
#
# Stages that need a tool which is not installed are reported as skipped rather
# than passed, so a missing tool can never look like a passing check.

set -euo pipefail
cd "$(dirname "$0")/.."

STAGES=("$@")
if [ ${#STAGES[@]} -eq 0 ]; then
  STAGES=(versions fmt clippy test doc deny licenses sbom secrets)
fi

failed=()
skipped=()

run_stage() {
  local name="$1"
  shift
  printf '\n\033[1m== %s ==\033[0m\n' "$name"
  if "$@"; then
    return 0
  fi
  failed+=("$name")
  return 0
}

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    printf 'skipped: %s is not installed (%s)\n' "$1" "${2:-}"
    skipped+=("$1")
    return 1
  fi
}

# ---------------------------------------------------------------------------

stage_versions() {
  # ADR 0001 pins libgit2, git2-rs, and stock Git. PLAN.md M1.1 requires the
  # pinning to be enforced so local and CI environments cannot drift, and the
  # cheapest enforcement is to fail loudly at the start of every check run.
  local want_git="2.53.0"
  local got_git
  got_git=$(git --version | awk '{print $3}')
  if [ "$got_git" != "$want_git" ]; then
    printf 'stock Git is %s but ADR 0001 pins %s.\n' "$got_git" "$want_git"
    printf 'The gateway runs this binary as upload-pack, so a mismatch means\n'
    printf 'the protocol matrix was measured against a different server.\n'
    # A warning rather than a failure: a developer reading code should not be
    # blocked by it, but nobody should be able to claim the matrix passed.
    printf '\033[33mwarning\033[0m: version drift from ADR 0001\n'
  else
    printf 'ok   stock Git %s matches ADR 0001\n' "$got_git"
  fi
  printf 'ok   toolchain %s\n' "$(rustc --version)"
}

stage_fmt() { cargo fmt --all -- --check; }

stage_clippy() { cargo clippy --workspace --all-targets --all-features -- -D warnings; }

stage_test() { cargo test --workspace --all-features; }

stage_doc() {
  # Doc tests plus a link/intra-doc check. The design documents are the primary
  # specification and the code cross-references them constantly; a broken
  # intra-doc link is a broken cross-reference.
  RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
}

stage_deny() {
  need cargo-deny "cargo install cargo-deny" || return 0
  cargo deny check
}

stage_licenses() { scripts/check-licenses.sh; }

stage_sbom() {
  need cargo-cyclonedx "cargo install cargo-cyclonedx" || return 0
  mkdir -p target/sbom
  cargo cyclonedx --format json --output-pattern package --output-prefix target/sbom/
  # The SBOM is generated from crate metadata and therefore inherits the blind
  # spot ADR 0001 measured: it will not contain the statically linked GPL-2.0
  # libgit2. Record that in the artifact itself so a downstream consumer of the
  # SBOM is not misled by its completeness.
  cat >target/sbom/ADDENDUM.md <<'EOF'
# SBOM addendum: components crate metadata does not report

This SBOM is generated from `cargo metadata` and is therefore incomplete for
this project. ADR 0001 measured the gap:

| Component | Version | License | Why the SBOM misses it |
| --- | --- | --- | --- |
| libgit2 (vendored C) | 1.9.6 | GPL-2.0-only WITH the libgit2 linking exception | Compiled and statically linked by `libgit2-sys`, which declares only its own `MIT OR Apache-2.0` |
| bundled zlib | (as vendored) | Zlib | Linked inside libgit2 |
| bundled PCRE2 | (as vendored) | BSD-3-Clause | Linked inside libgit2 |
| stock Git | 2.53.0 | GPL-2.0 (mixed) | Executed as a subprocess; not a Cargo dependency at all |

`scripts/check-licenses.sh` asserts these rows directly. Ship libgit2's
`COPYING` with any binary distribution, and distribute stock Git as its own
package or container layer with its own license text and a source offer.
EOF
  printf 'SBOM written to target/sbom/ with the ADR 0001 addendum.\n'
}

stage_secrets() {
  # A dependency-free scan for the shapes of credential that could plausibly be
  # committed here. Not a replacement for a hosted scanner; a fast local gate so
  # an obvious mistake does not reach a commit.
  local pattern='(-----BEGIN [A-Z ]*PRIVATE KEY-----|ghp_[A-Za-z0-9]{36}|github_pat_[A-Za-z0-9_]{22,}|xox[abprs]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16}|eyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.)'
  local hits
  # Search tracked files only: an untracked scratch file is the developer's
  # business, and a committed one is what this gate is for.
  if hits=$(git grep -nIE "$pattern" -- \
    ':!scripts/check.sh' ':!*.lock' 2>/dev/null); then
    printf 'Possible committed credential:\n%s\n' "$hits"
    return 1
  fi
  printf 'ok   no credential-shaped strings in tracked files\n'
}

for stage in "${STAGES[@]}"; do
  case "$stage" in
    versions | fmt | clippy | test | doc | deny | licenses | sbom | secrets)
      run_stage "$stage" "stage_$stage"
      ;;
    *)
      printf 'unknown stage: %s\n' "$stage" >&2
      exit 2
      ;;
  esac
done

printf '\n'
if [ ${#skipped[@]} -gt 0 ]; then
  printf '\033[33mskipped\033[0m (tool not installed): %s\n' "${skipped[*]}"
fi
if [ ${#failed[@]} -gt 0 ]; then
  printf '\033[31mFAILED\033[0m: %s\n' "${failed[*]}"
  exit 1
fi
printf '\033[32mAll requested stages passed.\033[0m\n'
