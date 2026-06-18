# Install footnote under hermes-agent

Run footnote skills - especially the loop family (target, megawalk, operator) - under hermes instead of Claude Code.

## Prerequisites

- Python 3.11 or later
- [`uv`](https://github.com/astral-sh/uv) package manager
- `hermes-agent` CLI on PATH (installed per its own instructions)
- `git` and `bash`

```bash
python --version    # >= 3.11
command -v uv
command -v hermes-agent
command -v bash
```

If any of these are missing, install them before continuing. The footnote plugin itself has no runtime dependencies beyond bash and standard Unix tools.

## 1. Install footnote skills

Hermes loads skills from `~/.hermes/skills/` (see `agent/skill_commands.py:210`). Symlink the footnote skills directory into that path:

```bash
mkdir -p ~/.hermes/skills
ln -sfn /path/to/footnote/skills ~/.hermes/skills/footnote
```

Verify the symlink:

```bash
ls -la ~/.hermes/skills/footnote/SKILL.md 2>/dev/null || \
  ls ~/.hermes/skills/footnote/ | head -5
```

Invoking `hermes-agent -p "/think what should I build next"` now loads the `think` skill and runs it. Every skill marked `OOTB` for the `HER` column in [SKILL-COMPAT-MATRIX.md](./SKILL-COMPAT-MATRIX.md) works at this point. The loop family needs the next two steps.

## 2. Install the loop wrapper

The wrapper runs hermes as a subprocess, scans its stdout for `<promise>MISSION COMPLETE</promise>`, and re-invokes it with conversation history re-hydrated until the tag appears or a safety cap is hit. Without the wrapper, hermes exits at `api_call_count >= max_iterations` (`run_agent.py:9333`) regardless of whether target considered the work done.

```bash
mkdir -p ~/.local/bin
ln -sfn /path/to/footnote/scripts/run-target-loop.sh ~/.local/bin/run-target-loop
```

Add `~/.local/bin` to `$PATH` if it is not already. Then:

```bash
run-target-loop --driver hermes --max-iter 10 --prompt-file /tmp/my-prompt.txt
```

Auto-detection falls back to `--driver hermes` when `$HERMES_SESSION_ID` is set or `~/.hermes/config.yaml` exists and `hermes-agent` is on PATH.

## 3. Install the promise-tag reader (optional but recommended)

Without the reader, the wrapper falls back to raw `grep <promise>MISSION COMPLETE</promise>` on hermes stdout. The grep path is real and works, but it has edge cases (tag nested in a code block, chunked output, ANSI wrapping).

The reader plugin gets structured access to the final assistant message and writes `.fno/target-promise.signal` with the last tag's content. The wrapper reads that file before falling back to the grep.

### Option A (preferred, portable): SKILL.md-side sentinel

This is already baked into `skills/target/SKILL.md`. When the assistant emits a `<promise>` tag, the skill instructs it to also write `.fno/target-promise.signal`. No bot-side code required.

No action needed if you are running a recent footnote checkout.

### Option B (hermes-specific reinforcement): Python plugin

Robust against model regressions where the LLM forgets the Option A instruction. Install the Python reader plugin:

```bash
mkdir -p ~/.hermes/plugins
ln -sfn /path/to/footnote/plugins/hermes/promise-tag-reader \
  ~/.hermes/plugins/promise-tag-reader
```

Hermes picks up `~/.hermes/plugins/*` on startup. Confirm by running `hermes-agent` and checking its startup log for `plugin loaded: promise-tag-reader`.

## 4. Smoke test

From any repo (throwaway worktrees are fine):

```bash
cd /tmp
git init hermes-target-smoke
cd hermes-target-smoke
mkdir -p .fno

cat > /tmp/hermes-smoke-prompt.txt << 'EOF'
Output exactly this and nothing else:

<promise>MISSION COMPLETE: smoke test</promise>
EOF

run-target-loop --driver hermes --max-iter 3 --prompt-file /tmp/hermes-smoke-prompt.txt
```

Expected outcome:

- Exit code 0
- `.fno/target-promise.signal` exists and contains `MISSION COMPLETE: smoke test`
- `.fno/target-loop.log` shows exactly one iteration

### Scripted verification

Run the full smoke test harness to verify the install end-to-end:

```bash
bash tests/ootb/hermes-smoke.sh
```

The script creates a throwaway worktree of your hermes-agent checkout, symlinks the footnote skill and plugin directories, and runs the wrapper with a hello-world prompt. Exit codes:

- **0** - loop completed, sentinel written, log shows one iteration
- **1** - wrapper failed; see stderr for the captured wrapper output
- **77** - prerequisites missing (`hermes-agent` not on PATH or no checkout at `$HERMES_REPO`). Not a failure - this is the standard "skipped" signal.

Override the hermes checkout path with `HERMES_REPO=/path/to/hermes-agent bash tests/ootb/hermes-smoke.sh`.

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `run-target-loop: hermes-agent not found` | CLI not on PATH | Add it to PATH or pass the absolute path; wrapper exits 77 (skipped) when the driver CLI is missing. |
| Wrapper loops forever at max-iter | `<promise>` tag never emitted | Run the skill once without the wrapper and verify the tag appears in the raw output. If not, the model is not honoring the skill instruction. |
| `.fno/target-promise.signal` stale between runs | Wrapper did not clean up | The wrapper deletes the signal at iteration start; if a previous run crashed, remove it manually. |
| Nix-wrapped invocation fails | Nix shell not detected | Hermes auto-wraps in Nix; pass `--no-nix` if the project does not use Nix. See hermes README. |
| `.env` file writes blocked | footnote skill policy | Expected - use `.env.local` or `.envrc` for secrets. |

## Switching drivers mid-project

Set `$FNO_DRIVER` to override auto-detection:

```bash
FNO_DRIVER=claude-code run-target-loop --prompt-file ...
```

This is useful when running the same repo under Claude Code and hermes on alternate days.

## Known limitations (v1)

- Subagent dispatch on hermes uses `delegate_task` (see `docs/providers/provider-adapters.md`). Parallel children run via hermes `ThreadPoolExecutor`, default 3-concurrent. Configurable per-child model via `model_override`.
- Cache-metric features from Claude Code (`token-doctor`) are not available on hermes. See compatibility in [SKILL-COMPAT-MATRIX.md](./SKILL-COMPAT-MATRIX.md).
- Claude Code's Stop-hook replacement here is the external wrapper loop, not an in-process hook. Signals are delivered via the sentinel file, not via process-return semantics.

## What next

- Run `hermes-agent -p "/target fix the typo in README"` to see the full loop in action.
- Read [SKILL-COMPAT-MATRIX.md](./SKILL-COMPAT-MATRIX.md) to plan which footnote skills fit your workflow.
- See [SETUP-OPENCLAW.md](./SETUP-OPENCLAW.md) if you also run openclaw.
