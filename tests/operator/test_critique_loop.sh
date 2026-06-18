#!/usr/bin/env bash
# test_critique_loop.sh - convergence + ceiling + parser-fallback behavior.
#
# Acceptance criteria covered:
#   AC1.3-HP   threshold from settings is honored
#   AC1.3-EDGE max-iter ceiling fires with FAILED reason=max_iterations_reached
#   AC2.5-HP   converges at threshold; FINAL_SCORE recorded
#   AC2.5-EDGE max-iter bail recorded with last score
#   parser fallbacks: unparseable score, unparseable next-subcommand
#
# Drives skills/do/scripts/run-critique-loop.sh against the
# _impeccable_stub.sh fake. The shell script is the testable port of the
# frontend-executor agent's inner loop; the contract is shared.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOOP_SCRIPT="$REPO_ROOT/skills/do/scripts/run-critique-loop.sh"
STUB="$REPO_ROOT/tests/operator/_impeccable_stub.sh"

PASS=0
FAIL=0

assert_contains() {
    local label="$1" needle="$2" haystack="$3"
    if printf '%s\n' "$haystack" | grep -qF "$needle"; then
        echo "  PASS: $label"
        PASS=$(( PASS + 1 ))
    else
        echo "  FAIL: $label (missing '$needle')"
        echo "  ----- output -----"
        printf '%s\n' "$haystack" | sed 's/^/    /'
        echo "  ------------------"
        FAIL=$(( FAIL + 1 ))
    fi
}

assert_not_contains() {
    local label="$1" needle="$2" haystack="$3"
    if printf '%s\n' "$haystack" | grep -qF "$needle"; then
        echo "  FAIL: $label (unexpected '$needle')"
        FAIL=$(( FAIL + 1 ))
    else
        echo "  PASS: $label"
        PASS=$(( PASS + 1 ))
    fi
}

run_loop() {
    local sequence="$1" threshold="$2" max_iter="$3"
    local extra_env="${4:-}"
    local log
    log=$(mktemp)
    local out
    out=$(env STUB_SCORE_SEQUENCE="$sequence" \
              STUB_INVOCATION_LOG="$log" \
              CRITIQUE_THRESHOLD="$threshold" \
              MAX_ITER="$max_iter" \
              IMPECCABLE_CMD="bash $STUB" \
              $extra_env \
              bash "$LOOP_SCRIPT" 2>&1)
    rm -f "$log"
    printf '%s\n' "$out"
}

echo "AC2.5-HP: converges below threshold (30, 33, 38; threshold 35) -> SUCCESS at iter 3"
out=$(run_loop "30 33 38" 35 8)
assert_contains "RESULT line"          "RESULT: SUCCESS" "$out"
assert_contains "ITERATIONS=3"         "ITERATIONS: 3"   "$out"
assert_contains "FINAL_SCORE=38/40"    "FINAL_SCORE: 38/40" "$out"

echo ""
echo "AC2.5-EDGE: max-iter ceiling fires (30, 30, 30, 30; max=3) -> FAILED reason=max_iterations_reached"
out=$(run_loop "30 30 30 30" 35 3)
assert_contains "RESULT line"          "RESULT: FAILED" "$out"
assert_contains "ITERATIONS=3"         "ITERATIONS: 3"  "$out"
assert_contains "FINAL_SCORE=30/40"    "FINAL_SCORE: 30/40" "$out"
assert_contains "ERROR contains reason" "max_iterations_reached" "$out"

echo ""
echo "AC1.3-HP: threshold tunes (score 36, threshold 38 -> continues; threshold 35 -> SUCCESS)"
out=$(run_loop "36" 38 2)
assert_contains "threshold=38: FAILED" "RESULT: FAILED" "$out"
out=$(run_loop "36" 35 2)
assert_contains "threshold=35: SUCCESS" "RESULT: SUCCESS" "$out"
assert_contains "threshold=35: iter 1"  "ITERATIONS: 1"  "$out"

