# Install footnote under Hermes Agent

Run footnote skills under Hermes Agent instead of Claude Code. The loop family (target, execute) runs through the loop wrapper.

## Prerequisites

- Hermes Agent, with the `hermes` CLI on PATH (installed per its own instructions)
- A working default model in `~/.hermes/config.yaml`
- `git` and `bash`

```bash
command -v hermes
hermes chat -q 'Reply with: ok' -Q
command -v bash
```

The second command must print a reply. If it fails, fix the model or provider first, for example with `hermes config set model.default <model>`. The `hermes-agent` command is Hermes's legacy runner. footnote does not use it.

## 1. Install footnote skills

Hermes loads skills from `~/.hermes/skills/`. Symlink the footnote skills directory into that path:

```bash
mkdir -p ~/.hermes/skills
ln -sfn /path/to/footnote/skills ~/.hermes/skills/footnote
```

Verify the symlink:

```bash
ls ~/.hermes/skills/footnote/
```

`hermes chat -q "/think what should I build next"` now loads the `think` skill and runs it. The `hermes` column in [docs/harnesses/verb-matrix.md](./harnesses/verb-matrix.md) reads `unmeasured` on every verb until a capability row is measured for it. [SKILL-COMPAT-MATRIX.md](./SKILL-COMPAT-MATRIX.md) explains the cells.

## 2. Install the loop wrapper

The wrapper is `fno-agents loop run` with the `hermes` driver (`scripts/lib/driver-hermes.sh`). Each iteration is one `hermes chat -q` turn with `--format stream-json --yolo`. The turn's last JSONL record is `result`, and it carries the Hermes session id. The next iteration passes that id to `--resume`, so Hermes keeps the conversation in its own session store.

```bash
mkdir -p ~/.local/bin
ln -sfn /path/to/footnote/scripts/run-target-loop.sh ~/.local/bin/run-target-loop
```

Add `~/.local/bin` to `$PATH` if it is not already. Then:

```bash
run-target-loop --driver hermes --max-iter 10 --prompt-file /tmp/my-prompt.txt
```

`--max-turns` maps to Hermes `--max-turns`, the tool-call cap per turn. `--model` maps to Hermes `--model`.

The loop runtime stops a run on a `termination` event. Claude Code emits it from its Stop hook through `fno-agents loop-check`. Hermes does not emit it yet, so a Hermes run continues to `--max-iter` and exits 1, even after the work is done.

## 3. Install the promise-tag reader (optional)

The reader is a Hermes plugin. After each model reply it writes `.fno/target-promise.signal` with the last `<promise>` tag's content. The signal is a record of the model's claim. The loop runtime does not stop on it.

```bash
mkdir -p ~/.hermes/plugins
ln -sfn /path/to/footnote/plugins/hermes/promise-tag-reader \
  ~/.hermes/plugins/promise-tag-reader
hermes plugins enable promise-tag-reader
```

A directory plugin needs a `plugin.yaml` and an entry under `plugins.enabled` in `config.yaml`. Without both, Hermes does not load it. Confirm with `hermes plugins list`.

## 4. Smoke test

```bash
bash tests/ootb/hermes-smoke.sh
```

The script creates a throwaway worktree of your Hermes checkout, symlinks the footnote skill and plugin directories, and runs the wrapper with a hello-world prompt. Exit codes:

- **0** - loop completed, sentinel written, log shows one iteration
- **1** - wrapper failed, see stderr for the captured wrapper output
- **77** - prerequisites missing (`hermes` not on PATH or no checkout at `$HERMES_REPO`). This is the standard skipped signal, not a failure.

Until Hermes emits a `termination` event, the wrapper exits 1 at the iteration cap, so this script fails at the exit-code check.

Override the Hermes checkout path with `HERMES_REPO=/path/to/hermes-agent bash tests/ootb/hermes-smoke.sh`.

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| Wrapper exits 77 | `hermes` not on PATH | Add it to PATH, or set `HERMES_CLI` to the binary. |
| Every iteration fails with `HTTP 404` in the `result` record | The default model is gone | Set a new one with `hermes config set model.default <model>`. |
| Wrapper runs to `--max-iter` | No `termination` event from Hermes | Expected until Hermes emits one, see section 2. |
| `.env` file writes blocked | footnote skill policy | Expected. Use `.env.local` or `.envrc` for secrets. |

## Switching drivers mid-project

Set `$FNO_DRIVER` to override auto-detection:

```bash
FNO_DRIVER=claude-code run-target-loop --prompt-file ...
```

Use it to run the same repo under Claude Code and Hermes on alternate days.

## Known limitations

- Subagent dispatch on Hermes uses `delegate_task` (see `docs/harnesses/harness-adapters.md`).
- Cache-metric features from Claude Code (`token-doctor`) are not available on Hermes. See [SKILL-COMPAT-MATRIX.md](./SKILL-COMPAT-MATRIX.md).
- The loop does not stop on its own yet, see section 2.

## What next

- Run `hermes chat -q "/target fix the typo in README"` to see one target turn.
- Read [SKILL-COMPAT-MATRIX.md](./SKILL-COMPAT-MATRIX.md) to plan which footnote skills fit your workflow.
- For OpenClaw, see [SETUP-OPENCLAW.md](./SETUP-OPENCLAW.md).
