---
name: do
description: "Renamed to execute. Preserves the old spelling for one release."
argument-hint: "[flat|waves|operator] <plan-path>"
requires:
  binaries:
    - "fno >= 0.1"
---

# Do

This skill is now `/fno:execute`. Nothing else changed: the routes (`flat`, `waves`, `operator`) and the plan-path argument are the same.

Print once that `/fno:do` is the old spelling, `/fno:execute` is the new spelling, and the old spelling is removed next release. Then stop. Do not invoke another skill or execute the plan from this compatibility shim.

## Known Limitations and Deferred Work

- The compatibility spelling does not execute plans. See [LIMITATIONS.md](LIMITATIONS.md).
