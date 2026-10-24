# Optional hooks fail open fast

A hook that waits blocks a turn. Context that arrives late is worth less than a turn that starts on time. When the machine is busy, every optional footnote hook runs under one load-aware budget that pays LESS attention to context, never more.

## The budget

`scripts/lib/hook-budget.sh` is the one budget. It reads the one-minute load average and the online core count, then picks a tier:

| Machine state | Budget | Meaning |
|---|---|---|
| Idle: load1 <= cores | 3s | the generous read |
| Loaded: load1 > cores | 1s | shorten, do not lengthen |
| Overloaded: load1 >= 8x cores | skip | `hook_overloaded` is true; the hook exits 0 before its preamble |
| Load unreadable | 3s | fail open; the wall-clock bound still caps |
| Async run (`FNO_HOOK_ASYNC=1`) | 10s | the harness runs it in the background, so no turn waits; the overload skip still applies |

There is no zero-budget tier. CI runners proved why: under shard load, a zero budget silenced hooks whose contracts require the read to run, and five suites caught it. The busy tier's 1s bound is the floor, and unreadable load always runs.

The overloaded tier is the one skip, and only for a readable load. At 8+ waiting jobs per core the hook's own preamble can pass the harness's 4s outer cap on its own. Running the read then guarantees a killed hook, discarded output, a red warning, and a delayed turn. Skipping leaves every cursor untouched: mail stays pending, an announcement is re-seen, an offer is re-scanned next turn. The 2026-10-06 screenshots showed five prompt hooks timing out on every prompt at 16 to 60 waiting jobs per core. A suite that runs a real hook pins `FNO_HOOK_BUDGET_SKIP_PER_CORE` past any runner load, so CI never trips the tier. Production leaves it unset. A fired bound still reads as silence: exit 0 with empty output. A turn never inherits an error from optional context. The bound rides `with_timeout` from `scripts/lib/with-timeout.sh`, which needs no coreutils `timeout` and works on stock macOS.

Three probes sit outside the budget because their EXIT CODE is the data. `frontdoor-nudge-session-start`: 2 means fno-py, 124 means a wedged socket that still proves the Rust door. `worktree-peers-session-start`: 124 reads as staleness-unknown. `inject-fno-agent-whoami`: the suite pins the cap's duration. Each keeps its fixed `with_timeout` bound from before the budget existed. A fired bound can land on a loaded runner's fork latency. A probe whose answer rides the exit code must not shorten its bound with the load.

## Which hooks ride the budget

The optional families: context, nudge, inject, announce. That covers `prompt-outstanding`, `born-with-why-offer-inject`, `inject-mail-notify`, `inject-announce`, `law-stage-inject`, `inject-fno-agent-whoami`, `inject-mail-drain-session-start`, `outstanding-session-start`, `worktree-peers-session-start`, `frontdoor-nudge-session-start`, `agy-team-inject`, `context-nudge` (reads), and `operator-capture-nudge`. Both `hooks/hooks.json` and `hooks/codex-hooks.json` set each one's `timeout` entry as a BACKSTOP just above the internal budget. A wedged hook that ignores its own bound still dies at the harness layer.

Two Stop-path context reads also serve from a stale-while-revalidate cache: the context nudge probe and the operator-capture queue depth. A fresh copy costs milliseconds. A served copy past two thirds of its life arms a detached refresher, so the refresh happens off the turn path. A live read that skipped or expired under load serves the stale copy rather than nothing. The nudge probe carries a fingerprint of the transcript (size and mtime) inside the cache file. A grown transcript always re-measures, so the gate never serves pressure truth stale by choice. Keys are per session or per transcript. Files live under `~/.fno/cache/hook-budget/`.

## Gates that decide keep their own budgets

