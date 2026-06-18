#!/bin/bash
# enforce-fork-context.sh
# Hook script to enforce context: fork skills are run correctly
#
# This hook runs once when a skill with context: fork is loaded.
# It outputs a warning/reminder to ensure the skill runs in the correct context.

SKILL_NAME="${1:-unknown}"
EXPECTED_MODEL="${2:-haiku}"

# Check if CLAUDE_SUBAGENT is set (indicates we're in a spawned Task context)
if [ -n "$CLAUDE_SUBAGENT" ]; then
    # We're in a subagent context - good!
    echo "✓ Running in forked context as expected"
    exit 0
fi

# Check if we're running with the expected model
# Note: This may not always be available, so we warn but don't block
if [ -n "$CLAUDE_MODEL" ] && [ "$CLAUDE_MODEL" != "$EXPECTED_MODEL" ]; then
    echo "⚠️  CONTEXT FORK WARNING"
    echo "   Skill '$SKILL_NAME' specifies context: fork, model: $EXPECTED_MODEL"
    echo "   Current model appears to be: $CLAUDE_MODEL"
    echo ""
    echo "   This skill should be invoked via Task tool with model: $EXPECTED_MODEL"
    echo "   Example:"
    echo "     Task(prompt='Execute skill content', model='$EXPECTED_MODEL', ...)"
    echo ""
    exit 1  # Block execution in wrong context
fi

# If we can't detect context, output a reminder
echo "📋 Context Fork Reminder"
echo "   Skill '$SKILL_NAME' has context: fork, model: $EXPECTED_MODEL"
echo "   Ensure this skill is running in a forked Task subagent, not main context."
exit 0  # Allow but warn
