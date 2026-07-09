# Codex Provider Guide

footnote ships a native Codex plugin plus a local-development fallback.

## Native Quick Start

The repo carries a local Codex marketplace fixture at `.agents/plugins/marketplace.json`.
It points at this checkout and exposes `.codex-plugin/plugin.json`.

```bash
codex plugin marketplace add .
codex plugin add fno@footnote-local
```

Then install `fno` from that marketplace in the Codex app. The plugin manifest exposes:

- skills from `skills/`
- a plugin-bundled `SessionStart` hook through `hooks/codex-hooks.json`
- project custom agents from tracked `.codex/agents/*.toml`

Codex treats plugin hooks as untrusted until you approve them. Approve the footnote
`SessionStart` hook when prompted; it injects project vision, `fno whoami`, worktree
hygiene, and setup nudges through `hookSpecificOutput.additionalContext`.

## Codex App Worktrees

Codex app worktrees are managed under `$CODEX_HOME/worktrees` and start from tracked
files in the selected Git branch. That is why `.codex/agents/*.toml`,
`.codex-plugin/plugin.json`, `hooks/codex-hooks.json`, and
`.agents/plugins/marketplace.json` are committed.

Use the Codex app Worktree mode for background tasks. When a worktree needs a branch
and PR, use **Create branch here** in the app, then push and open the PR. Use Handoff
when you want to move the thread and code between Local and a Codex-managed worktree.

footnote's CLI-created worktrees under `~/conductor/workspaces/` are still used by
`/fno:target` and background dispatch. They are separate from Codex app managed
worktrees.

## Local-Development Fallback

For older Codex builds or CLI-only sessions where plugin-bundled hooks are unavailable,
wire the SessionStart hook into user config:

```bash
fno setup cli-hooks-codex
```

The compatibility command remains available:

```bash
fno setup cli-hooks --no-gemini
```

Native plugin hooks are preferred when the Codex build supports them. The user-level
`$CODEX_HOME/config.toml` hook is a fallback for local development; Codex records its
approval separately under `[hooks.state]`. Check the effective fallback wiring and trust
state without modifying either hook layer:

```bash
fno doctor --codex-hooks
```

Codex may report `loading hooks from both ... hooks.json and ... config.toml` when the
legacy `$CODEX_HOME/hooks.json` and preferred TOML layer both contain SessionStart hooks.
If the JSON entries are footnote-owned, migrate only those entries with:

```bash
fno setup cli-hooks-codex --migrate-legacy-hooks-json
```

The migration preserves foreign JSON hooks. For example, a `herdr-agent-state.sh` hook is
not owned by footnote and remains in `hooks.json`; consolidate it into `config.toml`
manually if desired. Do not delete the legacy file until every foreign hook has been
accounted for.

For dev-only skill symlinks:

```bash
./scripts/setup.sh --provider codex
```

This populates `.agents/skills/plugin--fno--*` without replacing the native plugin
marketplace fixture.

## Custom Agents

Codex reads project custom agents recursively from `.codex/agents/*.toml`. Those files
are generated from canonical `agents/*.md` definitions:

```bash
python scripts/sync-codex-agents.py
python scripts/sync-codex-agents.py --check
```

Run the generator after changing `agents/*.md`. The check mode fails when generated
Codex agents are missing, stale, or no longer parse as TOML.

The generator preserves native Codex model names plus explicit `sandbox_mode` and
`nickname_candidates` fields. Claude-only model tiers (`haiku`, `sonnet`, `opus`, and
`inherit`) are omitted so Codex can use its configured model. Source tools determine a
predictable sandbox (`workspace-write` for write-capable tools, otherwise `read-only`),
while Claude-only `skills` and `disallowedTools` remain visible in the generated
developer instructions as behavioral context.

## Target Loop Hooks

Custom agents and target loop hooks are separate surfaces. The files under
`.codex/agents/` make footnote's specialist agents available to Codex; they do not
make `/fno:target` continue autonomously.

Target continuation is driven by hook events. `hooks/codex-hooks.json` wires the
Codex-supported subset needed for target loops: `Stop` for
`hooks/target-stop-hook.sh` (`fno-agents loop-check` + `finalize`), `PostToolUse`
for claim heartbeat/context monitoring, compact handoff hooks, subagent guards,
and the PreToolUse state/git protection guards.

Do not copy the full Claude hook manifest into Codex. Codex does not support every
Claude lifecycle event in `hooks/hooks.json`; `WorktreeCreate`, `CwdChanged`,
`FileChanged`, `SessionEnd`, and `StopFailure` are intentionally excluded here.

## Dependency Model

Core dependencies:

- `bash`
- `git`
- `gh`
- `jq`

Optional dependencies are reported by `./scripts/doctor.sh`.
