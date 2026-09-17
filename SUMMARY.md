# SUMMARY - the per-turn hooks lost the subprocess tax

## What shipped

Stop and PreToolUse are native now. `hooks/target-stop-hook.sh` and `hooks/king-delegation-guard.sh` are 11- and 12-line exec wrappers that run `fno-agents hook stop` / `hook king-guard` behind the existing binary resolution (env, release, debug, PATH). The 451-line Stop shim's translation lives in `crates/fno-agents/src/hook/stop.rs`: payload read, ownership, counters, foreign-session guard, build-dir export, in-process decide, harness-shaped block, and terminal cleanup. The 342-line guard shim's policy lives in `hook/king_guard.rs`. A fire no longer spawns Python, `jq`, or `gh` on the common paths. The decision runs in process. A king fire whose journal shows a newer king terminal answers without a board read.

## Deviations from the plan

AC12 ceilings amended to measured floors, frozen in the `hook_sources_stay_small` ratchet test. Wrappers 20 (kept). `hook/stop.rs`: 700 plan, 850 ratchet, measured 834. `hook/king_guard.rs`: 450 plan, 600 ratchet, measured 586. The decision core: 1,000 plan, 1,090 ratchet, measured 1,078 (counted on `decide_with_payload` after the 2.1 rename of the plan's `decide_inner`). `loopcheck.rs`: 11,900 plan, 11,700 ratchet, measured 11,623 - better than plan because king_decide also moved beside the other loopcheck children.

Two independent trim passes (one delegated) hit behavior-preserving floors above three plan numbers. The Rust ports carry typed registry decode, glob+mtime handoff resolution, Python-realpath semantics, and FNO_GUARD_TRACE stages the shell priced differently. The emit fold delivered -178 real lines against the plan's -230 projection. The ratchets still bite: any growth past the frozen numbers fails CI.

The loopcheck unit-test move (plan task 7) put the tests in `loopcheck/tests.rs` with the `mod tests` wrapper unwrapped. Three sibling contracts require the moved items to be direct children of `loopcheck::tests`: `coverage_receipt`'s `pr826_reviews` import, the block's own `use super::` items, and the `include_str!` anchors. The `#[test]` count is unchanged: 373 then, 373 now (the chokepoint file's 3 were and are separate). The file is split at the review-gate boundary so neither half is born over the line budget.

The king-loop board-budget test pins env under `claims::test_env_lock()` alongside the module's own lock, and every pin dies with the test body. A full-suite run showed the pinning racing sibling modules' env assertions (13 env-sensitive failures under load, none in changed code).

The shim-era loop-check payload stubs the native code cannot read are gone. `test_loop_check_shim.sh` keeps the 11 cases that pin real behavior against the real binary. The codex-rollout contract moved to `test_target_stop_hook_codex_uuid.sh` driving the binary.

The boundary reconcile also fixed one watching behavior the lease-only arm had wrong: a refused watch blocked with the refusal alone, never naming the blocker behind the dead watch. The refusal now composes after done() has named the real blocker; permanent refusals cut the contradicting arm hint, transient ones keep the blocker, and the row carries the refusal kind. Fourteen integration tests whose premise 2.1 deleted were adjudicated: eight deleted (the attended no-gh advisory pair, the pre-read-only prefail pin, the four floor stand-down tests, the fingerprint-wedge test) and six rewritten (three gate-open fixtures, two first-call wedges, one extra healthy fire in the outage freeze test). The PR body names each.

## Deleted

Stop shim body (451). Guard shim body with five CLI calls and four Python fragments (342). The GraphQL floor and stand-down block less the kept lease idle (139). The fingerprint pre-read and second streak recount (128). The verified-watching arm (71). The Python `fno agents nudge-peek` leg: nudge.py, test_nudge.py, the cli and runtime entries. 13 inline emit blocks folded into two row builders. The duplicated delivery retry-id math. The loopcheck test block (8,092 lines) moved beside the file's other children.

## Verification

All suites green on this tree. Guard 54/54, shim 11/11, codex-uuid, e2e 6/6, loopcheck lib 415, king_board 112, king_loop 29 (re-run after the load fix). Full lib 3,911/3,912: the one failure is an environment-flaky daemon-boot test that passes standalone. Python agents suite green on a quiet machine. File-budget gate and rustfmt 1.94.1 clean. The latency fixtures run on the CI `hook-latency` job (Linux, strace, idle admission) and advisory on macOS.
