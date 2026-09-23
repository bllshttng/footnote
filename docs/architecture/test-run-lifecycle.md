# A test run must not outlive itself

Every `fno doctor test` suite now runs through one native owner: `fno-agents test-run`, in `crates/fno-agents/src/test_run.rs`. A test run used to leave three kinds of mess behind it on the same box. Too many threads. Too many concurrent suites. Processes that outlived the run that spawned them.

## The three defects, and the one fix for each

**Threads exceeding the CPU count.** `cli/src/fno/test_cmd.py`'s `_run_rust` used to pass the `fno doctor lanes` worker-headroom reading straight through as `--test-threads`, with no ceiling. A lane is a worker-session slot, not a thread. An idle 12-core box reads 64 lanes free. 64 threads on 12 cores is the fan-spin the operator reported. The clamp is `max(1, min(lane_count, os.cpu_count()))`. When the caller gave no explicit `--test-threads`/`--jobs` override, the clamp applies. Even an explicit override is itself clamped to the CPU count, never trusted past it.

**A clean exit leaving a group-mate running.** The pre-native `wait_or_kill_group` in `cli/src/fno/test_runner.py` killed the run's process group only on `TimeoutExpired` or an exception. Picture a leader (cargo) that exits normally, after backgrounding a child that has not yet finished. That child stayed alive in the same process group, unreachable by a plain `wait()`-based reaper. Only `killpg` reaches every process sharing a pgid, regardless of parent/child lineage. One such orphaned `deps/` test binary held 225 of the machine's 228 zombies for 3h32m. The native owner runs `cleanup_group` after every run: success, failure, timeout, or a signal to the owner itself. It never skips cleanup on the clean-exit path the way the old code did.

**Two suites racing for the same machine.** Nothing previously stopped two `fno doctor test` invocations from both spawning workers at once. The native owner admits under one machine-wide `test:suite` claim first (`crates/fno-agents/src/claims.rs`). A contender waits and spawns zero workers until the holder releases.

## The owner's contract

`fno-agents test-run --timeout SECS [--claims-root PATH] -- ARGV...` is internal dispatch only. `bin/client.rs` matches it the same way it matches `probe-run`. It is never a public `fno` verb. `cli/src/fno/test_runner.py`'s `run_suite_bounded` is the sole caller. When the `fno-agents` binary is not installed, it falls back to the pre-native Python group-kill (timeout only), so a bare checkout degrades rather than fails.

Each run does the same five things in order. First, acquire the suite claim, unless this run is itself nested inside another (see below). Second, spawn the argv as the leader of a fresh session via `setsid()`. It and everything it forks then share a pgid the owner never touches. Third, wait for the argv, bounded by the timeout. The timeout counts from admission, so a run that queued keeps its whole budget. Fourth, unconditionally `killpg` the group: SIGTERM, then SIGKILL after a grace window, confirmed empty by `killpg(pgid, 0)` rather than inferred from the leader's own exit. Fifth, release the claim.

The queue wait has no timer, the same as the cargo doors. It ends on admission or on SIGINT or SIGTERM. A waiter admitted after a fleet stop refuses like a fresh run.

SIGINT and SIGTERM delivered to the owner itself are caught, not fatal by default. A Ctrl-C or a supervisor kill triggers the same cleanup path, and the owner exits `128 + signal` afterward. A plain Python subprocess wrapper no longer isolates the owner into its own group, so a terminal signal reaches the owner directly. Left uncaught, that signal skips cleanup entirely under the default disposition.

A nested invocation is one test run that itself shells out to another `fno doctor test`. It reads `FNO_TEST_OWNER_PID`/`FNO_TEST_OWNER_BIRTH` from its environment, set by the outer owner on every child it spawns. When that pid is verified alive with a matching birth stamp, the nested run skips admission. It never deadlocks on a claim its own ancestor holds. A stale or foreign token is never trusted. That case (inherited from an unrelated ancestor shell) re-acquires a fresh claim like any top-level run.

When the test itself passed, cleanup failure still reports as a nonzero exit. A green suite that leaked its group must not read as done.

## Keepers bound to a test's lifetime

