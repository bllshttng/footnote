# Merge-triggered auto-continue

When a backlog node's PR merges, footnote dispatches a fresh background `/target no-merge` worker for the next now-unblocked node, so a merge-gated epic walks itself group-by-group across merges with no manual re-invocation. The trigger is the **merge event**, not the loop terminal.

## Why

A megawalk over a decomposed epic ships group 1 as a no-merge PR, then dies on `NoWork`: groups 2 and 3 are `blocked_by` the unmerged PR, and the walker terminating there is correct (keeping a process parked across a human merge that may be hours away is the wrong shape). But nothing resumed after the human merged on the GitHub app, so the chain stalled silently. Auto-continue closes that gap without keeping any process alive across the merge.

## Shape

A small shared verb, `fno backlog advance`, that any merge-detector calls after the node-close write commits. Because the **merge event** drives the next dispatch (not the loop driver), megawalk, `/target`, and `/megatron` all inherit auto-continue with no driver-specific code (Locked Decision 1).

```
merge group-1 PR on the GitHub app        (no terminal alive)
        |
        v   [next time any session opens]
SessionStart hook -> fno backlog reconcile (detached, throttled ~15 min)
        |
        +-- closes drifted node ab-G1 (PR merged outside the ship gate)
        +-- fno backlog advance --closed ab-G1 --project P
              |
              +-- armed? walker free? -> fno backlog next --project P -> ab-G2
              +-- reserve dispatch:ab-G2 (TTL bridge token)
              +-- fno agents spawn --provider claude -> /target no-merge ab-G2
              +-- emit advance_dispatched{node: ab-G2}
        |
   worker builds G2, ships a no-merge PR ... human merges G2 ... repeat for G3
```

The terminal path (`/pr merged <pr>`) is the same minus the SessionStart hook.

## The verb: `fno backlog advance`

`cli/src/fno/backlog/advance.py`. Decision matrix, in order, emitting **exactly one** event per run (Locked Decision 12, AC1-UI):

| Step | Condition | Outcome | Event |
|------|-----------|---------|-------|
| 1 | not armed | skip | `advance_skipped{disabled}` |
| 2 | `walker:<root>` claim live | skip (the live walk owns it) | `advance_skipped{walker-live}` |
| 3 | `fno backlog next` errors | skip (never guess a node) | `advance_skipped{next-error}` |
| 3 | no ready node | skip | `advance_skipped{no-work}` |
| 4 | `node:<id>` or `dispatch:<id>` already live | skip (already being worked) | `advance_skipped{already-claimed}` |
| 5 | `dispatch:<id>` acquire loses the O_EXCL race | skip | `advance_skipped{already-claimed}` |
| 6 | spawn raises name-collision | skip, release reservation | `advance_skipped{already-claimed}` |
| 6 | spawn fails otherwise | fail, release reservation | `advance_failed{node,error}` |
| 7 | spawned | dispatched | `advance_dispatched{node,short_id}` |

`advance` is strictly **non-fatal**: any failure resolves to `advance_failed` / `advance_skipped` and the host op (reconcile / post-merge) still completes (Locked Decision 7). It never merges anything (Locked Decision 6): it dispatches no-merge workers only; auto-merge stays an independent opt-in.

## Claim choreography (the LD#11 / AC1-CLAIM problem)

`advance` is a short-lived process; the worker it spawns is a separate, long-lived one. The just-dispatched node must stay "claimed" across the gap so a concurrent reconcile/post-merge does not double-dispatch, but a PID-liveness claim held by `advance` would go stale the instant `advance` exits (orphaning it).

The answer is the codebase's existing bridge-token pattern, identical to `skills/target/scripts/handoff.sh` and `skills/target/scripts/dispatch-node.sh`:

- `advance` reserves `dispatch:<id>` as a **TTL claim** (3 minutes, not PID-liveness) before spawning. The TTL outlives `advance`'s exit, so for the boot window the node reads as already-claimed (AC1-CLAIM). On any spawn failure the reservation is released so the node stays re-dispatchable (AC2-FR).
- The spawned worker acquires `node:<id>` cleanly on its own `fno target init` (it is free at that point - `advance` never holds `node:<id>`). Once the worker owns `node:<id>`, the `dispatch:<id>` reservation expires harmlessly by TTL.
- `advance` honors a live `walker:<root>` (a megawalk owns the project) and skips, so a merge landing mid-walk never produces a second worker (AC2-EDGE).

Claim roots are routed like the `fno claim` CLI's `_node_aware_root`: `node:<id>` lives in the global (`$HOME`) claims root; `walker:<root>` and `dispatch:<id>` use the canonical-repo-root claims dir. The walker key is `walker:<canonical_repo_root>`, byte-identical to the key the Rust megawalk loop writes.

## Enable resolution

Opt-in, default off (Locked Decision 3). `auto_continue_enabled()` resolves, highest precedence first:

1. `FNO_AUTO_CONTINUE` env override (explicit force on/off; tests + same-process).
2. campaign-arm marker file `.fno/.auto-continue-armed` (written by `/megawalk auto-continue`).
3. `config.auto_continue.enabled` in config.toml (project-local overrides global via the deep-merge in `load_settings`).
4. default `False`.

A malformed `config.auto_continue` block (a non-boolean `enabled`, or a scalar where the block should be a mapping) degrades to disabled rather than raising out of the settings load (AC2-ERR), and any settings-read failure in the resolver is swallowed to `False`.

