# Plan fidelity gate and the scope denominator

Operator finding 2026-08-11: "it keeps cutting out scope without me knowing until i check later."

Two holes caused it, and the second made the first unfixable. This doc is the contract for both fixes. It also covers the carveout-severity and ratio-measurement work that shipped with them.

## The denominator: count is a declaration, never a detection

Detection of multi-deliverable scope from node prose is impossible. Three real specimens prove it. One enumerates with an ordinal run. One uses a cardinal governing a plural. The one that actually failed carries a coordinated noun phrase with zero numerals.

A regex tight enough to skip that node's measurement digits misses its ask. A regex loose enough to catch the ask fires on every measurement bullet. Keying a refusal on detection is keying it on a coin flip.

So the gate never asks how many deliverables a node has. It asks whether a denominator exists at all:

```
denominator_absent := payload_is_code AND plan_path == "" AND deliverables is None
```

Three structured field reads, zero prose. `payload_is_code` is `domain == "code"` at init. The merge gate uses the real PR diff. `fno do target init` refuses a code node with no plan and no declared count. It names both exits that create a denominator:

1. `/fno:blueprint quick "..."` writes a plan enumerating the tasks. `plan_path` fills. The denominator is the task count.
2. `fno do target init --deliverables N` stamps `deliverables: N` into the immutable manifest.

Exit 2 is deliberately cheap. A run that stamps 1 and ships one of four leaves a falsifiable claim on the record. That is exactly what that failed node lacked. A missing band and a never-planned band stop being indistinguishable.

### The enumerated_scope ratchet

`enumerated_scope` lives in `cli/src/fno/target/denominator.py`. It is a narrow, high-precision predicate over a node's title and details. It fires on an ordinal run like `(1)…(2)` or a numbered list. It fires on a cardinal 2-10 governing a plural noun within three tokens. It fires on `both X and Y`.

It gates nothing on its own. Its only power is to withdraw exit 2 for an unambiguously enumerated node. That forces a real plan instead of a declared count.

A non-fire asserts nothing. The pinned miss does not fire, and that is correct. That failed node's coordinated-noun-phrase ask is protected by `denominator_absent`, not by this ratchet.

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

## Carveout severity, stamped by provenance

`fno backlog carveout add` gained a `severity` field, one of `critical`, `high`, `medium`, or `low`. It is plumbed to `RawItem.severity`. The existing `severity_to_priority` in `retro/classify.py` maps it to `p0..p3`. The routing already existed. Only the field was missing.

A carveout created to satisfy the fidelity gate is, by construction, a planned deliverable that went unbuilt. The gate stamps that severity from provenance. The filer cannot pick it and cannot downgrade it. `--severity` stays available for hand-filed carveouts. It defaults to today's `p3` behavior.

## The deliverables-1 ratio measurement

The cheap exit `--deliverables 1` is load-bearing risk. If it becomes reflexive, the gate degrades to a formality. The ratio of `deliverables: 1` inits to plan-backed inits is measured off the ledger. Read it with `fno do target denominator-ratio`. If it climbs past roughly 80 percent, the exit is a bypass and `enumerated_scope` needs widening.

A stamped 1 still beats today. A stamped 1 is falsifiable. An absent denominator is not.
