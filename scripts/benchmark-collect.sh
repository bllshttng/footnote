#!/usr/bin/env bash
# benchmark-collect.sh - Collect metrics after a benchmark run
#
# Usage: ./scripts/benchmark-collect.sh <tool-name> <session-id> [baseline-commit]
#
# Run this from inside the tool's worktree after the benchmark completes.
# Creates results/ directory with all metrics for comparison.
#
# Uses ccusage for authoritative cost/token data and session-cost.py for
# operational metadata (compactions, duration, branch attribution).

set -euo pipefail

TOOL="${1:?Usage: benchmark-collect.sh <tool-name> <session-id> [baseline-commit]}"
SESSION_ID="${2:?Usage: benchmark-collect.sh <tool-name> <session-id> [baseline-commit]}"
BASELINE="${3:-0c25545}"

# Reject flag-shaped or empty args: previously this script would happily accept
# "--tool" as $TOOL and produce files like results/--tool-ccusage.json, which
# masked the real failure (wrong invocation). Fail fast instead.
for arg_name in TOOL SESSION_ID; do
  arg_value="${!arg_name}"
  if [[ -z "$arg_value" || "$arg_value" == -* ]]; then
    echo "benchmark-collect: invalid $arg_name='$arg_value' (must be non-empty and not flag-shaped)" >&2
    echo "Usage: benchmark-collect.sh <tool-name> <session-id> [baseline-commit]" >&2
    exit 2
  fi
done

RESULTS_DIR="results"
mkdir -p "$RESULTS_DIR"

echo "Collecting benchmark metrics for: $TOOL"
echo "Session: $SESSION_ID"
echo "Baseline: $BASELINE"
echo "---"

# 1. ccusage - authoritative cost, token counts, per-model breakdown
echo "[1/6] ccusage (costs, tokens, model breakdown)..."
npx ccusage session -i "$SESSION_ID" --json --breakdown 2>/dev/null \
  > "$RESULTS_DIR/${TOOL}-ccusage.json" || echo '{"error": "ccusage failed"}' > "$RESULTS_DIR/${TOOL}-ccusage.json"

