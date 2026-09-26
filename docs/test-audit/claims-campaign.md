# Claims campaign ledger

Campaign scope: the claim-lock owner. The Python leg is `cli/src/fno/claims/` and its tests. The Rust leg is `crates/fno-agents/src/claims*.rs`, `claim_*.rs`, `reclaim.rs` and its tests. Out of scope: `cli/tests/unit/test_intake_claims.py` (intake-plan claims frontmatter) and `cli/tests/unit/test_pr_merge.py` (uses claim fixtures only).

## Baseline

Pinned SHA: `e159615a16cd947b9278c1e165b61955ec1ddec5` (origin/main at campaign start). Main advanced to `8e6c42375203` during the read-only pass and was reconciled at the end.

Pass/fail: every in-scope Python file passes. `fno doctor test` over all 19 in-scope files: 473 collected, 448 passed, 25 skipped, 0 failed, 90s.

Rust: `cargo test -p fno-agents claim` filter reads 300 tests (298 passed, 2 failed). Both failures are out of scope:

### Baseline failures (bug reports, not deleted)

| Test | File | Symptom |
|---|---|---|
| `cargo_build_dirs::tests::reclaim_tree_build_output_removes_target_and_build_dirs_with_bytes` | crates/fno-agents/src/cargo_build_dirs.rs:1652 | Asserts a reclaimed size of 4,140 bytes against a tree under /private/var/folders; matches the `claim` filter only through the `reclaim_` prefix. |
| `cargo_build_dirs::tests::reclaim_tree_build_output_keeps_a_locked_target` | crates/fno-agents/src/cargo_build_dirs.rs:1677 | Same family, same environment dependence. |

These are cargo_build_dirs tests, not claims tests. They fail on this host because the fixture computes expected bytes over a `/private/var` symlinked temp path. They are recorded for the subsystem owner. This campaign does not touch them.

### Declaration counts

Python: 446 declarations in 19 files. Largest: test_claim_reap.py 85, test_claims_cli.py 79, test_claims_core.py 77.

Rust: 169 `#[test]`/`#[tokio::test]` declarations in 11 files. The largest: claims.rs 71, claim_verbs.rs 20, claims_gate_tests.rs 15, reclaim.rs 14, claim_queue.rs 11, claims_identity_tests.rs 10, claims_reservation_tests.rs 10, claims_long_holds.rs 7, claims_release_stopped.rs 4, claims_root.rs 4, claim_store.rs 3.

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

Zero-coverage files are a flag, not a verdict: the owner suite is not necessarily their only exerciser. The cutover checks other suites before calling any of it dead.

## Lanes

By production owner, not file prefix:

1. Root routing: `claims/io.py` (`claims_root_for`), `claims_root.rs` / `test_claims_root_routing.py` (parity guard `test_claims_root_routing.py` stays while both legs live).
2. Lock core and store: `claims/core.py`, `types.py`, `incarnation.py`, `tasks.py` / `claims.rs`, `claim_store.rs`, `claim_queue.rs`.
3. Verbs: `claims/cli.py` / `claim_verbs.rs`.
4. Reap and release: `test_claim_reap.py`, `test_claim_closure_release.py`, `test_claim_force_release.py`, `test_claim_rebind.py`, `test_claim_ttl.py` / `reclaim.rs`, `claims_release_stopped.rs`.
5. Lanes and mutex: `claims/lanes.py` / `test_lane_slots.py`, `test_mutex_steal.py`, `test_claims_concurrency.py`.
6. Identity and opt-out: `claims/self_identity.py`, `claims/optout_lease.py`, `claims/hostid.py`, `claims/session_pid.py` / `claims_identity_tests.rs`.
7. Verdict, roster, silence, client: `claims/verdict.py`, `claims/roster.py`, `claims/events.py` / `test_claim_verdict.py`, `test_claim_status_worked.py`, `test_claims_status_roster.py`, `test_claims_silence.py`, `test_claims_client.py`, `claims_gate_tests.rs`, `claims_reservation_tests.rs`, `claims_long_holds.rs`.
8. Cross-implementation: `test_claims_cross_impl.py` (both legs write the same lockfiles, so it stays while both legs live).

