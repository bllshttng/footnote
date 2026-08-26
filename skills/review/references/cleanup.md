# Cleanup pass (the apply-or-skip terminus)

Run AFTER a review whose findings are handled and the code works. This is not a review: it carries no gate weight, emits no attestation, opens no threads, and never blocks a ship. It is fno's own pass, so it runs inline on every harness; "unavailable on this harness" is not an outcome this mode can produce.

## What runs

Four angles over the diff under cleanup, each ONCE:

- **Reuse** - a hand-rolled version of something this repo or its stdlib already provides.
- **Simplification** - code that could be shorter or plainer with no behavior change.
- **Efficiency** - avoidable repeated work on a path that runs more than once.
- **Altitude** - logic living at the wrong layer (a caller doing a callee's job, or the reverse).

Each angle ends in exactly one disposition:

- **APPLY** - make the change now, in this context, as an ordinary edit. Small by construction; if an "apply" wants to grow past the diff it looked at, it is a finding for the next review, not this pass.
- **SKIP** - record the angle and the reason in one line: `cleanup: <angle> skipped - <reason>`. A skip with no reason is not a skip, it is an angle that never ran.

No verify pass, no confidence scoring, no re-review, no second round. The pass is done when each of the four angles has one disposition on the record.

## Population

Cleanup candidates are exactly the classifier's non-blocking categories (style-adjacent cleanup, dead code, naming). A candidate that would classify BLOCKING (correctness, security) was already a review finding; this pass never downgrades one by applying it quietly.