The pane keeper (`pane_keeper.rs`) and the graph store keeper (`graph_keeper.rs`) both outlive their launcher by design. That is the whole point of a keeper, for a production session. A test-owned keeper is the opposite case. A keeper spawned inside a test fixture must die with that test.

Left alone, a test-owned keeper becomes exactly the leaked-process shape this document exists to close. Five confirmed orphaned `fno-agents-worker --pane` processes were traced to keeper-hosting tests, with no `Drop` on the pty type to reap them.

The pane keeper (`pane_keeper.rs`), graph store keeper (`graph_keeper.rs`), and `fno-agents-daemon` read the same `FNO_TEST_OWNER_PID`/`FNO_TEST_OWNER_BIRTH` identity. The test-run owner threads it through its child's environment. When that env is present, each keeper polls the owner's liveness at least every 250ms. On the owner's death, or a birth-identity mismatch, each process exits through its owner-specific path. The pane and graph keepers kill their hosted child and unlink the socket. The daemon sends SIGTERM to itself and runs its existing graceful shutdown path. A declared dead owner is refused at daemon startup with exit 3. None waits for an explicit Shutdown or Kill frame. A wedged or killed test never sends one.

A keeper with no such environment is completely unaffected. That covers every production pane and every production graph store. The polling thread never spawns for one.

This is deliberately never a name- or path-based sweep. Locked Decision 4 for this feature refuses to infer permission to kill a pane from its socket filename, `/tmp`, PPID 1, or an argv substring. That heuristic cannot tell a leaked test pane from a live production one.

## Build admission

The suite claim covers `fno doctor test` only. A bare `cargo build` or `cargo test` does not pass through it. On 2026-09-16 two rustc test builds from separate worktrees ran at once and took the load to 508 on 12 cores. The `jobs = 3` cap in `~/.cargo/config.toml` limits one cargo. It cannot stop two.

So every compile asks for admission. `.cargo/config.toml` sets `scripts/lib/cargo-rustc-wrapper.sh` as the rustc wrapper, for every worktree and every harness. Before each compile the wrapper runs `fno-agents test-run build-admit --cargo-pid <cargo> --worktree <checkout>`. That call takes the machine-wide `build:cargo` claim with holder `cargo:<checkout>:<cargo pid>`.

- The claim records the cargo pid and has no TTL. Once that cargo exits, the claim is free, so there is no release call. With a TTL, a dead cargo's claim reads `suspect`, and a newcomer waits for the whole TTL.
- A second cargo waits. It compiles nothing while it waits, and prints `cargo admission: holding; <holder> (pid N); waited Ns` at most every 30 seconds.
- A cargo started under the holding cargo, such as a test that runs cargo, is admitted at once. The check walks the process ancestors of the waiting cargo.
- A waiter yields while the holder runs a nested cargo. Cargo takes its build-dir lock before it calls the wrapper. So a waiter can hold the lock that the nested cargo needs, and each then waits on the other. `crates/fno/tests/cross_door_property.rs` builds fno-agents from inside `cargo test -p fno`, which is that shape. The waiter scans the process table every 5 seconds for it.
- A holder cargo that has run no compile for 30 seconds loses the slot to a waiter. A compile is a process under the holder whose argv names `rustc`, or a `build-script-*` binary. The argv can name rustc bare, through sccache, or through this wrapper. So a cargo in its test phase holds the slot for at most about 35 seconds past its last compile. A `cargo run` program holds it for the same window. The waiter releases the idle holder's claim, prints `cargo admission: taking over; ...`, and takes the claim with a reason naming the idle holder. A displaced holder's next rustc call then waits like any cargo. A holder the process table cannot see keeps the slot.
- A signal that stops the wait stops the compile too. The wrapper exits with the signal's code and starts no rustc.
- A compiler probe (`-vV` or `--print`) never asks. Cargo metadata and IDE probes must not block.
- Admission fails open. With no `fno-agents` on PATH, or an older one that lacks `build-admit`, the wrapper prints one line and builds.

