# Active Backlog Dispatcher

The always-on backlog drain: a supervised background task inside the per-user
supervisor daemon that continuously converges ACTIVE MISSIONS - epics with
`mission_active=true` - by dispatching their ready leaf children across all
projects, sleeping between drains. Config-gated, default-off, fail-safe.

> **Mission-scoped drains (K2).** The daemon originally ran one drain
> loop **per enabled project**, each acquiring `walker:<cwd>` and dispatching one
> node per tick through a local `drain_tick`. That per-project interval arm is
> **deleted** (epic Locked Decision 4); merge-triggered `fno backlog advance` is
> the same-project coverage. The daemon now runs one loop **per active mission**,
> and each tick dispatches by shelling K1's converge core
> (`fno backlog advance --epic <id> --continuation --json`), which fans children
> out across all projects with its own per-dependent-root `walker:` respect,
> `max_lanes` cap, and claim dedup. The fire-and-forget reconcile machinery
> (`reconcile_pending` / `map_outcome` / crash floor) and the `CircuitBreaker`
> are unchanged. Sections below are updated to this model; a few cross-cutting
> ones (Why, circuit breaker, nudge, megawalk relationship) are unaffected.

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
config.active_backlog (master switch) + epics with mission_active=true
        |
   daemon enters Serving --> spawn drain supervisor (supervised tokio task)
        |
   each pass: resolve active-mission targets (fno config active-backlog --json)
        |
   per active mission, one loop; each mission tick (spawn_blocking):
     reconcile prior fire-and-forget dispatches from events -> feed breaker
       -> Closed: active_backlog_dispatched; Parked: breaker++ (defer at limit)
       -> worker died with no termination event: crash floor -> failure
     dispatch: fno backlog advance --epic <id> --continuation --json
       -> deactivated / all_done: active_backlog_mission_retired, loop exits
       -> dispatched: [nodes] recorded in `pending` for a later reconcile
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
  drain targets. It returns **one target per active mission** (an epic with
  `mission_active=true`, read from the graph), each carrying the epic id on
  `mission` plus the epic project's cwd + interval + failure_limit. Keeping the
  config logic in Python (the single source of truth) matches the daemon's other
  config-ish reads. `config.active_backlog` is the master switch (`any_enabled()`
  + a valid interval); each mission is additionally gated on
  `is_enabled_for(epic project)`, so an explicitly-disabled project's mission
  does not drain.

- **Mission drain tick** (`crates/fno-agents/src/active_backlog.rs`,
  `mission_drain_tick` / `dispatch_mission`). Reconciles prior fire-and-forget
  dispatches from events (feeding the breaker) and then dispatches by shelling
  `fno backlog advance --epic <id> --continuation --json`. The converge core owns
  all dispatch policy (cross-project fan-out, per-root `walker:` respect,
  `max_lanes`, claim dedup), so it is never forked. `--continuation` means the
  daemon never (re)activates a mission and retires an already-inactive one, so an
  operator `--stop` between ticks sticks. A `deactivated`/`all_done` receipt
  retires the loop (`active_backlog_mission_retired`).

- **Circuit breaker** (`CircuitBreaker`). A pure, cross-tick per-node
  consecutive-failure counter with Hermes semantics: increment on a failed
  drain, reset to zero on a successful close. When the streak reaches
  `failure_limit` the daemon trips: it `fno backlog defer`s the node (graph
  state) and resets the streak. Deferring (rather than an endlessly-refreshed
  claim) is what makes the park **recoverable** - `fno backlog undefer` returns
  the node to the ready pool with a fresh `failure_limit` attempts, exactly as
  the plan specifies.

- **Resident supervisor** (`run_supervisor`). Spawned when the daemon enters
  `Serving`. It spawns ONE independent drain loop **per active mission** (keyed by
  epic id), so one mission's convergence never blocks or starves another. Each
  loop owns its own breaker + `pending` set, offloads each tick to
  `spawn_blocking`, and re-resolves its mission's liveness between ticks: an epic
  that drops out of the resolved target set (its `mission_active` cleared) exits
  the loop, as does a `deactivated`/`all_done` receipt. A tick panic is caught,
  emitted as `active_backlog_task_crashed`, and the loop restarts with
  exponential backoff (never taking down the `agent.*` / `channel.*` serve loop).
  Any active mission keeps the daemon out of `IdlePendingExit`.

