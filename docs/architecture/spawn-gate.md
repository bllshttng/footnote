# The spawn gate: why it refuses

The gate lives in `cli/src/fno/agents/spawn_gate.py` (the only gate on every `fno agents spawn` path) and in its Rust twin `crates/fno-agents/src/spawn_gate.rs`. This doc carries the two long stories from the docstrings. The code keeps its prose short.

## The load ceiling reads two instruments (measured twice)

`_check_load_ceiling` runs three thresholds. Below `max_load_per_cpu x cpus` the gate admits the spawn and probes nothing, so the common path costs no subprocess. Above the trigger the gate asks footprint whose CPU this is. When the fleet holds more than `max_fleet_cpu_share` of capacity, the refusal fires. `hard_max_load_per_cpu x cpus` refuses with no attribution check.

The history: the check once refused on the 1-min load average. The same refusal printed the contradicting footprint attribution: `load 127.6 exceeds ... 96.0` beside `attributes 0.79/12.00 cores (6.6% capacity)`. Load average counts runnable PLUS blocked processes, so it is not a CPU measure and belongs to nobody. On 2026-08-29 the three largest consumers on the refusing box were desktop applications. An operator killed one unscoped ripgrep, and the 1-min load moved from 374 to 179 with no agent stopped. A gate that refuses beside its own contradicting measurement teaches an operator to reach for `--force`. That is how a guard becomes a formality.

The hard backstop exists because a pure fleet-share governor admits onto a box that already thrashes from foreign work. Keep the backstop well above the trigger. The `AgentsBlock` defaults are 8 and 40.

## The registry schema check refuses writes it cannot understand

`_check_registry_schema` refuses a spawn into a fleet whose shared registry this fno cannot write. A node claim and a mail stamp are both WRITES, so a registry ahead of this fno blocks the whole spawn path. On 2026-08-28 a worker reported "Claim store is not writable for this Codex session". The worker blamed a sandbox permission profile, and the report was believed. Nothing in that chain named the registry.

So the check refuses and does not warn. The message carries both integers, the file, and the repair verb. On the edges it matches the other guards. An unreadable file skips the check: a spawn is not the place to adjudicate a torn registry. A refusal there can make the repair verb itself unspawnable.

The event is the other half. Every degraded READ prints a banner. A refused WRITE returns an error to one caller, and the caller reports it in its own words to a king who is not watching. Nothing collects those reports into "the fleet cannot write". Emission is best-effort and never blocks the refusal.
