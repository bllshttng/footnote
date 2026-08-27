# Cleanup pass (the apply-or-skip terminus)

Run AFTER a review whose findings are handled and the code works. This is not a review: it carries no gate weight, emits no attestation, opens no threads, and never blocks a ship. It is fno's own pass, so it runs inline on every harness. "Unavailable on this harness" is not an outcome this mode can produce.

## What runs

Four angles over the diff under cleanup, each ONCE:

- **Reuse** - a hand-rolled version of something this repo or its stdlib already provides.
- **Simplification** - code that can be shorter or plainer with no behavior change.
- **Efficiency** - avoidable repeated work on a path that runs more than once.
- **Altitude** - logic living at the wrong layer (a caller doing a callee's job, or the reverse).

Each angle ends in exactly one disposition:

- **APPLY** - make the change now, in this context, as an ordinary edit. Small by construction. An apply that outgrows the diff it looked at is a finding for the next review.
- **SKIP** - record the angle and the reason in one line: `cleanup: <angle> skipped - <reason>`. A skip with no reason is not a skip, it is an angle that never ran.

No verify pass, no confidence scoring, no re-review, no second round. When each of the four angles has one disposition on the record, the pass is done.

## Population

Cleanup candidates are exactly the classifier's non-blocking categories (style-adjacent cleanup, dead code, naming). A candidate that classifies BLOCKING (correctness, security) was already a review finding. This pass never downgrades one by applying it quietly.
