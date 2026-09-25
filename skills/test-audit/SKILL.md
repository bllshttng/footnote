---
name: test-audit
description: "Invoke whenever writing, changing, reviewing, or sweeping tests. Authoring gate for new tests plus audit workflow for low-value, implementation-coupled, or duplicative tests and the test-only production seams they demand. Campaign mode prunes one subsystem's whole test surface."
---

# Test Audit

Three modes, one value bar. Authoring mode gates every new or changed test at write time. Audit mode sweeps tests that re-assert source, duplicate stronger proof, couple behavior to implementation, or keep test-only production seams alive. Continue broad audits as separate follow-up PRs. Optimize for confidence, not deletion count. Campaign mode prunes one whole subsystem's test surface. Read [CAMPAIGN.md](CAMPAIGN.md) before starting one.

## Authoring gate

Before adding any test, answer four questions. A missing answer means do not add it yet:

1. What observable behavior, invariant, or independent contract does it protect?
2. What credible regression makes it fail?
3. Why does existing coverage not already catch that failure? Each contract has one primary test owner at the strongest boundary. Another layer needs its own distinct risk. Example: a transport or lifecycle failure the owner cannot reach. Prefer extending a table-driven case or shared fixture over a near-duplicate test. Consolidate duplicated setup in the same change.
4. Does it need a production seam (export, flag, wrapper, injection hook) that no production caller needs? If yes, move the test to the real boundary instead.

Then check the test against every [junk pattern](#junk-patterns). A match fails the gate unless the [retention bar](#retention-bar) names the contract it independently guards. A test that breaks under behavior-preserving refactoring asserts implementation, not behavior. Rewrite it at the owning boundary before landing it.

Bug regression tests must fail on the pre-fix code for the intended reason. They must pass after the owner-boundary repair. A regression test that never demonstrably failed proves the mock, not the fix. One regression at the owner boundary covers the bug. Do not replay the same scenario at every layer it crosses.

## Junk patterns

The shared checklist for both modes. The authoring gate rejects a new test that matches one. Audits hunt for existing tests that do.

- assertion-free coverage probes.
- self-comparisons and identity copiers.
- copied fixtures, inventories, manifests, or export lists.
- exact source, import, or string greps.
- private predicate or call-shape tests duplicated at real boundaries.
- duplicate invocations of the same contract.
- provider-local replays of shared helpers.
- tests whose only purpose is preserving test-only exports, globals, or wrappers.
- dead production code whose only callers are tests.
- expected values produced by the helper or renderer under test.
- mocks that implement the asserted behavior. Also one identical mock standing in for different APIs.
- fixtures that supply the receipt, admission, or callback ordering the owner must produce. Also persistence asserted against a store the path never writes.
- capability tests that restate declared flags instead of exercising the promised delivery or acknowledgement.
- negative controls that pass for an unrelated reason, such as a denial from a different guard.
- names or fixtures that promise more than the input exercises. Example: a "retires the window" test asserting the window was not cleared.

## Value bar

Tests justify their maintenance cost by protecting behavior, a credible regression, or an independently meaningful contract. In an audit, an existing test that must change for behavior-preserving source reorganization is suspect. It is not automatically deletable. The authoring gate still rejects new ones.

Before judging a candidate, read the complete test and production owner. Also read its entry point, callers, callees, sibling implementations, overlapping tests, CI routing, and relevant history. Read root and scoped `AGENTS.md` files first. When the test claims dependency-backed behavior, inspect the dependency source or types directly.

## Discovery

Keep discovery read-only. Report evidence before editing. When the scope is broad and the environment allows parallel lanes, run them:

- core source and packages.
- plugins or extensions.
- UI, apps, scripts, and tooling.
- one cross-cutting pattern sweep.

Outside campaign mode, prefer a few high-confidence candidates over a large speculative inventory. Hunt for the [junk patterns](#junk-patterns).

## Retention bar

Keep a test that independently enforces a contract. Covered contracts: public API, plugin SDK, protocol, config, migration, storage, security, platform, default, prompt-byte, generated cross-language, package, release, architecture. Also keep:

- call ordering that is observable behavior.
- regressions with a credible failure mode.
- source inspection as the cheapest independent guard. It fails on a contract change: the user-facing key, byte, or path. It survives an identifier-only refactor.
- a retained test that fails on the baseline. Treat it as a possible product bug. Reproduce it and repair the owner instead of deleting it.

Static or slow is not a deletion reason. A test that resembles implementation can still be the independent contract. Prove otherwise before removing it.

## Candidate evidence

Record every field below before editing. A missing field means the candidate is not ready for deletion:

- exact test name and location.
- what failure it can actually detect.
- non-test callers of the covered production or support seam.
- stronger remaining owner-boundary proof, or why no proof is needed.
- relevant history and the reason the test or seam exists.
- production or test-support deletion unlocked.
- risk and the focused validation command.

## Edit shape

Choose one coherent owner-boundary batch. Delete obsolete test-only exports, globals, wrappers, and dead production paths. Do not preserve aliases. Move retained regressions to their canonical owners. Consolidate repeated package or dependency assertions into one generic contract.

Prefer net-negative production LOC. Do not add replacement tests that restate the same implementation. Do not convert uncertain candidates into cleanup to increase deletion counts.

## Validation

Never edit source or tests while the suite is running in the checkout.

1. Run the smallest owner and sibling tests with the repo's documented runner. On this repo: `fno doctor test [paths...]` for Python, `cargo test -p <crate> <filter>` for Rust. Never a bare `pytest` in a worktree: it can import the wrong tree and report a false green.
2. For removed source greps or plan assertions, run the executable script or dry-run that owns the real contract.
3. Run targeted formatting, then `git diff --check`.
4. Run the changed-surface gate the repository policy requires. On this repo: `bash scripts/ci/check-file-budget.sh` after a code-growing commit. For markdown: `fno doctor lint style --surface markdown --diff-base origin/main`.
5. Inspect `git diff --numstat`. Report production/tooling separately from tests and test support.
6. After final audit edits, run the repository's review lane (`/fno:review <level> --comment`) on the final HEAD.

## Landing and continuation

Commit, push, or open a PR only within the run's authority. Land one coherent PR at a time. After landing, merge current `main` into the branch. Never rebase a pushed branch. Rerun read-only discovery for the next high-confidence batch.

## Handoff

Report:

- root cause and removed low-value categories.
- production owner simplifications.
- retained false positives and why they remain valuable.
- focused and full proof actually run.
- production versus test LOC.
- PR and merge state.
- named follow-ups.

## Known Limitations and Deferred Work

- The gate reads. Proving a deleted contract lost its only proof stays with the campaign's preservation review. See [LIMITATIONS.md](LIMITATIONS.md).
