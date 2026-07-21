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

## Deferred: native-plan-mode auto-launch (Task 3.3a)

Auto-launch on a native Plan Mode approval (dispatch from `hooks/capture-plan-mode.sh` after the sidecar write) is deferred to a follow-up. Task 3.3's other half, the capture-hook fix (read the plan body from `tool_response.filePath` first, drop the phantom `approved`/`decision`/`isError` rejection gates, add the `awaitingLeaderApproval` skip), is already implemented in a separate change that rewrote `capture-plan-mode.sh`. Adding the auto-launch to the earlier version of that file would collide; the follow-up builds on the capture-hook fix once it lands.

## Components

| File | Role |
|---|---|
| `skills/target/scripts/dispatch-node.sh` | Layer 1 dispatch primitive (US5) |
| `skills/target/SKILL.md` (`### 0a. Background Dispatch`) | `/target bg <node...>` subcommand |
| `skills/blueprint/scripts/autolaunch-on-ready.sh` | Layer 2 ready-gated auto-launch (US6) |
| `skills/blueprint/SKILL.md` (tail) | invokes the auto-launch helper after intake in every mode |
| `skills/target/references/settings.md` | documents `config.target.auto_launch_on_blueprint` |
| `tests/test-bg-dispatch.sh` | hermetic AC5 + AC6 regression harness (mock `fno` + `get_config` stub) |

## Multi-CLI

`/target bg` and the auto-launch dispatch require `claude --bg` plus the `fno agents` daemon, both Claude-Code-specific. On a driver without them the dispatch reports the failure and the node stays `ready` (degrade, never fake a launch). The auto-launch gate defaults OFF everywhere, so non-CC drivers see unchanged `/blueprint` behavior. See [SKILL-COMPAT-MATRIX.md](../SKILL-COMPAT-MATRIX.md).
