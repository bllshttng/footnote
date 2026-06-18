# Install footnote under openclaw

Run footnote skills - especially the loop family (target, megawalk, operator) - under openclaw instead of Claude Code.

## Prerequisites

- Node.js 22 or later
- [`pnpm`](https://pnpm.io)
- `openclaw` CLI on PATH (installed per its own instructions)
- `git` and `bash`

```bash
node --version      # >= 22
command -v pnpm
command -v openclaw
command -v bash
```

If any of these are missing, install them before continuing. The footnote plugin itself has no runtime dependencies beyond bash and standard Unix tools.

## 1. Install footnote skills

Openclaw loads skills from `~/.openclaw/workspace/skills/<name>/SKILL.md` per the local skill loader (`src/agents/skills/local-loader.ts`). Symlink the footnote skills directory into that path:

```bash
mkdir -p ~/.openclaw/workspace/skills
ln -sfn /path/to/footnote/skills ~/.openclaw/workspace/skills/footnote
```

You can also override the discovery base via `$OPENCLAW_BUNDLED_SKILLS_DIR` if openclaw is installed as a bun-compiled binary. See `src/agents/skills/bundled-dir.ts:39`.

Verify:

```bash
ls ~/.openclaw/workspace/skills/footnote/ | head -5
```

Invoking `openclaw -p "/think what should I build next"` now loads the `think` skill and runs it. Every skill marked `OOTB` for the `OC` column in [SKILL-COMPAT-MATRIX.md](./SKILL-COMPAT-MATRIX.md) works at this point. The loop family needs the next two steps.

## 2. Install the loop wrapper

The wrapper runs openclaw as a subprocess, scans its stdout for `<promise>MISSION COMPLETE</promise>`, and re-invokes it with conversation history re-hydrated until the tag appears or a safety cap is hit. Openclaw's `agent_end` hook (`plugins/hook-types.ts:62`) is observational only, not blocking, so an external wrapper is required for autonomous execution.

```bash
mkdir -p ~/.local/bin
ln -sfn /path/to/footnote/scripts/run-target-loop.sh ~/.local/bin/run-target-loop
```

Add `~/.local/bin` to `$PATH` if it is not already. Then:

```bash
run-target-loop --driver openclaw --max-iter 10 --prompt-file /tmp/my-prompt.txt
```

Auto-detection resolves `--driver openclaw` when `openclaw` is on PATH and no `$CLAUDECODE_SESSION_ID` or `$HERMES_SESSION_ID` is set.

## 3. Install the promise-tag reader (optional but recommended)

Without the reader, the wrapper falls back to raw `grep <promise>MISSION COMPLETE</promise>` on openclaw stdout. The grep path is real and works, but it has edge cases (tag nested in a code block, chunked output, ANSI wrapping).

The reader plugin uses openclaw's `before_agent_reply` hook (`src/plugins/hook-types.ts:55-84`), gets the draft response before the user sees it, and writes `.fno/target-promise.signal` with the last tag's inner content.

### Option A (preferred, portable): SKILL.md-side sentinel

This is already baked into `skills/target/SKILL.md`. When the assistant emits a `<promise>` tag, the skill instructs it to also write `.fno/target-promise.signal`. No plugin code required.

No action needed if you are running a recent footnote checkout.

### Option B (openclaw-specific reinforcement): TypeScript plugin

Robust against model regressions where the LLM forgets the Option A instruction.

```bash
mkdir -p ~/.openclaw/plugins
ln -sfn /path/to/footnote/plugins/openclaw/promise-tag-reader \
  ~/.openclaw/plugins/promise-tag-reader
```

The plugin implements the typed `before_agent_reply` hook (see the plugin's `index.ts`). It has no runtime dependencies beyond Node's standard library.

Verify:

```bash
openclaw --list-plugins 2>&1 | grep promise-tag-reader
```

## 4. Smoke test

From any repo (throwaway worktrees are fine):

```bash
cd /tmp
git init openclaw-target-smoke
cd openclaw-target-smoke
mkdir -p .fno

cat > /tmp/openclaw-smoke-prompt.txt << 'EOF'
Output exactly this and nothing else:

<promise>MISSION COMPLETE: smoke test</promise>
EOF

run-target-loop --driver openclaw --max-iter 3 --prompt-file /tmp/openclaw-smoke-prompt.txt
```

Expected outcome:

- Exit code 0
- `.fno/target-promise.signal` exists and contains `MISSION COMPLETE: smoke test`
- `.fno/target-loop.log` shows exactly one iteration

### Scripted verification

Run the full smoke test harness to verify the install end-to-end:

```bash
bash tests/ootb/openclaw-smoke.sh
```

The script creates a throwaway worktree of your openclaw checkout, symlinks the footnote skill and plugin directories, and runs the wrapper with a hello-world prompt. Exit codes:

- **0** - loop completed, sentinel written, log shows one iteration
- **1** - wrapper failed; see stderr for the captured wrapper output
- **77** - prerequisites missing (`openclaw` not on PATH or no checkout at `$OPENCLAW_REPO`). Not a failure - this is the standard "skipped" signal.

Override the openclaw checkout path with `OPENCLAW_REPO=/path/to/openclaw bash tests/ootb/openclaw-smoke.sh`.

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `run-target-loop: openclaw not found` | CLI not on PATH | Add it to PATH or pass the absolute path; wrapper exits 77 (skipped) when the driver CLI is missing. |
| Wrapper loops forever at max-iter | `<promise>` tag never emitted | Run the skill once without the wrapper and verify the tag appears in the raw output. If not, the model is not honoring the skill instruction. |
| `.fno/target-promise.signal` stale between runs | Wrapper did not clean up | The wrapper deletes the signal at iteration start; if a previous run crashed, remove it manually. |
| Monorepo scope warnings | footnote skill boundary | Expected - footnote respects each project's monorepo scope. Narrow scope with `--scope path/to/project`. |
| Plugin not loaded | Openclaw discovery path mismatch | Confirm the symlink target exists and `openclaw --list-plugins` shows it. Try restarting openclaw. |

## Switching drivers mid-project

Set `$FNO_DRIVER` to override auto-detection:

```bash
FNO_DRIVER=claude-code run-target-loop --prompt-file ...
```

This is useful when running the same repo under Claude Code and openclaw on alternate days.

## Known limitations (v1)

- Subagent dispatch on openclaw uses subprocess-spawn (`process({action: "log", command: "openclaw -p '...'"})`). Sequential unless the skill orchestrates parallelism via multiple `process` calls. See `docs/providers/provider-adapters.md`.
- Multi-soul orchestration (one openclaw-as-orchestrator + N openclaw-as-workers with distinct `SOUL.md` personas) is future work. v1 treats openclaw subagents as single-soul subprocess spawns. The `openclaw-persona-forge` skill in ECC generates the SOUL.md files; integration into the target loop is a later spec.
- Cache-metric features from Claude Code (`token-doctor`) are not available on openclaw. See compatibility in [SKILL-COMPAT-MATRIX.md](./SKILL-COMPAT-MATRIX.md).

## What next

- Run `openclaw -p "/target fix the typo in README"` to see the full loop in action.
- Read [SKILL-COMPAT-MATRIX.md](./SKILL-COMPAT-MATRIX.md) to plan which footnote skills fit your workflow.
- See [SETUP-HERMES.md](./SETUP-HERMES.md) if you also run hermes-agent.
