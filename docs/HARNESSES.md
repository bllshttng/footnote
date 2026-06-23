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

Other CLIs (Cursor, GitHub Copilot Agents, Kiro, OpenCode, Pi, Qoder, Rovo Dev, Trae) are out of scope for footnote orchestration.

## What this means in practice

- **Skills are portable markdown** and work on every CLI in scope.
- **The autonomous target loop** runs natively on Claude Code, Codex, and Gemini: a stop-equivalent hook blocks session exit until a `<promise>` tag appears. Hermes and Openclaw use a loop wrapper (`scripts/run-target-loop.sh --driver <name>`), which polls for the same tag.
- **Parallel subagent dispatch** (`/review sigma`, `/speculate`) needs Claude Code; everywhere else those skills run sequentially.
- **Context file:** footnote makes `AGENTS.md` canonical; `CLAUDE.md` and `GEMINI.md` are one-line stubs that import it, so every CLI inlines identical content.

## Official CLI documentation

| CLI | Docs |
|---|---|
| Claude Code | https://code.claude.com/docs |
| Codex CLI | https://developers.openai.com/codex |
| Gemini CLI | https://geminicli.com/docs |

For per-skill cross-CLI consequences see [docs/SKILL-COMPAT-MATRIX.md](SKILL-COMPAT-MATRIX.md); for how footnote wires into each CLI's hook surface see [docs/architecture/multi-cli-hooks.md](architecture/multi-cli-hooks.md).
