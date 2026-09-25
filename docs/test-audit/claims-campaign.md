# Claims campaign ledger

Campaign scope: the claim-lock owner. Python leg `cli/src/fno/claims/` and its tests; Rust leg `crates/fno-agents/src/claims*.rs`, `claim_*.rs`, `reclaim.rs` and its tests. Out of scope: `cli/tests/unit/test_intake_claims.py` (intake-plan claims frontmatter) and `cli/tests/unit/test_pr_merge.py` (uses claim fixtures only).

## Baseline

Pinned SHA: `e159615a16cd947b9278c1e165b61955ec1ddec5` (origin/main at campaign start; main advanced to `8e6c42375203` during the read-only pass; reconciled at the end).

Pass/fail: every in-scope Python file passes. `fno doctor test` over all 19 in-scope files: 473 collected, 448 passed, 25 skipped, 0 failed, 90s.

Rust: `cargo test -p fno-agents claim` filter reads 300 tests (298 passed, 2 failed). Both failures are out of scope:

### Baseline failures (bug reports, not deleted)

| Test | File | Symptom |
|---|---|---|
| `cargo_build_dirs::tests::reclaim_tree_build_output_removes_target_and_build_dirs_with_bytes` | crates/fno-agents/src/cargo_build_dirs.rs:1652 | Asserts a reclaimed size of 4,140 bytes against a tree under /private/var/folders; matches the `claim` filter only through the `reclaim_` prefix. |
| `cargo_build_dirs::tests::reclaim_tree_build_output_keeps_a_locked_target` | crates/fno-agents/src/cargo_build_dirs.rs:1677 | Same family, same environment dependence. |

These are cargo_build_dirs tests, not claims tests. They fail on this host because the fixture computes expected bytes over a `/private/var` symlinked temp path. Recorded for the subsystem owner; this campaign does not touch them.

### Declaration counts

Python: 446 declarations in 19 files. Largest: test_claim_reap.py 85, test_claims_cli.py 79, test_claims_core.py 77.

Rust: 169 `#[test]`/`#[tokio::test]` declarations in 11 files: claims.rs 71, claim_verbs.rs 20, reclaim.rs 14, claims_gate_tests.rs 15, claim_queue.rs 11, claims_identity_tests.rs 10, claims_reservation_tests.rs 10, claims_long_holds.rs 7, claim_store.rs 3, claims_release_stopped.rs 4, claims_root.rs 4.

### Baseline coverage (--cov=fno.claims over the 19-file owner suite)

Totals: 1,978 / 2,732 lines, 72.4%.

| File | Covered/Statements |
|---|---|
| claims/core.py | 692/860 |
| claims/cli.py | 640/808 |
| claims/io.py | 155/177 |
| claims/roster.py | 92/96 |
| claims/types.py | 81/91 |
| claims/events.py | 75/83 |
| claims/lanes.py | 59/60 |
| claims/verdict.py | 47/89 |
| claims/optout_lease.py | 43/173 |
| claims/hostid.py | 33/58 |
| claims/session_pid.py | 30/30 |
| claims/self_identity.py | 26/121 |
| claims/__init__.py | 5/5 |
| claims/incarnation.py | 0/68 |
| claims/tasks.py | 0/13 |

Zero-coverage files are a flag, not a verdict: the owner suite may not be their only exerciser. The cutover checks other suites before calling any of it dead.

## Lanes

By production owner, not file prefix:

1. Root routing: `claims/io.py` (`claims_root_for`), `claims_root.rs` / `test_claims_root_routing.py` (parity guard `test_claims_root_routing.py` stays while both legs live).
2. Lock core and store: `claims/core.py`, `types.py`, `incarnation.py`, `tasks.py` / `claims.rs`, `claim_store.rs`, `claim_queue.rs`.
3. Verbs: `claims/cli.py` / `claim_verbs.rs`.
4. Reap and release: `test_claim_reap.py`, `test_claim_closure_release.py`, `test_claim_force_release.py`, `test_claim_rebind.py`, `test_claim_ttl.py` / `reclaim.rs`, `claims_release_stopped.rs`.
5. Lanes and mutex: `claims/lanes.py` / `test_lane_slots.py`, `test_mutex_steal.py`, `test_claims_concurrency.py`.
6. Identity and opt-out: `claims/self_identity.py`, `claims/optout_lease.py`, `claims/hostid.py`, `claims/session_pid.py` / `claims_identity_tests.rs`.
7. Verdict, roster, silence, client: `claims/verdict.py`, `claims/roster.py`, `claims/events.py` / `test_claim_verdict.py`, `test_claim_status_worked.py`, `test_claims_status_roster.py`, `test_claims_silence.py`, `test_claims_client.py`, `claims_gate_tests.rs`, `claims_reservation_tests.rs`, `claims_long_holds.rs`.
8. Cross-implementation: `test_claims_cross_impl.py` (both legs write the same lockfiles; stays while both legs live).

## Ledger

Marks: R retain (contract + caught bug), F fix assertion, C consolidate (keeper named), D delete (proof named). One row per declaration; parameterized rows marked individually when they differ.

### Lane 1: root routing

| Test | Mark | Evidence |
|---|---|---|
| `test_claims_root_routing.py` (3) | R | Parity guard: Python `claims_root_for` and Rust `claims_root_for` must agree on prefix routing while both legs write the same lockfiles. Fails on a one-sided prefix change. |

### Lane 8: cross-implementation

| Test | Mark | Evidence |
|---|---|---|
| `test_claims_cross_impl.py` (18) | R (all) | Both legs write the same lockfile format; each case drives one leg and asserts on the other's read. This is the only proof the two implementations interoperate. Retired only when one leg dies. |

(Lanes 2 to 7: filled during the read-only pass below.)

## Preservation

(TBD at cutover: per-contract mutations and the coverage-containment result.)

## Reconcile

(TBD: merge of main, final counts, CI minutes.)
