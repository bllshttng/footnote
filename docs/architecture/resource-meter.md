# The resource meter and the lane advisor

The operator ask behind this feature was a live monitor. It must say how the machine is doing and make a best guess on how many more lanes the fleet can take. Three surfaces answer it. `fno doctor lanes` is the on-demand verb: one number and its reasoning. The status row carries the same reading as a live one-line meter, once you switch the meter on. The court panel puts both in front of a person, behind `prefix` then `C` in the mux.

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

## The court panel

Press `prefix` then `C` in the mux. The panel shows the 1-minute load against the cap, whole-machine CPU and free memory, the census, the lane advisor's own answer, and the age of the reading. Every number on it comes from one `fno doctor lanes --json` call. The panel adds no capacity model of its own, because two estimators that disagree is a worse problem than an invisible one.

The cap is the thing the panel exists to make visible. It is `max_load_per_cpu` times the CPU count, and it is the only gate on a spawn. Before this panel, only an agent running a hidden verb saw it.

Three render rules keep the panel honest, and each closes a way a monitor can lie.

The `read` line always shows the fold's age. A reading past the cache TTL renders with its age and the word `stale`, never as `unknown`. An operator watching `unknown` every second learns nothing from the panel and reaches for `--force`, which is how a guard becomes a formality.

The attribution gap gets its own line and is never folded into a count. The gap is a failure to attribute a PROCESS to a registry ROW, so it cannot change how many rows exist: every row carries its crown level whether or not it carries a pid. It belongs beside the CPU reading it qualifies, where it says what it means, which is that the fleet CPU share is an undercount rather than headroom.

A refusal prints the advisor's own words with no lane number beside them. A dark sensor is not headroom, and only the advisor knows which sensor went dark.

## Two measured budgets, and why neither was chosen by reasoning

The panel's fold budget is 10 seconds and its cache TTL is 15. Both are pinned to one measurement: `fno doctor lanes --json` took 8.07 seconds on a machine at 1-minute load 107. The verb's own `macmon` sample is bounded at 5.0 seconds, so a read over five seconds is its designed worst case rather than a fault.

When the machine is busy, a shorter budget guarantees a degrade. That is the only time a person opens the panel. When the TTL is under the read's own cost, every open refetches and every reading is stale on arrival. That is a busy loop wearing a cache's clothes.

A long fold budget costs nothing here. The fold runs off the UI loop, one at a time, and the overlay opens on the keypress whether or not the fold has landed. `kill_on_drop` reaps the child on a timeout, so an overrun cannot leak a Python process.

## The census counts rows for people and processes for tests

`census.kings` and `census.workers` are counts of registry ROWS. `census.tests` is a count of PROCESSES. The two must never be added together or folded into each other.

Kings come from `gather_court` over the same live rows list the per-lane cost divides by. So `kings` plus `workers` always equals `roster_rows`, and the two halves can never describe different fleets. The panel reports `king_conflicts` beside the count, because a bare king number hides the case that matters. Two live rows over one scope both report agreement while the fleet has two kings.

A running test is a process whose OWN program is a test runner, matched on `argv[0]` plus the first non-flag arguments. Never on the whole command line. On the machine this was measured against, `ps | grep -i pytest` reported four running tests while two ran. One decoy was a shell wrapper whose command line happened to hold `cargo test`. The other was a leaked keeper process whose socket path sat under a `pytest-of-<user>` temp directory. The substring was in the path, never in the program.

## The roster count is read in process

`fno doctor footprint` sets its leak threshold from the count of live registry rows. That count used to arrive over a `fno agents list --status live --json` subprocess under a 5.0 second budget. The read measured 8.5 seconds at rest and 21.7 seconds under load. So the roster went dark exactly when the reading mattered. The report printed `roster unavailable`, and then `unexplained processes: unknown`.

It now reads `load_registry` in process, which answers the same question from the same file in well under a second. There is no budget left to miss and no cache to age. An incomplete or unreadable registry still degrades the threshold away. Its reason names the registry, not a timeout that no longer happens.
