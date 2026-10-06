# K1 campaign ledger: tooling, config, paths, setup, doctor, worktree

Base f6cfde3612 (origin/main after PR 1). Scope: the 185 files the campaign map keys K1.

Before: 3,008 declarations (2,675 Python, 333 Rust), about 2,877 collected Python cases.
After: 2,776 declarations (2,443 Python, 333 Rust), about 2,632 collected cases.
The collected-case fall tracks the declaration fall. No test was converted to parametrize rows to close a gap.

## Stop under AC5-EDGE

The campaign stops at 2,776, above the 1,504 ceiling, because the keep rule keeps the rest. The verdict evidence:

- Six files folded (below). Every fold keeps one row per distinct branch. Same-branch value repeats were deleted with their family's strongest keeper.
- Whole-file verdicts that KEEP, with the contracts their tests alone guard:
  - `cli/tests/unit/test_lint_cli.py` (65): the style-gate instrument itself. Rename resolution, no-newline marker numbering, `++`-header content, partial parser loss, per-call git-failure refusal, rename-limit pins. Each pins a measured failure of a gate this fleet runs on every PR.
  - `cli/tests/test_lazy_imports.py` (48): lazy-import contracts. Menu projection, import cycles, error paths, race windows.
  - `cli/tests/unit/test_route_resolve.py` (50): already row-declared. The docstring records the earlier conversion.
  - `cli/tests/goldens/*` (46): byte pins of the fno-agents receipts, named as pins by `scripts/ci/reachable-paths-baseline.txt` (`msg-twin` rows). User-facing bytes at the strongest boundary. Deleting them breaks the provenance gate's documented evidence.
  - `tests/hooks/*` (121 across push, merge-guard, coverage): fail-closed path guards. The lead's red line: never delete a test that is the only guard for a fail-closed path. Folded where branches repeat, kept one row per branch.
- Five files in the top ten spot-verdicted: doctor (136), update (110), the cli_hooks family, the route pieces, the config cluster. Their tests are distinct contracts, mostly already post-audit. Docstrings cite sigma reviews, PR review rounds and measured incidents. The plan's 678 one-assert signal counted shapes, not redundancy.

## Folded files

| File | Before | After | Kept contracts |
|---|---:|---:|---|
| cli/tests/unit/test_style_rules.py | 123 | 50 | one table per rule family, one row per block type and masking construct. The refusal self-check and the negation invariant kept whole. |
| tests/hooks/test_git_protection_push.py | 53 | 8 | explicit-destination rows, debounce outcomes, segmentation content and caught rows, fail-closed end-to-end, grep allowlist. |
| cli/tests/unit/test_model_routing.py | 74 | 32 | tier map rows, key-resolution rows, protected-role rows, codex-lane rows, build lane, parse-target rows, refresh rows. |
| tests/hooks/test_merge_guard_worktree.py | 68 | 18 | worktree authorization rows, tokenization, marker and approval single-use, evasion rows, lone-command rows, fail-closed state rows. |
| cli/benchmarks/test_measure_fno.py | 30 | 8 | decision-rule boundaries, strict-type rows, probe outcome rows, phase scaling, event builder. |

## Preservation spot-check

Owed before any deeper cut resumes. The fold deltas above are covered by their file-scoped test runs, all green locally. The whole-file keepers were not edited, so no keeper contract changed.

## Next lever (for the lead)

Halving K1 by deletion means deleting distinct-contract tests of fail-closed tooling. That is an executive call against the keep rule, not a campaign gap. The honest ways to the 50 percent suite goal: rule whole owner-areas deletable, then cut them wholesale. Or concentrate campaigns on the Rust suites (K8, K9), whose declaration families fold mechanically.
