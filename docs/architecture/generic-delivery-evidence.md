---
created: 2026-08-02T17:00
status: review
---

# Generic Delivery Evidence

## Overview

Generic delivery evidence gives an explicitly activated company-work plan one function-agnostic completion path.
It derives a complete, ordered verdict from current runtime facts, then lets the Rust control plane terminate as `DoneDelivery` only when every declared requirement passes.
Existing PR and research sessions keep their exact `DonePRGreen` and `DoneAdvisory` paths.

## Component Graph

```mermaid
graph TD
    P[Plan frontmatter] --> A[completion: delivery]
    P --> C[CompanyWorkRefs]
    J[Coherent events.jsonl snapshot] --> R[Delivery event reader]
    X[Approval and effect events] --> R
    O[delivery_evidence_observed] --> R
    R --> F[DeliveryEvidenceFact values]
    C --> E[Canonical Python evaluator]
    F --> E
    E --> V[Complete DeliveryVerdict]
    V --> B[Hidden fno delivery evaluate boundary]
    B --> L[Rust loop-check]
    L --> T[DoneDelivery]
    T --> Z[Rust finalize]
    Z --> S[Plan stamp and graduation]
    Z --> H[Deterministic delivery receipt]
```

## Activation

Generic completion is opt-in.
A plan activates it only with `completion: delivery`, a valid `company_work.work_order`, at least one deliverable, and at least one declared required evidence ID.
`company_work` by itself, a projection-carried `passed` value, an empty requirement list, a process receipt, or a function name does not activate or satisfy the finish line.

```yaml
completion: delivery
company_work:
  work_order: {node_id: x-example, attempt_id: attempt-1}
  deliverables:
    - id: published-output
      kind: arbitrary-output
      work_order_id: x-example
      attempt_id: attempt-1
      required_evidence_ids: [artifact-ready, approval-ready, destination-ack]
```

Plan-carried evidence results declare projection shape only and never establish runtime delivery.

## Evaluation Flow

```mermaid
flowchart LR
    A[Read file metadata] --> B[Read exact journal bytes]
    B --> C[Re-read metadata]
    C -->|changed or partial| U[undeterminable]
    C -->|stable| D[SHA-256 fact revision]
    D --> N[Normalize current producer events]
    N --> E[Evaluate every declared slot]
    E -->|all passed| P[passed verdict]
    E -->|any failed| F[failed aggregate]
    E -->|otherwise blocked| K[blocked aggregate]
    E -->|otherwise incomplete| U2[unknown aggregate]
```

The evaluator retains one row per declared requirement in declaration order.
Non-passing aggregate precedence is `failed`, then `blocked`, then `unknown`, but precedence never removes the other rows.
Missing, stale, malformed, conflicting, unreadable, future-dated, attempt-mismatched, or mixed-revision facts remain unknown and cannot pass.
Observation subjects are excluded, so later business performance cannot rewrite delivery truth.

## Producer Boundary

Approval and effect lifecycle state remains owned by `fno.approvals`.
Delivery consumes the public `evidence_projection` seam and the documented approval and effect events.
The adapter selects current source events from one coherent snapshot, validates work-order and attempt bindings, and projects facts into uniquely matching required slots.
It never authorizes a decision, dispatches an effect, mutates the effect journal, or declares aggregate success.

An acknowledged effect may cover separately declared effect and destination-acknowledgment slots for the same effect ID.
`failed` remains failed, `blocked` remains blocked, and `prepared`, `executing`, or `unknown` remain unknown.
A declined approval is a blocked prerequisite rather than evidence that the deliverable failed.

## CLI Contract

The inspection and Rust process boundary is hidden from the curated top-level menu:

```bash
fno delivery evaluate --json --plan-path path/to/plan.md --events .fno/events.jsonl
```

The response version is `delivery-evaluate-response.v1`, with status `inactive`, `evaluated`, or `undeterminable`.
Rust accepts a pass only from a strict, complete evaluated response whose fact revision matches the verdict and whose non-observation requirement rows all pass with producer and source-revision evidence.
Unknown versions, extra fields, malformed JSON, partial verdicts, and passed observation rows remain nonterminal.

## Terminal and Finalization

Loop-check honors cancellation, budget, abort, and open operator findings before accepting `DoneDelivery`.
It must durably append the session-bound `delivery_verdict_evaluated` event before returning the terminal.
Finalize consumes that selected event without evaluating again.

Finalize stamps and graduates the plan when safe, writes a deterministic `fno-delivery://` receipt and generic handoff, and records the normal ledger and run summary.
It does not query GitHub, stamp a PR number, arm auto-merge, assume a merge, or invoke the research evaluator.
Cross-project plans retain the expected-URL-count graduation guard.

## Compatibility

Legacy PR and research adapters run as equivalence shadows and do not replace their authorities.
Tests pin pass-versus-continue behavior and the exact `DonePRGreen` and `DoneAdvisory` serializations.

## Verification

```bash
fno test cli/tests/unit/delivery cli/tests/unit/test_plan_company_contract.py cli/tests/events/test_validator_parity.py cli/tests/events/test_schema_manifest.py cli/tests/test_plan_schema_drift.py
cargo test --manifest-path crates/fno-agents/Cargo.toml generic_completion
cargo test --manifest-path crates/fno-agents/Cargo.toml --test finalize_e2e
```

The repository has no root `Cargo.toml`, so root-level `cargo test -p fno-agents` is not valid.

## Failure Recovery

- `inactive` means the plan did not explicitly opt in.
- `undeterminable` means the plan or journal could not be read coherently or validated; fix the named diagnostic and rerun.
- A non-passing verdict names every failed, blocked, or unknown slot; repair the producer-owned fact rather than editing the derived verdict.
- A missing selected-verdict event leaves finalize partial and the plan ungraduated; rerun loop-check after the event log is writable.
- A journal change during evaluation is expected concurrency behavior; rerun after the producer write settles.