The idle window is neither a TTL nor a CPU read. Age cannot tell a hung test from a long compile. A CPU read fails too. sccache runs rustc under its own server, so a compiling cargo reads 0 percent CPU while it holds the slot. The window asks one question: does the holder's cargo have a compile process under it right now. That question separates a test phase from a compile phase. A misread costs nothing. Two compiles can overlap until the displaced holder's next rustc call, which then waits like any cargo. That is why an argv read is acceptable here, where the pane-keeper decision refuses argv inference for killing panes: nothing dies on a misread.

A waiting build writes a marker under `<claims root>/.fno/claims/build-waiters/`, keyed by its checkout. The stop hook reads that marker for its own cwd and each parent up to the first `.git`, for both drivers. So a worktree nested inside another checkout never reads that checkout's hold. While the waiter lives, `loop-check` allows the stop with the hold as its message and counts no fire. An agent that backgrounds a held build therefore idles instead of burning to `NoProgress`. The same early allow covers a fleet incident stop, because the stop hook's pause read folds in `fleet_incident` beside the manual sentinel.

## Run admission

Both cargo doors queue in arrival order beside the suite door. A waiter holds a ticket in a FIFO dir beside the claim's lockfile and attempts the acquire only at the front. The compile and run-slot doors are new to this. The suite door has queued this way since the claim queue shipped.

The build claim covers the compile door only. A fresh-build `cargo test` calls no rustc, so it passed admission entirely while it ran every test binary. On 2026-09-17 at 22:37 this 12-CPU machine carried 9 live cargo runs at once, and every mitigation was a human killing processes by hand. So the machine holds a pool of run slots beside the build claim: `test.max_cargo_runs` in `.fno/config.toml`, default 2. A cargo run takes a slot at the first door it reaches. The slot is keyed to the cargo pid with no TTL. When that cargo exits, the slot frees.

Both doors are the same wrapper. Before every compile, the rustc wrapper asks `build-admit` for the build claim. Before every test binary and doctest, the same wrapper asks `run-admit` for a slot. Cargo calls it as the target runner, set in this repo's `.cargo/config.toml` for the two macOS target triples. The line is keyed by target triple, not `cfg(all())`, on purpose. When two runner entries match one target, cargo refuses to start. The error reads `several matching instances of target.'cfg(..)'.runner`, and two cfg runners reproduce it on cargo 1.94.1. A triple runner instead takes precedence over any cfg runner. Measured on cargo 1.94.1: a repo triple runner plus a user `cfg(unix)` runner exits 0 with the repo runner firing. A user triple runner for the same host is overridden silently by this repo's line. A user's own cfg runner keeps working everywhere else. Linux hosts set no runner, so CI compiles and tests unwrapped. The slot keys are `test:cargo-run:0` up to one below the cap. The lock order is fixed: a cargo takes its run slot first, then `build:cargo`. A runner-door cargo never needs `build:cargo`. Cargo finishes every compile before it runs a binary. A nested cargo is admitted to both by ancestry. So no cargo waits on a slot while it holds `build:cargo`. The machine runs at most one cargo that has compiled anything, and at most `test.max_cargo_runs` cargo runs in all.

- A cargo under a slot holder is admitted without a second slot. A doctest's rustdoc and a test that starts cargo are that shape. The status pass reads every slot for an ancestor holder before any acquire writes.
- A waiter prints `cargo admission: holding; 2 of 2 cargo run slots held by <holder> (pid N), ...` at most every 30 seconds. The line names every holder. The waiter also writes the same stop-hook marker as a build waiter.
- The audit trail is the `claim_acquired` event each admission writes. Its claim reason carries the slot number and the wait.
- Admission fails open at the runner door, exactly as at the compile door. With no `fno-agents` on PATH, or one that exits below 128, the wrapper prints `run admission unavailable (exit N)` once per cargo. Then the binary runs anyway.

What the cap does not cover: a cargo started outside a footnote checkout reaches neither door. Config discovery follows the cwd. A checkout whose tree predates the merge runs fresh builds unslotted until it rebases. Its compiles still take a slot, because that comes from the deployed fno-agents. A wedged cargo holds its slot until it exits. A `cargo run` of a long program holds a slot for the program's life. A rust-analyzer check waits like any cargo. A foreground Bash call that times out kills its own shell. The cargo it started keeps its slot until it finishes or is killed.

## Lanes

