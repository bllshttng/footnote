#!/usr/bin/env bash
# test_init_join_auto.sh -- the join: auto trigger in
# init-target-state.sh fires join, or parks with the width in the message.
#
# `join: auto` is the plan's opt-in for init-driven join. The
# trigger's decision matrix is the contract:
#
#   key absent/manual        -> nothing, no probes at all
#   auto + armed + width >= 2  -> `fno backlog join <node>` exactly once
#   auto + not armed          -> PARKED, message names width
#   auto + width 1            -> "nothing to join", no spawn attempted
#   width unmeasurable          -> join NOT fired (an absent answer is
#                                  never width 1)
#   join refused (rc 5 etc.)    -> init survives, refusal named
#
# Both probes (width, armed) are faked via a stub FNO_PYTHON interpreter, so
# the scenarios never read the host's real settings and never spawn real
# workers. The stub records `backlog join` argv so the assertions pin the
# node id, not just that some spawn happened.
#
# Exit codes: 0 all scenarios passed, 1 assertion failed,
# 77 skipped (missing dependencies).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
INIT="${REPO_ROOT}/hooks/helpers/init-target-state.sh"

log()  { printf '[join-auto] %s\n' "$*"; }
fail() { printf '[join-auto] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[join-auto] PASS: %s\n' "$*"; }
skip() { printf '[join-auto] SKIP: %s\n' "$*" >&2; exit 77; }

command -v git &>/dev/null || skip "git not on PATH"
[[ -f "$INIT" ]] || fail "init script not found at $INIT"
bash -n "$INIT" || fail "bash -n rejected $INIT (syntax error)"
pass "init script passes bash -n"

_ALL_TMPS=()
trap 'rm -rf ${_ALL_TMPS[@]+"${_ALL_TMPS[@]}"}' EXIT

_PLAN_BODY='---
title: join auto fixture
status: ready
created: 2026-09-02
difficulty: medium
node: x-d4ab
ORCH_LINE
---

# Join auto fixture

## Execution Strategy

```yaml
execution_mode: mixed
waves:
  - wave: 1
    mode: sequential
    name: One
    difficulty: medium
    tasks: ['1.1', '1.2']
tasks:
  - id: '1.1'
    title: A
    surface: [a.py]
    verify: pytest -q
    acceptance: []
  - id: '1.2'
    title: B
    surface: [b.py]
    verify: pytest -q
    acceptance: []
```
'

# make_repo <varname> <orch-line> <fake-width> <fake-armed> <join-rc>
#   orch-line: the frontmatter line for join, or "" for absent
#   fake-width / fake-armed: what the stub interpreter answers; "" = probe fails
make_repo() {
  local _varname="$1" _orch="$2" _w="$3" _a="$4" _jrc="$5" _dir
  _dir="$(mktemp -d -t init-mech-join.XXXXXX)" || fail "mktemp failed"
  eval "${_varname}=\"\${_dir}\""
  (cd "$_dir" && git init -q && mkdir -p .fno home/.fno bin) \
    || fail "repo setup failed in $_dir"
  printf '# isolated\n' > "${_dir}/.fno/config.toml"
  printf '# isolated\n' > "${_dir}/home/.fno/config.toml"
  : > "${_dir}/join-calls.log"

  printf '%s' "${_PLAN_BODY/ORCH_LINE/$_orch}" > "${_dir}/plan.md"

  cat > "${_dir}/bin/fno" << STUB
#!/usr/bin/env bash
if [[ "\$1" == "backlog" && "\$2" == "join" ]]; then
  echo "\$*" >> "${_dir}/join-calls.log"
  exit ${_jrc}
fi
exit 0
STUB
  chmod +x "${_dir}/bin/fno"

  # Stub interpreter: init resolves FNO_PYTHON itself and would find the real
  # checkout venv, so the test pins it first and the probes answer from here.
  # argv: -m fno.backlog.join_trigger <width|armed> <arg>
  cat > "${_dir}/bin/fakepython" << STUB
#!/usr/bin/env bash
case "\$3" in
  width)
    if [[ -n "${_w}" ]]; then printf '%s\n' "${_w}"; exit 0; fi
    exit 1 ;;
  armed)
    if [[ -n "${_a}" ]]; then printf '%s\n' "${_a}"; exit 0; fi
    exit 1 ;;
esac
exit 2
STUB
  chmod +x "${_dir}/bin/fakepython"
}

run_init() {
  local _dir="$1"; shift
  (cd "$_dir" && env \
    "${_SCRUB_ENV[@]}" \
    PATH="${_dir}/bin:${PATH}" \
    HOME="${_dir}/home" \
    FNO_PYTHON="${_dir}/bin/fakepython" \
    TARGET_START=1 \
    TARGET_INPUT="x-d4ab" \
    TARGET_PLAN_PATH="${_dir}/plan.md" \
    TARGET_SESSION_ID="mech-join-worker" \
    "$@" \
    bash "$INIT") > "${_dir}/out.log" 2> "${_dir}/err.log"
}

joins() { wc -l < "$1/join-calls.log" | tr -d ' '; }

