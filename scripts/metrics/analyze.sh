#!/usr/bin/env bash
# Analyze subagent metrics for optimization insights
#
# Usage: ./analyze.sh [metrics-file]
#
# Default metrics file: .fno/metrics.jsonl

set -euo pipefail

METRICS_FILE="${1:-.fno/metrics.jsonl}"

if [[ ! -f "$METRICS_FILE" ]]; then
  echo "No metrics file found at $METRICS_FILE"
  echo "Run some tasks first to generate metrics."
  exit 0
fi

echo "Agent Performance Summary"
echo "═════════════════════════"
echo ""

# Count events by agent
echo "## Task Counts by Agent"
echo ""
jq -rs '
  [.[] | select(.event == "stop")] |
  group_by(.agent) |
  map({
    agent: .[0].agent,
    total: length,
    success: [.[] | select(.result == "SUCCESS")] | length,
    failed: [.[] | select(.result == "FAILED")] | length,
    blocked: [.[] | select(.result == "BLOCKED")] | length
  }) |
  sort_by(-.total) |
  .[] |
  "\(.agent): \(.total) total (\(.success) success, \(.failed) failed, \(.blocked) blocked)"
' "$METRICS_FILE"

echo ""
echo "## Success Rates"
echo ""
jq -rs '
  [.[] | select(.event == "stop")] |
  group_by(.agent) |
  map({
    agent: .[0].agent,
    total: length,
    success: ([.[] | select(.result == "SUCCESS")] | length)
  }) |
  map(. + {rate: ((.success / .total) * 100 | floor)}) |
  sort_by(-.rate) |
  .[] |
  "\(.agent): \(.rate)% success rate (\(.success)/\(.total))"
' "$METRICS_FILE"

# Calculate average tokens if available
echo ""
echo "## Token Usage (if tracked)"
echo ""
jq -rs '
  [.[] | select(.event == "stop" and .tokens > 0)] |
  group_by(.agent) |
  map({
    agent: .[0].agent,
    avg_tokens: ([.[] | .tokens] | add / length | floor),
    total_tasks: length
  }) |
  sort_by(-.avg_tokens) |
  .[] |
  "\(.agent): avg \(.avg_tokens) tokens (\(.total_tasks) tasks)"
' "$METRICS_FILE" 2>/dev/null || echo "(No token data available)"

# Compaction counts
echo ""
echo "## Context Compactions (if tracked)"
echo ""
jq -rs '
  [.[] | select(.event == "stop" and .compactions > 0)] |
  group_by(.agent) |
  map({
    agent: .[0].agent,
    avg_compactions: ([.[] | .compactions] | add / length),
    total_tasks: length
  }) |
  sort_by(-.avg_compactions) |
  .[] |
  "\(.agent): avg \(.avg_compactions | . * 10 | floor / 10) compactions/task (\(.total_tasks) tasks)"
' "$METRICS_FILE" 2>/dev/null || echo "(No compaction data available)"

# Recent activity
echo ""
echo "## Recent Activity (last 10 tasks)"
echo ""
jq -rs '
  [.[] | select(.event == "stop")] |
  sort_by(.time) |
  reverse |
  .[:10] |
  .[] |
  "[\(.time | split("T")[0])] \(.agent): \(.task) - \(.result)"
' "$METRICS_FILE"

# Recommendations
echo ""
echo "## Optimization Recommendations"
echo ""

# Check for high failure rates
HIGH_FAILURE=$(jq -rs '
  [.[] | select(.event == "stop")] |
  group_by(.agent) |
  map({
    agent: .[0].agent,
    total: length,
    failed: ([.[] | select(.result == "FAILED")] | length)
  }) |
  map(. + {rate: ((.failed / .total) * 100)}) |
  .[] |
  select(.rate > 25 and .total >= 3) |
  .agent
' "$METRICS_FILE" 2>/dev/null)

if [[ -n "$HIGH_FAILURE" ]]; then
  echo "⚠ High failure rate detected for:"
  echo "$HIGH_FAILURE" | while read agent; do
    echo "  - $agent: Consider adding more domain-specific skills"
  done
else
  echo "✓ No agents with high failure rates detected"
fi

# Check for high compaction (context bloat)
HIGH_COMPACTION=$(jq -rs '
  [.[] | select(.event == "stop" and .compactions > 0)] |
  group_by(.agent) |
  map({
    agent: .[0].agent,
    avg: ([.[] | .compactions] | add / length)
  }) |
  .[] |
  select(.avg > 2) |
  .agent
' "$METRICS_FILE" 2>/dev/null)

if [[ -n "$HIGH_COMPACTION" ]]; then
  echo "⚠ High context compaction detected for:"
  echo "$HIGH_COMPACTION" | while read agent; do
    echo "  - $agent: Consider reducing preloaded skills"
  done
fi

echo ""
echo "═════════════════════════"
echo "Analysis complete."
