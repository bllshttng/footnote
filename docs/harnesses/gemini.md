<!-- style-exception: mechanical verb rename preserves pre-existing prose -->
# Gemini Provider Guide

> **Deprecated upstream.** Google has deprecated the Gemini CLI; its successor, Antigravity (`agy`), is a separate harness with its own adapter (see [HARNESSES.md](../HARNESSES.md)). Gemini remains supported here in sequential mode, but it receives no new capability work.

Gemini runs sequentially: shared skills plus Gemini hooks, with no concurrent subagent dispatch.

The experimental project-agent mode (`.gemini/agents/` + a `gemini_experimental_agents` opt-in, surfaced as `provider_mode: experimental_agents`) has been **removed**. It was an opt-in hedge on the Gemini CLI gaining subagent parity, and the CLI's deprecation settled that question. `harness_mode` / `provider_mode` are now constant `standard` for every harness.

## What works

- `GEMINI.md` project context
- Gemini extension manifest via `gemini-extension.json`
- SessionStart context hook (vision, `fno whoami`, worktree hygiene, first-run setup nudge), installed with `fno setup cli-hooks` into `~/.gemini/settings.json` (`hooks.SessionStart` -> `hooks/session-start.sh`). Idempotent, backs up first, never clobbers your other hooks.
- Shared skills under `skills/`
- Sequential `do`, `operator`, and `target` execution

## Quick start

Nothing beyond the Gemini extension and hooks. There is no opt-in step.

## Behavior rules

- The runtime never assumes Gemini subagent support; parallel waves downgrade to sequential main-thread dispatch (`SEQUENTIAL_FALLBACK_PROVIDERS` in `skills/execute/orchestrator.py`).
- Hooks improve lifecycle continuity, but hooks alone do not enable agent-backed orchestration.

## Migration

Nothing to do. If you previously set `config.gemini_experimental_agents` or `FNO_GEMINI_EXPERIMENTAL_AGENTS`, both are now inert and can be deleted; execution was already sequential in every case where the opt-in was not fully satisfied.

`.gemini/agents/` files are no longer consulted by the runtime.