echo ""
echo "Score parser fallback: unparseable score treated as 0; max-iter eventually fires"
# Override stub to return no parseable score line.
TMPSTUB=$(mktemp)
cat > "$TMPSTUB" <<'EOF'
#!/usr/bin/env bash
# Stub that emits no parseable score - exercises parser fallback to 0.
case "${1:-craft}" in
    critique) echo "this output has no score field"
              echo "next-subcommand: craft" ;;
    *)        echo "ok" ;;
esac
EOF
chmod +x "$TMPSTUB"
out=$(IMPECCABLE_CMD="bash $TMPSTUB" CRITIQUE_THRESHOLD=35 MAX_ITER=2 bash "$LOOP_SCRIPT" 2>&1)
rm -f "$TMPSTUB"
assert_contains "WARN logged"          "WARN: critique score unparseable" "$out"
assert_contains "FAILED outcome"       "RESULT: FAILED" "$out"
assert_contains "FINAL_SCORE=0/40"     "FINAL_SCORE: 0/40" "$out"

echo ""
echo "Next-subcommand parser fallback: unparseable -> defaults to 'craft'"
TMPSTUB=$(mktemp)
cat > "$TMPSTUB" <<'EOF'
#!/usr/bin/env bash
# Stub returns a parseable score but no next-subcommand line.
case "${1:-craft}" in
    critique) echo "score: 30/40"
              echo "no next subcommand line here" ;;
    *)        echo "ok" ;;
esac
EOF
chmod +x "$TMPSTUB"
out=$(IMPECCABLE_CMD="bash $TMPSTUB" CRITIQUE_THRESHOLD=35 MAX_ITER=2 bash "$LOOP_SCRIPT" 2>&1)
rm -f "$TMPSTUB"
assert_contains "WARN logged for next" "WARN: next subcommand unparseable" "$out"
# Subcommands run should still be just 'craft' (the default fallback).
assert_contains "subcommands_run=craft" "SUBCOMMANDS_RUN: [craft,craft]" "$out"

echo ""
echo "Critique non-zero exit bails FAILED with diagnostic (not silent)"
TMPSTUB=$(mktemp)
cat > "$TMPSTUB" <<'EOF'
#!/usr/bin/env bash
# Stub where 'critique' crashes with a non-zero exit code.
case "${1:-craft}" in
    critique) echo "boom: thing not configured" >&2 ; exit 7 ;;
    *)        echo "ok" ;;
esac
EOF
chmod +x "$TMPSTUB"
out=$(IMPECCABLE_CMD="bash $TMPSTUB" CRITIQUE_THRESHOLD=35 MAX_ITER=8 bash "$LOOP_SCRIPT" 2>&1)
rm -f "$TMPSTUB"
assert_contains "FAILED on critique nonzero"  "RESULT: FAILED" "$out"
assert_contains "ITERATIONS=1 (no burn)"      "ITERATIONS: 1"  "$out"
assert_contains "rc=7 surfaces in ERROR"       "rc=7"          "$out"
assert_contains "stderr surfaces in ERROR"     "boom"          "$out"

echo ""
echo "Subcommand non-zero bails with stderr in diagnostic"
TMPSTUB=$(mktemp)
cat > "$TMPSTUB" <<'EOF'
#!/usr/bin/env bash
# Stub where 'craft' crashes with a non-zero exit code.
case "${1:-craft}" in
    critique) echo "score: 30/40" ;;
    *)        echo "missing dependency: foobar" >&2 ; exit 13 ;;
esac
EOF
chmod +x "$TMPSTUB"
out=$(IMPECCABLE_CMD="bash $TMPSTUB" CRITIQUE_THRESHOLD=35 MAX_ITER=8 bash "$LOOP_SCRIPT" 2>&1)
rm -f "$TMPSTUB"
assert_contains "FAILED on subcommand nonzero" "RESULT: FAILED" "$out"
assert_contains "rc=13 surfaces"                "rc=13"          "$out"
assert_contains "stderr surfaces"               "missing dependency" "$out"

