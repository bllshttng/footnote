#!/usr/bin/env bash
# Tests for scripts/autocorrect-pack.sh's verify: section.
#
# The pack's job under test is transport: resolve the fno-agents binary,
# run `corrections-verify --markdown --since <window>d`, and embed the block
# (or the no-corrections line, or the unavailable line) into the yaml. The
# verdict math is native (corrections_verify.rs, cargo-tested), so this
# suite stubs the binary and isolates via FNO_HOME + CLAUDE_DIR_OVERRIDE so
# the real ~/.fno and ~/.claude are never touched.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PACK="$SCRIPT_DIR/../autocorrect-pack.sh"
PASS=0
FAIL=0

pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

fixture() {
    local d
    d=$(mktemp -d)
    mkdir -p "$d/fno"
    echo "$d"
}

stub_agents() {
    local d="$1" out="$2" rc="${3:-0}"
    cat > "$d/stub-fno-agents" <<EOF
#!/usr/bin/env bash
printf '%s' '${out}'
exit ${rc}
EOF
    chmod +x "$d/stub-fno-agents"
}

run_pack() {
    local d="$1"
    FNO_HOME="$D/fno" CLAUDE_DIR_OVERRIDE="$D/claude" FNO_AGENTS_BIN="$D/stub-fno-agents" \
        bash "$PACK" --dry-run > "$D/packet.yaml" 2> "$D/err.txt"
}

# ---- T01: binary runs -> packet carries the verify: block ----
echo "T01: verify block embeds the corrections-verify markdown"
D=$(fixture)
printf '%s\n' "2026-09-10T12:00:00Z | S1 | git-rule-edit | rules/style.md | enforce emdash ban" > "$D/fno/corrections.log"
stub_agents "$D" "- 2026-09-10T12:00:00Z rules/style.md: improved (keep)
"
run_pack "$D"
RC=$?
if [[ $RC -eq 0 ]]; then pass "rc=0"; else fail "rc=$RC"; fi
if grep -q '^verify: |$' "$D/packet.yaml"; then
    pass "verify: block key present"
else
    fail "verify: key missing"
fi
if grep -q '^  - 2026-09-10T12:00:00Z rules/style.md: improved (keep)$' "$D/packet.yaml"; then
    pass "verdict line embedded under verify:"
else
    fail "verdict line missing: $(grep -A2 'verify:' "$D/packet.yaml")"
fi
rm -rf "$D"

# ---- T02: no applied rows -> the no-corrections line, packet still validates ----
echo "T02: no applied corrections reads the no-corrections line"
D=$(fixture)
printf '%s\n' "2026-09-10T12:00:00Z | S1 | target-postmortem | /tmp/pm.md | NoProgress: d" > "$D/fno/corrections.log"
stub_agents "$D" "no applied corrections in window
"
run_pack "$D"
RC=$?
if [[ $RC -eq 0 ]]; then pass "rc=0"; else fail "rc=$RC"; fi
if grep -q '^  no applied corrections in window$' "$D/packet.yaml"; then
    pass "no-corrections line embedded"
else
    fail "no-corrections line missing: $(grep -A2 'verify:' "$D/packet.yaml")"
fi
if grep -q '^watermark:' "$D/packet.yaml"; then
    pass "packet still ends with watermark section"
else
    fail "packet truncated"
fi
rm -rf "$D"

# ---- T03: binary missing -> unavailable line, packet still complete ----
echo "T03: corrections-verify unavailable keeps the packet whole"
D=$(fixture)
printf '%s\n' "2026-09-10T12:00:00Z | S1 | git-rule-edit | rules/style.md | enforce emdash ban" > "$D/fno/corrections.log"
FNO_HOME="$D/fno" CLAUDE_DIR_OVERRIDE="$D/claude" FNO_AGENTS_BIN="$D/absent-binary" \
    bash "$PACK" --dry-run > "$D/packet.yaml" 2> "$D/err.txt"
RC=$?
RC=$?
if [[ $RC -eq 0 ]]; then pass "rc=0"; else fail "rc=$RC"; fi
if grep -q '^  unavailable  # fno-agents corrections-verify did not run$' "$D/packet.yaml"; then
    pass "unavailable line present"
else
    fail "unavailable line missing: $(grep -A1 'verify:' "$D/packet.yaml")"
fi
if grep -q '^watermark:' "$D/packet.yaml"; then
    pass "packet still ends with watermark section"
else
    fail "packet truncated"
fi
rm -rf "$D"

echo ""
echo "Results: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
