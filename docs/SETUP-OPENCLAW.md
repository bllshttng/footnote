# Install footnote under openclaw

Run footnote skills - especially the loop family (target, execute) - under openclaw instead of Claude Code.

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

`openclaw agent --session-key fno-try --message "/think what should I build next"` now loads the `think` skill and runs it. The `openclaw` column in [docs/harnesses/verb-matrix.md](./harnesses/verb-matrix.md) reads `unmeasured` on every verb until a capability row is measured for it. [SKILL-COMPAT-MATRIX.md](./SKILL-COMPAT-MATRIX.md) explains the cells. The loop family needs the next two steps.

## 2. Install the loop wrapper

The wrapper is `fno-agents loop run` with the `openclaw` driver (`scripts/lib/driver-openclaw.sh`). Each iteration is one `openclaw agent --json` turn through the Gateway. Set `OPENCLAW_LOCAL=1` to run the embedded agent with `--local` instead. The first turn opens a fresh session key. The JSON reply carries `sessionId`, and the next iteration passes it to `--session-id`, so openclaw keeps the conversation in its own store. openclaw has no per-turn tool cap, so `--max-turns` is not passed.

The loop runtime stops a run on a `termination` event. Claude Code emits it from its Stop hook through `fno-agents loop-check`. openclaw does not emit it yet, so an openclaw run continues to `--max-iter` and exits 1, even after the work is done.

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

- `.fno/target-promise.signal` exists and contains `MISSION COMPLETE: smoke test`
- Exit code 1 at the iteration cap, until openclaw emits a `termination` event (see section 2)

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
| Wrapper runs to `--max-iter` | No `termination` event from openclaw | Expected until openclaw emits one, see section 2. |
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

- Subagent dispatch on openclaw uses subprocess-spawn (`process({action: "log", command: "openclaw agent --session-key <key> --message '...'"})`). Sequential unless the skill orchestrates parallelism via multiple `process` calls. See `docs/harnesses/harness-adapters.md`.
- Multi-soul orchestration (one openclaw-as-orchestrator + N openclaw-as-workers with distinct `SOUL.md` personas) is future work. v1 treats openclaw subagents as single-soul subprocess spawns. The `openclaw-persona-forge` skill in the upstream skills pack generates the SOUL.md files. Integration into the target loop is a later spec.
- Cache-metric features from Claude Code (`token-doctor`) are not available on openclaw. See compatibility in [SKILL-COMPAT-MATRIX.md](./SKILL-COMPAT-MATRIX.md).

## What next

- Run `openclaw agent --session-key fno-try --message "/target fix the typo in README"` to see one target turn.
- Read [SKILL-COMPAT-MATRIX.md](./SKILL-COMPAT-MATRIX.md) to plan which footnote skills fit your workflow.
- See [SETUP-HERMES.md](./SETUP-HERMES.md) if you also run hermes-agent.
