# Machine footprint

`fno doctor footprint` is the read-only instrument for the fleet's own machine cost. It measures two closure numbers.

Sustained `fno` CPU must stay at or below one core. `fno` process count must stay at or below live roster rows plus one daemon. Resident memory is reported for context. It is not part of the closure criterion.

The CPU number is `sustained_cpu_cores`. It comes from `ps -Ao pid,etime,%cpu,rss,command` after rows are split by elapsed time.

A process with `etime` below `SUSTAINED_FLOOR_SECONDS` (default 30 seconds) contributes to `transient_call_count` and `process_count`, but not sustained CPU. This prevents one-shot Python CLI startup cost from appearing as daemon load.

Only rows whose command starts with the `fno`, `fno-py`, `fno-agents` or `fno-agents-daemon` executable are attributable to fleet overhead. `claude` worker processes are the work the fleet runs, not the fleet's own control-plane cost, so they are excluded.

The verb runs `ps` once with stdout redirected to a temporary file, then reads that file. It reads the process threshold from `fno agents list --json` and adds one daemon allowance. It never uses load average and never counts through a `ps | wc` or `ps | grep` pipeline. `--json` emits the same thresholds and measurements for automation.

Exit codes are intentionally separate. `0` means both closure numbers are within threshold. `3` means at least one number is over budget. `4` means the instrument failed to read required data, including an unavailable roster or unparsed `ps` rows. A transient CLI burst does not produce exit 3 by itself.

Load average is refused because it did not describe this machine's contention. On 2026-08-22, `uptime` reported load averages of 206.45 / 219.30 / 182.03 on a 12-core machine. A full `ps` snapshot showed 30 runnable processes out of 1280. It showed 672 percent CPU out of 1200 available, or about 56 percent utilization. The instrument measures fleet-attributable rows instead of converting that system signal into a footprint claim.

This is a human-, CI- or king-invoked reading, not a daemon or poller. A watcher adds the cost being measured.
