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

## Soft hooks (Codex workaround)

Codex does not yet provide stable native lifecycle hooks. Use script checkpoints from router skills:

- `scripts/hooks/session-start.sh`
- `scripts/hooks/pre-compact.sh`
- `scripts/hooks/pre-tool-use.sh`
- `scripts/hooks/session-end.sh`

These are best-effort controls and should be treated as mandatory workflow steps in Codex sessions.

## Dependency model

Core dependencies:
- `bash`
- `git`
- `gh`
- `jq`

Optional dependencies are reported by `./scripts/doctor.sh`.
