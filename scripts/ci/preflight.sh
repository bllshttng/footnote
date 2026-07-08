#!/usr/bin/env bash
# scripts/ci/preflight.sh - hermetic "CI's verdict, earlier" runner.
#
# One command to run before pushing. It validates the invoking checkout's
# committed HEAD inside a persistent, hermetic preflight worktree so a local
# green means CI green - without the canonical checkout's .fno/config.toml
# leaking into the config candidate chain (the PR-churn class this exists to
# kill). Deterministic checks only; no LLM review (that stays at config.review.*).
#
# Flow: resolve the persistent preflight worktree -> refuse a dirty invoking
# tree -> lock -> reset the worktree to the invoking HEAD (caches preserved) ->
# build a hermetic env -> smoke.sh --keep-going -> rust-ci legs (pinned fmt,
# cargo test, advisory audit) -> one summary + exit.
#
# Usage:
#   scripts/ci/preflight.sh [--retry-failed]
#     --retry-failed   re-run only the steps smoke.sh recorded last time
#                      (a SUBSET; run a full preflight before the settle push).
#
# Exit codes: 0 all non-advisory suites passed; 1 a suite failed; 2 bad usage /
#   missing prerequisite; 3 lock held; 4 dirty invoking tree.
#
# Bash 3.2 compatible (macOS default). No flock dependency (atomic mkdir lock).

set -uo pipefail

PINNED_FMT="1.94.1"   # keep in lockstep with rust-ci.yml RUSTFMT_TOOLCHAIN

RETRY_FAILED=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --retry-failed) RETRY_FAILED=1 ;;
        -h|--help) grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "preflight: unknown arg '$1'" >&2; exit 2 ;;
    esac
    shift
done

# --- resolve invoking checkout + canonical repo -----------------------------
INVOKING_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
    echo "preflight: not a git repo" >&2; exit 2; }
COMMON_DIR="$(git rev-parse --path-format=absolute --git-common-dir)"
CANONICAL_ROOT="$(dirname "$COMMON_DIR")"
REPO_NAME="$(basename "$CANONICAL_ROOT")"

# --- resolve the persistent preflight worktree path -------------------------
# config.paths.worktrees_base if set (same knob as everything else), else the
# harness-native .claude/worktrees. Tilde-expanded.
WT_BASE="$(fno config get paths.worktrees_base 2>/dev/null | tail -1 | tr -d '[:space:]')"
if [[ -n "$WT_BASE" && "$WT_BASE" != "null" && "$WT_BASE" != *Error* ]]; then
    WT_BASE="${WT_BASE/#\~/$HOME}"
    PREFLIGHT_WT="$WT_BASE/$REPO_NAME/preflight"
else
    PREFLIGHT_WT="$CANONICAL_ROOT/.claude/worktrees/preflight"
fi

# --- refuse a dirty invoking tree (AC2-ERR) ---------------------------------
DIRTY="$(git -C "$INVOKING_ROOT" status --porcelain)"
if [[ -n "$DIRTY" ]]; then
    echo "preflight: refusing - invoking worktree has uncommitted changes." >&2
    echo "preflight validates the committed HEAD; commit or stash first:" >&2
    echo "$DIRTY" | sed 's/^/  /' >&2
    exit 4
fi
CANDIDATE_SHA="$(git -C "$INVOKING_ROOT" rev-parse HEAD)"
CANDIDATE_SHORT="$(git -C "$INVOKING_ROOT" rev-parse --short HEAD)"

# --- lock (atomic mkdir; steal a dead holder) -------------------------------
LOCKDIR="$COMMON_DIR/.preflight.lock.d"
acquire_lock() {
    if mkdir "$LOCKDIR" 2>/dev/null; then return 0; fi
    local holder_pid holder_line
    holder_line="$(cat "$LOCKDIR/holder" 2>/dev/null || echo '')"
    holder_pid="$(printf '%s' "$holder_line" | sed -n 's/.*pid=\([0-9]*\).*/\1/p')"
    if [[ -n "$holder_pid" ]] && ! kill -0 "$holder_pid" 2>/dev/null; then
        # dead holder: steal once
        rm -rf "$LOCKDIR"
        mkdir "$LOCKDIR" 2>/dev/null && return 0
    fi
    echo "preflight: lock held - $holder_line" >&2
    exit 3
}
acquire_lock
printf 'pid=%s started=%s host=%s sha=%s\n' "$$" "$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo unknown)" "$(hostname 2>/dev/null)" "$CANDIDATE_SHORT" > "$LOCKDIR/holder"

