# Provider cap actor

The one armed surface that owns every provider-cap move. The watchdog measures a strand. Only this actor can stop, move, or resume a session for quota reasons.

## Status and arming

`fno-agents provider-cap status [--json] [--max-age-s N]` reads the daemon's latest snapshot. Fresh means measured_at is within N seconds. Otherwise it computes on demand and says so. The daemon arm rebuilds the snapshot every 120 s. It persists armed or not. An unarmed tick row reads `provider_cap_off`, so the readout shows a live measurement.

## How a lane opens

For each registry row the snapshot reads:

- provider from `observed_model`. Declared fields are NULL fleet-wide.
- capped: the newest assistant entry is an API error carrying `429` or a quota marker. This mirrors `error_taxonomy.py`. Liveness is that timestamp, never mtime.
- held: compacting, or a compaction stamp inside its ceiling. Listed, never acted on.
- every row is swept with no liveness filter.

A lane is `open` at `quorum` capped members (default 2). One capped member plus the account's runtime-state lock also opens it. The reset epoch comes from that record. A lane with no reset prints `reset=unknown`. An account missing `reset_timezone` is named on every read.

## Leaving

Decision ladder per open lane:

1. Reset under `min_wait_minutes` (default 30): hold, `reason=short-reset`.
2. Reset unknown: ask. Never auto-move on an unmeasured window.
3. `mode=auto`, or now inside `sleep_hours`: act on all members.
4. Otherwise ask. One `operator_question` row names the lane, the reset, and every member. Nothing moves until `fno agents provider-cap decide <lane> --answer all|some:<ids>|wait` records an answer. A question that outlives its reset closes `superseded-by-reset`.

Destination: walk the spawn grid via `fallback_chain::resolve`, skipping the capped lane. Usage is refreshed first. Stale or exhausted after refresh is not a destination. With no healthy destination: `wait reason=no-healthy-destination`.

Migration per member, journalled to `<home>/provider-cap/<lane>-<epoch>.jsonl`:

- claude-to-claude: `/model <name>` via `fno agents mail send --raw`.
- other moves: a handoff doc names the node, branch, worktree, plan path, and the old transcript path. Spawn the successor with that doc, confirm via `fno agents truth`, then stop the old session. Spawn-confirmed-before-stop: a failed spawn leaves the capped session where it was.

## Returning

Planned as a dedicated follow-up node and not built yet: canary first, then a trickle, never a clock-only return. The one-liner: resume one canary at reset, require a clean survive window, then announce or trickle the rest.

## Config

```toml
[provider_cap]
enabled = false            # the actor's own arm; independent of recovery.watchdog
mode = "ask"               # ask | auto
min_wait_minutes = 30      # a reset closer than this is waited out
sleep_hours = ""           # "23:00-07:00" (IANA zone below); empty = never asleep
sleep_timezone = ""        # IANA name; empty = local
canary_survive_minutes = 15
quorum = 2
```

## State

`<agents home>/provider-cap/` holds `snapshot.json`, `decision-<lane>.json`, question markers, and the journals. Recorded in `docs/state-root-inventory.md`. Compaction stamps live in `<agents home>/compacting/<session>.json`. The `PreCompact` hook writes them best-effort via `fno-agents compaction mark`.
