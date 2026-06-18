# Cross-CLI Tool Mapping

When this skill references Claude Code tools, use the equivalent on your platform:

## Skill Invocation

| Action | Claude Code | Gemini CLI | Codex CLI |
|--------|------------|------------|-----------|
| Load a skill | `Skill` tool with `skill: "fno:plan"` | `activate_skill` tool with skill name | Skills auto-loaded; reference with `$skill-name` |
| Run a slash command | `/fno:plan` | `/fno:plan` (if command exists) or `activate_skill` | `$footnote-plan` |

## Subagent Dispatch

| Action | Claude Code | OpenClaw | Gemini CLI | Codex CLI |
|--------|------------|----------|------------|-----------|
| Spawn subagent | `Agent` tool with prompt + subagent_type | `process` tool with `action: "log"` for background, `action: "run"` for foreground | Default: sequential fallback. Optional: project agents in `.gemini/agents/` when experimental agents are enabled and opted in | `spawn_agent` / project-scoped custom agents in `.codex/agents/` |
| Isolated worktree agent | `Agent` tool with `isolation: "worktree"` | `process` tool targeting worktree directory | Default: use manual git worktree. Experimental Gemini project agents are same-repo helpers, not worktree isolation | Use custom agents for same-repo work; use manual git worktree for isolation |
| Parallel dispatch | Multiple `Agent` calls with `run_in_background: true` | Multiple `process` calls with `action: "log"` | Not supported natively - execute sequentially | Limited - use `spawn_agent` |

## File Operations

| Action | Claude Code | Gemini CLI | Codex CLI |
|--------|------------|------------|-----------|
| Read file | `Read` | `read_file` | `Read` |
| Write file | `Write` | `write_file` | `Write` |
| Edit file | `Edit` | `replace` | `Edit` |
| Run command | `Bash` | `run_shell_command` | `Bash` |
| Search files | `Grep` | `search_file_content` | `Grep` |
| Find files | `Glob` | `glob` | `Glob` |

## Shared Helper CLIs

| Action | Claude Code | Gemini CLI | Codex CLI |
|--------|------------|------------|-----------|
| Backlog task management | `fno backlog ...` | `fno backlog ...` | `fno backlog ...` |
| Discovery brief generation | `python3 scripts/discovery-brief.py ...` | `python3 scripts/discovery-brief.py ...` | `python3 scripts/discovery-brief.py ...` |
| Roadmap validation | `bash scripts/validate-roadmap.sh` | `bash scripts/validate-roadmap.sh` | `bash scripts/validate-roadmap.sh` |
| Scope coordination | `python3 scripts/scope-coordinator.py ...` (when present) | `python3 scripts/scope-coordinator.py ...` (when present) | `python3 scripts/scope-coordinator.py ...` (when present) |

## Session Loop (Stop Hook)

| Action | Claude Code | Gemini CLI | Codex CLI |
|--------|------------|------------|-----------|
| Block exit | `{"decision":"block","reason":"..."}` | `{"decision":"deny","reason":"..."}` | `{"decision":"block","reason":"..."}` |
| Hook event | `Stop` | `AfterAgent` | `Stop` |
| Last output source | Grep transcript JSONL | `prompt_response` in hook input | `last_assistant_message` in hook input |

## Graceful Degradation

If a tool isn't available on your platform:
- **No configured custom agents**: Hooks handle multi-CLI agent adaptation automatically. Execute the wave sequentially if agent dispatch is unavailable.
- **No `Skill` tool**: Read the skill file directly with your file read tool, then follow its instructions.
- **No stop hook**: The autonomous loop won't work. Use `/think` → `/blueprint` → `/do` as a manual 3-step workflow instead.
