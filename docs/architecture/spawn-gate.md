# The spawn gate: one gate, one answer

There is ONE spawn gate: `crates/fno-agents/src/spawn_gate.rs`, with the lane axes in `spawn_gate_lanes.rs`. Every door asks the same gate: pane, routed, account, and the native bg/headless arms. Python's `cli/src/fno/agents/spawn_gate.py` is a TRANSPORT over the `fno-agents spawn-gate` verb. It carries the caller's identity in. It carries the refusal out, as data. This doc carries the long stories from the docstrings. The code keeps its prose short.

## The verb contract

`fno-agents spawn-gate` reads one JSON payload on stdin. It writes one JSON answer on stdout. The verb exits 0 whenever it produced an ANSWER, including a refusal. A refusal is data. The caller decides what to do with it. The gate's own prose (`spawn queued: ...` during a 600-second queue) streams on stderr.

Two modes:

- `gate` runs the full admission gate. Its payload is `{mode, name, substrate, force, no_wait, route_provider, account, caller_session, succession_scope, holder_pid, seed, session_phase}`. An admitted answer is `{status: "admitted", gate_key, gate_holder, worker_key, worker_holder}`. When a claim is not held, its key is null. A refusal is `{status: "refused", exit_code, receipt, event}`. When the seed first verb or explicit `session_phase` label names review, the gate refuses. Seed verbs use `spawn_phase.toml`.
- `probe` is the read-only capacity reading. `fno agents gate-status`, the lane readouts, `explain` and the advance width all consume it. The payload is `{mode: "probe", caller_session, only}`. Set `only: ["lanes"]` to skip the CPU and RAM reads. This is for callers already on the spawn path. The answer keeps the probe's verdict shape. It adds three blocks every reader consumes. `lanes` covers every capped provider and every provider a live row names, each `{cap, live, counted}`. `share` is `{kings, share, held, held_rows, unattributed}`. `rows` holds the explain Gate dicts in `{name, measured, threshold, verdict, key, note}` shape.

## Claims cross the boundary owned by the caller

Every claim the verb takes in gate mode is stamped with the PYTHON caller's pid. The holder reads `spawn-gate:<holder_pid>:<name>`. The native claim verdict therefore judges the real holder. It never judges the verb process, which has already exited by the time dispatch returns. The verb hands the held keys back in the admitted answer. The Python `GateGuard` releases them with the ordinary release path.

## Crown succession reuses one verified slot

When crown settlement confirms the caller will vacate its sole live row for `succession_scope`, the gate subtracts one from the slot count. The exception ends on predecessor exit or failed live-row registration by the successor. While succession is pending, the fleet can sit at most one row above `max_live`.

## The exit-code allocation table

One table is shared by both trees. It lives in `cli/src/fno/agents/spawn_gate.py` and is mirrored in `spawn_gate.rs`. `cli/tests/unit/test_exit_code_allocation.py` keeps it unique. Values >= 64 claim a number once. The same NAME at the same number in both trees is byte-parity.

- 75 queue timeout
- 76 no-wait
- 77 RAM floor
- 78 provider cap. The quota lock and the lane faults keep this number, so exit-code consumers are unaffected. The receipt's `reason` discriminates `provider_cap`, `provider_quota_lock`, `gate_mutex_unavailable`, and `lane_reservation_unavailable`.
- 79 load
- 80 king share
- 81 registry schema
- 82 and 83, the fleet incident pair, byte-parity
- 84 state root ungranted. Permanent until a human grants.
- 85 the Python sandbox probe
- 86 the per-territory team cap. Exit 86 is the permanent, non-queueable territory-cap refusal. Unreadable territory attribution uses the same exit. Callers do not retry it as capacity.
- 87 gate unavailable. The gate verb is missing, failed, or timed out. Fail closed: never admit on an unreadable gate.
- 88 blueprint thread cap. More than `agents.profiles.blueprint.max_live` live `bp` threads, or more than one per territory, refuses the spawn and teaches the subagent path.
- 89 review session. A seed or label that names review causes a permanent refusal. The refusal runs before `--force` and the `FNO_SPAWN_GATE=0` bypass. Run the inline fno review lane.

## Reading a refusal

The last `spawn-gate: refused on <axis> (<reason>, exit <code>): <figures>` line on stderr is the verdict. It names the axis that refused and its breach. The figures are the receipt's scalar fields. Every other gate line starts `spawn-gate note:` and passed. The Python reader `cli/src/fno/backlog/advance.py` (`_gate_refusal_detail`) keys on the `spawn-gate: ` prefix and reads the last such line, so it gets the verdict, never a passing reading.

A stderr with no `spawn-gate:` line means the gate admitted and something after it failed. The real error is the first non-note line there, never a `spawn-gate note:` line.

## Refusal events: Python via the seam, routing from Rust

Some spawns ENTER Python: pane, routed, and account. For them the verb returns the event fields, and the Python transport emits `spawn_gate_refused` through `_refuse`. The journal vocabulary is unchanged. The native unrouted arms now emit too. The route-slot verb appends one `spawn_gate_refused` row, `gate: "routing"`, at the verb entry, so a refused native spawn leaves a trace beside the Python rows. The Rust side has no config-resolved state-dir parity yet, so the row lands in the space journal rather than the global one. The verb read is the answer: `fno doctor event find spawn_gate_refused --field gate=routing` scans every journal and rotation. The emit and its guard test live in `crates/fno-agents/src/route_slot.rs` (`journal_routing_refusal` and the `journal_rows` tests). See `scripts/ci/check-gate-refusals-emit.sh` for the Python seam guard.

## The registry schema check refuses writes it cannot understand

`check_registry_schema` refuses a spawn into a fleet whose shared registry this binary cannot write. A node claim and a mail stamp are both WRITES. A registry ahead of this fno therefore blocks the whole spawn path. On 2026-08-28 a worker reported "Claim store is not writable for this Codex session". The worker blamed a sandbox permission profile, and the report was believed. Nothing in that chain named the registry.

So the check refuses and does not warn. The message carries both integers, the file, and the repair verb. On the edges it matches the other guards. An unreadable file skips the check, because a spawn is not the place to adjudicate a torn registry. A refusal there can make the repair verb itself unspawnable. Both trees read the version from `src/registry_schema.toml`. build.rs projects the same file into the wheel. The two sides of the check therefore cannot disagree about a number.
