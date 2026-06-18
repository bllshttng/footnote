---
created: 2026-04-22T00:00
status: accepted
---

# Think / What-If / Spec Failure-Modes Contract

## Overview

`/think` is now contractually obligated to produce a `## Failure Modes`
section in every design doc it saves. `/blueprint` reflects the other side of
that contract: when its input is a design-doc file, it grep-scans for the
heading and refuses to plan when it is missing. `/think what-if` plays the
deep-dive role and emits the same section on demand, so `/think` can hand
off to it for complex features and fold the findings back in.

The goal is to make failure-mode thinking mechanical, not optional. Before
this contract, design docs trended toward happy-path coverage and `/blueprint`
generated phase files with AC1-HP and AC2-ERR only. The acceptance criteria
that catch silent failures, race conditions, and invariant breaks (the
AC4-EDGE family) were aspirational. Now they are seeded directly from the
design doc's Failure Modes section, cited by name, so a reviewer can trace
every edge-case test back to the design-time reasoning that produced it.

## Contract Shape

The three skills agree on one structure:

```markdown
## Failure Modes

**Boundaries**
- The system must handle a cart with 0 items (render empty state)
- The system must reject line-item quantities above 10,000

**Errors**
- The system must preserve form state when /checkout returns 500

**Invariants**
- The system must preserve the invariant that an order has exactly one address

**Concurrency**
- The system must handle two submit clicks within 100ms as a single order
```

Rules the parser relies on:

- Level-2 heading `## Failure Modes` (case-sensitive, on its own line).
- Four bold sub-section labels in any order: Boundaries, Errors,
  Invariants, Concurrency.
- Each sub-section is a bold label on its own line followed by a dash-bullet
  list. The parser scans bullets until the next bold label or the next
  `##` heading.
- Every bullet begins with "The system must handle", "must reject", or
  "must preserve" so the language carries a testable obligation.

The contract is deliberately heading-shaped rather than frontmatter-shaped:
design docs are read by humans first, and a YAML field would bury the
content in metadata. A prominent heading is grep-friendly AND scannable.

## Flow Across Skills

```
user asks /think for design
  |
  |-- Step 6b enforces inline Failure Modes sub-section enumeration
  |
  |-- if feature is complex (>=3 external deps OR auth/payments/concurrency),
  |   emit hand-off prompt:
  |     Run /think what-if <domain> <depth> failure-modes "<scope>" to stress-test: <categories>
  |
  +-- saves design doc with mandatory `## Failure Modes` heading

user runs /think what-if with failure-modes modifier (or standalone)
  |
  |-- explores via 12-dimension matrix
  |
  +-- writes what-ifs.md; with failure-modes modifier, adds a top-level
      `## Failure Modes` block using the same Boundaries / Errors /
      Invariants / Concurrency vocabulary

user runs /blueprint with the design-doc path
  |
  |-- Failure Mode Ingestion gate: grep -q '^## Failure Modes$'
  |   |
  |   |-- missing -> refuse with verbatim message, halt with non-zero status
  |   |   "Design doc at {path} is missing ## Failure Modes section. Run /think first."
  |   |
  |   +-- present -> parse the four sub-sections, preserve wording
  |
  +-- seeds AC4-EDGE acceptance criteria per phase file (full mode)
      or inline per Change (quick mode), citing each source bullet:
      "AC4-EDGE: Cites "Double-submit" from design doc"
```

## Why a Grep Gate and Not an LLM Check

`/blueprint`'s refusal path runs as a shell grep before any LLM token is spent
on plan generation. The gate is mechanical for three reasons:

1. **Determinism** - an LLM "check" can rationalize an in-prose mention or
   a paraphrased heading as satisfying the contract. Grep cannot.
2. **Upstream pressure** - the whole point is to force failure-mode
   thinking into `/think`, not paper over a skipped step in `/blueprint`. An
   auto-generated fallback defeats the reason the gate exists.
3. **Speed** - grep is cheap. Rejecting early means the user re-runs
   `/think` before investing in a bad plan.

The refusal message is a verbatim template so it reads the same whether the
caller is target, operator, or a user shell. The halt returns a non-zero
status, so autonomous pipelines see it as a hard failure rather than a
warning they can ignore.

## Interaction with Target

`/target` runs `/think` first (for idea inputs) and `/blueprint` next. If `/think`
honors its Step 6b, `/blueprint`'s gate always passes. If `/think` is skipped
(plan-path input) the gate is skipped too, because the plan already exists
and was presumably produced by a compliant `/think` or vetted by a human.

No state changes in target-state.md are needed; the contract is purely
document-level. The stop hook's gate audit is unaffected.

## Standalone Modes Preserved

- `/think` outside a target session still enforces Step 6b - the contract
  lives in the skill, not the pipeline.
- `/think what-if` without the `failure-modes` modifier is unchanged from prior
  behavior: no mandatory Failure Modes summary, just the 12-dimension
  exploration. This keeps `/think what-if` usable for incident postmortems,
  red-team exercises, and roadmap pre-mortems that have no `/think`
  context.
- `/blueprint` with a raw feature description (not a design-doc path) skips the
  grep gate because there is no file to scan. A warning surface here would
  punish legitimate use cases.

## Related Files

- `skills/think/SKILL.md` Step 6b and Step 8 (Save Design Document).
- `skills/think/what-if.md` "Invoked from /think" section.
- `skills/blueprint/SKILL.md` "Failure Mode Ingestion" section.
- `skills/blueprint/references/phase-template.md` AC4-EDGE citation examples.
