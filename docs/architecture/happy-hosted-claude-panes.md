# Happy-hosted claude panes: who owns the session id

`fno agents spawn --harness claude --monitor happy ...` puts the `happy` wrapper between fno and claude.
That one hop changes who owns the session id, and getting it wrong produced a worker that reported `live` for about 55 minutes with no session behind it.

## happy discards a pinned `--session-id`

happy's `claudeLocal()` calls `extractFlag(["--session-id"], true)`, which **removes** the flag and its value from the caller's argv.
It re-adds `--session-id` only on its `!hookSettingsPath` branch.
happy's start path assigns `hookSettingsPath` unconditionally (it generates a settings file carrying its `SessionStart` hook), so that branch is unreachable for a normal launch.

The claude child therefore mints its own session id, and any id fno pinned is gone.

This is deterministic, not a race. It applies to every happy-hosted claude pane.

## Why a pinned id was worse than no id

Every supervision surface fno owns keys on the session id: `fno agents peek`, `fno agents truth`, the registry row's `harness_session_id`, and transcript lookup.
Handed an id that was discarded, all of them fail the same way whether the worker is healthy or dead:

- `fno agents peek <name>` reports "peer not found"
- the registry row reads `session=none`
- no transcript is ever found at the receipt's id

So the receipt asserting a session id it could not deliver is what made `live` and `dead` indistinguishable.
The pane still had a real pid, a real pane id, and a painted first frame, which is why every documented liveness tell read healthy.

## What fno does now

1. **It does not pin.** `dispatch_spawn_pane` resolves the monitor before building the argv and skips minting a `session_uuid` on the happy route. `happy_pane_argv` refuses outright if an argv still carries `--session-id`, so an in-process caller that builds its own argv cannot reintroduce the lie.
2. **It discovers.** `_backfill_claude_session_id` finds the transcript that appeared for this cwd after the pane started - the same shape as the existing codex and opencode backfills. It is stability-gated: an id is accepted only once the same single id repeats on the next probe, so a same-cwd sibling spawning at the same moment yields nothing rather than being mis-claimed.
3. **It reports the miss.** No transcript means no claude session started, so the row lands as `spawning`, not `live`, and an `agent_session_id_uncaptured` event is emitted. A pane created but not addressable is a state worth naming; calling it live is the failure this page exists to prevent.

The default (unmonitored) claude route is unchanged: fno pins `--session-id` there and claude honours it.

## Two namespaces, not one

A pane's own id (what `fno mux pane read` shows) and the claude session id are separate namespaces, the way mail handles and roster short ids already are.
On the default route they coincide because fno pins the same uuid into both.
On the happy route they never coincide, and expecting them to match is not a defect signal.

The defect signal is the absence of any claude session at all.

## Reading a suspect pane

Counters, not prose.
A pane displaying a plugin's session-start memory dump looks exactly like an agent working, and has been mistaken for one twice.
Read the context/token counters, and sample a known-working sibling in the same window: "0%" only reads as wrong next to a "15%".