Every admission door orders its waiters in up to three lanes, best first: `priority`, then `queue` (arrival order), then, at the suite door only, `full`. A lane is one claim-queue dir beside the claim's lockfile. A waiter tries the acquire only while every better lane is empty and it sits at the front of its own lane. An empty lane reserves nothing. No lane ever signals a running holder. A holder runs to completion (or to its own budget) regardless of who waits.

The priority lane names one checkout. The user sets it, or a king sets it on the user's word:

```
fno agents claim acquire test:priority --holder worktree:<checkout> --ttl 30m --pid-unavailable -R "<why>"
fno agents claim status test:priority
fno agents claim release test:priority --holder worktree:<checkout>
```

A second acquire is refused and names the holder. A lane with no TTL is ignored. The lane reorders all three doors: compile admission, run slots, and the suite claim. The idle takeover at the build door is unchanged. A holder cargo that runs no compile still loses the slot after the idle window, lane or no lane.

The full lane exists at the suite door only. A whole-crate argv selects every test in a crate: no test-name filter, no `--test`/`--bin`/`--example`/`--bench`/`--doc`, no nextest filterset. `--lib` alone counts as whole. The run waits in the full lane while targeted runs queue, for at most its own budget, then joins the normal queue at the back. The guard refuses a whole suite from an agent outright unless the command carries the `FNO_TEST_FULL=1` prefix. The full lane is that escape hatch's queue, not a normal path.

The waiting line names what a waiter is doing. `holder_left_s=` on the suite wait reads the holder's remaining budget and goes negative once the holder overruns it. `lane=`, `yielding_to=` and `priority=` name the waiter's lane, the better lane it is yielding to, and the checkout that holds the priority lane.

## What this does not cover

Raw `pytest`, raw `cargo test`, or tests under `crates/fno`, invoked outside `fno doctor test`, bypass the wrapper entirely. They get none of this: no admission, no thread clamping, no group cleanup. Since 2026-09-19 the tool boundary closes that door for agent sessions. The `hooks/test-run-guard.sh` PreToolUse shim refuses a raw `pytest`, `uv run pytest`, or `cargo test` in command position inside a footnote checkout, through its native policy `fno-agents hook test-run-guard`. The refusal names the narrowest door: `fno doctor test <test files>`, or `fno doctor test rust --manifest-path crates/<crate>/Cargo.toml --lib <module>::` for the crates. A whole crate suite, through `fno doctor test rust` or `fno-agents test-run`, is refused too unless the command carries the `FNO_TEST_FULL=1` prefix. CI runs every suite on every PR. That verb acquires the machine-wide `test:suite` claim. Scripts that invoke pytest internally still fail open, and non-footnote repositories are out of scope. Rust tests under `crates/fno-agents` declare their own test-binary identity. They apply it to every daemon or client spawn. Where a bare `cargo test` does run, on a macOS host it takes a run slot at the runner door. It still gets no suite claim, no thread clamp, or group cleanup. The contract is scoped to the front doors this repo's tooling actually uses, not to every possible way of invoking a test binary. A regression controller that wants isolated test state runs a prebuilt binary directly, instead of going through the wrapper. That path skips recursively acquiring the live machine's `test:suite` claim.

A process that calls `setsid()`, like `claude daemon run`, leaves the owner's session entirely. `cleanup_group` reaches the leader's own process group by construction. A daemon that called `setsid` is no longer in that group, so step four above never signals it. Measured 2026-09-10: three `claude daemon run` processes, each rooted in a deleted pytest garbage directory, survived every group cleanup and detached to ppid 1. That population is covered instead by cwd. The session reaper `_reap_session_processes` in `cli/tests/conftest.py` reaps, and fails the session on, any process whose working directory lies under the run's own basetemp.

The census keys on a path where "Keepers bound to a test's lifetime" refused to, and the difference is what the path proves. Locked Decision 4 rejected socket filenames, `/tmp`, and argv substrings because a production keeper and a leaked test keeper carry indistinguishable ones. A basetemp is created by the one pytest session that names it, so a process rooted under it was started inside that session. A `garbage-<uuid>` directory exists only after pytest proved the owning session's lock stale, so no live session roots there. Neither fact holds for a `/tmp` path or a socket filename, which prove nothing about who started the process.
