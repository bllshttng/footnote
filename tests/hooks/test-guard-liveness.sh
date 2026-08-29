#!/usr/bin/env bash
# test-guard-liveness.sh - positive liveness markers for PreToolUse guards.
#
# A guard that cannot prove it ran is indistinguishable from a guard that
# never launched. Every instrumented guard must emit exactly one
# guard_decision event row per invocation, naming the guard and its
# decision, into the pinned events file (FNO_EVENTS_PATH). Exactly-one, not
# at-least-one: double emission means both decision exits fired. The
# control case proves the zero state is real - no invocation, no row. For
# join-partition-write-guard the deny half needs the uv-hosted helper and a
# joined partition, so it is exercised on its allow path only; the marker
# contract (one row per run) is what this file pins.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HOOKS="${REPO_ROOT}/hooks"

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); printf '[liveness] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[liveness] FAIL: %s\n' "$*" >&2; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
EVENTS="$TMP/events.jsonl"
export FNO_EVENTS_PATH="$EVENTS"

count_for() { # <guard> <decision> -> row count in the pinned events file
    local n
    n=$(grep -c "\"guard\":\"$1\",\"decision\":\"$2\"" "$EVENTS" 2>/dev/null || true)
    printf '%s' "${n:-0}"
}

decision_of() { # guard stdout -> "allow" | "block" | "deny" | "MISSING"
    # Bash guards answer {"decision":"block",...}; python guards answer
    # {"hookSpecificOutput":{"permissionDecision":"deny",...}}. Match the
    # block field first: a bash block row also contains the word deny.
    local out="$1"
    if [[ -z "$out" || "$out" == "{}" ]]; then echo allow
    elif printf '%s' "$out" | grep -q '"decision":[[:space:]]*"block"'; then echo block
    elif printf '%s' "$out" | grep -q '"permissionDecision":[[:space:]]*"deny"'; then echo deny
    else echo MISSING; fi
}

# expect_row NAME GUARD_NAME WANT_DECISION PAYLOAD [INTERPRETER] [GUARD_REL]
#   Runs the guard once, asserts the decision, and asserts EXACTLY ONE new
#   row for that guard+decision appeared in the events file.
expect_row() {
    local name="$1" guard_name="$2" want="$3" payload="$4"
    local interp="${5:-bash}" guard_rel="${6:-}"
    local guard="${HOOKS}/${guard_rel:-$guard_name.sh}"
    local out before after got
    before=$(count_for "$guard_name" "$want")
    out=$(printf '%s' "$payload" | "$interp" "$guard" 2>/dev/null)
    got=$(decision_of "$out")
    if [[ "$got" != "$want" ]]; then
        fail "$name: want decision $want, got $got (${out:-<empty>})"
        return
    fi
    after=$(count_for "$guard_name" "$want")
    if [[ $((after - before)) -ne 1 ]]; then
        fail "$name: expected exactly 1 new '$want' row for $guard_name, delta $((after - before)) (events: $(cat "$EVENTS" 2>/dev/null || printf none))"
        return
    fi
    pass "$name"
}

echo "=== PreToolUse guard liveness markers ==="

# ── control: no invocation, no row ────────────────────────────────────────────
if [[ -e "$EVENTS" ]]; then
    fail "control: events file must not exist before any guard runs"
else
    pass "control: zero guard_decision rows without a run"
fi

# ── graph-write-protect (Edit|Write|Bash) ─────────────────────────────────────
expect_row "gwp allows an ordinary edit" graph-write-protect allow \
    '{"tool_name":"Edit","tool_input":{"file_path":"'"$TMP"'/src/notes.txt","new_string":"x"}}'
expect_row "gwp blocks a graph.json write" graph-write-protect block \
    '{"tool_name":"Write","tool_input":{"file_path":"'"$TMP"'/src/.fno/graph.json","content":"{}"}}'

# ── worktree-write-protect (Edit|Write) ───────────────────────────────────────
# Block needs a real git repo on its protected branch (same technique as
# test_worktree_write_protect.sh).
WWT_REPO="$TMP/canonical"
mkdir -p "$WWT_REPO"
git -C "$WWT_REPO" init -q -b main
git -C "$WWT_REPO" config user.email test@example.com
git -C "$WWT_REPO" config user.name Test
printf '# fixture\n' > "$WWT_REPO/README.md"
git -C "$WWT_REPO" add README.md
git -C "$WWT_REPO" commit -q -m init

expect_row "wwp allows a worktree edit" worktree-write-protect allow \
    '{"cwd":"'"$TMP"'/elsewhere","tool_name":"apply_patch","tool_input":{"command":"*** Begin Patch\n*** Update File: README.md\n*** End Patch"}}'
