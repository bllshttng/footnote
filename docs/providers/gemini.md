# Gemini Provider Guide

This repository supports Gemini through a two-tier adapter:

- Stable mode: sequential fallback with Gemini hooks and shared skills
- Experimental mode: project-scoped agents in `.gemini/agents/` for upgraded orchestration behavior

## What works in stable mode

- `GEMINI.md` project context
- Gemini extension manifest via `gemini-extension.json`
- Hook-assisted lifecycle via `hooks/hooks-gemini.json`
- Shared skills under `skills/`
- Sequential `do`, `operator`, and `target` execution when agent upgrade prerequisites are missing

## Experimental project-agent mode

Gemini can upgrade beyond stable fallback when all of these are true:

1. `config.gemini_experimental_agents: true` in `.fno/settings.yaml`, or `FNO_GEMINI_EXPERIMENTAL_AGENTS=1`
2. Generated project agents exist in `.gemini/agents/`
3. Gemini CLI has experimental agents enabled in its own settings

Generate the repo-scoped Gemini agents with:

```bash
python3 scripts/sync-gemini-agents.py --write
python3 scripts/sync-gemini-agents.py --check
```

Then refresh them inside Gemini CLI:

```text
/agents reload
```

## Quick start

Stable fallback works without any extra steps beyond the Gemini extension and hooks.

For experimental project-agent mode:

```bash
python3 scripts/sync-gemini-agents.py --write
bash scripts/test-sync-gemini-agents.sh
```

## Behavior rules

- The runtime must never silently assume Gemini agent support.
- If opt-in or generated artifacts are missing, Gemini stays on sequential fallback with an explicit downgrade reason.
- Hooks improve lifecycle continuity, but hooks alone do not enable agent-backed orchestration.

## Migration

Existing Gemini users do not need to change anything to keep the current sequential flow.

The project-agent surface is additive. If you opt in, keep the generated `.gemini/agents/` files synced from the shared sources, just like `.codex/agents/` for Codex.