## Ledger

Marks: R retain (contract + caught bug), F fix assertion, C consolidate (keeper named), D delete (proof named). One row per declaration. A class row carries the evidence line for its uniform-R members, and every F, C or D has its own row. Read pass completed on every declaration, including parameter tables.

### Lane 1: root routing

| Test | Mark | Evidence |
|---|---|---|
| `test_claims_root_routing.py` (3) | R | Parity guard: the Python prefix list and the Rust hand copy must stay equal while both legs write the same lockfiles; a one-sided prefix change routes the same key to two roots. |
| `claims_root.rs::tests` (4) | R | The Rust side of the same routing: colon-and-known-prefix partition, empty-env-is-unset, cwd-free resolution for global keys, and the resume-attach fallback matching Python's answer. |

### Lane 2: lock core and store

| Test | Mark | Evidence |
|---|---|---|
| `test_claims_core.py::TestAcquire` (16) | R | Acquire matrix: validation bounds, TTL/pid-unavailable schema v2, stale reclaim, the corroborated hybrid arm and its ambient-pid flip side (codex P1: a suspended session keeps its slot). |
| `test_claims_core.py::TestPidProvenanceStamping` (12) | R | Every writer earns its provenance stamp against the harness it stores; shared-host (codex app-server) never earns session-prover, refresh never re-poisons (the permanent-lease bug). |
| `test_claims_core.py::TestRelease` (7) | R | Release semantics incl. strict-holder mismatch and the under-mutex strict compare (the resurrection race). |
| `test_claims_core.py::TestRefresh` (9) | R | TTL extension, contention bound, verdict-gated renewal (expired-live extends, stale refuses byte-identical). |
| `test_claims_core.py::TestStatus` (10) | R | Status states plus basis (offhost vs pid-reuse), rootless node-key routing to the global root, unknown-not-free for unrouted keys. |
| `test_claims_core.py::TestList` (4) | R | Prefix filter, stale-excluded default with the dead-pid fixture discipline (the latent late-suite flake note). |
| `test_claims_core.py::TestForceRelease` (7) | R | Administrative override always succeeds, takes the recovery mutex, proceeds on timeout. |
| `test_claims_core.py::TestSessionIdStamping` (5) + `TestSessionWitnessVerdicts` (6) | R | The session id rides the record; the native witness heals a live session's verdict and bounds unknown; re-anchor moves a dead pid to the registry row pid. |
| `test_claims_io.py` (35) | R | Absent-not-null serialization discipline per field (expires_at, machine_id, session_id), corrupted/missing/newer-schema refusals, O_EXCL create races (two threads, one winner), archive naming, root resolution, the state-root denial refusal with the repo breadcrumb and its clear/keep rules. |
| `claims.rs::tests` (70 of 71; one D below) | R | Renewal family (fixed deadline, re-anchor, acquired_at hold, v2 refusal, span-growth P1, verdict-gated refusals), encode-key vectors, validation bounds, YAML parity, session witness family, liveness matrix with basis per cause, sweep buckets, pid exclusivity, zombie/reaped holders, machine-id stability, recovery-mutex wait/steal/grace, event-wire beside corpse locks, concurrent stealers, 8-thread acquire race. |
| `claim_store.rs::tests` (3) | R | Store import/export/release-stopped round trips over real lockfiles. |
| `claim_queue.rs::tests` (11) | R | Queue ordering property, corpse reaping, recycled-pid condemnation, foreign-machine skip, ticket monotonicity, hole non-reuse, bash-era phantom stamps, lane depth. |

Deletions in this lane:

