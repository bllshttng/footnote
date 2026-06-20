# Active Backlog Dispatcher

The always-on backlog drain: a supervised background task inside the per-user
supervisor daemon that continuously claims ready backlog nodes for a project and
dispatches them one at a time through the existing megawalk loop primitive,
sleeping between drains. Config-gated, default-off, fail-safe. Node `x-c070`.

## Why

Footnote's headless loop is already a deterministic dispatcher: `run_loop`
pulls `queue.next()`, hands the unit to `dispatcher.run(unit)`, and moves on.
Selection is a pure rule (status, dependencies, priority lane key, claim lease)
with no relatedness judgment anywhere on the path. The gap was that the loop is
*invocation-scoped*: a human starts `/megawalk`, or arms merge-triggered
auto-continue, and the loop terminates on `NoWork`. This feature adds the
**always-on** behavior so the board drains itself.

The motivating defect: when a foreground reasoning agent sits in the dispatcher
seat ("work the backlog"), it invents an unprompted thematic-coherence filter
("this node has nothing to do with the last one, I won't fire it") that fires
inconsistently and sometimes wrongly. This design removes the reasoning agent
from the firing decision entirely: firing becomes a rule the daemon executes,
and any legitimate scoping is a **deterministic** gate (project scope,
`--mission`, `blocked_by` edges), never an LLM inference.

## Shape

```
config.active_backlog (per project)
        |
   daemon enters Serving --> spawn drain supervisor (supervised tokio task)
        |
   each pass: resolve enabled targets (fno config active-backlog --json)
        |
   per target, one drain tick (spawn_blocking; run_loop is synchronous):
     acquire walker:<cwd>  (holder active-backlog:<pid>, released at tick end)
       -> held by another? yield (active_backlog_yield), end tick
     single-unit MegawalkQueue(max_units=1).with_mission -> run_loop
       -> NoWork: board drained for scope, end tick
       -> Closed: active_backlog_dispatched, breaker reset
       -> Parked: breaker++; at failure_limit, hold node claim + active_backlog_parked
     release walker:<cwd>
        |
   wait poll floor (interval), waking early on the nudge sentinel mtime
```

## Components

- **Config schema** (`cli/src/fno/config/__init__.py`, `ActiveBacklogConfig`).
  `config.active_backlog`: `enabled` (bool or per-project map), `interval`
  (duration string, default `5m`), `failure_limit` (default 3), `max_concurrent`
  (default 1; v1 asserts 1, defined now so v2 parallelism needs no migration),
  `mission` (optional). Mirrors the `config.auto_continue` / `config.target.blast`
  fail-safe posture: a malformed block degrades to disabled, a bad scalar is
  dropped to its default, and an invalid `interval` fails *closed* (the feature
  disables) rather than spinning a 0-sleep hot loop. Accessors `is_enabled_for`,
  `any_enabled`, `enabled_projects`, and `interval_seconds` centralize the
  fail-closed rule.

- **Target resolver** (`cli/src/fno/active_backlog.py`, surfaced as
  `fno config active-backlog --json`). The daemon is a per-user global process
  with no inherent project, so it shells this on entering Serving to learn its
  drain targets (project, cwd, interval, failure_limit, mission) from
  `config.active_backlog` + the workspace project->path map. Keeping the config
  logic in Python (the single source of truth) matches the daemon's other
  config-ish reads. Bool `enabled: true` drains every workspace project; a
  per-project map drains only its truthy keys.

- **Drain tick** (`crates/fno-agents/src/active_backlog.rs`, `drain_tick`).
  Acquires the `walker:<cwd>` singleton (holder `active-backlog:<pid>`),
  builds a single-unit `MegawalkQueue` (`max_units = Some(1)`) with the mission
  scope, and runs the unchanged `run_loop` primitive to drain one node to
  termination. Maps the outcome to dispatched / parked / skip / no-work and emits
  the decision event. Releases the walker singleton at tick end.

- **Circuit breaker** (`CircuitBreaker`). A pure, cross-tick per-node
  consecutive-failure counter with Hermes semantics: increment on a failed
  drain, reset to zero only on a successful close, trip at `failure_limit` with
  no auto-unpark. At the trip the daemon holds the node's `node:<id>` claim so
  `fno backlog next`'s live-claims filter excludes it from future selection (the
  park *is* the claim). Parked claims are TTL-refreshed each tick.

- **Resident supervisor** (`run_supervisor`). Spawned when the daemon enters
  `Serving`. One drain tick per enabled project per pass (serial, v1), each on
  `spawn_blocking` so the synchronous `run_loop` never stalls the daemon's async
  serve loop. A tick panic is caught, emitted as `active_backlog_task_crashed`,
  and the supervisor restarts with exponential backoff (never taking down the
  `agent.*` / `channel.*` serve loop). An enabled project keeps the daemon out
  of `IdlePendingExit`. Config changes are picked up by re-resolving targets each
  pass.