# Extract key values from ccusage for the summary
CCUSAGE_COST=$(python3 -c "
import json, sys
try:
    data = json.load(open('$RESULTS_DIR/${TOOL}-ccusage.json'))
    sessions = data if isinstance(data, list) else data.get('sessions', [data])
    total = sum(s.get('totalCost', s.get('cost', 0)) for s in sessions)
    print(f'{total:.2f}')
except: print('--')
" 2>/dev/null || echo "--")

CCUSAGE_INPUT=$(python3 -c "
import json, sys
try:
    data = json.load(open('$RESULTS_DIR/${TOOL}-ccusage.json'))
    sessions = data if isinstance(data, list) else data.get('sessions', [data])
    total = sum(s.get('inputTokens', s.get('input_tokens', 0)) for s in sessions)
    print(total)
except: print('--')
" 2>/dev/null || echo "--")

CCUSAGE_OUTPUT=$(python3 -c "
import json, sys
try:
    data = json.load(open('$RESULTS_DIR/${TOOL}-ccusage.json'))
    sessions = data if isinstance(data, list) else data.get('sessions', [data])
    total = sum(s.get('outputTokens', s.get('output_tokens', 0)) for s in sessions)
    print(total)
except: print('--')
" 2>/dev/null || echo "--")

CCUSAGE_CACHE=$(python3 -c "
import json, sys
try:
    data = json.load(open('$RESULTS_DIR/${TOOL}-ccusage.json'))
    sessions = data if isinstance(data, list) else data.get('sessions', [data])
    total = sum(s.get('cacheReadTokens', s.get('cache_read_input_tokens', 0)) for s in sessions)
    print(total)
except: print('--')
" 2>/dev/null || echo "--")

# 2. fno.cost._session_cost - compactions, duration, branch attribution
echo "[2/6] fno.cost._session_cost (compactions, duration, branch)..."
# The cost helper moved into the fno package (cli/src/fno/cost/). Point
# PYTHONPATH at the package source in a checkout so it runs pre-install.
_REPO_ROOT="$(git rev-parse --show-toplevel)"
if [[ -f "$_REPO_ROOT/cli/src/fno/cost/_session_cost.py" ]]; then
  export PYTHONPATH="$_REPO_ROOT/cli/src${PYTHONPATH:+:${PYTHONPATH}}"
  BRANCH=$(git branch --show-current)
  python3 -m fno.cost._session_cost --json --branch "$BRANCH" "$SESSION_ID" \
    > "$RESULTS_DIR/${TOOL}-session.json" 2>/dev/null || echo '{"error": "session-cost failed"}' > "$RESULTS_DIR/${TOOL}-session.json"
else
  echo '{"error": "fno.cost._session_cost not found"}' > "$RESULTS_DIR/${TOOL}-session.json"
fi

# Extract operational metadata from session-cost.py
SESSION_DURATION=$(python3 -c "
import json
try:
    data = json.load(open('$RESULTS_DIR/${TOOL}-session.json'))
    print(data.get('duration_minutes', '--'))
except: print('--')
" 2>/dev/null || echo "--")

SESSION_COMPACTIONS=$(python3 -c "
import json
try:
    data = json.load(open('$RESULTS_DIR/${TOOL}-session.json'))
    print(data.get('compactions', '--'))
except: print('--')
" 2>/dev/null || echo "--")

SESSION_MODEL=$(python3 -c "
import json
try:
    data = json.load(open('$RESULTS_DIR/${TOOL}-session.json'))
    print(data.get('primary_model', '--'))
except: print('--')
" 2>/dev/null || echo "--")

# 3. Git metrics
echo "[3/6] git metrics (commits, files, diff)..."
COMMIT_COUNT=$(git log --oneline "$BASELINE"..HEAD 2>/dev/null | wc -l | tr -d ' ')
git log --oneline "$BASELINE"..HEAD > "$RESULTS_DIR/${TOOL}-commits.txt" 2>/dev/null
git diff --stat "$BASELINE"..HEAD > "$RESULTS_DIR/${TOOL}-diff.txt" 2>/dev/null

FILES_CHANGED=$(git diff --name-only "$BASELINE"..HEAD 2>/dev/null | wc -l | tr -d ' ')
INSERTIONS=$(git diff --shortstat "$BASELINE"..HEAD 2>/dev/null | grep -oE '[0-9]+ insertion' | grep -oE '[0-9]+' || echo "0")
DELETIONS=$(git diff --shortstat "$BASELINE"..HEAD 2>/dev/null | grep -oE '[0-9]+ deletion' | grep -oE '[0-9]+' || echo "0")

# 4. Acceptance criteria (grep checks from spec)
echo "[4/6] acceptance criteria (8 grep checks)..."
AC_FILE="$RESULTS_DIR/${TOOL}-acceptance.txt"
cat > "$AC_FILE" << EOF
=== Acceptance Criteria for $TOOL ===
Date: $(date -u +%Y-%m-%dT%H:%M:%SZ)

1. Unmigrated legacy refs in megawalk (want 0):
   $(grep -rc 'tasks\.json' skills/megawalk/ 2>/dev/null | grep -v ':0$' | wc -l | tr -d ' ')

2. Old patterns --locked-by/--status in_progress (want 0):
   $(grep -rc '\-\-locked-by\|--status in_progress' skills/megawalk/ 2>/dev/null | grep -v ':0$' | wc -l | tr -d ' ')

3. Graph API used - roadmap-tasks.py next in SKILL.md (want >0):
   $(grep -c 'roadmap-tasks.py next' skills/megawalk/SKILL.md 2>/dev/null || echo "0")

4. emit_approve in stop hook (want >0 on every exit):
   $(grep -c 'emit_approve' hooks/megawalk-stop-hook.sh 2>/dev/null || echo "0")

5. Skill invocations in stop hook resume (want 0):
   $(grep -c "Skill(" hooks/megawalk-stop-hook.sh 2>/dev/null || echo "0")

6. Graph commands in references (want >0):
   $(grep -rc 'roadmap-tasks.py' skills/megawalk/references/ 2>/dev/null | grep -v ':0$' | wc -l | tr -d ' ')

7. roadmap-generator has --size (want >0):
   $(grep -c '\-\-size' agents/roadmap-generator.md 2>/dev/null || echo "0")

8. Plain text resume prompt in stop hook (want >0):
   $(grep -c 'Invoke Skill\|invoke.*target\|Feature ab-' hooks/megawalk-stop-hook.sh 2>/dev/null || echo "0")
EOF

# Count passes
PASSES=0
TOTAL=8
[[ $(grep -rc 'tasks\.json' skills/megawalk/ 2>/dev/null | grep -v ':0$' | wc -l | tr -d ' ') -eq 0 ]] && PASSES=$((PASSES + 1))
[[ $(grep -rc '\-\-locked-by\|--status in_progress' skills/megawalk/ 2>/dev/null | grep -v ':0$' | wc -l | tr -d ' ') -eq 0 ]] && PASSES=$((PASSES + 1))
[[ $(grep -c 'roadmap-tasks.py next' skills/megawalk/SKILL.md 2>/dev/null || echo "0") -gt 0 ]] && PASSES=$((PASSES + 1))
[[ $(grep -c 'emit_approve' hooks/megawalk-stop-hook.sh 2>/dev/null || echo "0") -gt 0 ]] && PASSES=$((PASSES + 1))
[[ $(grep -c "Skill(" hooks/megawalk-stop-hook.sh 2>/dev/null || echo "0") -eq 0 ]] && PASSES=$((PASSES + 1))
[[ $(grep -rc 'roadmap-tasks.py' skills/megawalk/references/ 2>/dev/null | grep -v ':0$' | wc -l | tr -d ' ') -gt 0 ]] && PASSES=$((PASSES + 1))
[[ $(grep -c '\-\-size' agents/roadmap-generator.md 2>/dev/null || echo "0") -gt 0 ]] && PASSES=$((PASSES + 1))
[[ $(grep -c 'Invoke Skill\|invoke.*target\|Feature ab-' hooks/megawalk-stop-hook.sh 2>/dev/null || echo "0") -gt 0 ]] && PASSES=$((PASSES + 1))

echo "Acceptance: $PASSES/$TOTAL" >> "$AC_FILE"

# 5. Summary JSON (merged from both sources)
echo "[5/6] generating summary..."
cat > "$RESULTS_DIR/${TOOL}-summary.json" << EOF
{
  "tool": "$TOOL",
  "session_id": "$SESSION_ID",
  "baseline": "$BASELINE",
  "collected_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "cost": {
    "source": "ccusage",
    "usd": "$CCUSAGE_COST"
  },
  "tokens": {
    "source": "ccusage",
    "input": "$CCUSAGE_INPUT",
    "output": "$CCUSAGE_OUTPUT",
    "cache_read": "$CCUSAGE_CACHE"
  },
  "operational": {
    "source": "session-cost.py",
    "duration_minutes": "$SESSION_DURATION",
    "compactions": "$SESSION_COMPACTIONS",
    "primary_model": "$SESSION_MODEL"
  },
  "git": {
    "commits": $COMMIT_COUNT,
    "files_changed": $FILES_CHANGED,
    "insertions": ${INSERTIONS:-0},
    "deletions": ${DELETIONS:-0}
  },
  "acceptance": {
    "passed": $PASSES,
    "total": $TOTAL
  }
}
EOF

# 6. Print report
echo "[6/6] done."
echo ""
echo "=== $TOOL Benchmark Report ==="
echo ""
echo "Cost & Tokens (ccusage):"
echo "  Cost:        \$$CCUSAGE_COST"
echo "  Input:       $CCUSAGE_INPUT tokens"
echo "  Output:      $CCUSAGE_OUTPUT tokens"
echo "  Cache read:  $CCUSAGE_CACHE tokens"
echo ""
echo "Operational (session-cost.py):"
echo "  Duration:    ${SESSION_DURATION} min"
echo "  Compactions: $SESSION_COMPACTIONS"
echo "  Model:       $SESSION_MODEL"
echo ""
echo "Git:"
echo "  Commits:     $COMMIT_COUNT"
echo "  Files:       $FILES_CHANGED (+${INSERTIONS:-0} -${DELETIONS:-0})"
echo ""
echo "Acceptance:    $PASSES/$TOTAL"
echo ""
echo "Raw data in $RESULTS_DIR/:"
echo "  ${TOOL}-ccusage.json     ccusage full output"
echo "  ${TOOL}-session.json     session-cost.py full output"
echo "  ${TOOL}-commits.txt      commit log"
echo "  ${TOOL}-diff.txt         file changes"
echo "  ${TOOL}-acceptance.txt   grep check details"
echo "  ${TOOL}-summary.json     combined summary"
