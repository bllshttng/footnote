# Target Plan Mode Phase 2: ready-gated bg-dispatch of /target

Phase 2 of the [native Plan Mode integration](target-plan-mode-integration.md). Phase 1 made the native-plan-mode front door manual: approve a plan, then run `/target` to detect, backfill, and execute it. Phase 2 removes the manual step and the context bottleneck for the way the developer actually works: every `/target` already runs in a background thread.

Guiding principle: **a fresh `claude --bg` process is the only real context "clear"** (an agent cannot `/clear` itself), so dispatching a backlog node as a fresh bg `/target` worker lets the planning session keep batching `/think` + `/blueprint` while dispatched workers run do -> review -> ship on their own.

## The self-sustain spike (Wave 1, gating)

The whole design was conditioned on one unproven assumption: that a `claude --bg` `/target <node>` self-sustains to a `<promise>` on its own (in-session Stop hook + supervisor respawn) without `scripts/run-target-loop.sh` wrapping it. Wave 1 proved it on a live throwaway run before any code was built.

Dispatch: `fno agents ask --provider claude tgteval "/target S no-merge ab-1234abcd"`. Observed from the worker's job state file plus the worker transcript:

- **Self-sustains: yes.** The worker crossed a Stop-hook cycle (its transcript went from 2 to 3 non-tool-result user turns: it finished a long first turn, the Stop hook blocked exit on `IN_PROGRESS`-without-promise, and the session resumed) and ran the full pipeline unattended: scoped the change, edited 7 files, ran the suite (608 passed, 28 skipped, 0 failures), committed atomically, passed sigma-review, and **opened a PR** end-to-end. No `run-target-loop` wrapper was needed.
- **The "un-setup CC bg worktree" risk does not arise.** Because the dispatched worker runs the full `/target` skill, it hit the canonical-`main` location HARD-GATE and (per the worktree convention) created a proper conductor worktree and ran `setup-worktree.sh` itself, so its `.fno/` symlinks are wired. The location gate forces a set-up worktree; the dispatcher does NOT need to pre-assign one.
- **Subscription billing / parent-clear survival** were not directly observable from the driver session, but hold by design: `fno agents ask --provider claude` builds `claude --bg --name` (subscription lane, never `--bare`/`-p`) and bg sessions are supervisor-managed.

Result: **Phase 2 stands alone on `fno agents ask`** (Locked Decision 6), no Bet #1 prerequisite.

## Architecture

### Layer 1: the dispatch primitive (US5)

```
You (planning session): /think + /blueprint  ->  node reaches status: ready
        |
        v  /target bg ab-A ab-B          (or --all-ready)
   skills/target/scripts/dispatch-node.sh
     per node:  resolve status -> claim-guard -> fno agents ask --provider claude
        |              (-> claude --bg --name target-<id>-<slug> "/target no-merge <node>")
        v
   fresh bg worker per node = clean context; runs do->review->ship.
   You keep planning here; observe workers via `fno agents list/logs`.
```

`skills/target/scripts/dispatch-node.sh` is a self-contained shell primitive (deps: `fno` + `jq`). It is the canonical dispatcher, called from three places: the `/target bg` subcommand (SKILL.md), the auto-launch helper (Layer 2), and a future native-plan-mode hook (deferred, see below). Per node it emits exactly one outcome line, never silent:

| Outcome | When |
|---|---|
| `launched <node> name=target-<id>-<slug> session=<sid> hint="fno agents logs ..."` | `ready` node, no live claim |
| `already-running <node> reason="<holder>"` | a live worker holds `node:<id>` |
| `parked <node> reason="<status> (not up-next)"` | blocked / deferred / idea / unknown |
| `skipped-done <node>` | done / shipped / superseded |
| `failed <node> reason="..."` | non-existent node, or `fno agents ask` non-zero |
| `deferred-cap <node> reason="--max N reached"` | soft `--max` cap hit |