| Test | Mark | Evidence |
|---|---|---|
| `claims.rs::classify_is_the_state_view_of_classify_with_basis` (1) | D | Production `classify` is the one-line delegate `classify_with_basis(...).0`; the test restates that delegation over six fixtures and can only fail if someone edits the delegation line. No contract lost: `classify_basis_names_each_cause` and `liveness_matches_python_classify_including_hybrid_arm` exercise the real classifier. |

### Lane 3: verbs

| Test | Mark | Evidence |
|---|---|---|
| `test_claims_cli.py` (72 of 79; marks below) | R | Exit-code contract per verb (1 held, 2 validation, 3 missing, 4 strict mismatch), contention-exhaustion not a traceback, the reconcile mutex, no-op release receipt honesty, do-row stamp/rollback windows (open row on kill, closed row protected, same-session rollback spared), roster crosscheck receipts (positive markers, degraded vs unconsulted, JSON parseability), global-root node-key resolution, import-stub capture regression. |
| `claim_verbs.rs::tests` (20) | R | Sweep payload shape/filter/corruption exclusion, handover witness subject switch (never answers from the minter; thread-worker by-name/alias join), primed batch wire (one interpreter, every subject, failed batch never hangs), served-liveness tier, handover/gate payload session splits with control twins. |

Deletions and consolidations in this lane:

| Test | Mark | Evidence |
|---|---|---|
| `test_claims_cli.py::test_help_lists_all_verbs` | D | Restates the declared CLI surface (junk pattern: capability test restating flags). Each verb is exercised by its own behavioral tests, which fail if the verb disappears. |
| `test_claims_cli.py::test_ttl_parser_seconds_no_unit/seconds/minutes/hours/empty/invalid` (6) | C | Six standalone cases over one pure function become one table-driven case; the conversion values are pinned per row. Keeper: the single parametrized `test_ttl_parser_table` this campaign writes in place. The CLI boundary stays covered by `test_acquire_invalid_ttl_format` and `test_acquire_with_ttl`. |

### Lane 4: reap and release

| Test | Mark | Evidence |
|---|---|---|
| `test_claim_reap.py` (81 of 85; one D below) | R | The native verdict door (dead/off-host/TTL-suspect/live), the expiry-travels arm for unidentifiable rows with its boundary twins, the load-bearing kill-without-release reap, both-roots sweep and dedup, off-host and suspect keeps, failure-reported-not-raised, dry-run/journal contracts, the abandonment probe (unknown keeps; roster joins with the row-absent guard; transcript fallback), shared-pid exclusivity (the 7-claim immortality specimen), mux-pane absence parsing, the walked-dir verdict fix, and the two skipped known-defect mirrors kept as the defect's record. |
| `test_claim_closure_release.py` (22) | R | Node closure releases the claim and clears the mirror on done/supersede only, configured-graph-scoped, broken store never fails the mutation; terminal-node settlement (a live holder on a closed node survives; a dead one settles); default-sweep mirror clear with explicit-root and dry-run negatives. |
| `test_claim_force_release.py` (4) | R | Force release names the path it read and the other root's stray file; byte-identity of the untouched file. |
| `test_claim_rebind.py` (16) | R | Full compare_and_rebind matrix: rebound/idempotent/refused states, mutex wait-not-refuse, handover security gate (a live published holder cannot be taken; spawn-handover can), metadata provenance rules. |
| `reclaim.rs::tests` (14) | R | Leak predicates per lane, apply receipts with byte counts, codex quarantine keep-rules (unreadable marker, converge lock, rollback failure, live symlink, near-miss names, foreign marketplace), daemon path excludes cwd roots. |
| `claims_release_stopped.rs::tests` (4) | R | Stopped-session release matrix: own handover released, live-pid kept with named reading, other-session and other-worker handovers untouched, zero-scan receipts. |
| `claims_long_holds.rs::tests` (7) | R | Row shape/threshold/order, witness healing with pid-disagreement naming, probe rendering arms, off-host never probes a foreign pid, verb CLI contract. |

Deletion in this lane:

| Test | Mark | Evidence |
|---|---|---|
| `test_claim_reap.py::TestClassifyForSweepMatchesIsProvablyDead` (4) | D | Compares two test-local wrappers (`is_provably_dead`, `classify_for_sweep`) that call the identical Rust door; the equality is a self-comparison. The real verdicts and buckets those wrappers return are asserted independently by `TestIsProvablyDead` and `TestExpiredTTLIsHostIndependent`. |

### Lane 5: lanes and mutex

| Test | Mark | Evidence |
|---|---|---|
| `test_lane_slots.py` (18) | R | Cap primitive: acquisition, cap refusal, sequential degradation, slot ownership on re-dispatch (no cap inflation), metadata authority, TTL coercion, CLI flow. |
| `test_mutex_steal.py` (20) | R | Steal predicate boundaries on a frozen clock (`<=` held), corpse theft, dangling symlinks (lstat), owner-token swap detection, the restore grace window, the cross-language threshold parity test (wire protocol), recovery-mutex corpses, and the poll-not-spin contract. |
| `test_claims_concurrency.py` (7) | R | Multi-process O_EXCL races (2 and 5 racers, one winner per trial), stale-recovery race, worktree-to-space root resolution with a real git worktree, holder-flip seam. |

### Lane 6: identity and opt-out

| Test | Mark | Evidence |
|---|---|---|
| `test_claims_session_pid.py` (11) | R | The Python shim contract: one exec per from_pid, cached, both halves from one read, five degrade modes, type-checked halves, the measured codex deny list. The Rust ancestor-walk semantics are pinned by spawn_context's own tests (named in the file docstring). |
| `claims_identity_tests.rs` (10) | R | Harness/session tag serialization (absent-not-null), identity resolution precedence, disagreement refusals (never launder the first-sorted marker), child-stamp scrubbing. |

### Lane 7: verdict, roster, silence, client

| Test | Mark | Evidence |
|---|---|---|
| `test_claim_status_worked.py` (8) | R | Stop-stamp vs fresh-tail liveness, live-worker join with degraded-coverage hedges, registry-only probe attribution, closed-phase filter. |
| `test_claims_status_roster.py` (12 of 13; one D below) | R | The refused ratio arm's regression fixture (64 of 129), unresolved-row fail-closed, transcript-dating arms, dead-pid falsifier, the closed-session receipt, stale-keeper fallback, and the closed-worker-over-unresolved ordering. |
| `test_claims_silence.py` (9) | R | The silence classifier over injected rows: positive scanned marker at zero, unreadable vs silent buckets, codex-transcript resolution. |
| `test_claim_verdict.py` (5 of 6; one D in lane 3) | R | Missing-binary refusal with remedy, one-batch native subprocess, verdict-delegation fail-closed on door omission (acquire refuses; status never reports free). |
| `test_claims_client.py` (2) | R | The Python-to-Rust flag wire: exact argv the native door receives. |
| `claims_gate_tests.rs` (15) | R | The short-lived holder arm: gate keys read the pid at any age, holder-process leases at expiry, spawning-session witness never heals a dead gate, refused-probe suspect, off-host clock path, provenance stamping. |

Deletion in this lane:

| Test | Mark | Evidence |
|---|---|---|
| `test_claims_status_roster.py::test_roster_reader_module_is_authority` | D | Identity check that the module re-imports roster.py's symbols; every verdict test already drives `read_roster` through the CLI, so a shadowed copy is exercised wherever it lives. Preserves a refactoring style, not behavior. |

Deletion in this lane:

| Test | Mark | Evidence |
|---|---|---|
| `test_claim_verdict.py::test_claim_clock_lives_with_claim_types` | D | Asserts `now_ms()` is non-decreasing: a tautology of any clock, no contract. The clock is exercised by every other test in the suite. |

### Lane 8: cross-implementation

