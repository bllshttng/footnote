# Plan fidelity gate and the scope denominator

Operator finding 2026-08-11: "it keeps cutting out scope without me knowing until i check later."

Two holes caused it, and the second made the first unfixable. This doc is the contract for both fixes. It also covers the carveout-severity and ratio-measurement work that shipped with them.

## The denominator: derived from the node's own details

Detection of multi-deliverable scope from node prose is impossible with high recall. Three real specimens prove it. One enumerates with an ordinal run. One uses a cardinal governing a plural. The one that actually failed carries a coordinated noun phrase with zero numerals.

A regex tight enough to skip that node's measurement digits misses its ask. A regex loose enough to catch the ask fires on every measurement bullet. Keying any behavior on full detection is keying it on a coin flip.

The gate era is over. `fno do target init` on a plan-less code node now states its own scope: it derives `deliverables: N` from the node's own details and proceeds, never refusing. The derivation reads only the unambiguous structures: the highest ordinal marker, a two-member construction, or else 1. The count is falsifiable, so a reader can recount the node's enumeration. An explicit `--deliverables N` still wins. `shipped M of N` stays expressible without a blueprint, which is what the refusal existed to force.

### The enumerated_scope predicate

`enumerated_scope` lives in `cli/src/fno/target/denominator.py`. It is a narrow, high-precision predicate over a node's title and details. It fires on an ordinal run like `(1)…(2)` or a numbered list. It fires on a cardinal 2-10 governing a plural noun within three tokens. It fires on `both X and Y`. `derive_deliverables` in the same module turns the same reads into the count init stamps.

It gates nothing on its own. It feeds the `target_denominator` event's `enumerated` flag and the derivation.

A non-fire asserts nothing. The pinned miss does not fire, and that is correct. That failed node derives a count of 1, and the init echo names the `--deliverables` override.

## The fidelity gate: one join, two dispositions

`build_plan_fidelity` lives in `cli/src/fno/scoreboard/fold.py`. It joins each planned ledger row to its delivery. For telemetry, an unjoined planned row is `unmeasurable`. It reports as `status: unjoined`, never as 0%. That is exactly wrong for a gate.

The inversion is one parameter on the same join, not a second implementation:

```python
build_plan_fidelity(..., unmeasurable="unjoined")  # telemetry: never punish the unmeasurable
build_plan_fidelity(..., unmeasurable="refuse")    # gate: an unjoined row is a refusal
```

`unmeasurable="unjoined"` is the default. It is byte-identical to the prior scoreboard output. `unmeasurable="refuse"` adds a top-level `gate` key. That key marks the unjoined rows as a would-be refusal. The join itself never changes. The scoreboard and the gate cannot disagree about the same PR.

The carveout waiver lives in `cli/src/fno/plan/fidelity.py`. The fold does not do it. Each unjoined planned row needs a covering carveout or the gate refuses. A PR-body sentence is not a carveout. `fno do plan fidelity --json <plan>` is the single entry point.

### Both readers are required

A guard on one of N reachable paths is decorative. The two readers enforce independently:

- **Stop gate** (`crates/fno-agents/src/loopcheck.rs`): `evaluate_plan_fidelity` shells `fno do plan fidelity --json`. It mirrors `evaluate_done_probes`. It blocks `DonePRGreen` until each shortfall carries a carveout. It fails open on a missing or stale `fno` so a stale install cannot wedge every run. The merge gate is the backstop. `fno doctor` flags the staleness.
- **Merge gate** (`cli/src/fno/pr/_merge.py`): it imports `compute_plan_fidelity` in-process. It refuses the merge on an uncovered shortfall. It fails open on a probe crash so a broken gate cannot wedge a green merge.

A stop-gate-only check is skipped by a direct `fno do pr merge`. A merge-gate-only check is skipped by every autonomous loop that terminates without merging. The join is plan-grain. An inline run with no separate planning thread has zero planned rows and passes. The gate catches an orphan plan that never shipped.

### A plan that forbids carve-outs

The waiver above asks whether a carve-out is tracked. It never asks whether the plan allowed one. A plan answers that with one frontmatter key, `carveouts`, set to `allowed` or `forbidden`. Absent means allowed, and the decision is unchanged.

On `forbidden` the decision refuses in two ways. A carveout filed by the plan's sessions no longer covers a shortfall. It refuses. The PR body is read over REST, and `scripts/ci/check-oos-tracked.sh` runs on it with `PLAN_CARVEOUTS=forbidden`. That script is the one body parser. In this mode any exclusion item refuses, tracked or waived, and the reason is its first stderr line.

Every read fails closed. An unreadable body, a missing script, or a timeout refuses. A node with no PR yet skips the body read, because the stop gate runs only after a PR is open.

The specimen is PR 1599. Its plan said one PR and no new nodes. It shipped five tracked exclusions under `## Explicitly not in this PR`, a heading the script did not match until this change.

## Carveout severity, stamped by provenance

`fno backlog carveout add` gained a `severity` field, one of `critical`, `high`, `medium`, or `low`. It is plumbed to `RawItem.severity`. The existing `severity_to_priority` in `retro/classify.py` maps it to `p0..p3`. The routing already existed. Only the field was missing.

A carveout created to satisfy the fidelity gate is, by construction, a planned deliverable that went unbuilt. The gate stamps that severity from provenance. The filer cannot pick it and cannot downgrade it. `--severity` stays available for hand-filed carveouts. It defaults to today's `p3` behavior.

## The deliverables-1 ratio measurement

The cheap exit `--deliverables 1` is load-bearing risk. If it becomes reflexive, the gate degrades to a formality. The ratio of `deliverables: 1` inits to plan-backed inits is measured off the ledger. Read it with `fno do target denominator-ratio`. If it climbs past roughly 80 percent, the exit is a bypass and `enumerated_scope` needs widening.

A stamped 1 still beats today. A stamped 1 is falsifiable. An absent denominator is not.
