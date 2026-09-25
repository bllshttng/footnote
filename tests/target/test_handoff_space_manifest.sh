#!/usr/bin/env bash
# handoff.sh resolved the session manifest cwd-relative
# ($FNO_DIR/target-state.md), but init writes it through fno.paths to the
# project space (<state>/spaces/<slug>/worktrees/<name>/target-state.md), so
# every self-handoff from a space-keyed worktree parked with
# "manifest .fno/target-state.md not found" before reading a thing.
#
# Strategy: run the real script in a sandbox with FNO_DIR unset and a pinned
# FNO_AGENTS_BIN stub answering `state path` with the space paths. A park on
# "plan file not found" naming the plan ONLY the space manifest knows proves
# the manifest was found and parsed; the old failure was parking before that.
# Also covers the legacy belt (a manifest an older init left in the checkout)
# and the explicit-FNO_DIR test override.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HANDOFF="${REPO_ROOT}/skills/target/scripts/handoff.sh"

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); printf '[handoff-space-manifest] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[handoff-space-manifest] FAIL: %s\n' "$*" >&2; }

[[ -f "$HANDOFF" ]] || { fail "handoff.sh not found at $HANDOFF"; exit 1; }
command -v jq >/dev/null 2>&1 || { printf '[handoff-space-manifest] SKIP: jq not on PATH\n' >&2; exit 77; }

TMP_DIR="$(mktemp -d)"
# Preserve the real exit status: pkill returning 1 (no straggler) must not
# fail the run, and set -e must not fire inside the trap.
trap 'rc=$?; set +e; pkill -9 -f "${TMP_DIR:-/nonexistent}" 2>/dev/null; sleep 0.5; rm -rf "${TMP_DIR:-/nonexistent}"; exit $rc' EXIT

NODE_ID="x-ebe8"

# make_manifest <file> <plan-path>
make_manifest() {
  cat > "$1" <<MAN
---
session_id: sess-space-1
plan_path: $2
target_size: S
auto_merge_approved: false
---
graph_node_id: ${NODE_ID}
target_claim_key: node:${NODE_ID}
MAN
}

# run_handoff <cwd> [env assignments...]; sets OUT and RC
run_handoff() {
  local dir="$1"; shift
  set +e
  OUT="$(cd "$dir" && env -u FNO_DIR -u FNO_AGENTS_BIN \
    HOME="${TMP_DIR}/home" \
    HANDOFF_VERIFY_TIMEOUT=5 HANDOFF_VERIFY_INTERVAL=1 \
    PATH="${BIN_DIR}:$PATH" \
    timeout 60 bash "$HANDOFF" --harness claude --model opus "$@" 2>&1)"
  RC=$?
  set -e
}

assert_reached_manifest() {
  # $1 = the plan path ONLY the expected manifest names
  if [ "$RC" -ne 10 ]; then
    fail "parked with rc=$RC (want 10): $(printf '%s' "${OUT:-}" | tail -2 | tr '\n' ' ')"
    return 1
  fi
  if printf '%s' "$OUT" | grep -q "manifest.*not found"; then
    fail "parked on the missing manifest before reading it"
    return 1
  fi
  if ! printf '%s' "$OUT" | grep -q "plan file not found: $(printf '%s' "$1" | sed 's/[][\\.*^$]/\\&/g')"; then
    fail "did not read the expected manifest (want plan $1): $(printf '%s' "${OUT:-}" | tail -2 | tr '\n' ' ')"
    return 1
  fi
  return 0
}

set -e

# A hermetic `fno` so the dependency guard never reaches a real install; the
# state-path verb rides the pinned fno-agents stub instead.
BIN_DIR="${TMP_DIR}/bin"; mkdir -p "$BIN_DIR"
printf '#!/usr/bin/env bash\nexit 0\n' > "$BIN_DIR/fno"
chmod +x "$BIN_DIR/fno"

# write_agents_stub <state-path> <events-path>: the stub answers `state path`
# with the case's space paths, so each case bakes its own.
write_agents_stub() {
  printf '#!/usr/bin/env bash\ncase "$3" in\n  target-state) printf "%%s\\n" "%s" ;;\n  events) printf "%%s\\n" "%s" ;;\n  *) exit 1 ;;\nesac\n' \
    "$1" "$2" > "$BIN_DIR/fno-agents"
  chmod +x "$BIN_DIR/fno-agents"
}

# ---------------------------------------------------------------------------
# 1. Manifest sits ONLY under the space path (the observed failure shape)
# ---------------------------------------------------------------------------
C1="${TMP_DIR}/c1"; WT1="${C1}/wt"
SPACE_STATE1="${C1}/space/worktrees/wt/target-state.md"
mkdir -p "$(dirname "$SPACE_STATE1")" "$WT1"
make_manifest "$SPACE_STATE1" "${WT1}/space-plan.md"

write_agents_stub "$SPACE_STATE1" "${C1}/space/events.jsonl"
run_handoff "$WT1" FNO_AGENTS_BIN="$BIN_DIR/fno-agents"
if assert_reached_manifest "${WT1}/space-plan.md"; then
  pass "space-only manifest found and parsed"
fi

# ---------------------------------------------------------------------------
# 2. Legacy belt: the resolver names a space path that does not exist, and an
#    older init left the manifest in the checkout
# ---------------------------------------------------------------------------
C2="${TMP_DIR}/c2"; WT2="${C2}/wt"
SPACE_STATE2="${C2}/space/worktrees/wt/target-state.md"   # never created
mkdir -p "${WT2}/.fno"
make_manifest "${WT2}/.fno/target-state.md" "${WT2}/legacy-plan.md"

write_agents_stub "$SPACE_STATE2" "${C2}/space/events.jsonl"
run_handoff "$WT2" FNO_AGENTS_BIN="$BIN_DIR/fno-agents"
if assert_reached_manifest "${WT2}/legacy-plan.md"; then
  pass "legacy checkout manifest claimed through the belt"
fi

# ---------------------------------------------------------------------------
# 3. Explicit FNO_DIR keeps the cwd-relative layout, no resolver consulted
# ---------------------------------------------------------------------------
C3="${TMP_DIR}/c3"; WT3="${C3}/wt"
mkdir -p "${WT3}/.fno"
make_manifest "${WT3}/.fno/target-state.md" "${WT3}/override-plan.md"
mv "$BIN_DIR/fno-agents" "${BIN_DIR}/fno-agents.bak"

run_handoff "$WT3" FNO_DIR=".fno"
if assert_reached_manifest "${WT3}/override-plan.md"; then
  pass "explicit FNO_DIR override still wins"
fi
mv "${BIN_DIR}/fno-agents.bak" "$BIN_DIR/fno-agents"

# ---------------------------------------------------------------------------
printf '\n[handoff-space-manifest] %d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
