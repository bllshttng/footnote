# Provider cap actor

The one armed surface that owns every provider-cap move. Split from the fleet watchdog (x-7e05): the watchdog measures and reports a strand; only this actor may stop, move, or resume a session for quota reasons. The design was settled with the operator on 2026-09-11 and is recorded on node x-7e05.

## How a lane opens

`fno-agents provider-cap status [--json] [--max-age-s N]` answers from the daemon's latest snapshot (fresh only if `measured_at` is within `--max-age-s`; otherwise it computes on demand and says so). The daemon's `provider_cap` arm rebuilds the snapshot every 120 s, armed or not; an unarmed arm persists the snapshot and writes a tick row with `skip_reason=provider_cap_off`, so the arms readout shows a live measurement rather than a wall plaque.

For each registry row the snapshot reads:

- provider lane from the row's `observed_model` (the declared `model`/`route_provider_id` fields are NULL fleet-wide; a row whose observed model is unreadable lands in the `unknown` lane, never inside a real one)
- capped: the transcript's newest assistant entry is an API error carrying `429` or a quota marker (`rate limit` / `quota exceeded` / `usage limit`, case-insensitive), the same signals `error_taxonomy.py` classifies as `PROVIDER_4XX_QUOTA`. The newest assistant timestamp is the liveness read; mtime is a lie (a user entry 1 minute old can sit 10 hours after the last assistant entry)
- held: a member whose `compaction_state` is `Compacting` or a stamp whose ceiling has not expired (`held=compacting`); listed, never acted on
- every registry row is swept, no liveness filter: the 429 itself destroys the liveness a filtered sweep would key on

A lane is `open` when `quorum` members are capped (default 2) or one member is capped and the account carries a runtime-state quota lock (`provider_health.<account>.rate_limited_until`). The reset epoch comes from that same record; a lane whose record has no reset prints `reset=unknown`, and an account missing `reset_timezone` is named on every status read (a wrong-by-eight-hours reset is the measured cost).

## Leaving (waves 3)

Decision ladder per open lane:

1. `reset - now < min_wait_minutes` (default 30): hold, `reason=short-reset`; the return wave resumes members at reset.
2. `reset` unknown: ask (never auto-move on an unmeasured window).
3. `mode=auto`, or now inside `sleep_hours` in `sleep_timezone`: act on all members.
4. Otherwise ask: one `operator_question` row in `questions.jsonl` naming the lane, the reset, every member, and destination options with fresh headroom; one `notify_operator` pointer. Nothing moves until `fno agents provider-cap decide <lane> --answer all|some:<ids>|wait` records an answer. A question that outlives its reset closes `superseded-by-reset` and the return wave takes over.

Destination: walk the same dispatch grid the spawn used (`fallback_chain::resolve`), skipping the capped lane. Every candidate account's usage is refreshed first (`fno config accounts usage --refresh`); stale-or-exhausted-after-refresh is not a destination (trap 3). With no healthy destination: `wait reason=no-healthy-destination`.

Migration per member (each step journalled to `<home>/provider-cap/<lane>-<epoch>.jsonl`):
- claude-to-claude: `/model <name>` via `fno agents mail send --raw`.
- any other move: a handoff doc naming node, branch, worktree, plan path, and the OLD transcript path, so the successor resumes with context instead of from zero; spawn on the destination with that harness's canonical verb; confirm through `fno agents truth`; then stop the old session and release its claim. Spawn-confirmed-before-stop, so a failed spawn leaves the capped session where it was.

## Returning (wave 4)

Leaving is evidenced by a positive 429 + quorum. Returning is evidenced by an absence, so it is gated on a positive survival reading:

1. One canary resumes through the x-6ac3 wake path; with no stranded member, one fresh dispatch routes to the lane.
2. The canary survives when, `canary_survive_minutes` (default 15) after resume, its newest assistant entry is newer than the resume time and not a new 429. Unreadable = `unknown`, blocks the trickle.
3. On survival: sleep hours or `mode=auto` resume the rest one per tick; otherwise one announcement via `fno agents mail team --scope all` with a veto window (`provider-cap decide <lane> --answer wait` holds).
4. A canary 429 reopens the lane with the new reset and returns to the leave ladder.

Sessions already migrated stay where they are; the grid returns new dispatches once headroom clears.

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

`<agents home>/provider-cap/`: `snapshot.json` (latest tick's snapshot), `decision-<lane>.json` (operator answers), `<lane>-<epoch>.jsonl` journals. Recorded in `docs/state-root-inventory.md`. Compaction stamps live in `<agents home>/compacting/<session>.json` and the `PreCompact` hook writes them best-effort via `fno-agents compaction mark`.
