# The spawn gate: one gate, one answer

There is ONE spawn gate: `crates/fno-agents/src/spawn_gate.rs` (with the lane axes in `spawn_gate_lanes.rs`). Every door - pane, routed, account, and the native bg/headless arms - asks the same gate. Python's `cli/src/fno/agents/spawn_gate.py` is a TRANSPORT over the `fno-agents spawn-gate` verb: it carries the caller's identity in, and the refusal (as data) out. This doc carries the long stories from the docstrings; the code keeps its prose short.

## The verb contract

`fno-agents spawn-gate` reads one JSON payload on stdin and writes one JSON answer on stdout. The verb exits 0 whenever it produced an ANSWER, including a refusal: a refusal is data, and the caller decides what to do with it. The gate's own prose (`spawn queued: ...` during a 600-second queue) streams on stderr.

Two modes:

- `gate` - the full admission gate. Payload: `{mode, name, substrate, force, no_wait, route_provider, account, caller_session, holder_pid}`. Admitted: `{status: "admitted", gate_key, gate_holder, worker_key, worker_holder}` (a key is null when that claim is not held). Refused: `{status: "refused", exit_code, receipt, event}`.
- `probe` - the read-only capacity reading behind `fno agents gate-status`, the lane readouts, `explain` and the advance width. Payload: `{mode: "probe", caller_session, only}`. `only: ["lanes"]` skips the CPU and RAM reads for callers already on the spawn path. The answer keeps the probe's verdict shape and adds three blocks every reader consumes: `lanes` (every capped provider AND every provider a live row names, each `{cap, live, counted}`), `share` (`{kings, share, held, held_rows, unattributed}`), and `rows` (the explain Gate dicts in `{name, measured, threshold, verdict, key, note}` shape).

## Claims cross the boundary owned by the caller

Every claim the verb takes in gate mode is stamped with the PYTHON caller's pid (`holder_pid`) and holder `spawn-gate:<holder_pid>:<name>`. The native claim verdict therefore judges the real holder, never the verb process, which has already exited by the time dispatch returns. The verb hands the held keys back in the admitted answer; the Python `GateGuard` releases them with the ordinary release path.

## The exit-code allocation table

One table, shared by both trees (`cli/src/fno/agents/spawn_gate.py` mirrored in `spawn_gate.rs`), kept unique by `cli/tests/unit/test_exit_code_allocation.py`: values >= 64 claim a number once, and the same NAME at the same number in both trees is byte-parity.

- 75 queue timeout - 76 no-wait - 77 RAM floor
- 78 provider cap (the quota lock and the lane faults keep it, so exit-code consumers are unaffected; the receipt's `reason` discriminates `provider_cap` / `provider_quota_lock` / `gate_mutex_unavailable` / `lane_reservation_unavailable`)
- 79 load - 80 king share - 81 registry schema
- 82, 83 the fleet incident pair (byte-parity)
- 84 state root ungranted (permanent until a human grants)
- 85 the Python sandbox probe
- 86 gate unavailable: the gate verb is missing, failed, or timed out. Fail closed, never admit on an unreadable gate.

## Refusal events stay Python-emitted

For spawns that ENTER Python (pane, routed, account), the verb returns the event fields (axis, axes_read, the measured figures) and the Python transport emits `spawn_gate_refused` through `_refuse` - one seam, unchanged journal vocabulary. The native unrouted arms emit nothing yet (x-ab75 owns a Rust emit, blocked on a config-resolved state-dir parity). See `scripts/ci/check-gate-refusals-emit.sh` for the seam guard.

## The registry schema check refuses writes it cannot understand

`check_registry_schema` refuses a spawn into a fleet whose shared registry this binary cannot write. A node claim and a mail stamp are both WRITES, so a registry ahead of this fno blocks the whole spawn path. On 2026-08-28 a worker reported "Claim store is not writable for this Codex session". The worker blamed a sandbox permission profile, and the report was believed. Nothing in that chain named the registry.

So the check refuses and does not warn. The message carries both integers, the file, and the repair verb. On the edges it matches the other guards: an unreadable file skips the check (a spawn is not the place to adjudicate a torn registry), because a refusal there can make the repair verb itself unspawnable. Both trees read the version from `src/registry_schema.toml` (build.rs projects the same file into the wheel), so the two sides of the check cannot disagree about a number.
