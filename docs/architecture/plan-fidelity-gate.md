# Plan fidelity gate and the scope denominator

Operator finding 2026-08-11: "it keeps cutting out scope without me knowing until i check later."

Two holes, and the second made the first unfixable. This doc is the contract for both fixes plus the carveout-severity and ratio-measurement work that shipped with them.

## The denominator: count is a declaration, never a detection

Detection of multi-deliverable scope from node prose is impossible. Proven on three real specimens: one node enumerates with an ordinal run, one with a cardinal governing a plural, and the one that actually failed with a coordinated noun phrase carrying zero numerals. A regex tight enough to skip that node's measurement digits misses its ask; a regex loose enough to catch the ask fires on every measurement bullet. Keying a refusal on detection is keying it on a coin flip.

So the gate never asks *how many* deliverables a node has. It asks whether a denominator exists at all:

```
denominator_absent := payload_is_code AND plan_path == "" AND deliverables is None
```

Three structured field reads, zero prose. `payload_is_code` is `domain == "code"` at init (the merge gate uses the real PR diff). `fno target init` refuses a code node with no plan and no declared count, naming both exits that create a denominator:

1. `/fno:blueprint quick "..."` writes a plan enumerating the tasks. `plan_path` fills. The denominator is the task count.
2. `fno target init --deliverables N` stamps `deliverables: N` into the immutable manifest.

Exit 2 is deliberately cheap. A run that stamps 1 and ships one of four leaves a falsifiable claim on the record, which is exactly what that failed node lacked. A missing band and a never-planned band stop being indistinguishable.

### The enumerated_scope ratchet

`enumerated_scope` (`cli/src/fno/target/denominator.py`) is a narrow, high-precision predicate over a node's title and details. It fires on an ordinal run (`(1)…(2)` or a numbered list), a cardinal 2-10 governing a plural noun within three tokens, or `both X and Y`. It gates nothing on its own. Its only power is to **withdraw exit 2** for an unambiguously enumerated node, forcing a real plan instead of a declared count.

A non-fire asserts nothing. The pinned miss (that failed node's coordinated-noun-phrase ask) does not fire, and that is correct: `denominator_absent` is what protects that node, not this ratchet.

## The fidelity gate: one join, two dispositions

`build_plan_fidelity` (`cli/src/fno/scoreboard/fold.py`) joins each planned ledger row to its delivery. For telemetry, an unjoined planned row is `unmeasurable` and reported as `status: unjoined`, never 0%. That is exactly wrong for a gate. The inversion is **one parameter on the same join**, not a second implementation:

```python
build_plan_fidelity(..., unmeasurable="unjoined")  # telemetry: never punish the unmeasurable
build_plan_fidelity(..., unmeasurable="refuse")    # gate: an unjoined row is a refusal
```

`unmeasurable="unjoined"` (the default) is byte-identical to the prior scoreboard output. `unmeasurable="refuse"` adds a top-level `gate` key marking the unjoined rows as a would-be refusal. The join itself never changes, so the scoreboard and the gate cannot disagree about the same PR.

The carveout waiver (the one thing the fold does not do) lives in `cli/src/fno/plan/fidelity.py`: each unjoined planned row needs a covering carveout or the gate refuses. A PR-body sentence is not a carveout. `fno plan fidelity --json <plan>` is the single entry point.

### Both readers are required

A guard on one of N reachable paths is decorative. The two readers enforce independently:

- **Stop gate** (`crates/fno-agents/src/loopcheck.rs`): `evaluate_plan_fidelity` shells `fno plan fidelity --json`, mirrors `evaluate_done_probes`, and blocks `DonePRGreen` until each shortfall carries a carveout. Fail-open on a missing or stale `fno` (no `plan fidelity` verb) so a stale install cannot wedge every run; the merge gate is the backstop, and `fno doctor` flags the staleness.
- **Merge gate** (`cli/src/fno/pr/_merge.py`): imports `compute_plan_fidelity` in-process and refuses the merge on an uncovered shortfall. Fail-open on a probe crash so a broken gate cannot wedge a green merge.

A stop-gate-only check is skipped by a direct `fno pr merge`; a merge-gate-only check is skipped by every autonomous loop that terminates without merging. The join is plan-grain, so an inline run with no separate planning thread has zero planned rows and passes; the gate catches an orphan plan that never shipped.

## Carveout severity, stamped by provenance

`fno carveout add` gained a `severity` field (`critical|high|medium|low`), plumbed to `RawItem.severity` and mapped by the existing `severity_to_priority` in `retro/classify.py` (`p0..p3`). The routing already existed; only the field was missing. A carveout created to satisfy the fidelity gate is, by construction, "a planned deliverable is unbuilt," and the gate stamps that severity from provenance. The filer cannot pick it and cannot downgrade it. `--severity` stays available for hand-filed carveouts and defaults to today's `p3` behavior.

## The deliverables-1 ratio measurement

The cheap exit (`--deliverables 1`) is load-bearing risk: if it becomes reflexive, the gate degrades to a formality. The ratio of `deliverables: 1` inits to plan-backed inits is measured off the ledger (`fno backlog deliverables-ratio`); if it climbs past roughly 80 percent, the exit is being used as a bypass and `enumerated_scope` needs widening. A stamped 1 still beats today, because a stamped 1 is falsifiable and an absent denominator is not.