- **Wake scheduler + nudge** (`wait_for_wake` + Python `touch_nudge`). The poll
  floor (`interval`) is the correctness guarantee. Layered over it, a best-effort
  nudge sentinel (`$HOME/.fno/.active-backlog-nudge`) is touched by
  `locked_mutate_graph` (after a board render) and by `fno backlog advance`;
  the supervisor watches its mtime and wakes early so a fresh ready node drains
  sooner than the floor. A burst of touches coalesces to a single wake. A missed
  nudge is harmless: the poll floor catches it within one interval.

## The single-owner contract

Walker exclusion is now enforced **per dependent root, inside the converge core**,
not at the daemon tick. `fno backlog advance --epic` calls `_walker_live_at(root)`
for each child before dispatching it, so a manual `/megawalk` owning a given
project's `walker:<root>` makes the converge core skip that project's children
while other projects keep dispatching. The daemon mission tick holds no
project-level walker singleton of its own (the old per-project `drain_tick`
acquire/release is deleted). Node/dispatch claim dedup (`node:<id>` /
`dispatch:<id>`) inside the converge core keeps the daemon and a manual walk from
both launching the same node.

> **Coarseness note (tracked follow-up).** `advance_epic` also checks the *epic
> repo's* walker once before enumerating children, so a `/megawalk` in the epic's
> own repo can pause the whole mission. Refining this to per-root only is a
> deferred item; see the mission-drain hardening follow-up.

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
| `active_backlog_dispatched{mission, dispatched}` | the mission tick fire-and-forgot one or more ready children |
| `active_backlog_parked{node_id, consecutive_failures}` | a node tripped the circuit breaker (reconcile auto-defer) |
| `active_backlog_skip{reason, ...}` | an `advance --epic` failure/unparseable receipt, or a node that failed without yet tripping the breaker |
| `active_backlog_mission_retired{mission}` | the mission deactivated / all children done; the loop exits |
| `active_backlog_task_crashed{mission, error}` | the supervised mission tick panicked (then restarts with backoff) |

## Failure handling

| Scenario | Handling |
|----------|----------|
| Manual `/megawalk` starts mid-drain | the converge core's per-child `_walker_live_at(root)` skips that project's children; other projects keep dispatching |
| Daemon crashes mid-dispatch | each worker owns `node:<id>` independently; a dispatched node closes at merge via `fno backlog reconcile`; a restarted mission loop rebuilds its `pending` set from events |
| Node crash-loops | per-node consecutive-failure counter (fed by the reconcile of the dispatched worker's termination); at `failure_limit` the node is `fno backlog defer`red (recoverable via `fno backlog undefer`) while independent branches keep dispatching |
| One mission converges slowly | each active mission has its own independent loop, so other missions keep dispatching concurrently |
| Backlog mutated many times quickly | the nudge is coalesced to one pending drain |
| Operator `--stop` between ticks | `--continuation` never reactivates; the tick returns `deactivated` and the loop retires (no zombie ticks) |
| Empty / all-done mission | `advance --epic` reports `all_done`; the loop emits `active_backlog_mission_retired` and exits |
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
| `cli/src/fno/active_backlog.py` | active-mission drain-target resolver + nudge sentinel touch |
| `cli/src/fno/config_cli.py` | `fno config active-backlog --json` |
| `cli/src/fno/backlog/advance.py` | `advance_epic` converge core (incl. `--continuation` daemon mode); nudge touch on dispatch |
| `cli/src/fno/graph/store.py` | nudge touch after `locked_mutate_graph` render |
| `crates/fno-agents/src/active_backlog.rs` | mission drain tick + loop, circuit breaker, reconcile, supervisor, wake scheduler |
| `crates/fno-agents/src/daemon.rs` | spawn / supervise / tear down the drain supervisor; idle-exit interaction |
| `crates/fno-agents/tests/active_backlog_drain.rs` | mission-drain-tick integration tests |
