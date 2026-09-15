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

The split: the merge phase asks the `authorized-merge` verb's `grant-queue` op for the durable-grant queue. It drains the queue under a fresh 150s slice (`arm="pr_watch_merge"`), so a sweep the alarm cut leaves the queue intact. The sweep itself resolves no grants, and its `merge_scan` receipt names only `scanned`. At its own end the merge phase stamps the `pr_watch_merge` row in the grammar `merge sweep=<cut|ok> candidates=<n> granted=<g> executed=<e> held=<h> failed=<f> skipped=<s> read_ms=<r>`, where the drain counts each queue row exactly once (`granted == executed + held + failed + skipped`) and `read_ms` is the queue read's cost. A later cut can no longer erase the merge record, and the row names whether it ran after a cut or a completed sweep.

The queue itself is Rust's (`crates/fno-agents/src/merge_grant.rs`): it drops `superseded` and `done` nodes next to `merge_status` merged/closed, reads each checkout's repo root and live config once per call whatever the candidate count, and rotates its head by the tick index the phase passes in the payload (`rotate = int(time.time() // interval_seconds)`). A slow held head therefore cannot starve the tail: every grant is reached within n ticks.

`run_execute_queue` owns the per-attempt discipline:

- It re-loads the entry under the per-PR lock. Every unattempted row is skipped with a reason on `pr_watch_skipped`: `merged` (an overlapping tick merged it), `parked` (retries exhausted), `not-open` (the listing's `last_seen_state` reads NOT_OPEN), `no-watermark` (no watcher row), `locked` (another merge holds the per-PR lock), or `execute-budget`.
- The floor is `max(_FIRE_FLOOR_S, slowest)`, where `slowest` is the longest attempt this drain has run: the drain never starts an attempt the slice cannot finish.
- The merge core's printed reason is kept on `fno.pr._merge.LAST_RECEIPT` and recorded on every held and failed `merge_grant_execution` row (capped at 300 characters). A hold whose reason starts with `PR already ` (the core's terminal exemption) stamps `last_seen_state = "NOT_OPEN"` on the row, so a closed or merged PR costs at most one attempt even while the listing lags.
- It persists `retries + 1` BEFORE the merge call. `WatermarkStore.set` persists per write. An alarm cut mid-call therefore counts as one failed attempt. The PR parks at `max_retries` instead of replaying at the head of every tick.
- rc 0 marks the merge done. rc 2 restores the prior retry count. A canonical guard's hold is a state to wait out, not a failure. Anything else is a failed attempt that feeds the park.
- The drain calls canonical `run_merge([str(pr)], cwd=..., authority="durable_grant", timeout_s=_ritual_timeout())` directly. The `timeout_s` bounds the authorized-merge owner call at the slice remainder. The interactive default stays 300s.

## Why every cure defers to a live tick

A bounce fired mid-tick runs `bootout` plus `kickstart -k`. The `-k` SIGTERMs the running tick. During one outage that killed eight ticks in one hour, one per session start, and kept the verdict dead. Three points own the guard since the launchd-backed rewrite.

First, the in-flight read is launchd's. `launchctl list sh.fno.pr-watcher` names the service's PID, and `ps -o etime=` ages it. A PID younger than one 600s StartInterval defers. An older tick is hung, and it still bounces. This replaces the cwd-routed `pr-watch:tick` claim, which covered only the sweep phase and read free while merge or recovery ran.

Second, heal, refresh and doctor all pass `defer_when_ticking=True`. The post-update refresh defers too. The plist runs the stable `~/.local/bin/fno-py` symlink, and every tick is a fresh process, so a new binary loads without a forced bounce.

Third, a deferred refresh still leaves its rewritten plist. The next bounce installs it.