TMPHOME=""
cleanup() { rm -rf "$LOCKDIR"; [[ -n "$TMPHOME" ]] && rm -rf "$TMPHOME"; }
trap cleanup EXIT INT TERM

# --- ensure / reset the preflight worktree ----------------------------------
echo "preflight: repo=$REPO_NAME candidate=$CANDIDATE_SHORT worktree=$PREFLIGHT_WT"
is_registered() { git -C "$INVOKING_ROOT" worktree list --porcelain | grep -qxF "worktree $PREFLIGHT_WT"; }

git -C "$INVOKING_ROOT" worktree prune >/dev/null 2>&1 || true  # drop dangling admin entries from a prior rm -rf
if is_registered; then
    : # exists and registered; reset below
elif [[ -e "$PREFLIGHT_WT" ]]; then
    echo "preflight: $PREFLIGHT_WT exists but is not a registered worktree - recreating" >&2
    rm -rf "$PREFLIGHT_WT"
    git -C "$INVOKING_ROOT" worktree prune >/dev/null 2>&1 || true
fi
if ! is_registered; then
    mkdir -p "$(dirname "$PREFLIGHT_WT")"
    git -C "$INVOKING_ROOT" worktree add --detach "$PREFLIGHT_WT" "$CANDIDATE_SHA" >/dev/null 2>&1 || {
        echo "preflight: git worktree add failed" >&2; exit 1; }
fi

# Sync to candidate; keep caches. Worktrees share the object DB, so no fetch.
if ! git -C "$PREFLIGHT_WT" reset --hard "$CANDIDATE_SHA" >/dev/null 2>&1; then
    echo "preflight: git reset --hard failed in the preflight worktree" >&2; exit 1
fi
# clean -fdx but preserve warm caches + the failure record.
git -C "$PREFLIGHT_WT" clean -fdx -e target -e cli/.venv -e .preflight-last-failures.txt -e .fno >/dev/null 2>&1 || {
    echo "preflight: git clean failed in the preflight worktree" >&2; exit 1; }

# --- hermetic env ------------------------------------------------------------
REAL_HOME="$HOME"
TMPHOME="$(mktemp -d)"
# An empty config file pinned via FNO_CONFIG is the load-bearing isolation seam:
# $FNO_CONFIG, when set, is the config loader's ONLY candidate, so the canonical
# checkout's .fno/config.toml (which a worktree reaches via the shared
# git-common-dir, NOT via HOME/cwd) can't leak worktrees_base and friends into
# the run. Empty file => the SettingsModel defaults, which is exactly what a
# fresh CI checkout (no committed .fno/config.toml) resolves to. This is what
# makes the PR-#269 local-only failure class not reproduce here (AC2-HP).
EMPTY_CONFIG="$TMPHOME/empty-config.toml"
: > "$EMPTY_CONFIG"

# Runs a command inside the preflight worktree with a scrubbed env: temp HOME,
# no FNO_* leakage, empty pinned config, worktree-pinned PYTHONPATH.
# Cache dirs are the documented re-export holes so builds stay warm.
run_hermetic() {
    (
        cd "$PREFLIGHT_WT" || exit 1
        local v
        for v in $(compgen -v | grep '^FNO_' || true); do unset "$v"; done
        export HOME="$TMPHOME"
        export FNO_CONFIG="$EMPTY_CONFIG"
        export FNO_GLOBAL_SETTINGS_PATH=/dev/null
        export FNO_THINK_SPAWN=0
        export PYTHONPATH="$PREFLIGHT_WT/cli/src"
        export CARGO_HOME="${CARGO_HOME:-$REAL_HOME/.cargo}"
        export RUSTUP_HOME="${RUSTUP_HOME:-$REAL_HOME/.rustup}"
        export UV_CACHE_DIR="${UV_CACHE_DIR:-$REAL_HOME/.cache/uv}"
        "$@"
    )
}

