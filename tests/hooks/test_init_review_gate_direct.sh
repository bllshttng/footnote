#!/usr/bin/env bash
# test_init_review_gate_direct.sh -- the review-capability gate must fire on the
# DIRECT `bash init-target-state.sh` path, not only inside `fno target init`.
#
# Before x-4a60 the two refusals lived solely in the Python wrapper, so the
# entry point SKILL.md documents ("if `fno` is unavailable, run
# hooks/helpers/init-target-state.sh directly") wrote a manifest and started a
# full run on a typo'd config.review.github_apps login, surfacing only at the
# stop gate. A guard on one of two reachable paths is decorative.
#
# The gate is delegated to `fno do target check-review-gate`, so these scenarios
# stub `fno` and assert on the SCRIPT's handling of its exit code. What the
# verdicts themselves decide is pinned in
# cli/tests/unit/test_target_init_review_gate.py; that split is deliberate, the
# tables stay in one language.
#
# Covers:
#   - AC1-HP:  stub exits 9 => script exits 2 and writes NO manifest
#   - AC4-ERR: stub exits 1 => `note:` line naming the rc, manifest still written
#   - AC4-ERR: stub exits 2 (Click's "No such command" on a STALE fno) => same
#              note-and-proceed, NEVER a refusal
#   - AC5-ERR: no fno on PATH => `fno absent` note, manifest still written
#   - AC3-CON: FNO_TARGET_INIT_GATED=1 => the verb is never invoked at all
#
# Exit codes:
#   0  all scenarios passed
#   1  assertion failed
#   77 skipped (missing dependencies)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
INIT="${REPO_ROOT}/hooks/helpers/init-target-state.sh"

log()  { printf '[review-gate-direct] %s\n' "$*"; }
fail() { printf '[review-gate-direct] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[review-gate-direct] PASS: %s\n' "$*"; }
skip() { printf '[review-gate-direct] SKIP: %s\n' "$*" >&2; exit 77; }

command -v git &>/dev/null || skip "git not on PATH"
[[ -f "$INIT" ]] || fail "init script not found at $INIT"

bash -n "$INIT" || fail "bash -n rejected $INIT (syntax error)"
pass "init script passes bash -n"

_ALL_TMPS=()
# The +"${...}" guard keeps an EMPTY array from tripping `set -u` on bash 3.2
# (macOS /bin/bash), which the two pre-append exits above can still reach.
trap 'rm -rf ${_ALL_TMPS[@]+"${_ALL_TMPS[@]}"}' EXIT

# ── Helper: isolated repo + a stub `fno` that logs every invocation ──
# The stub answers ONLY `do target check-review-gate` with $2; every other fno
# subcommand the script makes later (config get, claim status, backlog get)
# exits 0 silently, so a scenario tests the gate and nothing else.
# Usage: make_repo <tmpvar> <gate_exit_code>
make_repo() {
  local _varname="$1" _gate_rc="$2" _dir
  _dir="$(mktemp -d -t init-review-gate.XXXXXX)" || fail "mktemp failed"
  eval "${_varname}=\"\${_dir}\""
  (cd "$_dir" && git init -q && mkdir -p .fno home/.fno bin) \
    || fail "repo setup failed in $_dir"
  printf '# isolated\n' > "${_dir}/.fno/config.toml"
  printf '# isolated\n' > "${_dir}/home/.fno/config.toml"
  # Pre-create so the never-called scenario counts 0 instead of erroring.
  : > "${_dir}/gate-calls.log"

  cat > "${_dir}/bin/fno" << STUB
#!/usr/bin/env bash
if [[ "\$1" == "do" && "\$2" == "target" && "\$3" == "check-review-gate" ]]; then
  echo "gate-called" >> "${_dir}/gate-calls.log"
  echo "stub refusal: config.review.github_apps names a bot" >&2
  exit ${_gate_rc}
fi
if [[ "\$1" == "target" ]]; then
  echo "deprecated target root reached" >&2
  exit 2
fi
exit 0
STUB
  chmod +x "${_dir}/bin/fno"
}

# Usage: run_init <dir> [extra env assignments...]
run_init() {
  local _dir="$1"; shift
  (cd "$_dir" && env \
    PATH="${_dir}/bin:${PATH}" \
    HOME="${_dir}/home" \
    TARGET_START=1 \
    TARGET_INPUT="review-gate-probe" \
    "$@" \
    bash "$INIT") > "${_dir}/out.log" 2> "${_dir}/err.log"
}

gate_calls() { wc -l < "$1/gate-calls.log" | tr -d ' '; }

# ── AC1-HP: a refusal (exit 9) stops the bootstrap before any write ──
log "AC1-HP: gate exits 9 => script exits 2, no manifest"
make_repo TMP_REFUSE 9
_ALL_TMPS+=("$TMP_REFUSE")

run_init "$TMP_REFUSE"
_RC=$?
[[ "$_RC" -eq 2 ]] || fail "AC1-HP: expected exit 2, got $_RC (err: $(cat "$TMP_REFUSE/err.log"))"
pass "AC1-HP: script propagated exit 2"

[[ ! -f "$TMP_REFUSE/.fno/target-state.md" ]] \
  || fail "AC1-HP: manifest written despite refusal - a refused run left state behind"
pass "AC1-HP: no manifest written"

grep -q "config.review.github_apps" "$TMP_REFUSE/err.log" \
  || fail "AC1-HP: the gate's own stderr was swallowed (got: $(cat "$TMP_REFUSE/err.log"))"
pass "AC1-HP: the gate's refusal message reached stderr"

[[ "$(gate_calls "$TMP_REFUSE")" == "1" ]] \
  || fail "AC1-HP: expected exactly 1 gate call, got $(gate_calls "$TMP_REFUSE")"
