# The pr-watch merge phase and the heal deferral

The watcher's tick is a sequence of alarm-capped phases. Two of them carry
autonomy: the sweep (discover, decide, dispatch) and, after it, the merge
phase that drains durable-grant merges the sweep queued.

## Why the merge is its own phase

A durable-grant merge attempt measured ~120s. Inside the 150s sweep slice the
call and the scan could not both finish: the phase alarm cut the tick before
the receipt minted, the spent retry never persisted, and the same granted PR
headed every later tick. Completed ticks read `scanned` 13-23 before such an
outage and 1-2 during it, and the tick verdict read `dead` with `cut:
['sweep']`.

The split: the sweep's execute decision only queues `(candidate, key, grant
fields)` on `TickResult.execute_queue`, stamps the poll cursor, and mints its
receipt. `_run_phase("merge", ...)` then drains the queue under a fresh 150s
slice (`arm="pr_watch_merge"`), so the scan always completes and the merge
call may occupy the whole merge slice.

`run_execute_queue` owns the per-attempt discipline:

- It re-loads the entry under the per-PR lock and skips when
  `merge_dispatched` is already set: the tick lock releases when `tick()`
  returns, so an overlapping tick may have merged the queued PR already.
- Under `_FIRE_FLOOR_S` of slice left it emits `execute-budget` and leaves
  the entry untouched; the next tick's sweep rebuilds the queue.
- It persists `retries + 1` BEFORE the merge call. `WatermarkStore.set`
  persists per write, so an alarm cut mid-call counts as one failed attempt
  and parks the PR at `max_retries` instead of replaying it at the head of
  every tick.
- rc 0 marks the merge done; rc 2 restores the prior retry count (a
  canonical guard's hold is a state to wait out, not a failure); anything
  else is a failed attempt that feeds the park.
- The `timeout_s` threaded through `run_merge` bounds the authorized-merge
  owner call at the slice remainder (`_ritual_timeout()`); the interactive
  default stays 300s.

## Why the heal defers to a live tick

A `dead` verdict fired the SessionStart self-heal, whose bounce runs
`bootout` plus `kickstart -k`; the `-k` SIGTERMs a running tick. During one
outage this killed eight ticks in one hour, one per session start, keeping
the verdict dead. `bounce(defer_when_ticking=True)` reads the
`pr-watch:tick` claim first and defers while it is live and younger than one
StartInterval (600s, above the 480s tick ceiling); an older live claim is a
hung tick and still bounces. The heal passes the flag; the post-update
refresh does not, because a new binary must load.
