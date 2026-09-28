#!/usr/bin/env bash
# Tests for scripts/corrections-insights-tag.sh
# The /fno:intel report is the only S2 source: the ingest requires
# --insights-file, keys its dedupe on the quoted correction text (reports
# cover overlapping 14-day windows, so the same correction reappears on a
# new line with a new repeat count), and keeps its watermark beside the log
# under FNO_HOME. Nothing may be written under $HOME/.claude.
#
# Isolates via FNO_HOME and HOME so the real ~/.fno and ~/.claude are
# never touched.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INGEST="$SCRIPT_DIR/../corrections-insights-tag.sh"
PASS=0
FAIL=0

if [[ ! -f "$INGEST" ]]; then
    echo "FAIL: $INGEST not found - cannot run tests"
    exit 1
fi

pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

D=$(mktemp -d)
trap 'rm -rf "$D"' EXIT
export FNO_HOME="$D/fno"
export HOME="$D/home"
mkdir -p "$FNO_HOME" "$HOME"

bash "$SCRIPT_DIR/../corrections-log-init.sh" >/dev/null 2>&1

# S2 row count via awk (field 2 of the pipe-delimited row).
count_s2() { awk -F' \\| ' '$2 == "S2"' "$FNO_HOME/corrections.log" | wc -l | tr -d ' '; }

# Fixture reports in the shape of skills/intel/references/report-shape.md.
cat > "$D/report-a.md" <<'EOF'
## Operator corrections

- "use rg not grep" (x2, ab12cd34, signal=workflow_friction) #agent-correction
- "never attest ahead of the fork" (x1, ef456789, signal=review_discipline) #agent-correction
EOF

# Overlapping window: the first correction again, new line, new repeat count.
cat > "$D/report-b.md" <<'EOF'
## Operator corrections

- "use rg not grep" (x3, ab12cd34, signal=workflow_friction) #agent-correction
EOF

# ---- T01: first run lands two S2 rows carrying signal= ----
echo "T01: two fixture corrections ingest as two S2 rows"
bash "$INGEST" --insights-file "$D/report-a.md" >/dev/null 2>&1
RC=$?
if [[ $RC -eq 0 ]]; then pass "rc=0"; else fail "rc=$RC (expected 0)"; fi
if [[ "$(count_s2)" == "2" ]]; then pass "two S2 rows"; else fail "want 2 S2 rows, got $(count_s2)"; fi
SIGNAL_ROWS=$(awk -F' \\| ' '$2 == "S2" && $5 ~ /signal=/' "$FNO_HOME/corrections.log" | wc -l | tr -d ' ')
if [[ "$SIGNAL_ROWS" == "2" ]]; then pass "rows carry signal="; else fail "want 2 rows with signal=, got $SIGNAL_ROWS"; fi

# ---- T02: second run on the same report adds zero ----
echo "T02: re-run is a no-op"
bash "$INGEST" --insights-file "$D/report-a.md" >/dev/null 2>&1
if [[ "$(count_s2)" == "2" ]]; then pass "no duplicates on re-run"; else fail "re-run grew the log to $(count_s2)"; fi

# ---- T03: overlapping report quoting a seen correction adds zero ----
echo "T03: overlapping report adds zero"
bash "$INGEST" --insights-file "$D/report-b.md" >/dev/null 2>&1
if [[ "$(count_s2)" == "2" ]]; then pass "same quote not re-ingested"; else fail "overlapping report grew the log to $(count_s2)"; fi

# ---- T04: a genuinely new correction still lands ----
echo "T04: unseen correction lands"
printf -- '- "brand new correction" (x1, ff0199aa, signal=tooling) #agent-correction\n' > "$D/report-c.md"
bash "$INGEST" --insights-file "$D/report-c.md" >/dev/null 2>&1
if [[ "$(count_s2)" == "3" ]]; then pass "new correction ingested"; else fail "want 3 S2 rows, got $(count_s2)"; fi

# ---- T04b: the same quote twice in one report lands once ----
echo "T04b: in-run duplicate"
printf -- '- "doubled correction" (x1, aa11bb22, signal=tooling) #agent-correction\n- "doubled correction" (x1, aa11bb22, signal=tooling) #agent-correction\n' > "$D/report-dup.md"
bash "$INGEST" --insights-file "$D/report-dup.md" >/dev/null 2>&1
if [[ "$(count_s2)" == "4" ]]; then pass "same quote lands once per report"; else fail "in-run duplicate: want 4 S2 rows, got $(count_s2)"; fi

# ---- T05: --insights-file is required ----
echo "T05: missing flag is refused"
MSG=$(bash "$INGEST" 2>&1); RC=$?
if [[ $RC -eq 2 ]]; then pass "rc=2"; else fail "rc=$RC (expected 2)"; fi
if [[ "$MSG" == *"/fno:intel"* ]]; then pass "message names /fno:intel"; else fail "message does not name /fno:intel: $MSG"; fi

# ---- T06: a missing path keeps exit 1 ----
echo "T06: missing path"
bash "$INGEST" --insights-file "$D/absent.md" >/dev/null 2>&1
RC=$?
if [[ $RC -eq 1 ]]; then pass "rc=1"; else fail "rc=$RC (expected 1)"; fi

# ---- T07: nothing lands under $HOME/.claude ----
echo "T07: placement rule"
STRAY=$(find "$HOME/.claude" -type f 2>/dev/null)
if [[ -z "$STRAY" ]]; then pass "no file under \$HOME/.claude"; else fail "files under \$HOME/.claude: $STRAY"; fi
if [[ -f "$FNO_HOME/corrections.log.wm" ]]; then pass "watermark beside the log"; else fail "watermark missing at \$FNO_HOME/corrections.log.wm"; fi

echo ""
echo "Results: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