pass "AC1-HP: gate invoked exactly once"

# ── AC4-ERR: a BROKEN gate (any non-9 non-zero) must not brick bootstrap ──
log "AC4-ERR: gate exits 1 => note naming the rc, run proceeds"
make_repo TMP_BROKEN 1
_ALL_TMPS+=("$TMP_BROKEN")

run_init "$TMP_BROKEN"
_RC=$?
[[ "$_RC" -eq 0 ]] || fail "AC4-ERR: a broken gate blocked bootstrap (exit $_RC; err: $(cat "$TMP_BROKEN/err.log"))"
pass "AC4-ERR: script exited 0"

[[ -f "$TMP_BROKEN/.fno/target-state.md" ]] \
  || fail "AC4-ERR: manifest not written - a broken gate must fail open"
pass "AC4-ERR: manifest written"

grep -q "review capability gate unavailable (rc=1)" "$TMP_BROKEN/err.log" \
  || fail "AC4-ERR: no note naming the return code (got: $(cat "$TMP_BROKEN/err.log"))"
pass "AC4-ERR: note names the return code"

# ── AC4-ERR (stale CLI): rc 2 is Click's UsageError, NOT a refusal ──
# Found by running this for real: an installed `fno` predating the verb prints
# "No such command 'check-review-gate'" and exits 2. If 2 meant "refused", every
# direct bootstrap would hard-refuse the moment the CLI fell behind source -
# the exact broken-gate-bricks-bootstrap failure the design forbids.
log "AC4-ERR: gate exits 2 (stale fno, no such command) => note, run proceeds"
make_repo TMP_STALE 2
_ALL_TMPS+=("$TMP_STALE")

run_init "$TMP_STALE"
_RC=$?
[[ "$_RC" -eq 0 ]] \
  || fail "AC4-ERR: a stale fno (Click exit 2) was treated as a refusal (exit $_RC) - bootstrap bricked"
pass "AC4-ERR: stale-CLI exit 2 did not refuse"

[[ -f "$TMP_STALE/.fno/target-state.md" ]] \
  || fail "AC4-ERR: manifest not written on a stale CLI"
pass "AC4-ERR: manifest written on a stale CLI"

grep -q "fno doctor --fix" "$TMP_STALE/err.log" \
  || fail "AC4-ERR: note does not point at the remedy (got: $(cat "$TMP_STALE/err.log"))"
pass "AC4-ERR: note names the stale-fno remedy"

# ── AC5-ERR: no `fno` at all - the honest limit, stated not hidden ──
log "AC5-ERR: fno absent => absent note, run proceeds"
make_repo TMP_NOFNO 2
_ALL_TMPS+=("$TMP_NOFNO")
rm -f "${TMP_NOFNO}/bin/fno"

# Keep git/python on PATH but drop the stub dir; a PATH with no `fno` anywhere
# is the real shape of the documented "if fno is unavailable" fallback.
_CLEAN_PATH="$(cd "$TMP_NOFNO" && command -v git | xargs dirname)"
(cd "$TMP_NOFNO" && env \
  PATH="${_CLEAN_PATH}:/usr/bin:/bin" \
  HOME="${TMP_NOFNO}/home" \
  TARGET_START=1 \
  TARGET_INPUT="review-gate-probe" \
  bash "$INIT") > "${TMP_NOFNO}/out.log" 2> "${TMP_NOFNO}/err.log"
_RC=$?
[[ "$_RC" -eq 0 ]] || fail "AC5-ERR: absent fno blocked bootstrap (exit $_RC; err: $(cat "$TMP_NOFNO/err.log"))"
pass "AC5-ERR: script exited 0"

# A free-text TARGET_INPUT (no node-id, no plan path) has no existing hold to
# check, so an absent `fno` degrades to a note-and-proceed rather than a
# refusal - the refusal path is exercised separately for a named node/plan.
grep -q "fno absent - no existing node or plan to hold; proceeding with free-text init" \
  "$TMP_NOFNO/err.log" \
  || fail "AC5-ERR: the degrade was silent (got: $(cat "$TMP_NOFNO/err.log"))"
pass "AC5-ERR: absent-fno note printed"

[[ -f "$TMP_NOFNO/.fno/target-state.md" ]] \
  || fail "AC5-ERR: manifest not written"
pass "AC5-ERR: manifest written"

# ── AC3-CON: the wrapper already paid; do not probe GitHub twice ──
log "AC3-CON: FNO_TARGET_INIT_GATED=1 => verb never invoked"
make_repo TMP_MARKED 9
_ALL_TMPS+=("$TMP_MARKED")

run_init "$TMP_MARKED" FNO_TARGET_INIT_GATED=1
_RC=$?
[[ "$_RC" -eq 0 ]] \
  || fail "AC3-CON: marked run refused anyway (exit $_RC) - the marker did not suppress the gate"
pass "AC3-CON: marked run proceeded"

[[ "$(gate_calls "$TMP_MARKED")" == "0" ]] \
  || fail "AC3-CON: gate invoked $(gate_calls "$TMP_MARKED")x despite the marker - GitHub probed twice per init"
pass "AC3-CON: gate never invoked under the marker"

# A marker with any OTHER value must not suppress the gate; only "1" counts.
make_repo TMP_BOGUS 9
_ALL_TMPS+=("$TMP_BOGUS")
run_init "$TMP_BOGUS" FNO_TARGET_INIT_GATED=0
_RC=$?
[[ "$_RC" -eq 2 ]] \
  || fail "AC3-CON: FNO_TARGET_INIT_GATED=0 suppressed the gate (exit $_RC) - a falsy value must not clear it"
pass "AC3-CON: only the literal 1 suppresses the gate"

log "all scenarios passed"
exit 0