Locked behaviors:
- **Subscription lane only.** Dispatch is always `fno agents ask --provider claude`; never `--bare`/`-p` (those force the API-credit pool and strip skills/hooks).
- **`no-merge` by default.** An autonomous fire-and-forget worker lands a PR for review, not an auto-merge. `--allow-merge` opts out.
- **Claim guard.** The dispatcher skips a node only when `fno claim status node:<id>` reports `live`; a `stale` claim is left for the worker's own atomic init-acquire to reclaim (recovery). The worker's `fno target init` is the real race-winner, so a narrow double-dispatch window still collapses to one execution.
- **Fire-and-forget.** The dispatcher returns immediately and NEVER writes the planning session's `target-state.md`.
- **No hard concurrency cap.** `--all-ready` surfaces the cost (`~Mx subscription quota while active`); quota is the throttle. `--max N` is an opt-in soft cap.

### Layer 2: ready-gated auto-launch (US6, opt-in, default OFF)

```
/blueprint finishes -> claimed node has a status
        |
        v  config.target.auto_launch_on_blueprint == true ?  --no--> nothing (manual dispatch as today)
        |                                                  yes
        v  node status == ready (unblocked, not deferred) ?
        |                  |
       yes                no  ->  PARK (pre-planned future work); never launch
        v
   dispatch via Layer 1 (no-merge default)
```

`skills/blueprint/scripts/autolaunch-on-ready.sh <plan-path>` runs as the last step of `/blueprint` in every mode. It is a no-op unless `config.target.auto_launch_on_blueprint: true` (default OFF; an absent key reads as off, so existing behavior is unchanged). When enabled, it resolves the plan's `claims: ab-XXX` node, and if that node is `status: ready` it dispatches via Layer 1, printing `auto-launched <node> ...`. A `blocked`/`deferred`/`idea` node prints `parked <node> ...` and is never launched. A dispatch failure prints `autolaunch-failed <node> ...` and leaves the node `ready` and the plan intact.

The gate reuses the **existing backlog state model** (Locked Decision 3): a `ready` node is unblocked and up-next; pre-planned future work the developer marked `blocked_by`/`deferred` is parked. No new concept. The developer's own discipline IS the "only launch what's up-next" gate.

## Configuration

`config.target.auto_launch_on_blueprint` in `.fno/config.toml` (project) or `~/.fno/config.toml` (global). Default `false`. Read with the `get_config "target.auto_launch_on_blueprint" "false"` pattern (same shape as `config.target.dedupe_dead_duplicates`). Manual dispatch via `/target bg <node...>` is always available regardless of the flag.

## Native-plan-mode auto-launch (Task 3.3a): no hook dispatch, and why

Task 3.3a was originally deferred as "dispatch from `hooks/capture-plan-mode.sh` after the sidecar write", waiting on the capture-hook fix (read the plan body from `tool_response.filePath` first, drop the phantom `approved`/`decision`/`isError` rejection gates, add the `awaitingLeaderApproval` skip) that a separate change had already landed.
That capture-hook fix is in. The hook dispatch it was waiting for is closed unbuilt, because the capture hook is the wrong seam and the native path already reaches Layer 2 without it.

**At capture time there is nothing to dispatch.**
The sidecar frontmatter is hook-generated (`captured_at`, `session_id`, `slug`, `source`, `status`) and carries no node id, a native plan has no `claims:` / `graph_node_id:`, and the sidecar path is not a plan the backlog knows.
Nothing the hook has written is resolvable to a node by any of `autolaunch-on-ready.sh`'s three tiers, so a ready-gate placed there has no ready node to see.
The plan is not executable at that moment either. `/blueprint` compiles acceptance criteria the native plan does not yet carry. The backfill that synthesizes them runs inside a later `/target`, long after the hook has exited.
See the "Why synthesis precedes /blueprint" section of [target-plan-mode-integration.md](target-plan-mode-integration.md).