# --- suites ------------------------------------------------------------------
LEG_NAMES=(); LEG_STATUS=(); LEG_SECS=()
record_leg() { LEG_NAMES+=("$1"); LEG_STATUS+=("$2"); LEG_SECS+=("$3"); }
FAIL=0

echo ""
echo "preflight: === smoke suite ($([[ $RETRY_FAILED -eq 1 ]] && echo retry-failed || echo keep-going)) ==="
SMOKE_ARGS=(--keep-going); [[ $RETRY_FAILED -eq 1 ]] && SMOKE_ARGS=(--retry-failed --keep-going)
s0="$SECONDS"
run_hermetic bash scripts/ci/smoke.sh "${SMOKE_ARGS[@]}"
sreq=$?
[[ $sreq -eq 0 ]] && record_leg "smoke suite" pass $(( SECONDS - s0 )) || { record_leg "smoke suite" fail $(( SECONDS - s0 )); FAIL=1; }

# rust-ci legs (pinned fmt, cargo test, advisory audit) ----------------------
have_pinned_fmt() { rustup toolchain list 2>/dev/null | grep -q "^$PINNED_FMT"; }

run_rust_leg() { # name status-var  cwd  cmd...
    local name="$1" cwd="$2"; shift 2
    echo ""
    echo "preflight: === $name ==="
    local t0="$SECONDS"
    run_hermetic bash -c "cd '$cwd' && $*"
    local rc=$?
    if [[ $rc -eq 0 ]]; then record_leg "$name" pass $(( SECONDS - t0 ))
    else record_leg "$name" fail $(( SECONDS - t0 )); FAIL=1; fi
}

if have_pinned_fmt; then
    run_rust_leg "cargo fmt --check (fno-agents, +$PINNED_FMT)" "crates/fno-agents" "cargo +$PINNED_FMT fmt --all --check"
    run_rust_leg "cargo fmt --check (fno, +$PINNED_FMT)" "crates/fno" "cargo +$PINNED_FMT fmt --all --check"
else
    echo "preflight: pinned rustfmt toolchain $PINNED_FMT not installed - fmt leg cannot match rust-ci" >&2
    echo "preflight: install it: rustup toolchain install $PINNED_FMT --component rustfmt" >&2
    record_leg "cargo fmt --check (+$PINNED_FMT MISSING)" fail 0; FAIL=1
fi

run_rust_leg "cargo test --all-targets (fno-agents)" "crates/fno-agents" "cargo test --all-targets"
run_rust_leg "cargo test --all-targets (fno)" "crates/fno" "cargo test --all-targets"

# advisory: never flips the exit code
echo ""
echo "preflight: === cargo audit (ADVISORY) ==="
if run_hermetic bash -c "command -v cargo-audit >/dev/null 2>&1"; then
    a0="$SECONDS"
    if run_hermetic bash -c "cd crates/fno-agents && cargo audit" && run_hermetic bash -c "cd crates/fno && cargo audit"; then
        record_leg "cargo audit (ADVISORY)" pass $(( SECONDS - a0 ))
    else
        record_leg "cargo audit (ADVISORY)" "advisory-fail" $(( SECONDS - a0 ))
    fi
else
    record_leg "cargo audit (ADVISORY)" "skipped (not installed)" 0
fi

# --- summary -----------------------------------------------------------------
echo ""
echo "preflight: SUMMARY  repo=$REPO_NAME  candidate=$CANDIDATE_SHORT  mode=$([[ $RETRY_FAILED -eq 1 ]] && echo RETRY-SUBSET || echo FULL)"
[[ $RETRY_FAILED -eq 1 ]] && echo "preflight: RETRY SUBSET - run a full preflight before the settle-green push"
for i in "${!LEG_NAMES[@]}"; do
    printf '  %-24s %5ss  %s\n' "${LEG_STATUS[$i]}" "${LEG_SECS[$i]}" "${LEG_NAMES[$i]}"
done
echo ""
if [[ $FAIL -eq 0 ]]; then
    echo "preflight: GREEN - safe to push $CANDIDATE_SHORT"
    exit 0
else
    echo "preflight: RED - fix, commit, then 'scripts/ci/preflight.sh --retry-failed'" >&2
    exit 1
fi
