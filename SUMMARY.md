# SUMMARY - x-00c5 combined session survival + codex shared-daemon ownership

All eight waves landed and committed on this branch.

## Fixed on discovery

The wave-6 resume rewrite put pre_exec on codex's resume row. The mux reader refused pre_exec on the resume lane, so restore read codex as unresumable. The reader now carries pre_exec there. The fail-open render composes it like the attach renderer. Reader and builder tests pin the shape.

## Deviations from the bound plan

1. The plan's resume-form extraction from agents_view.rs was skipped. The file measures 4,993 lines, under the shrink-only gate. No shrink was owed.
2. The harness-map and restore test updates were not needed. No row semantics changed. The three-level visibility note landed in the docs instead.
3. Matrix row (e) uses the real fno-agents binary with a private home. The daemon refuses planted rows missing invariant fields. The real binary proves the same contract.
4. The journey script skips cleanly without two local codex builds or auth. AC24-REMOTE stays the operator's post-merge readback.
5. AC23-EDGE rides an in-module lock-busy unit test. The lock API is crate-private, and the shape needs only the lock.
6. The window values ride the receipt as before and after strings. Absent reads None, never an invented default. A stripped key reads failed with writer=unknown.
7. The budget overruns paid with two extractions. The argv-fact helpers moved to server/argv_facts.rs with their tests. squad_store's inline tests moved to squad_store_tests.rs.
8. Two full-suite failures remain. Both sit in files this branch never touches. They match the known local flake class, and CI arbitrates.
