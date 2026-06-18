---
title: Minimal E2E Fixture Plan
scope: feature
created: 2026-04-22
---

# Minimal E2E Fixture Plan

Hermetic fixture plan used by `tests/integration/test_loop_e2e.py` to
drive `fno loop` through `review -> reconcile -> complete` end-to-end.
The plan intentionally has no real tasks; the test stubs every worker
and reality-check so no external side effects (network, subprocess,
disk outside tmp_path) fire.

## Scope Classification

- scope: feature
- contract: no production behavior change - this file exists only for
  the e2e test harness to point at.

## Critical Path Trace

```
test driver -> run_loop(state_path, plan_path=plan.md)
  -> _run_reconcile (stubbed) -> pr_merged
    -> _enforce_completion_gates -> check_gate_safe(each)
      -> quality_check_passed: artifact present -> ok
      -> output_validated: no artifact required (external-truth gate, ab-10cb7d28) -> ok
      (all optional gates skipped via no_X flags)
    -> write status: COMPLETE -> return action=complete
  -> check_gate_safe(loop_reached_complete) -> ok
```

No stubs remain in the critical path: every arrow resolves to real
production code. The test stubs only the boundary (reconcile worker,
gh reality check) so the loop internals stay honest.
