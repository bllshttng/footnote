# A test run must not outlive itself

Every `fno doctor test` suite now runs through one native owner: `fno-agents test-run`, in `crates/fno-agents/src/test_run.rs`. A test run used to leave three kinds of mess behind it on the same box. Too many threads. Too many concurrent suites. Processes that outlived the run that spawned them.

## The three defects, and the one fix for each

**Threads exceeding the CPU count.** `cli/src/fno/test_cmd.py`'s `_run_rust` used to pass the `fno doctor lanes` worker-headroom reading straight through as `--test-threads`, with no ceiling. A lane is a worker-session slot, not a thread. An idle 12-core box reads 64 lanes free. 64 threads on 12 cores is the fan-spin the operator reported. The clamp is `max(1, min(lane_count, os.cpu_count()))`. When the caller gave no explicit `--test-threads`/`--jobs` override, the clamp applies. Even an explicit override is itself clamped to the CPU count, never trusted past it.

**A clean exit leaving a group-mate running.** The pre-native `wait_or_kill_group` in `cli/src/fno/test_runner.py` killed the run's process group only on `TimeoutExpired` or an exception. Picture a leader (cargo) that exits normally, after backgrounding a child that has not yet finished. That child stayed alive in the same process group, unreachable by a plain `wait()`-based reaper. Only `killpg` reaches every process sharing a pgid, regardless of parent/child lineage. One such orphaned `deps/` test binary held 225 of the machine's 228 zombies for 3h32m. The native owner runs `cleanup_group` after every run: success, failure, timeout, or a signal to the owner itself. It never skips cleanup on the clean-exit path the way the old code did.

**Two suites racing for the same machine.** Nothing previously stopped two `fno doctor test` invocations from both spawning workers at once. The native owner admits under one machine-wide `test:suite` claim first (`crates/fno-agents/src/claims.rs`). A contender waits and spawns zero workers until the holder releases.

## The owner's contract

`fno-agents test-run --timeout SECS [--claims-root PATH] -- ARGV...` is internal dispatch only. `bin/client.rs` matches it the same way it matches `probe-run`. It is never a public `fno` verb. `cli/src/fno/test_runner.py`'s `run_suite_bounded` is the sole caller. When the `fno-agents` binary is not installed, it falls back to the pre-native Python group-kill (timeout only), so a bare checkout degrades rather than fails.

Each run does the same five things in order. First, acquire the suite claim, unless this run is itself nested inside another (see below). Second, spawn the argv as the leader of a fresh session via `setsid()`. It and everything it forks then share a pgid the owner never touches. Third, wait bounded by the timeout. Fourth, unconditionally `killpg` the group: SIGTERM, then SIGKILL after a grace window, confirmed empty by `killpg(pgid, 0)` rather than inferred from the leader's own exit. Fifth, release the claim.

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
- A second cargo waits. It compiles nothing while it waits, and prints `cargo admission: holding; <holder> is building` at most every 30 seconds.
- A cargo started under the holding cargo, such as a test that runs cargo, is admitted at once. The check walks the process ancestors of the waiting cargo.
- A waiter yields while the holder runs a nested cargo. Cargo takes its build-dir lock before it calls the wrapper. So a waiter can hold the lock that the nested cargo needs, and each then waits on the other. `crates/fno/tests/cross_door_property.rs` builds fno-agents from inside `cargo test -p fno`, which is that shape. The waiter scans the process table every 5 seconds for it.
- A signal that stops the wait stops the compile too. The wrapper exits with the signal's code and starts no rustc.
- A compiler probe (`-vV` or `--print`) never asks. Cargo metadata and IDE probes must not block.
- Admission fails open. With no `fno-agents` on PATH, or an older one that lacks `build-admit`, the wrapper prints one line and builds.

A waiting build writes a marker under `<claims root>/.fno/claims/build-waiters/`, keyed by its checkout. The stop hook reads that marker for its own cwd and each parent up to the first `.git`, for both drivers. So a worktree nested inside another checkout never reads that checkout's hold. While the waiter lives, `loop-check` allows the stop with the hold as its message and counts no fire. An agent that backgrounds a held build therefore idles instead of burning to `NoProgress`. The same early allow covers a fleet incident stop, because the stop hook's pause read folds in `fleet_incident` beside the manual sentinel.

## What this does not cover

Raw `pytest`, or tests under `crates/fno`, invoked outside `fno doctor test`, bypass the wrapper entirely. They get none of this: no admission, no thread clamping, no group cleanup. Rust tests under `crates/fno-agents` declare their own test-binary identity. They apply it to every daemon or client spawn. Bare `cargo test` therefore covers those daemon lifetimes but still does not acquire the machine-wide suite claim or clamp test threads. The contract is scoped to the front doors this repo's tooling actually uses, not to every possible way of invoking a test binary. A regression controller that wants isolated test state runs a prebuilt binary directly, instead of going through the wrapper. That path skips recursively acquiring the live machine's `test:suite` claim.

A process that calls `setsid()`, like `claude daemon run`, leaves the owner's session entirely. `cleanup_group` reaches the leader's own process group by construction. A daemon that called `setsid` is no longer in that group, so step four above never signals it. Measured 2026-09-10: three `claude daemon run` processes, each rooted in a deleted pytest garbage directory, survived every group cleanup and detached to ppid 1. That population is covered instead by cwd. The session reaper `_reap_session_processes` in `cli/tests/conftest.py` reaps, and fails the session on, any process whose working directory lies under the run's own basetemp.

The census keys on a path where "Keepers bound to a test's lifetime" refused to, and the difference is what the path proves. Locked Decision 4 rejected socket filenames, `/tmp`, and argv substrings because a production keeper and a leaked test keeper carry indistinguishable ones. A basetemp is created by the one pytest session that names it, so a process rooted under it was started inside that session. A `garbage-<uuid>` directory exists only after pytest proved the owning session's lock stale, so no live session roots there. Neither fact holds for a `/tmp` path or a socket filename, which prove nothing about who started the process.