**Why a marker file, not an env var, for the campaign arm (Discretion #4).** The dominant trigger is the *next* session's detached reconcile observing a web merge. An env var set by a live `/megawalk` does not survive to that later process, so the arm has to be persistent state. The env var is retained only as the highest-precedence explicit override.

## Triggers

- **`fno backlog reconcile`** (fired detached by the SessionStart hook `hooks/reconcile-session-start.sh`): after it closes each drifted, web/app-merged node, it calls `advance(closed_node_id=<id>, project=<project>)`. This is the dominant path because the operator merges on the web. Non-fatal: a failed advance never fails the reconcile sweep.
- **`/pr merged`**: after it closes the node + harvests retro, it calls `fno backlog advance`. This covers the case where the node was already closed before `/pr merged` ran (reconcile then no-ops, so its own advance never fires).

Both triggers observing one merge dispatch the successor at most once: the `dispatch:<id>` reservation (and the worker's `node:<id>` claim) dedups them (AC1-FR).

`advance` is invoked **only after the node-close write commits**, keyed by `closed_node_id` (AC1-RACE): within one reconcile/post-merge run the closed node is already reflected before `fno backlog next` is read, so the now-unblocked successor is selected. If `next` still returns nothing, the next throttled SessionStart reconcile (~15 min) retries, so the chain is never permanently stalled.

## Dispatch mechanics

The worker spawn mirrors `dispatch-node.sh` exactly: `no-merge` rides as a command token (`/target no-merge <id>`), not an env var (the shipped sibling proves the token is the reliable channel through `fno agents spawn`); the agent is named `target-<full-node-id>-<slug>`; cwd resolves to the node's recorded root (`--cwd`) or canonical main (`--fresh`). Subscription lane only (`fno agents spawn --provider claude`), never `-p` / API credit.

## Cross-project successor dispatch (G1)

`advance()` selects the project-scoped *next* ready node, so it can only continue a chain *within one project*: `fno backlog next --project <closed.project>` filters foreign nodes out. A multi-repo feature modeled as one node per project linked by `blocked_by` (backend node A in `etl`, frontend node B in `web`, `B blocked_by A`) would stall at the repo boundary - A merges, but B is never dispatched.

`advance_dependents()` (same module) closes that gap by following `blocked_by` **edges** instead of a project-scoped selection. After a node closes, for each now-unblocked **direct** dependent in a **different** project it spawns `/target no-merge <dep> --cwd <dep work-map root>`. The two paths are deliberately distinct (dispatch-by-edge vs select-next): `advance()` is untouched, so a pure single-project close dispatches nothing new, and the same-project continuation keeps using `next`.

- **Unblocked == ready.** `_direct_dependents` reads the graph fresh (after the close commits under `locked_mutate_graph`), so `recompute_statuses` already reflects the merge: a dependent whose only open blocker was the closed node reads `ready`. The filter is `_status == "ready"` + cross-project + direct edge - no hand-written unblock predicate. Plan-less (`idea`) dependents are not auto-dispatched.
- **Root from the work map, never guessed.** The dependent's `--cwd` is `project_root_from_settings(dep.project)` (exposed standalone as `fno backlog project-root <project>`). An unmapped project is refused by name (`advance_skipped{unmapped-project, detail=<project>}`), never launched against a guessed cwd.
- **At most one worker.** Reuses the same `dispatch:<id>` TTL reservation + `node:<id>` liveness gate, so a successor seen by both `advance()` and `advance_dependents()` - or by reconcile and the explicit `backlog advance --closed` (both fire in `/pr merged`) - dispatches exactly once. One decision event per dependent; strictly non-fatal.

Wired alongside `advance()` in `reconcile` and `cmd_advance`. The session-side mirror is **G2** (the `/do` session-project invariant): a `/do` wave in a foreign project is spawned (unblocked) or deferred via `fno carveout` (blocked, picked up later by this G1 path on merge), never executed in place. See `skills/do/references/session-project-invariant.md`.

## Scope

Phase 1 (this implementation): the `advance` verb + `config.auto_continue` + reconcile and post-merge wiring + the `/megawalk auto-continue` modifier. Project-scoped next-node selection (the same selection bare megawalk uses). Cross-project successor dispatch (G1, above) extends the merge event to follow `blocked_by` edges across projects.

Deferred (Phase 2, gated on Phase 1 dogfooding): a per-repo launchd watcher that fires reconcile/post-merge headlessly seconds after a web merge, removing the up-to-15-min reconcile-throttle latency. Epic-affinity in next-node selection (siblings-first) stays an optional refinement, out of Phase 1.

## Events

Three kinds, registered in `cli/src/fno/events/schema.yaml`, source `backlog`:

- `advance_dispatched{node_id, short_id, agent_name, closed_node_id?, cross_project?}` - surfaced loudly in the next SessionStart reconcile reminder. `cross_project: true` marks a G1 edge-following dispatch into a different project.
- `advance_skipped{reason, node_id?, closed_node_id?, detail?}` - `dispatched`/`failed` are surfaced; pure skips stay quiet. G1 adds the reasons `unmapped-project` / `no-project` / `dependents-error`.
- `advance_failed{node_id, error, closed_node_id?}` - surfaced loudly (a failed chain must be visible); the next reconcile retries.

## Files

- `cli/src/fno/backlog/advance.py` - the verb (resolver + decision matrix + seams) + `advance_dependents()` (G1 cross-project edge dispatch).
- `cli/src/fno/config/__init__.py` - `AutoContinueBlock`.
- `cli/src/fno/graph/cli.py` - `fno backlog advance` command + the reconcile trigger (both also call `advance_dependents`); `fno backlog project-root` (the unmapped-project detector G2 uses).
- `skills/do/references/waves.md`, `skills/do/references/flat.md`, `skills/do/references/session-project-invariant.md` - the G2 session-project invariant (spawn/defer/refuse foreign waves).
- `skills/pr/references/merged.md`, `skills/megawalk/SKILL.md`, `skills/megawalk/references/argument-parsing.md` - trigger + campaign-arm modifier.
- `cli/src/fno/events/schema.yaml` - the three event kinds (+ G1 `cross_project` field and reasons).
- Tests: `cli/tests/unit/test_auto_continue.py`, `cli/tests/unit/test_advance.py`, `cli/tests/unit/test_project_root_cmd.py`, `cli/tests/integration/test_backlog_reconcile.py`.

## Design doc

The design doc (in the maintainers' vault) records the 12 locked decisions, multi-perspective findings, and the full acceptance-criteria set this implements.
