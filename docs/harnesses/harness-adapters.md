# Provider Adapters for Phase Dispatch

How to spawn agents and dispatch phase work across different AI coding clients.

## Provider Detection

Detect the current provider at runtime by checking available tools:

| Check | Provider | Parallel Support |
|-------|----------|-----------------|
| `Agent` tool available | claude-code | Yes (run_in_background) |
| `delegate_task` tool available | hermes | Yes (ThreadPoolExecutor, default 3-concurrent) |
| `process` tool available | openclaw | Yes (action: log) |
| `run_shell_command` tool available | gemini | No (sequential fallback) |
| None of the above | generic | No (sequential fallback) |

## Agent Spawning

| Client | Tool | How to Spawn |
|--------|------|-------------|
| Claude Code | Agent tool | `Agent({subagent_type, prompt, run_in_background: true})` |
| Hermes | delegate_task tool | `delegate_task({goal, model_override?})` - see below |
| OpenClaw | process tool | `process({action: "log", command: "openclaw -p '{prompt}'"})` |
| Gemini CLI | sequential fallback | Execute inline, one task at a time |
| Codex CLI | spawn_agent | `spawn_agent({prompt})` |

### Hermes

Dispatch primitive: `delegate_task` (see hermes `tools/delegate_tool.py:680`).

- Synchronous from the parent's perspective: parent blocks on `ThreadPoolExecutor.map`.
- Parallel children: default 3 concurrent, tunable via `delegation.max_concurrent_children` in hermes config.
- Per-child model: pass `model_override` in the delegate_task call to route a specific child to a different model.
- Shared state: aggregated result is returned synchronously; there is no pub/sub or streaming back to the parent before children finish.
- Iteration budget: each child has its own `delegation.max_iterations` (default 50), separate from the parent's budget.
- No recursive delegation: a delegated child cannot itself call `delegate_task`.

When to use `delegate_task` vs subprocess fallback:

- **Use `delegate_task`** when the child work is bounded and result-oriented: a code change, a research pass, a test run. The aggregated return flows back without shell plumbing.
- **Fall back to subprocess spawn** (`hermes-agent -p ...`, or `scripts/lib/spawn-subagent.sh`) when a child needs its own session identity, a separate budget, or an independent context-compaction window.

### Openclaw

Dispatch primitive: subprocess-spawn openclaw itself.

- `process({action: "log", command: "openclaw -p '...'"})` for background runs.
- Sequential unless the skill orchestrates parallelism via multiple `process` calls (each spawns its own subprocess).
- No native in-session parallel child tool equivalent to hermes' `delegate_task` in v1.

Advanced: multi-soul orchestration. **Future work, not v1.**

- The `openclaw-persona-forge` skill in ECC (`~/code/tools/everything-claude-code/skills/openclaw-persona-forge/`) generates `SOUL.md` files per persona.
- Spawning `openclaw --soul path/to/orchestrator.md` and multiple `openclaw --soul path/to/worker-N.md` in parallel creates a heterogeneous subagent fleet.
- V1 of footnote does NOT implement multi-soul orchestration inside the autonomous loop; single-soul subprocess spawn is the default. Multi-soul support is listed in the future-work section of `docs/SETUP-OPENCLAW.md`.

## Review Phase Dispatch

The review phase benefits most from parallel agent dispatch. Adapt based on client:

### Claude Code (full parallel)

Spawn 2+ agents via Agent tool with `run_in_background: true`.
Available subagent_types: `code-reviewer`, `silent-failure-hunter`, `ux-flow-tester`, `integration-test-analyzer`, `multi-device-checker`.

```
Agent({
  subagent_type: "code-reviewer",
  prompt: "Review changes on branch...",
  run_in_background: true
})
```

### OpenClaw (parallel via process)

Spawn background processes. Poll for completion.
Use the coding-agent pattern from OpenClaw's bundled skills.

```
process({
  action: "log",
  command: "openclaw -p 'Review the changes for bugs and security issues...'"
})
```

### Gemini CLI / Codex CLI (sequential)

Run review inline in the main thread. Execute checks sequentially:
1. Run silent-failure-hunter grep patterns manually
2. Run code-reviewer checklist inline
3. Run automated checks (typecheck, lint, build)

This is slower but functionally equivalent.

## Execute Phase Dispatch

For parallel wave execution:

### Claude Code

Use Agent tool per task in the wave, with `run_in_background: true` for parallel tasks.

### OpenClaw

Use process tool per task, with `action: "log"` for background execution.

### Gemini / Codex / Generic

Execute tasks sequentially within each wave. No parallelism available.

## Ship Phase

PR creation is identical across all clients - uses `gh` CLI directly. No agent dispatch needed.

## External Review Phase

All clients use the same `gh api` polling pattern. The only difference is how the wait is implemented:
- Claude Code: CronCreate for scheduled checks
- Other clients: inline polling loop with sleep intervals
