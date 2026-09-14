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

The actor reads the quota lock from `provider-runtime-state.json`, the file Python's recovery sweep writes. Before this change it defaulted to `runtime-state.json`, a file nobody writes. When the lock's `rate_limited_until` passes and no member's 429 is newer, the lane reads `returning`, not open. The return ladder owns it. The leave question and any recorded decision close as `superseded-by-reset`.

The ladder, one step per tick (120s), with state in `<home>/provider-cap/return-<lane>.json` keyed by the reset epoch:

1. Wait out `recovery.provider_outage_reset_grace_seconds` (default and floor 120) past the reset.
2. Resume ONE canary: the first candidate by name (capped, not held, not spawn-confirmed into a successor).
3. Hold a survive window of `canary_survive_minutes` (default 15).
4. When the newest assistant entry postdates the resume and carries no new 429, the canary survived. `cap_unknown` or a missing member reads `unknown`. So does no turn since the resume. `unknown` blocks every further resume for that epoch, with one operator notice.
5. In `ask` mode outside sleep hours, announce through `fno agents mail team --scope all` first. Then hold a 10-minute veto before anything else resumes. `fno agents provider-cap decide <lane> --answer wait` holds the rest for as long as the answer stands.
6. Otherwise resume the remaining candidates one per tick until the journal reads `return: complete`.

A canary that hits a new 429 reopens the lane. The quota lock gains a fresh future reset, and the leave ladder owns the new strand. The watchdog never wakes on a passed window. Its verdict reads `429 window passed; return owned by provider-cap` ([fleet-watchdog.md](fleet-watchdog.md)).

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

The return ladder's grace lives under `[recovery]`: `provider_outage_reset_grace_seconds` (default and floor 120).

## State

`<agents home>/provider-cap/` holds `snapshot.json`, `decision-<lane>.json`, the per-epoch canary state `return-<lane>.json`, question markers, and the journals. Recorded in `docs/state-root-inventory.md`. Compaction stamps live in `<agents home>/compacting/<session>.json`. The `PreCompact` hook writes them best-effort via `fno-agents compaction mark`.