expect_row "wwp blocks a canonical-checkout edit" worktree-write-protect block \
    '{"cwd":"'"$WWT_REPO"'","tool_name":"apply_patch","tool_input":{"command":"*** Begin Patch\n*** Update File: README.md\n*** End Patch"}}'

# ── join-partition-write-guard (Edit|Write, allow path only) ──────────────────
expect_row "jpw allows a non-joined write" join-partition-write-guard allow \
    '{"cwd":"'"$TMP"'/elsewhere","tool_name":"Write","tool_input":{"file_path":"'"$TMP"'/elsewhere/x.py","content":"x"}}'

# ── plan-location-guard (Write) ───────────────────────────────────────────────
# Block needs the plans-dir resolver, fed by a fake fno on PATH that echoes
# an ABSOLUTE path under a real plans dir (same technique as
# test_plan_location_guard.sh; a relative echo fails resolution and the
# guard fail-opens to allow).
FAKEBIN="$TMP/bin"; PLANS="$TMP/vault/fno/plans"
mkdir -p "$FAKEBIN" "$PLANS" "$TMP/repo/docs"
printf '#!/usr/bin/env bash\n[[ "$1" == "do" ]] || exit 1\nshift\nif [[ "$1" == "plan" && "$2" == "path" ]]; then\n  case " $* " in\n    *" --slug "*) ;;\n    *) exit 2 ;;\n  esac\n  echo "%s/20260829-x.md"\n  exit 0\nfi\nexit 1\n' "$PLANS" > "$FAKEBIN/fno"
chmod +x "$FAKEBIN/fno"

PLAN_FM='---\nnode: x-0000\nslug: some-plan\ntype: feature\nstatus: ready\n---\n\n# Plan\n'
PATH_SAVE="$PATH"

PATH="$FAKEBIN:$PATH"
expect_row "plg blocks a plan saved outside the plans dir" plan-location-guard block \
    '{"tool_name":"Write","tool_input":{"file_path":"'"$TMP"'/repo/docs/my-plan.md","content":"'"$PLAN_FM"'"}}'
PATH="$PATH_SAVE"
expect_row "plg allows a non-plan write" plan-location-guard allow \
    '{"tool_name":"Write","tool_input":{"file_path":"'"$TMP"'/repo/docs/notes.txt","content":"x"}}'

# ── git-protection.py (Bash) ──────────────────────────────────────────────────
expect_row "gp allows a benign command" git-protection allow \
    '{"tool_name":"Bash","tool_input":{"command":"echo hello"}}' python3 "git-protection.py"
expect_row "gp denies a raw gh pr checks read" git-protection deny \
    '{"tool_name":"Bash","tool_input":{"command":"gh pr checks 12"}}' python3 "git-protection.py"

# ── bg-process-guard.py (Bash) ────────────────────────────────────────────────
expect_row "bpg allows a bounded command" bg-process-guard allow \
    '{"tool_name":"Bash","tool_input":{"command":"head -c 1M /dev/zero > /dev/null"}}' python3 "bg-process-guard.py"
expect_row "bpg denies an unbounded yes" bg-process-guard deny \
    '{"tool_name":"Bash","tool_input":{"command":"yes"}}' python3 "bg-process-guard.py"

# ── law-authority-gate.py (Bash) ──────────────────────────────────────────────
LAW_HASH="$(printf 'a%.0s' $(seq 1 64))"
expect_row "lag allows an unrelated command" law-authority-gate allow \
    '{"tool_name":"Bash","tool_input":{"command":"echo hello"}}' python3 "law-authority-gate.py"
expect_row "lag denies an unattended law enact" law-authority-gate deny \
    '{"tool_name":"Bash","tool_input":{"command":"fno inbox law enact lp-abcdef012345 --content-hash '"$LAW_HASH"'"}}' python3 "law-authority-gate.py"

# ── row shape: one deep check that a row is valid JSON with the contract ──────
SHAPE_OK=0
if [[ -s "$EVENTS" ]] && command -v jq >/dev/null 2>&1; then
    if jq -e 'select(.type == "guard_decision" and .source == "hook"
                 and (.data.guard | type == "string")
                 and (.data.decision == "allow" or .data.decision == "block" or .data.decision == "deny")
                 and (.ts | type == "string"))' "$EVENTS" >/dev/null 2>&1; then
        SHAPE_OK=1
    fi
fi
if [[ "$SHAPE_OK" == "1" ]]; then pass "rows carry the guard_decision contract"
else fail "no row satisfies the guard_decision shape contract in $EVENTS"; fi

echo "=== liveness: $PASS passed, $FAIL failed ==="
[[ "$FAIL" -eq 0 ]]
