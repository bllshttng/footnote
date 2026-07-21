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
#   missing prerequisite; 3 lock held; 4 dirty invoking tree; 5 VOID (the run
#   lost the shared worktree or its lock, so it earned no verdict - re-run;
#   this is NOT a suite failure and must not be reported as one).
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
WT_BASE="$(fno config get paths.worktrees_base 2>/dev/null | tail -1 | tr -d '[:space:]' || true)"
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
stamp_holder() {
    printf 'pid=%s started=%s host=%s sha=%s\n' "$$" "$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo unknown)" "$(hostname 2>/dev/null || echo unknown)" "$CANDIDATE_SHORT" > "$LOCKDIR/holder"
}
acquire_lock() {
    if mkdir "$LOCKDIR" 2>/dev/null; then stamp_holder; return 0; fi
    local holder_pid holder_line
    holder_line="$(cat "$LOCKDIR/holder" 2>/dev/null || echo '')"
    holder_pid="$(printf '%s' "$holder_line" | sed -n 's/.*pid=\([0-9]*\).*/\1/p')"
    if [[ -n "$holder_pid" ]] && ! kill -0 "$holder_pid" 2>/dev/null; then
        # Steal a dead holder by rename, never `rm -rf` + `mkdir`: rename is one
        # atomic operation, so exactly one of N concurrent stealers wins the
        # corpse. With rm -rf, a loser deletes the lockdir the winner just
        # recreated and both proceed into the one shared worktree - each then
        # reset --hard's it mid-run of the other, so a suite reports pass/fail
        # legs earned by somebody else's checkout.
        local mv_err reaped
        if mv_err="$(mv "$LOCKDIR" "$LOCKDIR.reap.$$" 2>&1)"; then
            # Steal-then-verify. Rename is atomic, but the dead-holder CHECK
            # above and this rename are not one operation: a racer that read the
            # same corpse can be descheduled while the winner reaps it and
            # installs its own lock, then rename away that LIVE lock believing
            # it is the corpse it validated. Both then run against the one
            # shared worktree, which is the whole bug. So confirm we moved the
            # exact holder we condemned; if not, put it back and lose the race.
            reaped="$(cat "$LOCKDIR.reap.$$/holder" 2>/dev/null || echo '')"
            if [[ "$reaped" == "$holder_line" ]]; then
                rm -rf "$LOCKDIR.reap.$$"
                mkdir "$LOCKDIR" 2>/dev/null && { stamp_holder; return 0; }
            else
                mv "$LOCKDIR.reap.$$" "$LOCKDIR" 2>/dev/null || rm -rf "$LOCKDIR.reap.$$"
            fi
            # Lost the race: re-read so we name the live winner rather than the
            # corpse we just reaped.
            holder_line="$(cat "$LOCKDIR/holder" 2>/dev/null || echo '')"
        else
            # The holder is provably dead, so a failed reap is an environment
            # problem (permissions, read-only .git). Saying "lock held" here
            # would send the user chasing a pid they can see is not running.
            echo "preflight: cannot reap a dead holder at $LOCKDIR: $mv_err" >&2
            exit 3
        fi
    fi
    if [[ -z "$holder_line" ]]; then
        # No parsable holder: nothing proves this lock is live, but we still
        # refuse rather than steal (a holder killed between mkdir and its stamp
        # looks identical to this). Name the path so the recovery is obvious.
        echo "preflight: lock held by an unidentified holder (no readable $LOCKDIR/holder)." >&2
        echo "preflight: if no preflight is running, remove it: rm -rf '$LOCKDIR'" >&2
    else
        echo "preflight: lock held - $holder_line" >&2
    fi
    exit 3
}
acquire_lock

TMPHOME=""
holder_pid_now() { sed -n 's/.*pid=\([0-9]*\).*/\1/p' "$LOCKDIR/holder" 2>/dev/null; }
# Release only a lock we still hold: if ours was stolen, the lockdir at this
# path now belongs to the stealer. An unreadable holder still releases - that is
# our own stamp having failed, and leaving it would wedge every later run.
# Parse the pid rather than matching the stamp's layout, so reordering the
# fields in stamp_holder cannot silently stop every run from releasing.
cleanup() {
    local pid_now; pid_now="$(holder_pid_now)"
    [[ -z "$pid_now" || "$pid_now" == "$$" ]] && rm -rf "$LOCKDIR"
    [[ -n "$TMPHOME" ]] && rm -rf "$TMPHOME"
}
trap cleanup EXIT INT TERM

