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

The split: the merge phase asks the `authorized-merge` verb's `grant-queue` op for the durable-grant queue. It drains the queue under a fresh 150s slice (`arm="pr_watch_merge"`), so a sweep the alarm cut leaves the queue intact. The sweep itself resolves no grants, and its `merge_scan` receipt names only `scanned`. At its own end the merge phase stamps the `pr_watch_merge` row in the grammar `merge sweep=<cut|ok> candidates=<n> granted=<g> executed=<e> skipped=<s>`. A later cut can no longer erase the merge record, and the row names whether it ran after a cut or a completed sweep.

`run_execute_queue` owns the per-attempt discipline:

- It re-loads the entry under the per-PR lock. It skips a row already merged (`merge_dispatched`) or parked.
- When `tick()` returns, the tick lock releases. An overlapping tick can have merged the queued PR already. A retries-exhausted row never retries from the queue.
- Under `_FIRE_FLOOR_S` of slice left it emits `execute-budget` and leaves the entry untouched. The next tick's merge phase rebuilds the queue.
- It persists `retries + 1` BEFORE the merge call. `WatermarkStore.set` persists per write. An alarm cut mid-call therefore counts as one failed attempt. The PR parks at `max_retries` instead of replaying at the head of every tick.
- rc 0 marks the merge done. rc 2 restores the prior retry count. A canonical guard's hold is a state to wait out, not a failure. Anything else is a failed attempt that feeds the park.
- The `timeout_s` threaded through `run_merge` bounds the authorized-merge owner call at the slice remainder (`_ritual_timeout()`). The interactive default stays 300s.

## Why every cure defers to a live tick

A bounce fired mid-tick runs `bootout` plus `kickstart -k`. The `-k` SIGTERMs the running tick. During one outage that killed eight ticks in one hour, one per session start, and kept the verdict dead. Three points own the guard since the launchd-backed rewrite.

First, the in-flight read is launchd's. `launchctl list sh.fno.pr-watcher` names the service's PID, and `ps -o etime=` ages it. A PID younger than one 600s StartInterval defers. An older tick is hung, and it still bounces. This replaces the cwd-routed `pr-watch:tick` claim, which covered only the sweep phase and read free while merge or recovery ran.

Second, heal, refresh and doctor all pass `defer_when_ticking=True`. The post-update refresh defers too. The plist runs the stable `~/.local/bin/fno-py` symlink, and every tick is a fresh process, so a new binary loads without a forced bounce.

Third, a deferred refresh still leaves its rewritten plist. The next bounce installs it.