| Test | Mark | Evidence |
|---|---|---|
| `test_claims_cross_impl.py` (18) | R (all) | Both legs write the same lockfiles; each case drives one leg and asserts on the other's read (status field parity both directions, cross release, stale reclaim + archive + audit events, hybrid-arm parity, byte-identical filename encoding, Python-vs-Rust race one winner per round, recovery-mutex wait/steal interop both directions, expires_at absence discipline, corrupted-file parity). This is the only proof the two implementations interoperate; retired only when one leg dies. |

### Retention bar

No D or C row above deletes a contract's only proof. Each names a stronger keeper that already existed, or a contract that never existed (the four identity/tautology rows). The two skipped known-defect mirrors in test_claim_reap.py stay R. They are the defect's written record, and the fix that clears the defect flips them on.

## Layer plan

One contract, one primary owner, already true lane by lane. The campaign found no redundant layer, only wrapper restatements and same-file duplicates. Cutover is therefore exactly the ledger's F/C/D rows, applied file by file:

1. D rows: delete the four declaration groups and the Rust delegation test.
2. C rows: collapse the six `_parse_ttl` cases into one parametrized table test. Delete the three test_claim_ttl.py rows whose keepers live in test_claims_core.py (`test_AC3_HP_refresh_extends_expires_at`, `test_AC3_FR_refresh_pid_liveness_returns_none`, `test_AC3_ERR_refresh_wrong_holder_raises`).
3. F row: `test_AC2_FR_release_emits_duration` gains the assertion it never had (capture the emitted event, require `duration_held_ms` present and non-negative).
4. No production seam is unlocked: no deleted test owned a test-only export, and `cli/src/fno/claims` takes no edits.

## Preservation

Review pass: every D and C row was compared against its named keeper lane by lane before the edit. No contract lost its only proof. Two mutations prove the strengthened and consolidated keepers can fail:

| Mutation | Keeper that went red |
|---|---|
| `claims/events.py` `emit_claim_released` drops the `duration_held_ms` field | `test_claims_core.py::TestRelease::test_AC2_FR_release_emits_duration` fails ("missing required data field: duration_held_ms"); three sibling release tests fail on the same typed-schema enforcement. Restored byte-identical (`git diff` empty). |
| `claims/cli.py` `_parse_ttl` minutes arm returns seconds (`n * 1000`) | `test_claims_cli.py::test_ttl_parser_table[5m-300000]` fails (`assert 5000 == 300000`); three CLI tests using minute TTLs fail with it. Restored byte-identical (`git diff` empty). |

The one Rust D (the `classify` delegation restatement) needed no mutation. It names no contract of its own. The real classifier arms stay covered by `classify_basis_names_each_cause` and `liveness_matches_python_classify_including_hybrid_arm`, both green in the post-cutover run.

Coverage containment (`--cov=fno.claims`, owner suite, before vs after the cutover): the executed-line sets are IDENTICAL. 1,978 / 2,732 lines covered at baseline and after, zero lines lost, zero gained. No production line lost its coverage, and no dead-code deletion was unlocked.

Post-cutover runs: the six edited Python files pass (261 passed, 5 skipped). The Rust `claim` filter reads 297 passed, with the same two pre-existing out-of-scope `cargo_build_dirs` failures as baseline. The delta is exactly the one deleted delegation test.

## Reconcile

origin/main had moved 195 commits ahead by reconcile time (2026-09-26). The branch merged it (never rebased), and the merge touched none of the edited files. No new claims regression needed porting. The owner suites rerun green on the merged head: Python 438 passed, 25 skipped, 463 collected, exactly baseline 473 minus the 10 removed collections. The Rust `claim` filter reads 295 passed with the same two pre-existing out-of-scope `cargo_build_dirs` failures, plus two new claim-named tests main added in other files.

Declaration totals: 446 Python + 169 Rust = 615 before. 431 + 168 = 599 after. 16 were removed (8 D, 8 C absorbed). The parser table keeps its six cases as rows of one test. The `cli/src/fno/claims` production tree is untouched, so the campaign's production-line change is zero.

CI minutes: filled from the smoke-duration lines of this PR's run against the last main run, in the running-total row (README.md).