# --- ensure / reset the preflight worktree ----------------------------------
echo "preflight: repo=$REPO_NAME candidate=$CANDIDATE_SHORT worktree=$PREFLIGHT_WT"
# grep, not grep -q: -q exits on first match and SIGPIPEs `git worktree list`,
# which under pipefail returns 141 (false) and would falsely recreate the wt.
is_registered() { git -C "$INVOKING_ROOT" worktree list --porcelain | grep -xF "worktree $PREFLIGHT_WT" >/dev/null; }

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
# clean -fdx but preserve warm caches + the failure record ONLY. Excluding all
# of .fno would leave stale per-run state (e.g. triage-log.jsonl a smoke test
# reads) that could mask a regression a fresh CI checkout would catch, so we
# scope the exclusion to the single retry-record file.
git -C "$PREFLIGHT_WT" clean -fdx -e target -e cli/.venv -e .fno/preflight-last-failures.txt >/dev/null 2>&1 || {
    echo "preflight: git clean failed in the preflight worktree" >&2; exit 1; }

# --- hermetic env ------------------------------------------------------------
REAL_HOME="$HOME"
TMPHOME="$(mktemp -d)"

# The env deliberately mirrors a fresh CI checkout: temp HOME (no ~/.fno, no
# ~/.claude, no ~/.gitconfig), FNO_* scrubbed, worktree-pinned PYTHONPATH, and
# the pytest spawn-leak guard. We intentionally do NOT pin FNO_CONFIG or
# FNO_GLOBAL_SETTINGS_PATH: pinning either one diverges from CI and breaks the
# suite's own config-fixture tests (an empty FNO_CONFIG clobbers a test's
# monkeypatched config; a /dev/null global path redirects config WRITES into
# /dev/). Two other ambient inputs a bare FNO_* scrub misses are sealed below:
#   - Ambient harness identity: preflight always runs inside a live harness, so
#     CLAUDE_CODE_SESSION_ID / CODEX_* / GEMINI_SESSION_ID are set and
#     resolve_self_model() would resolve the real session's model instead of the
#     "unknown" floor a fresh checkout produces. run_hermetic unsets every
#     HARNESS_SESSION_MARKERS name (derived from the Python single source of
#     truth, fail-closed to a literal list).
#   - Canonical config climb: a linked worktree reaches the main checkout's
#     .fno/config.toml via the shared git-common-dir (not HOME/cwd), leaking
#     worktrees_base into path/worktree tests. run_hermetic exports
#     FNO_NO_CANONICAL_CONFIG=1 so _settings_yaml_locations() drops that one
#     candidate. See docs/preflight.md.

# Derive the ambient harness marker names from the Python single source of truth
# (HARNESS_SESSION_MARKERS) so the scrub list never drifts from the tuple. Fail
# closed on EITHER a nonzero exit OR empty output (a broken venv that prints a
# partial line before erroring must not slip past an emptiness-only check), warn,
# and fall back to a hardcoded literal list - never silently skip the scrub.
if HARNESS_MARKERS="$(PYTHONPATH="$PREFLIGHT_WT/cli/src" python3 -c \
    'from fno.harness_identity import HARNESS_SESSION_MARKERS; print(" ".join(m[0] for m in HARNESS_SESSION_MARKERS))' 2>/dev/null)" \
   && [[ -n "$HARNESS_MARKERS" ]]; then
    :
else
    echo "preflight: WARN harness-marker fetch failed; using hardcoded fallback list" >&2
    HARNESS_MARKERS="CODEX_THREAD_ID CLAUDE_CODE_SESSION_ID CODEX_SESSION_ID GEMINI_SESSION_ID"
fi

run_hermetic() {
    (
        cd "$PREFLIGHT_WT" || exit 1
        local v
        for v in $(compgen -v | grep '^FNO_' || true); do unset "$v"; done
        for v in $HARNESS_MARKERS; do unset "$v"; done
        export HOME="$TMPHOME"
        export FNO_THINK_SPAWN=0
        export FNO_NO_CANONICAL_CONFIG=1
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
have_pinned_fmt() { rustup toolchain list 2>/dev/null | grep "^$PINNED_FMT" >/dev/null; }

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

# --- verdict tripwire --------------------------------------------------------
# Belt-and-braces over the lock: re-verify we still own both the worktree and
# the lock before attributing a verdict to our candidate. Any residual clobber -
# a future lock bug, a hand-run `git reset` in the shared worktree - becomes a
# loud VOID instead of a GREEN or RED silently earned by another checkout.
# Compare shas only; the preflight worktree is always detached HEAD.
VOID_REASON=""
if ! WT_HEAD_NOW="$(git -C "$PREFLIGHT_WT" rev-parse HEAD 2>&1)"; then
    VOID_REASON="cannot read the preflight worktree at $PREFLIGHT_WT: $WT_HEAD_NOW"
elif [[ "$WT_HEAD_NOW" != "$CANDIDATE_SHA" ]]; then
    VOID_REASON="worktree moved off our candidate mid-run (now ${WT_HEAD_NOW:0:12}, expected $CANDIDATE_SHORT)"
elif [[ "$(holder_pid_now)" != "$$" ]]; then
    VOID_REASON="another preflight took our lock mid-run"
fi
if [[ -n "$VOID_REASON" ]]; then
    echo "preflight: VOID - $VOID_REASON." >&2
    echo "preflight: verdict discarded - nothing here was earned by $CANDIDATE_SHORT. Re-run; this is not a code failure." >&2
    exit 5
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
