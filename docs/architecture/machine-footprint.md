# Machine footprint

`fno doctor footprint` is the read-only instrument for the fleet's total machine cost. It measures direct fno control-plane work plus the transitive descendants those workers launched.

The closure criterion is `fleet_cpu_cores` at or below one core and `direct_process_count` at or below live roster rows plus one daemon. `process_count` still reports the full attributed tree. Resident memory is reported for context. It is not part of the closure criterion.

The verb reads one file-backed `ps -Ao pid,ppid,etime,%cpu,rss,command` snapshot. It resolves each row's parent chain with a cycle-safe, memoized walk. A directly attributable row is a process whose executable is `fno`, `fno-py`, `fno-agents`, `fno-agents-daemon` or `fno-agents-worker`, including Python-launched `fno-py`. Every transitive descendant inherits attribution, so shells, `cargo`, `rustc`, `pytest` and future workload commands land in the fleet total. An unrelated tree with byte-identical commands remains outside the fleet.

The direct bucket is `sustained_cpu_cores`. A directly attributable process with `etime` below `SUSTAINED_FLOOR_SECONDS` (default 30 seconds) contributes to `transient_call_count`, `direct_process_count` and `process_count`, but not direct sustained CPU. The descendant bucket is `descendant_cpu_cores` and counts descendant CPU immediately, including short-lived build and test processes. `fleet_cpu_cores` is the sum of direct sustained and descendant CPU. `descendant_process_count` identifies how much of the process count came from launched work.

The observer process and its complete descendant subtree are excluded from CPU, process, RSS and top-consumer totals. Missing parents and ancestry cycles terminate safely without attributing a row unless the chain reaches a valid fno root. Each PID is counted at most once.

`--json` emits `sustained_cpu_cores`, `descendant_cpu_cores`, `fleet_cpu_cores`, `descendant_process_count`, `direct_process_count`, `process_count`, `rss_gb`, `cpu_capacity_cores`, `fleet_percent_capacity` and `fleet_percent_measured_cpu` with the thresholds and exit code. `fleet_percent_capacity` is fleet cores divided by constrained logical CPU capacity: the minimum of the affinity count, the host count and the cgroup quota. The spawn gates compute their load ceilings from the affinity/parallelism count alone, so on a quota-constrained host the footprint denominator can be smaller than the ceiling the refusing gate used; the two numbers answer different questions (machine capacity vs admission ceiling). `fleet_percent_measured_cpu` is fleet CPU divided by total CPU in the non-observer rows of the snapshot.

The hidden `--cause-only --json` mode performs only the `ps` read and emits the CPU fields without reading the live roster. It is used by spawn-gate diagnosis, not by the standard closure verdict.

Exit codes are intentionally separate. `0` means the standard closure numbers are within threshold, or that cause-only measurement succeeded. `3` means a standard CPU or direct-process threshold is over budget. `4` means required input cannot be read, including an unavailable roster, malformed `ps` rows or incomplete cause evidence. A transient direct CLI burst does not produce exit 3 by itself.

Load average remains the spawn admission signal. Footprint does not replace it because a point-in-time CPU snapshot cannot answer whether runnable work is queued. When load crosses the configured ceiling, both spawn gates retain exit 79. When cause evidence is available, they append bounded footprint cause evidence. The evidence distinguishes a fleet-heavy refusal from a refusal caused by other machine processes. Missing, timed-out or malformed evidence prints `spawn-gate: footprint cause unavailable; load refusal unchanged` and never changes admission.

This is a human-, CI- or king-invoked reading, not a daemon or poller. A watcher adds the cost being measured.

Mux admission is a separate native pre-spawn instrument. Every Rust child launch acquires a per-user machine-global lock. It takes a process snapshot without starting an observer. The permit remains alive through the child-creation syscall. The census attributes descendants of active `fno` binaries or the current cargo-test binary. It includes live and zombie rows. An ancestry cycle or unreadable required row returns unavailable. The mux root process is not a worker slot.

The Python pane launcher carries the resolved limits as `FNO_MUX_MAX_LIVE` and `FNO_MUX_PANE_GROUP_MAX`. Every `PanePlacement` carries `max_panes`. A legacy client that omits the wire field receives the conservative configured/default pane cap. Fleet and tab ceilings remain separate. They add no new operator setting.

Every refusal is a positive, parseable marker: `process admission refused: count=<n> ceiling=<n> scope=<fleet|tab> reason=<over-limit|measurement-unavailable|lock-unavailable>`. Unknown measurement never becomes count zero or headroom. Recovery starts by reconciling or reducing live rows. Then rerun `fno doctor footprint`. There is no force bypass.
