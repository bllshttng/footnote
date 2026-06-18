> **SUPERSEDED (2026-06-05, ab-d0337fbc):** the gate boolean transition matrix in this file (Completion Gates section) was deleted by the control-plane collapse wedge. The acceptance criteria gate before /do waves remains active. Kept for historical context; see docs/architecture/control-plane-loop.md.

# Phase Transition Guards

**Load when:** transitioning between any two phases of the pipeline (after execute, after validate, after ship, etc.). The matrix below is the authoritative list of preconditions.

After completing each phase:
1. **INVOKE the next skill immediately** - do NOT stop between phases

## Pre-Execution: Acceptance Criteria Gate (MANDATORY)

Before `/do waves`, verify plan has testable acceptance criteria (BDD Given/When/Then, AC1-/AC2- style, or `acceptance-criteria.md`). If none found, use the tdd skill's BDD criteria generation (`references/bdd-acceptance-criteria.md`) to generate them before proceeding. TDD requires criteria to write meaningful failing tests.

## Ordering invariants

The pipeline runs in a fixed order. Do not skip phases unless the corresponding skip flag (`no_docs`, `no_external`, etc.) is set in the manifest. The skip flags are immutable after init - check the manifest read by `fno target init`, do not decide to skip phases based on your judgment mid-run.

Key ordering rules that survive the control-plane collapse:
- Run docs BEFORE /pr create so docs ride in the same PR (avoid a follow-up PR).
- Run browser testing BEFORE /pr create (same reason).
- Do not run auto-merge until external review is satisfied (or `no_external` is set).

These are workflow rules, not gate checks. The loop-check verb verifies the outcome (PR + CI + review); you ensure the pipeline ran in the right order.

## Why ordering matters

An LLM under context pressure may rationalize skipping a phase. The skip flags in the immutable manifest are the anti-skip mechanism: if a skip flag is not set, the phase must run. The loop-check verb does not enforce ordering; it checks the final state. Correct ordering is the skill's responsibility.