echo ""
echo "Agent/shim contract: regex strings match between markdown and shell"
AGENT_DOC="$REPO_ROOT/agents/frontend-executor.md"
if [[ -f "$AGENT_DOC" ]]; then
    # The score regex is the load-bearing parser. Both files must reference
    # the same /40 denominator and case-insensitive `score:` form. A change
    # in one without the other silently breaks production.
    if grep -q 'score:' "$AGENT_DOC" && grep -q '/40' "$AGENT_DOC"; then
        echo "  PASS: agent doc references score:NN/40 form"
        PASS=$(( PASS + 1 ))
    else
        echo "  FAIL: agent doc missing score regex anchor"
        FAIL=$(( FAIL + 1 ))
    fi
    # Settings keys
    for key in critique_threshold max_iterations_per_task; do
        if grep -q "$key" "$AGENT_DOC"; then
            echo "  PASS: agent doc references settings key $key"
            PASS=$(( PASS + 1 ))
        else
            echo "  FAIL: agent doc missing settings key $key"
            FAIL=$(( FAIL + 1 ))
        fi
    done
    # Return contract field names match what the shim emits on SUCCESS.
    for field in RESULT ITERATIONS FINAL_SCORE SUBCOMMANDS_RUN; do
        if grep -q "$field" "$AGENT_DOC"; then
            echo "  PASS: agent doc declares $field field"
            PASS=$(( PASS + 1 ))
        else
            echo "  FAIL: agent doc missing $field field"
            FAIL=$(( FAIL + 1 ))
        fi
    done
fi

echo ""
echo "CLAUDE.md verification (AC2.2-HP)"
CLAUDE_MD="$REPO_ROOT/CLAUDE.md"
if [[ -f "$CLAUDE_MD" ]]; then
    if grep -q 'Per-task executors' "$CLAUDE_MD"; then
        echo "  PASS: CLAUDE.md has 'Per-task executors' subsection"
        PASS=$(( PASS + 1 ))
    else
        echo "  FAIL: CLAUDE.md missing 'Per-task executors' subsection"
        FAIL=$(( FAIL + 1 ))
    fi
    if grep -q 'frontend-executor' "$CLAUDE_MD"; then
        echo "  PASS: CLAUDE.md mentions frontend-executor"
        PASS=$(( PASS + 1 ))
    else
        echo "  FAIL: CLAUDE.md missing frontend-executor mention"
        FAIL=$(( FAIL + 1 ))
    fi
fi

echo ""
echo "Settings drift assertion: example file matches in-code defaults"
SETTINGS_FILE="$REPO_ROOT/.fno/settings.yaml.example"
if [[ -f "$SETTINGS_FILE" ]]; then
    # Anchor the regex to lines that ARE the key, not lines that mention it
    # (avoids matching a future sibling like critique_threshold_override:).
    threshold_in_settings=$(awk '/^[[:space:]]+critique_threshold:[[:space:]]/ {gsub(/[^0-9]/,""); print; exit}' "$SETTINGS_FILE")
    max_iter_in_settings=$(awk '/^[[:space:]]+max_iterations_per_task:[[:space:]]/ {gsub(/[^0-9]/,""); print; exit}' "$SETTINGS_FILE")
    assert_contains "threshold matches default 35" "35" "$threshold_in_settings"
    assert_contains "max_iter matches default 8"   "8"  "$max_iter_in_settings"
    # AC2.1-HP: all three keys must be present
    if grep -q '^[[:space:]]\+auto_route_frontend:' "$SETTINGS_FILE"; then
        echo "  PASS: auto_route_frontend key present"
        PASS=$(( PASS + 1 ))
    else
        echo "  FAIL: auto_route_frontend key missing"
        FAIL=$(( FAIL + 1 ))
    fi
else
    echo "  SKIP: $SETTINGS_FILE not present"
fi

echo ""
echo "==="
echo "test_critique_loop: ${PASS} passed, ${FAIL} failed"
[[ $FAIL -eq 0 ]] || exit 1
