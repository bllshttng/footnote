# Harness Capabilities Reference

footnote runs as a host runtime on several AI coding CLIs. This is the public summary of what works where; the exhaustive substrate facts (per-event hook mappings, frontmatter matrices, directory conventions, variable substitution) are maintained internally.

## CLIs in scope

| CLI | Role for footnote | Parallel subagents |
|---|---|---|
| Claude Code | Native target. All hooks, all features. | Yes |
| Codex CLI | Native (skills + `AGENTS.md`). | Sequential |
| Gemini CLI | Multi-CLI hook integration. | Sequential |
| Hermes | Loop-wrapper path. | Sequential |
| Openclaw | Loop-wrapper path. | Sequential |
| OpenCode | Native stop-hook plugin (world-gated, in-session re-drive) + loop-wrapper fallback. Reads `AGENTS.md` natively. | Sequential |
| Antigravity CLI (`agy`) | Native `Stop`-hook adapter (world-gated, `decision:"continue"` re-drive). Claude-shaped hook events, Gemini-family wire format. | Sequential |

Other CLIs (Cursor, GitHub Copilot Agents, Kiro, Pi, Qoder, Rovo Dev, Trae) are out of scope for footnote orchestration.

## What this means in practice

- **Skills are portable markdown** and work on every CLI in scope.
- **The autonomous target loop** runs natively on Claude Code, Codex, and Gemini: a stop-equivalent hook blocks session exit until a `<promise>` tag appears. Hermes and Openclaw use a loop wrapper (`scripts/run-target-loop.sh --driver <name>`), which polls for the same tag. OpenCode is first-class: `fno setup` installs a local-file plugin (`~/.config/opencode/plugins/footnote.js`, no npm needed) that hooks `session.idle`, synthesizes a transcript, and shells `fno-agents loop-check` for the SAME world-gated completion check claude uses (promise scan + PR-for-HEAD + CI green + bots reviewed + no blocking finding). On a non-terminal decision it re-drives the same session in-context via `client.session.prompt`; on a terminal decision loop-check emits the `termination` event itself. loop-check is the sole completion authority, so OpenCode and Claude Code share one gate with no drift. **Antigravity CLI (`agy`)** is native the same way through a different surface: `fno setup` registers a `Stop`-hook adapter (`hooks/agy-target-stop-hook.sh`) in agy's `hooks.json` (`~/.gemini/config/hooks.json`). agy's hooks use Claude-shaped event names but a Gemini-family wire format (camelCase stdin, `decision:"continue"` to keep working, JSON-only stdout), so the adapter synthesizes a claude-shaped transcript from agy's `transcript.jsonl` and shells the SAME `fno-agents loop-check` gate. `fullyIdle == false` keeps the session working until background tasks finish; a missing binary allows the stop (never an unstoppable loop) while a transient gate failure continues and retries.
- **Parallel subagent dispatch** (`/review sigma`, `/speculate`) needs Claude Code; everywhere else those skills run sequentially.
- **Context file:** footnote makes `AGENTS.md` canonical; `CLAUDE.md` and `GEMINI.md` are one-line stubs that import it, so every CLI inlines identical content.

## Official CLI documentation

| CLI | Docs |
|---|---|
| Claude Code | https://code.claude.com/docs |
| Codex CLI | https://developers.openai.com/codex |
| Gemini CLI | https://geminicli.com/docs |
| OpenCode | https://opencode.ai/docs |
| Antigravity CLI (`agy`) | https://antigravity.google/docs/cli/reference |

For per-skill cross-CLI consequences see [docs/SKILL-COMPAT-MATRIX.md](SKILL-COMPAT-MATRIX.md); for how footnote wires into each CLI's hook surface see [docs/architecture/multi-cli-hooks.md](architecture/multi-cli-hooks.md).
