# The spawn gate: why it refuses

The gate's contracts live in `cli/src/fno/agents/spawn_gate.py` (the sole gate on every `fno agents spawn` path) and its Rust twin `crates/fno-agents/src/spawn_gate.rs`. This doc carries the two long stories that were living in docstrings, so the code keeps its prose short.

## The load ceiling asks two instruments (x-7c0f, measured twice)

`_check_load_ceiling` runs three thresholds. Below `max_load_per_cpu x cpus` the gate admits without probing, so the common path costs no subprocess. Above the trigger the gate asks footprint whose CPU this is. When the fleet holds more than `max_fleet_cpu_share` of capacity, the refusal fires. `hard_max_load_per_cpu x cpus` refuses regardless of attribution.

The history: the check once refused on the 1-min load average while printing footprint's contradicting attribution in the same refusal: `load 127.6 exceeds ... 96.0` beside `attributes 0.79/12.00 cores (6.6% capacity)`. Load average counts runnable PLUS blocked processes, so it is not a CPU measure and belongs to nobody. On 2026-08-29 the refusing box's three largest consumers were desktop applications. Killing one unscoped ripgrep moved the 1-min load from 374 to 179 with no agent stopped. A gate that refuses beside its own contradicting measurement teaches an operator to reach for `--force`. That is how a guard becomes a formality.

The hard backstop exists because a pure fleet-share governor can admit onto a box already thrashing from foreign work. Keep the backstop well above the trigger. The `AgentsBlock` defaults are 8 and 40.

## The registry schema check refuses writes it cannot understand

`_check_registry_schema` refuses a spawn into a fleet whose shared registry this fno cannot write. Claiming a node and stamping mail are both WRITES, so a registry ahead of this fno blocks the whole spawn path. On 2026-08-28 that produced a worker that reported "Claim store is not writable for this Codex session", attributed it to a sandbox permission profile, and was believed. Nothing in that chain named the registry.

So the check refuses rather than warns, and the message carries both integers, the file, and the repair verb. On the edges it matches the other guards. An unreadable file skips: a spawn is not the place to adjudicate a torn registry. A refusal there can make the repair verb itself unspawnable.

The event is the other half. Every degraded READ prints a banner. A refused WRITE returns an error to one caller, who reports it in its own words to a king who is not watching. Nothing collects those into "the fleet cannot write". Emission is best-effort and never blocks the refusal.
