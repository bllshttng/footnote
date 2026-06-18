# Skill Portability Architecture

How footnote skills work standalone, without the full plugin.

## Problem

Skills originally required the footnote plugin because they referenced shared scripts via `${CLAUDE_PLUGIN_ROOT}`, depended on PreToolUse hooks for state initialization, and hard-invoked sibling skills with `footnote:` prefixes.

## Solution: Three-Layer De-Internalization

### 1. Script Vendoring

Skills that need shared scripts (config.sh, validate-plan.sh, checkpoint.sh, etc.) get copies in their own `scripts/` directory. Paths use `${SKILL_DIR}` instead of `${CLAUDE_PLUGIN_ROOT}`:

```
skills/do/
  SKILL.md              # references ${SKILL_DIR}/scripts/config.sh
  scripts/
    config.sh           # vendored copy
    checkpoint.sh       # vendored copy
    run-target-loop.sh   # vendored copy
    session-cost.py     # vendored copy
    lib/
      cost_tracker.py   # vendored dependency
```

Trade-off: duplication vs. independence. When the source script changes, vendored copies need manual sync. This is acceptable because portability is the primary goal and scripts change infrequently.

### 2. Hook Replacement with Inline Init

Skills that used PreToolUse hooks (think, spec) for session state initialization now include the init logic inline in their SKILL.md:

```bash
mkdir -p .fno
if [[ ! -f .fno/target-state.md ]]; then
  TIMESTAMP=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  cat > .fno/session-state.md << STEOF
---
type: plan
status: IN_PROGRESS
created_at: ${TIMESTAMP}
---
STEOF
fi
```

The inline block replicates the hook's behavior: creates session-state.md with YAML frontmatter, guards against overwriting target state, and cleans up the session-registered sentinel.

### 3. Advisory Skill References

Hard invocations (`invoke /fno:fix --from-debug`) become conditional advisories:

```
If a fix skill is installed, invoke it: /fix --from-debug
If no fix skill is available, present findings to the user.
```

This lets skills compose when available without failing when absent.

## Provider Detection

The target loop detects the current AI coding client at runtime:

| Available Tool | Provider | Parallel Support |
|---------------|----------|-----------------|
| `Agent` | Claude Code | Full parallel |
| `process` | OpenClaw | Parallel via background processes |
| `run_shell_command` | Gemini CLI | Sequential fallback |
| None of above | Generic | Sequential fallback |

See `docs/providers/provider-adapters.md` for dispatch patterns.

## Skill Categories

| Category | Count | Requires Plugin | Example |
|----------|-------|----------------|---------|
| Portable | 22 | No | tdd, think, spec, do |
| Internal | 7 | Yes | target, operator, sigma-review |

Internal skills stay plugin-dependent because they orchestrate multiple agents, read plugin-level configuration, or depend on hook infrastructure that cannot be replicated inline.