**The native path inherits Layer 2 for free.**
The front door calls `/blueprint` on the enriched doc, and `/blueprint` runs `autolaunch-on-ready.sh` as its last action in every mode.
An approved native plan reaches exactly the same ready-gated dispatch the blueprint path uses, with no hook involvement.

**That inheritance needed one guard, which is the real content of this task.**
`/blueprint` is called at front-door step 5, before the human is asked "Execute autonomously? [y/N]" at step 6.
With the gate ON the dispatch fired while that confirm was still outstanding: answering `N` produced a bg worker the human had just declined, and answering `y` produced a second worker racing the front-door session, which executes the plan itself.
Step 4b of `autolaunch-on-ready.sh` now parks with `plan-mode-front-door-owns-it` when the plan is a native-plan-mode plan.

**The plan states its own provenance, so the guard does not have to guess.**
`backfill-plan.sh` stamps `source: claude-plan-mode` into the enriched doc's frontmatter, and `/blueprint` round-trips frontmatter, so the key is still there when the gate runs.
Step 4b reads that one key from the plan's frontmatter only, because a `source:` line in a doc body is prose and treating body text as authority is the same trap the `graph_node_id` tiers above already guard against.
Two rejected alternatives are worth recording, since both look reasonable and both are wrong.
Keying off "a `pending` sidecar exists" fails because a declined confirm deliberately leaves the sidecar `pending` and re-offerable, so every unrelated `/blueprint` inside the sidecar's TTL would park and the configured auto-launch path would silently stop working.
Correlating on the sidecar's first body line fails because that line is whatever heading the plan opens with: `## Overview` is a whole line in twelve docs in this repo, so a common heading parks unrelated plans, while a short heading correlates with nothing and silently restores the original bug.

A failed provenance read parks rather than dispatching, mirroring the ready-gate above.
This matters because a backfilled plan carries no `claims:` or `graph_node_id:` and so resolves through the `plan_path` tier, which reads the graph and never opens the plan: an unreadable plan is therefore not screened out by an earlier exit, and "could not read" must not be allowed to masquerade as "not a plan-mode plan".

**Known residual: this closes the auto-launch path, not every path.**
During the confirm window the node is `ready` and unclaimed, so anything else that dispatches a ready node (`fno backlog advance`, the megawalk walker, a direct `dispatch-node.sh`, another session's `/target`) can still start work the human has not approved.
Closing that properly means not leaving the node ready-and-unclaimed while the front door is still asking, which is a change to the front door rather than to this gate.

## Components

| File | Role |
|---|---|
| `skills/target/scripts/dispatch-node.sh` | Layer 1 dispatch primitive (US5) |
| `skills/target/SKILL.md` (`### 0a. Background Dispatch`) | `/target bg <node...>` subcommand |
| `skills/blueprint/scripts/autolaunch-on-ready.sh` | Layer 2 ready-gated auto-launch (US6); step 4b parks a native-plan-mode plan (the front door owns it) |
| `skills/blueprint/SKILL.md` (tail) | invokes the auto-launch helper after intake in every mode |
| `skills/target/references/settings.md` | documents `config.target.auto_launch_on_blueprint` |
| `tests/test-bg-dispatch.sh` | hermetic AC5 + AC6 regression harness (mock `fno` + `get_config` stub) |
| `tests/test-autolaunch-gate.sh` | hermetic gate harness: caller-is-holder, node-resolution tiers, and the native-plan-mode park |

## Multi-CLI

`/target bg` and the auto-launch dispatch require `claude --bg` plus the `fno agents` daemon, both Claude-Code-specific. On a driver without them the dispatch reports the failure and the node stays `ready` (degrade, never fake a launch). The auto-launch gate defaults OFF everywhere, so non-CC drivers see unchanged `/blueprint` behavior. See [SKILL-COMPAT-MATRIX.md](../SKILL-COMPAT-MATRIX.md).
