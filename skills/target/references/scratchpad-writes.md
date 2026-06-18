# Scratchpad Writes

**Load when:** completing the think or plan phase. Cross-phase state sharing happens through files in `scratchpad_path`, not via prompt injection.

## After /think completes

If `scratchpad_path` is set in target-state.md, write think findings:

```bash
SCRATCHPAD=$(sed -n 's/^scratchpad_path:[[:space:]]*//p' .fno/target-state.md 2>/dev/null)
if [[ -n "$SCRATCHPAD" && -d "$SCRATCHPAD" ]]; then
  cat > "$SCRATCHPAD/think-findings.md" << 'EOF'
  ## Design Decisions
  [Key architectural decisions and rationale]

  ## Constraints Discovered
  [Technical constraints, API limitations, existing patterns to follow]

  ## Rejected Alternatives
  [Approaches considered and why they were rejected]

  ## Open Questions
  [Anything unresolved that the plan should address]
  EOF
fi
```

If think is skipped (plan input), no think-findings.md is written. If scratchpad directory does not exist, think phase completes normally without error. Populate the file from the think phase's actual output, not the template literal.

## Pre-Plan Context Loading

If scratchpad exists and has think findings, read them before invoking /blueprint:

```bash
SCRATCHPAD=$(sed -n 's/^scratchpad_path:[[:space:]]*//p' .fno/target-state.md 2>/dev/null)
THINK_FINDINGS=""
if [[ -n "$SCRATCHPAD" && -f "$SCRATCHPAD/think-findings.md" ]]; then
  THINK_FINDINGS=$(cat "$SCRATCHPAD/think-findings.md")
fi
```

When invoking `/blueprint`, use think findings to inform plan creation:
- Constraints from think findings inform phase structure
- Rejected alternatives prevent re-exploring dead ends
- Design decisions guide the plan's technical approach

Do NOT pass think-findings.md as a prompt argument. Read it inline and let it inform your plan creation.

## After /blueprint completes

Write a condensed summary for workers (deliberately smaller than the full plan):

```bash
SCRATCHPAD=$(sed -n 's/^scratchpad_path:[[:space:]]*//p' .fno/target-state.md 2>/dev/null)
if [[ -n "$SCRATCHPAD" && -d "$SCRATCHPAD" ]]; then
  cat > "$SCRATCHPAD/plan-summary.md" << EOF
  ## Plan: {feature-name}
  Path: {plan-path}
  Phases: {count}
  Total tasks: {count}

  ## Key Constraints
  [From think findings + plan analysis]

  ## Execution Notes
  [Anything workers should know - test patterns, auth requirements, etc.]

  ## File Ownership Quick Reference
  [Condensed from INDEX - which tasks touch which files]
  EOF
fi
```

Workers read this for orientation, then read their specific phase file for task details. The full plan in the plan directory remains the source of truth. Plan summary should stay under ~100 lines.

## Why scratchpad over prompt injection

Cross-phase state in the prompt grows the conversation context for every subsequent phase. Files in `scratchpad_path` are read on demand by the phase that needs them, keeping context lean. The scratchpad is also archived alongside the plan on completion, preserving a forensic record of the run's intermediate state.
