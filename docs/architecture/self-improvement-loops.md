# Self-improvement loops

The three loops that turn what the fleet did into what the fleet does better. Each one is: capture writers feed a log, a trigger reads the log, the output lands somewhere a later session reads. This page is the map an operator asked for on 2026-09-16: where one loop starts and ends, and which config turns each on.

## The loops

| Loop | Trigger | Config key | Default | Output lands in | Read verb |
|------|---------|-----------|---------|-----------------|-----------|
| autocorrect | Monthly review + a 15-minute S0 watcher (`com.user.autocorrect`, `com.user.autocorrect-watcher`; install with `scripts/install-autocorrect-cron.sh`) | (launchd labels, not config) | not installed | Postmortems to `~/.fno/postmortems/`, lessons to agent memory, backlog nodes via `cli/src/fno/retro/` | `fno backlog find` / the monthly review pack |
| S2 corrections (the intel path) | Runs when `/fno:intel` writes its report; the writer is `scripts/corrections-insights-tag.sh` | (none; watermark in `~/.fno/corrections.log.wm`) | always available | S2 rows in `~/.fno/corrections.log`, one per `#agent-correction` line, carrying `signal=<friction category>` | `fno outstanding` scoring via the `verifyDecisions` lane |
| evals | The daemon's evals phase on the pr-watch tick | `evals.n` (days between regression runs; `0` disables) | `7` | Run receipts + trend rows (`fno-agents evals-arm`, `evals-trend`) | `fno doctor evals` |
| pr-watch heal | The pr-watch tick over every red open PR | `auto_heal.enabled` | `false` | Heal attempts + reports per PR (`fno do pr heal` applies the mechanical fix) | `fno do pr heal <n>` |

## What feeds what

- The transcript fold (`fno-agents intel`, report via `/fno:intel`) is the S2 writer's only report source. Before intel, no writer produced a file with `#agent-correction` lines. The S2 path had been starving since it shipped. The fold classifies turns by provenance first, so only operator-typed turns reach the corrections the skill quotes.
- The S0 watcher and the S2 rows share `~/.fno/corrections.log`. S0 rows come from postmortems, S2 rows from intel reports. The `verifyDecisions` lane scores the `signal=` field across both.
- Evals and heal are daemon-tick loops: both read what the other loops wrote. A healed PR re-runs evals banks, and a regression bank failure files a node the autocorrect review can pick up.

## Tuning

Start nothing by hand: the launchd installer is idempotent (`--uninstall` removes), and `evals.n: 0` plus `auto_heal.enabled: false` are the stock quiet posture. The loop map's failure mode is double-writing, not under-writing: every writer above routes through `corrections_build_line` in `scripts/lib/corrections-lock.sh`, which validates and escapes. Do not append to the log directly.
