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
2. **It does not guess.** The row lands id-less and `spawning`, and an `agent_session_id_uncaptured` event is emitted. A pane created but not yet addressable is a state worth naming; calling it live is the failure this page exists to prevent.
3. **The worker names itself.** Its SessionStart hook restamps the row via `restamp_harness_session_id`, keyed on `FNO_AGENT_SELF` - the one identity on the row a harness cannot re-mint. That restamp promotes a `spawning` row to `live`, because a worker that has proven its own identity is addressable.

The default (unmonitored) claude route is unchanged: fno pins `--session-id` there and claude honours it.

### Why the spawn does not read the transcript store

Finding the transcript that appeared for this cwd after the pane started is the obvious move, and it was tried. It is unsound.

Two happy panes starting in one cwd snapshot the same baseline. If pane A's transcript is the only new one for as long as the probe runs, and pane B writes its registry row first, B records A's session id. Peek, mail, truth, and resume for B then all target A, which is very much alive and doing something else.

Recency cannot separate them either: a claude transcript is appended to for the life of its session, so a sibling that merely *writes* during the probe looks newer than the spawn.

Binding a row to a healthy stranger is strictly worse than leaving it unbound. The worker naming itself is proof; anything the spawn can observe from outside is inference.

## Two namespaces, not one

A pane's own id (what `fno mux pane read` shows) and the claude session id are separate namespaces, the way mail handles and roster short ids already are.
On the default route they coincide because fno pins the same uuid into both.
On the happy route they never coincide, and expecting them to match is not a defect signal.

The defect signal is the absence of any claude session at all.

## Resume reads the canonical id

`harness_session_id` is the canonical session id every supervision surface keys on.
`short_id` is claude's transport key alone.
It is the 8-hex jobId `claude attach` takes.
A mux row carries a `harness_session_id` but no `short_id` by design.
`_validate_single_live_ref` enforces mux XOR worker XOR bg.
A row never holds both a pane ref and a worker transport key.
Resume that read `short_id` alone saw "no session id" on every pane worker and refused, never reaching the relaunch arm.

Both runtimes resolve one id before reading.
Python's `load_registry` folds the legacy per-provider keys into `harness_session_id` and drops them on read.
The Rust loader mirrors `harness_session_id` back into `claude_session_uuid`.
That lets the raw-Value helpers, which still read the legacy key, resolve it.
Any new reader of a raw registry row must run that backfill before reading either field.
Skipping it re-creates the gap on a row shape the loader already healed.

Resume probes liveness on the canonical uuid, not the transport short_id.
A pane worker has no short_id to gate on.
Gating on it short-circuited the truth probe to "inconclusive" for a session the operator can see is gone.
A truth verdict the falsifier confirms is process-gone (`pane-gone`, `process-gone`) routes to relaunch.
A `silent` or `no-evidence` unreachable stays inconclusive.
The process can still be alive, and relaunching opens a second writer on one transcript.
A recorded route that cannot be read refuses rather than relaunching on the default account.
A row that records no route refuses for the same reason.
That silent fallback is what produced a session that looked alive and ate every turn.
A mux row relaunches on its recorded session via `fno mux pane run`, the one-verb form of the manual recovery.

## Reading a suspect pane

Counters, not prose.
A pane displaying a plugin's session-start memory dump looks exactly like an agent working, and has been mistaken for one twice.
Read the context/token counters, and sample a known-working sibling in the same window: "0%" only reads as wrong next to a "15%".
