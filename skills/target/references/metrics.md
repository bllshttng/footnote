# Metrics Dashboard

Show token usage metrics across Claude Code sessions.

## Usage

```bash
/metrics           # Show last 10 sessions
/metrics 20        # Show last 20 sessions
```

## What It Shows

1. **Recent Sessions Table**
   - Date, Project, Branch
   - Total tokens (main + subagents)
   - Main session tokens
   - Agent count
   - Estimated cost (~$5/1M tokens)

2. **Current Session Agents** (if in project)
   - Agent ID
   - Task description
   - Token usage

## Implementation

Run the metrics dashboard script:

```bash
LIMIT="${1:-10}"
~/.claude/scripts/metrics-dashboard.sh "$LIMIT"
```

## Output Format

The dashboard uses color coding:
- **Yellow**: Large sessions (>50M tokens)
- **White**: Medium sessions (>20M tokens)
- **Dim**: Small sessions (<20M tokens)

Cost estimates use ~$5/1M tokens (Opus average).
