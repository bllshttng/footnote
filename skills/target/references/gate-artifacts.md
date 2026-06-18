# Gate Artifacts (SUPERSEDED)

This document described the pre-wedge gate-attestation / phase-verifier machinery
that the control-plane collapse removed (ab-d0337fbc wedge + ab-f8e5f214 step 6).
There are no gate artifacts, no phase verifiers, and no promise-time self-grade.
Completion authority is exactly three external reads (PR + CI + reviews) plus a
budget ceiling, decided by `fno-agents loop-check`.

See `docs/architecture/control-plane-loop.md` for the current model.
