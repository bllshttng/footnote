# PR status verdict: the narrative behind `fno do pr status`

The docstrings in `cli/src/fno/pr/_status.py` carry one-line contracts and point here. This file holds the full reasoning those docstrings used to restate, moved under the file-budget gate's named remedy ("long prose belongs in docs/") so the tree number and the reasoning both survive.

## `verdict_for`: pure verdict computation

Returns `(verdict, exit_code, counts)`. Classifies only the latest run per check name so a superseded CANCELLED run (left in the rollup by a force/amend push) no longer yields a false red.

`counts["total"]` is the deduped count over the WHOLE rollup, which carries two different kinds of row: GitHub check-runs (the `name` key, produced by Actions and Checks-API apps) and commit StatusContexts (the `context` key, posted to the statuses endpoint). `counts["check_runs"]` and `counts["statuses"]` split that total, because a reader who compares `total` against `gh api .../check-runs` sees a phantom gap otherwise: that endpoint never returns statuses. Measured 2026-08-20 - the tally said 15, the check-runs endpoint named 13 jobs, and the gap was fno's own statuses. The coverage-context filter (`without_coverage_statuses`) feeds this function, so the two review-coverage StatusContexts are already absent from every count here. The two sub-counts need not sum to `total`: a rollup row carrying neither key is counted in neither (it is also never deduped).

`counts["fail_check_runs"]` and `counts["fail_statuses"]` split the fail bucket the same way, and they are what lets a caller name a red honestly. The VERDICT deliberately does not split: a failing StatusContext is a real red (`stacked-base-guard` is one), so it must never read green. What the split fixes is the ATTRIBUTION - see `_ready_blockers`.

`counts["unsettled"]` counts latest runs with NO settled marker (an absent result: cancelled, stale, still running), and `settled` is derived from it positively elsewhere - never from the absence of a pending run. `counts["unsettled_fail"]` narrows that positive count to rows that also classify as failures, so callers can distinguish a taken-away run from a concluded failure without inferring either outcome from an absence.

A would-be-green tally is refused when NO entry is a real check-run (the `name` key, produced by GitHub Actions/apps via the check-runs API) - a conflicting PR (mergeable_state dirty) gets zero workflow runs, but fno's own self-published StatusContexts (review-coverage, stacked-base-guard) still post and can all pass, so an all-`context` rollup read as green with no CI having run at all. A fail or pending StatusContext is still a real, actionable signal and stays red/pending.

Known tradeoff, not footnote's own blast radius: a repo whose ONLY real CI still rides the legacy commit-status API (no GitHub Actions, no Checks-API app) would never clear this and would hold forever under `require_checks_pass` (unknown holds, never fails - see the checks arm of `crates/fno-agents/src/authorized_merge.rs`). footnote's own workflows (guards.yml et al.) are all Actions/CheckRuns, so this repo never hits it; a fork that genuinely needs status-only CI as its sole signal should route around this via `require_checks_pass=false`.

## `run_status`: the authoritative read

Prints a one-line JSON verdict for a PR; the exit code is ALWAYS the CI verdict's code (0/1/2/3/4/127) - the review fields are additive and advisory (optional stays advisory; an unresolved optional finding on a green PR still exits 0). `ready` is the one field that conjoins them all (CI green, optional findings resolved, coverage a counted pass), with `ready_blockers` naming any conjunct that failed; a caller branching on the exit code is untouched. `review_reader` is injectable for tests; it defaults to the real time-boxed read.

The review and coverage reads are computed AFTER the authoritative CI verdict so a slow/failed read can never delay or corrupt it, and every one of them degrades to "unknown"/None on failure: the verdict plus exit code stay authoritative. The rerun-recovery probe (`rerun_recovery`) rides the same discipline: a green read gains the `rerun_recovered` / `recovered_failures` keys, present iff the probe ran, and the merge gate's flake hold consumes the same fact.

## `verdict_line`: slot ordering

The conclusion goes LAST and every slot before it is a fact, so a reader who stops early holds facts and no verdict - never a verdict about a different question. Each of the four misreadings this retires read a subset of the JSON and answered confidently anyway: a state slot that is always present cannot be omitted by the reader either, `verdict` and `settled` now sit adjacent, a head mismatch carries both commits in one line, and the mergeable slot states its own meaning instead of handing the reader a word to interpret. Slots are fixed, always present, in one order; a wrong value renders LOUD (capitals, parentheticals) so scanning for trouble is a real reading strategy. Keyed on the payload dict, never on run_status locals: `_cache._serve` degrades a stale row IN PLACE, so a payload-keyed renderer tells the degraded truth with no second code path.

## Payload-keyed note renderers

`coverage_recompute_note`, `failures_note`, and `rerun_recovery_note` are shared by the live read and the cache serve, so a degraded reason, a failure detail, or a rerun warning reaches every terminal rather than only the one session whose read produced the row. The bare success note stays silent: it adds nothing the coverage line does not already say, and a prefix that fires on every first poll trains a reader to skip the line where a reason appears. `failures_note` reads the PAYLOAD, never run_status locals, so every watcher sharing the row sees why the PR is red without any of them re-reading the log.

## `_ready_blockers`: the ready vocabulary

Which conjuncts of `ready` fail, in a stable order. A bare `ready: false` has one explanation per conjunct (a red check, an unresolved optional finding, coverage unknown or uncovered) and a reader cannot tell them apart; the list is the positive marker that names what is holding. A red is split by the KIND of result: `ci_red` when a check-run reached a failing conclusion, `ci_cancelled_retrigger` when every failure is a taken-away run, and `commit_status_red` when a commit StatusContext failed and no job failed at all.

`unknown` coverage blocks and is named as its own blocker: the reason a read returned unknown is a separate question, this only reports that the answer is missing. Fail-closed everywhere: an unset unresolved count blocks as `optional_reviews_unknown`, and the coverage conjunct engages wherever the merge gate's coverage guard would (`review_lane`; a repo with no review lane has no coverage answer to fail).

The coverage conjuncts are the merge gate's own helpers, read through them - one copy, never a restatement: `covered_conjuncts` names the row conjuncts (uncovered, no_local_pass, stale_head). The configured round cap is not a blocker here: at the cap the gate discharges the review obligation and the PR merges on green CI, so a held PR is held by its row conjuncts alone. So `ready` is a claim about those conjuncts and nothing wider.

A TERMINAL PR (merged or closed) is exempt from the coverage conjunct: the gate guards what would merge, and a PR merged out-of-band (UI, bare gh) has no "would" left to guard. That exemption arrives as `review_lane=False` from the caller's terminal arm and needs no conjunct of its own here. It had one - a `merged` flag - and it was decorative: `merged` is only ever True inside the branch that already sets `review_lane=False`, so it never changed an outcome while reading like the protection its name promised. A guard that cannot fire is worse than no guard, because the reader trusts the name.
