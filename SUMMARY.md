# SUMMARY - x-0c29: the configured rounds are the review gate

MERGED. PR #1512 closed 2026-09-06T13:07:34Z at e94756594 with CI 33/33 green and review rounds 2-of-2 (round 1 pass at 1fbf6cec, round 2 pass at 4d5d07f30, carried across the two one-line CI-fix commits under the interdiff budget). Five task commits plus ten CI-fix commits on origin/main.

## What shipped

- The Python and Rust coverage gates discharge the review obligation at `config.review.max_rounds`: COVERED, no IMPOSSIBLE state, no exit 5, no filing capped findings as nodes. Origin never gates (`require_corroboration` deprecated no-op; a declined-with-reason disposition is terminal whoever attested).
- `fno do review classify --attest <reviewer>` writes the head-pinned attestation row itself, verdict measured from the classified findings (`prose_unparseable` always fails). `emit-attestation.sh` delegates its findings path to the verb; `consume-peer-verdict.sh` calls the verb directly.
- `reviewed_count` counts every counted verdict whatever it concluded; `passed_count` (new) rides every row; every covered-proxy reader keys on the word.
- `scripts/ops/retire-round-cap-findings.sh` supersedes the vented node population (dry run default; run once from the post-merge ritual).

## Deviations and environment notes

- `docs/configuration-guide.md` is generated from `config/registry.py`; hand edits from Task 2.3 were replaced by regeneration in the CI-fix commit, and the `github_approval_satisfies` registry docstring dropped the retired "corroboration term" phrase.
- The graph store lane on this machine is wedged (a 16-hour-old keeper listens but never answers; `backlog status --snapshot` times out and killing the listener caused a keeper spawn storm). `fno do pr bind-created` and `closure-trailer` could not run; the Backlog-Closure trailer was rendered through `fno.pr.closure.render_closure_trailer` (the same producer the verb calls) and the node binding is left to the finalize backstop.
- `test_pr_merge_grant` failed 4 cases locally until `fno-agents-worker` was built in the worktree: the store keeper lookup falls through to a stubbed `shutil.which` when no dev artifact exists. CI has the artifact; building it in a fresh worktree is the local remedy.
- Branch rebased twice onto origin/main (main merged PRs 1508/1509 mid-flight); one comment-only conflict in `events/__init__.py` resolved to this branch's wording.

## Verification

- `fno doctor test` gate suites: 231 passed (coverage_check, coverage_gate, pr_status, pr_attestation).
- cargo: loopcheck 438, disposition_gate 5, coverage_receipt 9, coverage_tiling 53.
- bash: test_emit_attestation.sh 67/0, test_code_review_attest.sh 59/0, test_attest_model.sh 35/0.
- Gates: check-file-budget, check-no-internal-refs, check-reachable-paths, state-roots ratchet, `fno doctor lint style --surface markdown`, mypy (546 files clean).
- Zero-sweep: no IMPOSSIBLE/blockers_impossible/file_findings_at_cap/corroboration_term/rests_on_self_attestation_alone in cli/src, crates/fno-agents/src, hooks.
- Review round 1 attested pass (0 blocking, 2 nonblocking test-coverage findings) at 1fbf6cec through the new `classify --attest` verb.
- Review round 2 attested pass at 4d5d07f30 (0 blocking, the same 2 nonblocking findings carried and re-validated): the cap is spent at 2-of-2 and the PR merges on green CI.
