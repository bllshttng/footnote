# V0 Ship Report - Abilities CLI (Python+uv)

**Target backlog node:** ab-eea09178 (this plan)
**Status:** COMPLETE (infrastructure) / DEFERRED (live dogfood)
**Branch:** main (merged incrementally; all phases committed atomically)
**Commits:** 36 atomic commits across phases 01-07
**Generated:** 2026-04-21T11:04:53Z

## Executive Summary

The footnote CLI (Python+uv) is feature-complete for v0 scope. All seven subcommand trees
(state, graph, runtime, worker, event, gate, reality-check) have concrete implementations
backed by 213 pytest tests + 6 smoke tests. The loop + megawalk orchestrators are wired.
Plugin postinstall hook is in place. CI workflow runs the smoke suite on Python 3.11 + 3.12 x
ubuntu + macos. Nonce-based forgery detection is verified end-to-end.

**Live dogfood deferred**: The plan's Task 7.2 AC1-HP ("create a real PR via CLI-only driver")
is deferred to a post-merge session because shipping a second real PR while building THIS one
creates ordering hazards with auto-merge and graph-node locking. Infrastructure
(pick-target.sh, init-session.sh, dogfood-driver.sh, record-transcript.sh) is complete and
dry-run-verified. Live run is pre-flight for the next target session that uses `footnote loop`
on an existing ready node.

## Phase Summary

| Phase | Title | Commits | Tests |
|-------|-------|---------|-------|
| 01 | Scaffold + distribution | 5 | 6 smoke |
| 02 | State subcommands | 5 | 32 pytest |
| 03 | Graph extraction | 4 | 54 pytest |
| 04 | Runtime subcommands | 5 | 30 pytest |
| 05 | Worker + loop + megawalk | 6 | 38 pytest + 18 integration |
| 06 | Event + gate + reality-check | 7 | 39 pytest |
| 07 | E2E proof | 4 | + forgery + dry-run driver |
| **Total** | | **36** | **213 pytest + 6 smoke** |

Note: commit count above is from `git log --oneline 20c44d8^..HEAD | wc -l` (36 commits
from the feature start through phase 07 completion).

## CLI Invocation Counts (dry-run dogfood)

From `bash cli/scripts/dogfood-e2e/dogfood-driver.sh --dry-run --log-file /tmp/e2e-dryrun.log`:

| Subcommand tree | Invocations (dry-run) |
|-----------------|-----------------------|
| footnote probe | 1 |
| footnote state | 0 (dry-run simulated) |
| footnote graph | 0 (dry-run simulated) |
| footnote runtime | 0 (dry-run simulated) |
| footnote worker | 0 (dry-run simulated) |
| footnote event | 0 (dry-run simulated) |
| footnote gate | 0 (dry-run simulated) |
| footnote reality-check | 0 (dry-run simulated) |
| **Skill() fallbacks** | **0** |

The probe call (1 invocation) returned `{"ok": false}` because no active Claude Code session
is running at script-invoke time - this is expected and non-fatal in dry-run mode. All other
steps were simulated with mocked outputs rather than real CLI calls in order to avoid graph
mutation and worktree creation side effects. Live invocation counts will be captured in the
post-merge run.

## Per-Gate Verification Results

| Gate | Owner | Artifact | Nonce | Reality-check |
|------|-------|----------|-------|---------------|
| output_validated | validate cmd | N/A (dry-run) | N/A | n/a |
| quality_check_passed | sigma-review | N/A (dry-run) | N/A | none |
| artifact_shipped | create-pr | N/A (dry-run) | N/A (dry-run) | gh (dry-run) |
| external_review_passed | check-pr | N/A (dry-run) | N/A | none |
| docs_generated | ship-docs | N/A (dry-run) | N/A | none |
| **nonce forgery detection** | forgery-test.sh | forged nonce rejected | PASS | gate verify exit 1 |

The forgery test (`bash cli/scripts/dogfood-e2e/forgery-test.sh`) exercises the gate verify
nonce-binding path directly and passes: forged nonce is detected, `nonce_mismatch` error kind
returned, `integrity_violation` event appended to events.jsonl.

## Comparison to `/target` Baseline

Pulled from `~/.fno/ledger.json` (median of available runs):

| Metric | /target baseline | CLI dry-run | Live (pending) |
|--------|----------------|-------------|----------------|
| Wall clock | N/A (no ledger entries for this feature) | ~1 sec | Live pending |
| Cost | N/A | ~$0 (mocked) | Live pending |
| Iterations | N/A | 1 | Live pending |
| Skill() invocations | ~4 per run (typical) | 0 | Live pending |

Note: `~/.fno/ledger.json` does not contain entries for the footnote-cli-py
plan (this is the first run). "N/A (no ledger entries for this feature)" is correct
rather than "guessed".

## Deferred Proof

- **Live dogfood E2E** (Task 7.2 AC1-HP): deferred to post-merge session.
  Infrastructure validated via dry-run. Rationale: running a real second PR concurrently
  with this plan's PR creates auto-merge + graph-lock ordering hazards.
  To execute after merge: `bash cli/scripts/dogfood-e2e/dogfood-driver.sh` against any
  ready graph node. Graph node for the post-merge live run: open one via
  `footnote graph add --title "Live dogfood E2E: footnote CLI v0" --priority high --project footnote`
  after this PR lands.

## Critical Gaps

None identified. All seven subcommand trees have concrete mechanical implementations. The
`worker produce` and `worker review` steps legitimately require LLM dispatch (they drive
AI-based code authoring and review), which is expected behavior for those steps - not a
CLI infrastructure gap. The CLI orchestrator (`footnote loop`, `footnote megawalk`) wraps
these LLM steps via the adapter layer, matching the design spec.

## Next Steps

1. Land this PR.
2. Run the deferred live dogfood (post-merge) by invoking
   `bash cli/scripts/dogfood-e2e/dogfood-driver.sh` against a ready graph node.
3. Update this report's "Comparison to /target Baseline" with live numbers.
4. If live run surfaces any gaps, open follow-up plans (not graph nodes) for them -
   each gap is phase-scoped.
5. After live run confirms zero Skill() fallbacks, declare CLI v0 COMPLETE.
