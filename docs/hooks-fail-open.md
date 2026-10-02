# Optional hooks fail open fast

A hook that waits blocks a turn. Context that arrives late is worth less than a turn that starts on time. When the machine is busy, every optional footnote hook runs under one load-aware budget that pays LESS attention to context, never more.

## The budget

`scripts/lib/hook-budget.sh` is the one budget. It reads the one-minute load average and the online core count, then picks a tier:

| Machine state | Budget | Meaning |
|---|---|---|
| Idle: load1 <= cores | 3s | the generous read |
| Loaded: cores < load1 <= 2x cores | 1s | shorten, do not lengthen |
| Past the threshold: load1 > 2x cores | skip | exit 0, empty output |
| Load unreadable | 3s | fail open; the wall-clock bound still caps |

A fired bound or a skip reads as silence: exit 0 with empty output. A turn never inherits an error from optional context. The bound rides `with_timeout` from `scripts/lib/with-timeout.sh`, which needs no coreutils `timeout` and works on stock macOS.

## Which hooks ride the budget

The optional families: context, nudge, inject, announce. That covers `prompt-outstanding`, `born-with-why-offer-inject`, `inject-mail-notify`, `inject-announce`, `law-stage-inject`, `inject-fno-agent-whoami`, `inject-mail-drain-session-start`, `outstanding-session-start`, `worktree-peers-session-start`, `frontdoor-nudge-session-start`, `agy-crown-inject`, `context-nudge` (reads), and `operator-capture-nudge`. Both `hooks/hooks.json` and `hooks/codex-hooks.json` set each one's `timeout` entry as a BACKSTOP just above the internal budget. A wedged hook that ignores its own bound still dies at the harness layer.

Two Stop-path context reads also serve from a stale-while-revalidate cache: the context nudge probe and the operator-capture queue depth. A fresh copy costs milliseconds. A served copy past two thirds of its life arms a detached refresher, so the refresh happens off the turn path. A live read that skipped or expired under load serves the stale copy rather than nothing. Keys are per session or per transcript. Files live under `~/.fno/cache/hook-budget/`.

## Gates that decide keep their own budgets

These hooks decide something, so they do not ride the load budget. A decision here blocks, refuses, attests, or delivers control. The list: `target-stop-hook.sh`, the PreToolUse guards (`graph-write-protect`, `claude-config-write-guard`, `worktree-write-protect`, `generated-write-guard`, `join-partition-write-guard`, `plan-location-guard`, `king-delegation-guard`, `pretooluse-bash-dispatch`, `effect-guard-dispatch`, `subagent-worktree-guard`, `review-hold`), `edit-integrity.sh`, `code-review-attest.sh`, `target-subagent-guard.sh`, `target-stopfailure.sh`, `inject-control-drain-tool-boundary.sh`, and the SessionStart runner `context-run.sh`. The runner's producers carry their own 45s bound. `inject-control-drain-tool-boundary.sh` is control-lane delivery. `inside-leg-report.sh` and `register-session-start.sh` are status and state writers, not optional context. Both keep their own bounds too.

## Third-party hooks: give yours the same budget

If you add your own hook, wrap it in the same fail-open budget. Do not trust the harness ceiling. The hook can be a codegraph prompt-hook, a project linter, anything that shells out:

```bash
#!/usr/bin/env bash
set -uo pipefail
# Point this at the plugin's copy, or vendor scripts/lib/with-timeout.sh
# and scripts/lib/hook-budget.sh into your project.
source /path/to/footnote/scripts/lib/hook-budget.sh 2>/dev/null || exit 0
hook_run_optional your-command --with args
exit 0
```

`hook_run_optional` applies the tier table and turns a skip or a fired bound into silence. Then set a small `timeout` backstop for that hook in `settings.json`, just above the 3s idle budget. The backstop exists for the day the wrapper does not finish in milliseconds.

```json
{
  "hooks": {
    "UserPromptSubmit": [
      {
        "matcher": "",
        "hooks": [
          {"type": "command", "command": "your-wrapper.sh", "timeout": 4}
        ]
      }
    ]
  }
}
```