- **Wake scheduler + nudge** (`wait_for_wake` + Python `touch_nudge`). The poll
  floor (`interval`) is the correctness guarantee. Layered over it, a best-effort
  nudge sentinel (`$HOME/.fno/.active-backlog-nudge`) is touched by
  `locked_mutate_graph` (after a board render) and by `fno backlog advance`;
  the supervisor watches its mtime and wakes early so a fresh ready node drains
  sooner than the floor. A burst of touches coalesces to a single wake. A missed
  nudge is harmless: the poll floor catches it within one interval.

## The single-owner contract

Exactly one walker owns `walker:<cwd>` at any time. The daemon acquires it at
the *start* of each tick and *releases* it at tick end. Releasing per-tick is
deliberate: it lets a human `/megawalk` grab the singleton between ticks, after
which the daemon's next acquire fails and the tick yields
(`active_backlog_yield{walker-live}`). Holding the claim across the whole drain
would make the daemon a permanent owner a manual walk could never displace,
which v1 forbids. Merge-triggered `fno backlog advance` already no-ops while the
walker is live, so the daemon and `advance` never both dispatch.

`/megawalk` is conceptually a daemon client, not a peer dispatcher, but
re-pointing its skill body (attach-and-nudge when the daemon is live, bounded
foreground drain when off) is a deferred follow-up (D1). v1 already yields to a
live manual walk through the shared claim.

## Relationship to megawalk

The daemon does not replace megawalk; it changes which layer owns the *trigger*.
The `run_loop` primitive and the `MegawalkQueue` engine (selection, live-claims
filter, park-exclusion, lane order, `--mission`, and the v2 parallel scheduler)
are reused unchanged; the daemon depends on the engine, never deletes it. What
consolidates is the trigger layer: `/megawalk`, `fno backlog advance`, headless
`loop run`, and now the resident daemon all spin up the same engine and all grab
the same `walker:<cwd>` singleton, so they are mutually exclusive.

## Events

All transitions are emitted through the loop `Journal` (project journal fatal,
global mirror best-effort), so an auditor can reconstruct the full drain history
from `events.jsonl` alone:

| Event | When |
|-------|------|
| `active_backlog_dispatched{node_id, termination}` | a node closed successfully |
| `active_backlog_yield{reason: walker-live}` | the walker singleton is held by another walker |
| `active_backlog_parked{node_id, consecutive_failures}` | a node tripped the circuit breaker |
| `active_backlog_skip{reason, ...}` | a selection/loop error, or a node that failed without yet tripping the breaker |
| `active_backlog_task_crashed{project, error}` | the supervised drain task panicked (then restarts with backoff) |

## Failure handling

| Scenario | Handling |
|----------|----------|
| Manual `/megawalk` starts mid-drain | the daemon's next tick fails to acquire `walker:<cwd>` and yields |
| Daemon crashes mid-dispatch | the worker owns `node:<id>` independently; on restart the live-claims filter excludes the in-flight node; the orphaned `walker:<cwd>` clears by TTL or same-holder re-acquire |
| Node crash-loops | per-node consecutive-failure counter; at `failure_limit` the node is parked (claim held, excluded) and the tick moves on |
| Backlog mutated many times quickly | the nudge is coalesced to one pending drain |
| Config disabled while a node is in flight | the current dispatch finishes (targets are re-resolved only between ticks); no further tick is scheduled; the walker claim is released on the final tick |
| Empty / mission-empty scope | the tick ends with `NoWork`, dispatches nothing, and the daemon sleeps |
| Invalid `interval` | config fails closed: the feature disables (no 0-sleep loop) |

## Scope (v1) and deferred work

- **Serial**: one in-flight node per project per tick. `max_concurrent` is
  defined (default 1) but v1 asserts 1; parallel, dependency-aware drain reuses
  megawalk's parallel scheduler in v2.
- **Deferred (D1)**: re-point the `/megawalk` skill body from a peer dispatcher
  to a daemon client. v1 already yields to a live manual walk via the shared
  claim, so this does not block v1.

## Code map

| File | Role |
|------|------|
| `cli/src/fno/config/__init__.py` | `ActiveBacklogConfig` schema + fail-safe coercion |
| `cli/src/fno/active_backlog.py` | drain-target resolver + nudge sentinel touch |
| `cli/src/fno/config_cli.py` | `fno config active-backlog --json` |
| `cli/src/fno/graph/store.py` | nudge touch after `locked_mutate_graph` render |
| `cli/src/fno/backlog/advance.py` | nudge touch on dispatch |
| `crates/fno-agents/src/active_backlog.rs` | drain tick, circuit breaker, supervisor, wake scheduler |
| `crates/fno-agents/src/daemon.rs` | spawn / supervise / tear down the drain task; idle-exit interaction |
| `crates/fno-agents/tests/active_backlog_drain.rs` | drain-tick integration tests |
