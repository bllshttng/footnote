---
name: tdd
description: "Test-Driven Development: write the test first, watch it fail, implement minimal code. Use when implementing a feature or bugfix, during /execute, or when a plan carries acceptance criteria."
---

# Test-Driven Development (TDD)

**Core principle:** If you didn't watch the test fail, you don't know if it tests the right thing.

## The Iron Law

NO PRODUCTION CODE WITHOUT A FAILING TEST FIRST.

## The cycle

1. **RED:** write ONE minimal test for the next acceptance criterion (one behavior per test, clear name, real assertions). Only a change with those surfaces carries database and UI assertions.
2. **Verify RED (mandatory):** run the test. It must FAIL - not error - because the behavior is missing, with the expected failure message. A test that passes immediately tests existing behavior: write a different test. A test that errors: fix the error until it fails correctly.
3. **GREEN:** write the simplest code that passes. Nothing beyond what the test requires.
4. **Verify GREEN (mandatory):** the test passes and the other tests still pass. Still failing? Fix the implementation, not the test.
5. **REFACTOR:** only after green, keep tests passing, add no behavior.
6. **Commit** the pair.

## Acceptance criteria become tests

Each criterion from `/blueprint` maps to one named test. `AC1-HP` -> the happy path. `AC2-ERR` -> the error state. `AC3-UI` -> the visible change. `AC4-EDGE` -> the boundary.

## Recovery rule (code exists without a failing test)

Delete the code you wrote for the unproven behavior (the one with no failing test) and keep everything else: unrelated work, earlier green cycles, refactors. Then write the failing test that PROVES the behavior is missing. Run it and watch it fail for the right reason, then re-implement minimally against it. "Keep it as reference" is deletion deferred. Delete means delete. Exploration code is the same: delete it, start from the test.

## In `/execute`

Per task: read the task -> load /tdd -> RED, verify, GREEN, verify, refactor, commit -> next task. Worked examples and the rationalization table: [references/examples.md](references/examples.md).

## Verification checklist

Before marking the task complete, check the six marks. Test first. Watched failure for the correct reason. Minimal code. All tests green with clean output. Edge cases covered. Committed. A box you cannot check means the cycle was skipped - go back to RED.

## Known Limitations and Deferred Work

- TDD does not prove untested integration boundaries. See [LIMITATIONS.md](LIMITATIONS.md).