_SCRUB_ENV=(-u CLAUDE_CODE_SESSION_ID -u CLAUDECODE_SESSION_ID -u CODEX_THREAD_ID
            -u CODEX_SESSION_ID -u GEMINI_SESSION_ID -u OPENCODE_SESSION_ID
            -u TARGET_TRANSCRIPT_ID -u TARGET_SESSION_ID -u FNO_AUTO_CONTINUE)

# ── Key absent (or manual): the operator-reviewed path, untouched ──
log "manual: no join key => no probes, no join"
make_repo TMP_MANUAL "" 3 "armed=true rank=config" 0
_ALL_TMPS+=("$TMP_MANUAL")

run_init "$TMP_MANUAL"
_RC=$?
[[ "$_RC" -eq 0 ]] || fail "manual: expected exit 0, got $_RC (err: $(tail -3 "$TMP_MANUAL/err.log"))"
[[ "$(joins "$TMP_MANUAL")" == "0" ]] || fail "manual: join fired without the key"
grep -q "join auto" "$TMP_MANUAL/err.log" \
  && fail "manual: join-auto chatter leaked into a manual plan"
pass "manual: silent, nothing fired"

# ── AC7: armed + width 3 => join once for the remainder ──
log "auto armed: join fires once, receipt names width"
make_repo TMP_ARMED "join: auto" 3 "armed=true rank=config" 0
_ALL_TMPS+=("$TMP_ARMED")

run_init "$TMP_ARMED"
_RC=$?
[[ "$_RC" -eq 0 ]] || fail "armed: expected exit 0, got $_RC (err: $(tail -3 "$TMP_ARMED/err.log"))"
[[ "$(joins "$TMP_ARMED")" == "1" ]] || fail "armed: expected exactly one join, got $(joins "$TMP_ARMED")"
grep -q "firing join for the remainder (width 3)" "$TMP_ARMED/err.log" \
  || fail "armed: the fire line with the measured width is missing"
grep -q "x-d4ab" "$TMP_ARMED/join-calls.log" \
  || fail "armed: join called without the node id"
pass "armed: join fired once for node x-d4ab at width 3"

# ── AC8: not armed => parked, reason AND width named ──
log "auto disarmed: parked with reason and width"
make_repo TMP_PARK "join: auto" 3 "armed=false rank=config" 0
_ALL_TMPS+=("$TMP_PARK")

run_init "$TMP_PARK"
[[ "$?" -eq 0 ]] || fail "parked: init must survive the park"
[[ "$(joins "$TMP_PARK")" == "0" ]] || fail "parked: join fired while disarmed"
grep -q "PARKED, auto_continue off (width 3)" "$TMP_PARK/err.log" \
  || fail "parked: the park line must name the reason and the measured width"
pass "parked: reason and width both named, nothing spawned"

# ── Width 1: nothing to join, no spawn attempted ──
log "auto armed narrow: nothing to join"
make_repo TMP_NARROW "join: auto" 1 "armed=true rank=config" 0
_ALL_TMPS+=("$TMP_NARROW")

run_init "$TMP_NARROW"
[[ "$?" -eq 0 ]] || fail "narrow: init must survive a width-1 plan"
[[ "$(joins "$TMP_NARROW")" == "0" ]] || fail "narrow: join attempted at width 1"
grep -q "nothing to join (width 1)" "$TMP_NARROW/err.log" \
  || fail "narrow: the narrow line is missing"
pass "narrow: named, no spawn"

# ── Unmeasurable width: join NOT fired ──
log "auto unreadable width: join not fired"
make_repo TMP_BLIND "join: auto" "" "armed=true rank=config" 0
_ALL_TMPS+=("$TMP_BLIND")

run_init "$TMP_BLIND"
[[ "$?" -eq 0 ]] || fail "blind: init must survive an unreadable width"
[[ "$(joins "$TMP_BLIND")" == "0" ]] || fail "blind: join fired on an unmeasured width"
grep -q "width unreadable; join not fired" "$TMP_BLIND/err.log" \
  || fail "blind: the unreadable line is missing"
pass "blind: absent answer never read as width 1"

# ── Garbage width: join NOT fired (the probe contract is int-or-nothing,
#    and the shell holds that invariant even when the probe breaks it) ──
log "auto garbage width: join not fired"
make_repo TMP_GARBAGE "join: auto" "banana" "armed=true rank=config" 0
_ALL_TMPS+=("$TMP_GARBAGE")

run_init "$TMP_GARBAGE"
[[ "$?" -eq 0 ]] || fail "garbage: init must survive a garbage width"
[[ "$(joins "$TMP_GARBAGE")" == "0" ]] || fail "garbage: join fired on a garbage width"
grep -q "width unreadable; join not fired" "$TMP_GARBAGE/err.log" \
  || fail "garbage: the unreadable line is missing"
pass "garbage: non-numeric output never read as a width"

# ── Join refused: init survives and names it ──
log "auto join refused: non-fatal, named"
make_repo TMP_REFUSED "join: auto" 3 "armed=true rank=config" 5
_ALL_TMPS+=("$TMP_REFUSED")

run_init "$TMP_REFUSED"
[[ "$?" -eq 0 ]] || fail "refused: a join refusal must not fail init"
grep -q "join refused, non-fatal" "$TMP_REFUSED/err.log" \
  || fail "refused: the refusal line is missing"
pass "refused: init survived, refusal named"

log "all scenarios passed"
exit 0
