# The fleet load report

`fno-agents intel --fleet` answers one question: how much load did the fleet put on this machine, and how many sessions can it hold before it stalls?

## The command

```bash
fno-agents intel --fleet
fno-agents intel --fleet --days 14
fno-agents intel --fleet --backfill
```

`--days` sets the window (30 days default). `--backfill` lifts the per-run read budget, so the first run reads every transcript in the window instead of spreading the scan over several runs. `--json` prints the report object instead of text. A budgeted run reports the files and bytes still unread, and computes no curve, threshold or cap until the backfill completes.

## What each section means

- **now**: the newest machine reading beside the memory the machine reports right now.
- **capacity curve**: hours bucketed by sessions active (claude, subagents and codex combined), with the median load of each bucket.
- **threshold**: a load level learned from the operator's own slowdown turns, not a fixed number.
- **suggested cap**: the top of the last session bucket whose median load stays under the threshold. The unit is sessions active per hour, not spawn-gate worker rows.
- **slowdown turns**: the operator turns that said the machine was slow or frozen, each with the nearest machine reading.
- **coverage**: the rows each series holds, each series' window, unparsed tick rows, and the fold receipt. A short window reads as short.

## The threshold and cap rule

The threshold is the 25th percentile, nearest rank, of the load at slowdown turns that have a machine reading within an hour. It needs at least two such turns. The cap walks the curve buckets upward, skips buckets with fewer than six hours, and stops at the first over-threshold bucket. While the backfill is incomplete, none of the three is computed: an unread transcript puts a loaded hour in a low bucket.

## Sources and windows

- machine_watch tick rows: the load reading lives in the tick detail sentence. Hot rows say `crosses band`, and `load_15m` can read `unavailable`.
- spawn-gate refusals per hour, from the flat journal rows.
- live workers, from crown check-ins that carry the count.
- memory history, from structured machine_sample rows. When none exist yet, the report says so honestly.
- transcript activity, read incrementally from the harness transcript stores with a byte budget per run.

## Empty states

A fresh install has no history. Every section then prints one sentence saying what is missing and why, for example `no suggested cap: 0 slowdown turns with a reading, 2 needed`. The page never invents a zero or hides a gap.
