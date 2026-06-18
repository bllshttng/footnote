#!/usr/bin/env bash
# Do Target Setup Script
# Minimal setup - delegates execution to /target skill

set -euo pipefail

# Defaults
MAX_ITERATIONS=40
EXPERTISE=""
PLAN_PATH=""
RESUME="false"

# Parse arguments
while [[ $# -gt 0 ]]; do
  case $1 in
    -h|--help)
      cat << 'EOF'
Do Target - Iterative development loop using /do orchestration

USAGE:
  /target [expertise] <plan-path> [OPTIONS]

ARGUMENTS:
  expertise   Optional: frontend, backend, architect, fullstack, devops, qa
  plan-path   Path to plan file or folder (required unless --resume)

OPTIONS:
  --max-iterations <n>   Maximum iterations (default: 40)
  --resume               Continue from existing state
  -h, --help             Show this help

EXAMPLES:
  /target path/to/plan.md
  /target frontend path/to/plan
  /target backend plan.md --max-iterations 20
  /target --resume
EOF
      exit 0
      ;;
    --max-iterations)
      MAX_ITERATIONS="$2"
      shift 2
      ;;
    --resume)
      RESUME="true"
      shift
      ;;
    frontend|backend|architect|fullstack|devops|qa|ml-engineer|data-engineer)
      EXPERTISE="$1"
      shift
      ;;
    *)
      PLAN_PATH="$1"
      shift
      ;;
  esac
done

# Validate
if [[ "$RESUME" == "false" && -z "$PLAN_PATH" ]]; then
  echo "❌ Error: Plan path required (or use --resume)" >&2
  exit 1
fi

# Create planning directory
mkdir -p .fno

# Initialize or read state
STATE_FILE=".fno/target-state.md"

if [[ "$RESUME" == "true" && -f "$STATE_FILE" ]]; then
  echo "📂 Resuming from existing state..."
  # Use -f2- to handle paths with colons (e.g., timestamps, Windows paths)
  ITERATION=$(grep '^iteration:' "$STATE_FILE" | cut -d: -f2- | xargs)
  PLAN_PATH=$(grep '^plan:' "$STATE_FILE" | cut -d: -f2- | xargs)
  EXPERTISE=$(grep '^expertise:' "$STATE_FILE" | cut -d: -f2- | xargs)

  # Validate extracted values
  if [[ -z "$PLAN_PATH" ]]; then
    echo "❌ Error: Could not parse plan path from state file" >&2
    exit 1
  fi
  if [[ -z "$ITERATION" ]]; then
    echo "❌ Error: Could not parse iteration from state file" >&2
    exit 1
  fi
else
  ITERATION=1

  # Write initial state
  cat > "$STATE_FILE" << EOF
---
plan: $PLAN_PATH
expertise: $EXPERTISE
iteration: $ITERATION
max_iterations: $MAX_ITERATIONS
status: IN_PROGRESS
started: $(date -u +"%Y-%m-%dT%H:%M:%SZ")
---

# Target State

## Progress
- [ ] Plan parsed
- [ ] Execution complete
- [ ] Review passed
- [ ] PR created

## Learnings
(Updated each iteration)

## Blockers
(Any issues preventing progress)
EOF
  echo "📝 Created state file: $STATE_FILE"
fi

# Output for skill to read
echo ""
echo "┌─────────────────────────────────────────┐"
echo "│           DO TARGET - Iteration $ITERATION        │"
echo "├─────────────────────────────────────────┤"
echo "│ Plan:      $PLAN_PATH"
echo "│ Expertise: ${EXPERTISE:-none}"
echo "│ Max iter:  $MAX_ITERATIONS"
echo "└─────────────────────────────────────────┘"
echo ""
echo "State: $STATE_FILE"
echo ""
echo "🚀 Skill will orchestrate: /do → /review → /pr create"
