# Provider-cap failover

A provider caps, every worker on it stops, and nothing wakes anything. The operator comes back to a fleet paused since the cap, because the absence of a ping reads exactly like work in progress.

This page states the law that governs every recovery arm here, the holes that made a cap invisible, and where each fix lives.

## The law

**No recovery mechanism may depend on the capped provider.**

Reaching a worker requires it to act, acting requires an API call, and the API is what is gone. Every arm in this subsystem is therefore a signal, a file write, or a spawn of a fresh process. None of them sends text to a session and waits for it to answer.

Three separate control paths failed the same way on 2026-08-17, which is why the law is written down rather than assumed:

| Path | What it asked of the capped session | Result |
|---|---|---|
| A checkpoint order mailed to eight z.ai workers | read and act on injected text | 1 delivered hosted, 7 queued durable and unconfirmed, one of those to a live session marked not injectable. 12 percent |
| The wake path | read and act on injected text | same shape; owned by the external-watchdog node |
| `fno agents stop` | shut itself down over its own API | burned the 30s shellout timeout, raised exit 15, process still alive |

A mail-based fleet warning is therefore not a cap mitigation, and that is a recorded operator decision. The warning surface is a local read (`fno config accounts window`) and a local notification, never a broadcast to workers.

## What was broken

**The refusal was never an event.** `recovery_sweep` classified a DEAD session's `output.result`. A capped worker does not die: it answers once with a refusal and then holds a full session of context, so `output_result` was empty and nothing fired. The refusal text was already in hand two lines earlier, in the transcript turn `truth_fn` returns, and nothing read it.

**The refusal sat below a gate built for silence.** `classify()` returns `NOT_STALE` while the transcript mtime is fresh, and a capped worker's last turn is refusal prose written seconds ago. A worker that had emitted `<promise>` earlier read `done` and went to `SKIP_TERMINAL` forever. Both gates measure quiet; a refusal is affirmative evidence and must not wait behind either.

**The real reset time was thrown away.** `_next_health` wrote `now + cooldown_ms / 1000.0`, and the rule for a 429 is exponential backoff starting at 2000ms. A refusal body carrying a reset nine hours out produced a two-second lock, so the provider unlocked seconds after a multi-hour cap and every reroute path was free to route straight back in. `normalize()` kept a 256-char excerpt and no reset field. The operator did the subtraction by hand because the number was discarded.

**The provider that caps is the one the quota layer cannot see.** `usage.py` registers probes for claude and codex only; glm, gemini, openclaw, hermes and every api_key record are `UNKNOWN` in v1, and `rotation.py` states the matching invariant that `UNKNOWN` never means exhausted. So a fully-built predictive layer was starved of input for the exact record that runs out.

**The out-of-band stop never ran for the population it saves.** `stop_agent` took the pid arm only when `short_id` was falsy. A capped worker HAS a short id. Its docstring scoped it to "the last resort after `stop_agent` finds no `short_id`", and a docstring that scopes a capability out of its real population is a capability nobody has.

**The trigger ran last on a deadline-bounded tick.** The recovery leg sat after the PR sweep. The tick arms a SIGALRM and re-raises `TickDeadlineExceeded`, which propagates before the recovery phase is reached, so a slow `gh` leg aborted the tick before the failover trigger. Measured: no `pr_watch_tick` heartbeat for six hours and eighteen minutes against a 600s interval, `failover_swapped` never emitted once.

## Where each fix lives

| Fix | Arm it uses | Code |
|---|---|---|
| Classify a live worker's refusal from its transcript turn | a read | `recovery.classify_worker_refusal` |
| Harvest the reset stamp and write it as the provider lock | a parse and a file write | `error_taxonomy._parse_reset_stamp`, `runtime_state._next_health` |
| Emit `worker_refused` above the staleness gate | an event write | `recovery.recovery_sweep` |
| Run the fleet leg first and heartbeat it | a file write on a launchd tick | `pr_watch/cli.py`, `fleet_state.py` |
| Project a probe-less window's close | local reads | `runtime_state.project_window`, `fno config accounts window` |
| Stop out of band | a signal | `dispatch._stop_by_pid` via `stop_agent`'s escalation |
| Re-dispatch across the harness axis | a fresh process | `config.agents.fallback`, `recovery._default_failover` |
| Turn silence into a finding | a read and an event write | `agents/sweep.py`, `fno agents sweep` |

## Two rules that look like details and are not

**A harvested reset writes `rate_limited_until`, never a synthetic usage window.** The lock is read with no TTL and can hold a nine-hour truth. A usage snapshot is dropped after `DEFAULT_USAGE_TTL_SECONDS` (300), so a snapshot-based write reads `EXHAUSTED` for five minutes and `UNKNOWN` afterwards, and `UNKNOWN` never means exhausted. A synthetic `UsageWindow` would also make `fno config accounts list` render a confident percentage no probe measured.

**A naive reset stamp with no known timezone refuses.** The z.ai stamps are Singapore time and the claude weekly reset was quoted Pacific. Being wrong by eight hours either unlocks a capped provider early or locks a healthy one out for an extra window. A naive stamp needs `accounts.<id>.reset_timezone` on the record and returns None without it. None means "keep today's backoff", which is today's behavior, so refusing costs nothing and guessing costs a window.

## The one thing that must never happen

A stop that cannot be PROVED must not be followed by a spawn. The capped worker is alive and holds the worktree, and two live sessions writing one worktree is a measured failure in this fleet. The order is stop, prove the process is gone, release the claim, spawn. Proof is `_pid_alive` returning an explicit `False`; "cannot tell" is not gone.

## What this does not close

Moving the fleet leg ahead of the PR legs puts the trigger inside the tick deadline rather than behind it. It does not bound the `gh` leg itself, so a tick that dies at its deadline every time still gets one fleet pass per interval.

A codex successor is only partly covered. The cadence-deadline predicate in `fno agents sweep` reads the full registry and sees it; recovery's own candidate set drops every non-claude row and every row without a live bg messaging socket, so a codex cap surfaces as `worker_silent` rather than `worker_refused`.
