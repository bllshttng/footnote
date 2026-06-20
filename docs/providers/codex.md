# Codex Provider Guide

This repository is a Claude marketplace project with a Codex adapter layer.

## What works in Codex

- Canonical plugin skills via generated symlinks in `.agents/skills`
- Shared automation scripts under `scripts/` and `plugins/*/scripts`
- Shared custom agents in `.codex/agents/`:
  - `archer`
  - `reviewer`
  - `roadmap-generator`
  - `verifier`

## Quick start

```bash
./scripts/setup.sh --provider codex
./scripts/doctor.sh
```

Then start Codex from this repository root so `.agents/skills` is in scope.

## Native lifecycle hooks

Codex now exposes native lifecycle hooks (`SessionStart`, `Stop`, `PreToolUse`, etc.) in `~/.codex/config.toml`. footnote's SessionStart context hook (project vision, `fno whoami`, worktree hygiene, and the first-run setup nudge) is the same `hooks/session-start.sh` wrapper Claude Code and Gemini use; it emits the unified `hookSpecificOutput.additionalContext` contract all three CLIs share.

Install it into your Codex config:

```bash
fno setup cli-hooks             # writes ~/.codex/config.toml (and ~/.gemini/settings.json)
fno setup cli-hooks --no-gemini # Codex only
```

The writer is idempotent, backs up `config.toml` first, and never clobbers your other hooks. **Codex treats a newly added hook as untrusted**, so after installing you must approve the footnote SessionStart hook in Codex before it runs.

### Soft-hook checkpoints (legacy)

The older skill-invoked checkpoints still exist for environments without native hooks:

- `scripts/hooks/session-start.sh`
- `scripts/hooks/pre-compact.sh`
- `scripts/hooks/pre-tool-use.sh`
- `scripts/hooks/session-end.sh`

## Dependency model

Core dependencies:
- `bash`
- `git`
- `gh`
- `jq`

Optional dependencies are reported by `./scripts/doctor.sh`.
