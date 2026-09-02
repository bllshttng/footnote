# The resource meter and the lane advisor

The operator ask behind this feature was a live monitor. It must say how the machine is doing and make a best guess on how many more lanes the fleet can take. Two surfaces answer it. `fno doctor lanes` is the on-demand verb: one number and its reasoning. The status row carries the same reading as a live one-line meter, once you switch the meter on.

## The feature is conditional

The meter needs `macmon` on PATH. Install it with `brew install macmon`. It is Apple Silicon only and needs no sudo. fno core does not depend on it. If it is absent, nothing breaks, and `config.resource_meter.enabled` ships false. Turn the meter on with `fno config set resource_meter.enabled true`, or in the settings modal's general tab beside the status-row toggle.

## What you get without macmon

Two arms still work, because they read fno's own numbers. The spawn-load arm compares the 1-minute load against `max_load_per_cpu x ncpu`. The unexplained-processes arm compares direct processes against the roster. The arms that go dark are whole-machine CPU, memory, and power and thermals. `fno doctor lanes` names which arms are dark and which still work, and refuses to print a lane number. A dark sensor is never treated as headroom.

## Why whole-machine

`fno doctor footprint` measures fno's own descendants, and that is the right answer to its own question. But the browser, Slack and every other app compete for the same box. A lane answer built on fno-only load will keep advising "room for more" while something else eats the machine. The lane advisor reads the whole machine first and the fleet second.

## Which memory signal is authoritative, and which state is dark

Swap is the pressure signal, but only for a machine that has a swap file. On the machine this feature was specified against, `ram_usage` read 81.5 GB of 103 GB. `swap_usage` and `swap_total` were both 0, and `sysctl vm.swapusage` confirmed no swap file exists. A swap-only rule reports infinite headroom there. A free-RAM rule is no better: `memory_pressure` called that same machine 87 percent free while the compressor held 7.3 GB. So the memory arm reads `swap_usage` for a machine with a swap file. On a machine without one, `memory_pressure` is the arm, and a machine where neither sensor answers gets no reading at all. Neither number alone is the answer.

## The two verdicts are different alarms

`fno doctor footprint` prints two readings and they must never share one exit code. "Unexplained processes" is a leak alarm: processes the roster cannot explain, exit 5. "Capacity" is a planning alarm: the spawn load against its ceiling, exit 3. When both fire, capacity takes the exit and the leak still prints. Conflating the two already caused a competent reader to misread the leak detector as a capacity ceiling repeatedly in a single session.