These hooks decide something, so they do not ride the load budget. A decision here blocks, refuses, attests, or delivers control. The list: `target-stop-hook.sh`, every PreToolUse guard, `edit-integrity.sh`, `code-review-attest.sh`, `target-subagent-guard.sh`, `target-stopfailure.sh`, `inject-control-drain-tool-boundary.sh`, and the SessionStart runner `context-run.sh`. The PreToolUse guards by name: `write-gate` (the graph/state, config-dir and generated-copies surfaces in one native gate), `worktree-write-protect`, `join-partition-write-guard`, `plan-location-guard`, `lead-delegation-guard`, `pretooluse-bash-dispatch`, `effect-guard-dispatch`, `subagent-worktree-guard`, and `review-hold`. The PermissionRequest hook `worktree-relocation-allow` decides too. The refusal-streak park check inside `pretooluse-bash` and its PostToolUse recorder `refusal-streak-record.sh` decide too, so both keep their own budgets. The runner's producers carry their own 45s bound. `inject-control-drain-tool-boundary.sh` delivers control mail. With `inbox.tool_boundary_ordinary` on, it delivers ordinary mail too. `inside-leg-report.sh` and `register-session-start.sh` are status and state writers, not optional context. Both keep their own bounds too.

## Four hook classes

Every hook fno registers is one of four classes. `hooks/hook-classes.tsv` holds the class of each registration, one row per harness, event and script. If a registration has no row, `scripts/ci/check-hook-classes.sh` fails CI. So a new hook must declare its class.

| Class | Runs | May deny | Bound | Fails |
|---|---|---|---|---|
| block | sync | yes | a timeout | open or closed, declared per row |
| wait | sync | no; adds context or writes state | a timeout | open |
| next-turn | async | no | a timeout | open; the result arrives at the next turn |
| rewake | async | no | a timeout | open; an urgent result wakes an idle session |

Only fixed-trigger checks belong in block. The gate also reads the manifest flags. A Claude entry with `"async": true` must be next-turn, and one with `"asyncRewake": true` must be rewake. A sync entry must be block or wait. A block row names its fail mode.

Per harness:

- **Claude.** All four classes are native. Two runs of `law-stage-inject.sh` are next-turn: the PostToolUse `Skill` and `Edit|Write` matchers. The law context arrives at the next model request. The tool call does not wait for the read. They carry `FNO_HOOK_ASYNC=1`, so `hook_budget_secs` gives them a 10s budget that load does not shorten. The overload skip still applies. The UserPromptSubmit run stays wait, because a law must arrive before the reply. No fno hook is rewake yet.
- **Codex.** `hooks/codex-hooks.json` has no async flag, so every Codex row is block or wait. A next-turn or rewake result on Codex has to ride the mail bus and arrive through the turn-boundary delivery ([mail-push-first-delivery](architecture/mail-push-first-delivery.md)). No Codex hook does that yet.
- **fnh.** fnh runs block (with fail closed), next-turn, and rewake natively. A rewake that ends after the fnh process exits is lost. A wake across processes needs the launcher.
- **OpenCode and Pi.** Their hooks are in-process plugin callbacks, not manifest entries, so the table does not cover them.

## Third-party hooks: give yours the same budget

If you add your own hook, wrap it in the same fail-open budget. Do not trust the harness ceiling. The hook can be a codegraph prompt-hook, a project linter, anything that shells out:

```bash
#!/usr/bin/env bash
set -uo pipefail
# Point this at the plugin's copy, or vendor scripts/lib/with-timeout.sh
# and scripts/lib/hook-budget.sh into your project.
source /path/to/footnote/scripts/lib/hook-budget.sh 2>/dev/null || exit 0
hook_overloaded && exit 0
hook_run_optional your-command --with args
exit 0
```

`hook_overloaded` skips the whole hook past the overloaded tier. `hook_run_optional` applies the tier table and turns a fired bound into silence. Then set a small `timeout` backstop for that hook in `settings.json`, just above the 3s idle budget. The backstop exists for the day the wrapper does not finish in milliseconds.

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
