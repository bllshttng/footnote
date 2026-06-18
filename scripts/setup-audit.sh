#!/bin/bash
# Setup script for /audit command - discovery/planning loop

set -e

# Parse arguments
TOPIC=""
MAX_ITERATIONS=20
OUTPUT_DIR="internal/web/plans"
PERSPECTIVES="ux,pm,po,eng"

while [[ $# -gt 0 ]]; do
  case $1 in
    --max-iterations)
      MAX_ITERATIONS="$2"
      shift 2
      ;;
    --output-dir)
      OUTPUT_DIR="$2"
      shift 2
      ;;
    --perspectives)
      PERSPECTIVES="$2"
      shift 2
      ;;
    *)
      if [ -z "$TOPIC" ]; then
        TOPIC="$1"
      fi
      shift
      ;;
  esac
done

if [ -z "$TOPIC" ]; then
  echo "Usage: /audit \"TOPIC\" [--max-iterations N] [--output-dir PATH] [--perspectives LIST]"
  echo ""
  echo "Examples:"
  echo "  /audit \"QR code feature completeness\""
  echo "  /audit \"attendance tracking\" --output-dir internal/web/plans/attendance"
  echo "  /audit \"parent experience\" --perspectives ux,pm"
  exit 1
fi

# Ensure .fno directory exists
mkdir -p .fno

# Create or update audit loop config
cat > .fno/audit-loop.local.md << EOF
---
topic: "$TOPIC"
max_iterations: $MAX_ITERATIONS
output_dir: "$OUTPUT_DIR"
perspectives: "$PERSPECTIVES"
started: $(date -u +"%Y-%m-%dT%H:%M:%SZ")
completion_promise: "All features for '$TOPIC' have been documented in plan folders with acceptance criteria"
---

# Audit Loop: $TOPIC

## Configuration
- **Max Iterations:** $MAX_ITERATIONS
- **Output Directory:** $OUTPUT_DIR
- **Perspectives:** $PERSPECTIVES

## Required Skills
Load these skills for the audit:
- \`/audit\` - Multi-perspective gap analysis
- \`/think\` - Brainstorming and exploration
- \`/plan\` - Creating plan folders
- \`/bdd-acceptance-criteria\` - Writing testable stories
- \`/linear\` - Creating tracking tickets

## Workflow

1. **READ** .fno/audit-progress.txt first (if exists)
2. **ANALYZE** current state of codebase
3. **IDENTIFY** gaps from all perspectives: $PERSPECTIVES
4. **CREATE** plan folders in $OUTPUT_DIR
5. **UPDATE** progress file after each action
6. **LOOP** - ask "what else is missing?" until complete

## Completion Criteria

Output \`<promise>All features for '$TOPIC' have been documented in plan folders with acceptance criteria</promise>\` ONLY when:
- All P1 features have plans
- All P2 features have plans
- All identified gaps have plans
- Each plan has acceptance criteria
- Plans are linked to Linear tickets
EOF

# Create or preserve progress file
if [ ! -f .fno/audit-progress.txt ]; then
  cat > .fno/audit-progress.txt << EOF
# Audit Progress: $TOPIC

Started: $(date -u +"%Y-%m-%dT%H:%M:%SZ")
Output: $OUTPUT_DIR

## Analysis Status
- [ ] Current state scan
- [ ] UX perspective
- [ ] PM perspective
- [ ] PO perspective
- [ ] Engineering perspective

## Plans Created
(none yet)

## Identified Gaps
(none yet)

## Next Actions
1. Scan codebase for implemented features
2. Begin UX perspective analysis
EOF
fi

echo "═══════════════════════════════════════════════════════════"
echo "Audit Loop Initialized"
echo "═══════════════════════════════════════════════════════════"
echo ""
echo "Topic: $TOPIC"
echo "Output: $OUTPUT_DIR"
echo "Perspectives: $PERSPECTIVES"
echo "Max Iterations: $MAX_ITERATIONS"
echo ""
echo "Progress file: .fno/audit-progress.txt"
echo ""
echo "Skills to load:"
echo "  /audit, /think, /blueprint, /bdd-acceptance-criteria, /linear"
echo ""
