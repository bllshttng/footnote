# Efficacy evals: golden /target tasks

## Overview

footnote has cost observability via `ledger.json` but no efficacy observability: nothing measures whether a control-plane change made the pipeline better or worse at actually shipping features. The Anthropic data-analytics team's experience is the cautionary precedent and the playbook: their skill-driven accuracy went from 21% to 95%, silently drifted back to 65% in a month, and every structural decision after that was settled by holding a fixed eval set and varying one component at a time ("ablate at PR granularity"). Their most useful experiment was a null result that redirected months of roadmap.

Slice 1 builds the minimum instrument: a small suite of golden tasks - tiny fixture projects with ready-made plans - run through the *real* `/target` pipeline end-to-end in advisory mode, checked by deterministic assertions, with every run appended to a history file. "Did that change help?" becomes a query against `evals-history.jsonl` rather than a debate.

The key enabling fact: `no_ship: true` is a first-class manifest flag (`TARGET_NO_SHIP` env / config), and loop-check terminates such sessions as `DoneAdvisory` on the promise tag with no GitHub reads. Eval runs therefore need zero GitHub and no new control-plane code.

This instrument is the prerequisite for the dynamic-harness work: harness shapes are only comparable once there is an instrument to compare them with.

## Methodology: telemetry, not test logs

Results are stored like telemetry, not assertions. A single run produces a history row; the signal comes from trends across rows. This matters because:

- LLM nondeterminism means a single assertion flip is noise, not a regression.
- Two labeled sweeps (before and after a change) are the comparison unit, not pass/fail from one run.
- Cost is tracked per run so token-count regressions surface alongside correctness regressions.

`fno evals diff --label before --label after` is the ablation surface: paste the delta into a PR description instead of "it feels better now."

## Five components

### 1. Fixture format: `evals/golden/<slug>/`

Each fixture is a directory with four required files:

```
evals/golden/<slug>/
├── task.yaml      # title, tags, budget_usd, max_iterations, timeout_secs
├── repo/          # tiny template project including its own test suite
├── plan.md        # ready-status quick plan (frontmatter status: ready)
└── assert.sh      # deterministic post-run assertions, TAP-lite output
```

`task.yaml` fields (all but `title` optional, with built-in defaults):

| Field | Type | Default | Purpose |
|---|---|---|---|
| `title` | string | required | Human-readable task title |
| `tags` | list | `[]` | Category labels (e.g. `["feature"]`, `["bugfix"]`) |
| `budget_usd` | float | 3.0 | Maximum spend cap; task never runs uncapped |
| `max_iterations` | int | 10 | Stop the session after this many iterations |
| `timeout_secs` | int | 1800 | Wall-clock timeout for the full run |

`plan.md` must carry `status: ready` in its YAML frontmatter so `/target` skips the think/blueprint phases and executes the plan directly.

`assert.sh` runs in the post-run workdir and emits one line per assertion: `ok <name>` or `not ok <name>`. This per-assertion granularity is what makes history rows diffable. A run of `assert.sh` that emits zero assertion lines counts as a failure - an empty script must not pass silently.

`repo/` must contain at least one `test_*.py` or `*_test.py` file. The fixture loader (`fixtures.py`) enforces all structural requirements at load time and raises `FixtureError` on the first violation.

Fixtures are colocated in the repo: a PR that changes pipeline behavior can update the affected fixture in the same diff.

### 2. Runner: `fno evals run`

`runner.py` is the pure orchestration logic; `cli.py` is the Typer sub-app registered as `fno evals`. Per selected task:

1. `mkdtemp` workdir; copy `repo/` into the workdir root; copy `plan.md`.
2. Write `.fno/settings.yaml` in the workdir with `no_ship: true` and path overrides (see isolation section).
3. `git init` + initial commit on a branch named `eval-run` (non-main branch required by `init-target-state.sh`).
4. Pre-init the target-state manifest via `hooks/helpers/init-target-state.sh` (falls back to a minimal manifest if the helper is absent).
5. Spawn `scripts/run-target-loop.sh` with `--budget` and `--max-iterations` from `task.yaml` as a new process group (`start_new_session=True`). The session runs the real pipeline: plan execution, TDD, sigma-review, advisory termination.
6. On termination: run `assert.sh`; collect cost from `scripts/metrics/session-cost.py`; run the isolation check.
7. Append exactly one history row via `O_APPEND` write.
8. Workdir removed on a clean full-pass; kept (path printed) on failure, timeout, or `--keep-workdir`.

The run header prints `fno doctor`'s verdict so a stale-install sweep is visibly suspect before any task fires (see operational notes).

Termination reason is extracted from `workdir/.fno/events.jsonl` (last `termination` event's `reason` field). Exit codes: `0` all tasks passed; `1` one or more failed or harness error; `2` unrecoverable setup error (missing loop script, unknown task slug).

**Termination-reason reality (first smoke sweep, 2026-06-05):** the spawned `claude --print` session is not the owner of the pre-initialized manifest, so the stop-hook's `loop-check` may never judge it and no `termination` event lands in the workdir's events.jsonl. The loop still ends correctly (the driver greps the promise tag), and the runner then falls back to exit-code mapping: `0` → `unknown-complete`, `1` → `MaxIterations`, `2`/`77` → `harness-error`, wall-clock kill → `timeout`. `DoneAdvisory` appears only when loop-check actually emits a termination event. Rows record whichever happened, verbatim - the field stays diffable either way. Making loop-check judge eval sessions is a filed follow-up.

Loop script resolution: `FNO_EVALS_LOOP_SCRIPT` env var takes priority, then `<repo_root>/scripts/run-target-loop.sh`. The runner rejects a missing or non-executable loop script before spawning anything.

### 3. History: `evals-history.jsonl`

Resolved via `fno.paths` (`config.paths.evals_history`, default `~/.fno/evals-history.jsonl`). Append-only; never rewritten. Each write is a single `os.write` with `O_APPEND | O_CREAT` so concurrent writers on POSIX filesystems do not interleave partial lines (writes within PIPE_BUF are atomic).

History row schema:

```json
{
  "ts": "2026-06-05T19:00:00Z",
  "task": "seeded-bug-fix",
  "label": "pr-460-after",
  "abilities_sha": "c7ad04c2...",
  "installed_rev": "c7ad04c2...",
  "model": "claude-sonnet-4-5",
  "driver": "claude-code",
  "termination_reason": "DoneAdvisory",
  "assertions": {"tests_pass": true, "no_unrelated_files": true},
  "passed": true,
  "total": 2,
  "tokens_total": 184000,
  "cost_usd": 1.92,
  "wall_secs": 412.0,
  "iterations": null,
  "session_id": "20260605T190000Z-...",
  "transcript_path": "/path/to/transcript.jsonl",
  "isolation": "clean",
  "workdir_kept": null
}
```

`passed` is `true` only when `total > 0` and every assertion is `ok`. `workdir_kept` is either the workdir path string (kept) or `null` (removed).

`abilities_sha` records the repo HEAD at run time; `installed_rev` records the contents of `~/.fno/installed-rev` (the revision the installed `fno` was built from). When these differ, the row reflects a measurement against a stale install.

### 4. Reporting: `fno evals report` and `fno evals diff`

Both commands read history tolerantly via `iter_rows_tolerant` (corrupt JSONL lines are skipped with a warning naming the line number; the command completes on remaining rows).

`fno evals report [--task slug]` shows:
- Latest result per task: pass/total, termination reason, cost, isolation verdict.
- Trend over recent runs (pass-rate and cost).
- A staleness warning when the newest row is older than `config.evals.staleness_days` (default 14 days).
- A `pass-with-bad-termination` flag for rows where assertions passed but the session terminated `Budget` or `NoProgress` rather than `DoneAdvisory`.

`fno evals diff --label A --label B` shows:
- Per task: assertion flips (regressions visually distinct from improvements), termination-reason changes, token/cost deltas.
- Tasks present in A but missing in B are listed as "missing in after", not silently dropped.
- If either label matches no history rows, the command exits nonzero naming the missing label.

### 5. Seed suite (4 tasks)

The four seed shapes in `evals/golden/`:

| Slug | Shape | Point of interest |
|---|---|---|
| `feature-add` | Add a new function to an existing module with a test | Happy path: does the pipeline implement and commit a tested feature? |
| `seeded-bug-fix` | Fixture repo has a known failing test; plan says fix it | Does the pipeline find and fix the root cause rather than deleting the test? |
| `refactor-under-tests` | Refactor task with green tests as the contract | Does the pipeline keep tests green without changing behavior? |
| `edge-case-heavy` | Feature plan with explicit AC-EDGE criteria | Does the pipeline implement the edge cases named in the plan? |

Tasks are deliberately single-file scale so a full sweep costs on the order of $10-15 at the per-task budget caps.

## The isolation invariant

An eval session must never mutate real operator state: `~/.fno/ledger.json`, `~/.fno/graph.json`, the footnote repo's `.fno/events.jsonl`, the memory directory, `~/.claude/corrections.log`. Prior art for the failure: megatron tests leaked telemetry into the real repo's events.jsonl via repo-root resolution.

Isolation is two layers.

### Preventive layer

`_write_workdir_settings` writes `.fno/settings.yaml` in the workdir with:

```yaml
config:
  no_ship: true
  state_dir: <workdir>/.fno/
  paths:
    graph_json: <workdir>/.fno/graph.json
    ledger_json: <workdir>/.fno/ledger.json
    briefs_dir: <workdir>/.fno/briefs/
    evals_history: <workdir>/.fno/evals-history.jsonl
```

Any Python `fno.paths`-aware writer that resolves paths from the project-local settings with `CWD = workdir` will land in the workdir. This covers `graph_json`, `ledger_json`, `briefs_dir`, and `evals_history`.

The fragment writer also drops a `.fno/.path-migration-done` sentinel, and the loop subprocess env carries `FNO_SKIP_MIGRATION=1`. Both suppress `fno`'s first-invocation path migration (`_check_migration` in `cli/src/fno/cli.py`), which would otherwise REWRITE this fragment on the first `fno` call inside the eval session - nulling the per-path overrides and resetting `state_dir` to `~/.fno/`. This was observed live in the first smoke sweep (2026-06-05); the regression is pinned by `test_evals_run_workdir_settings_migration_sentinel` and `test_evals_run_loop_env_skips_migration`.

**Known escapees the preventive layer cannot cover:**

- `scripts/metrics/register-task.py:23`: `LEDGER_JSON_PATH = Path.home() / ".fno" / "ledger.json"` - hardcoded, no config read, no `FNO_HOME`.
- `scripts/lib/paths.sh:11-13` (auto-generated by `fno paths emit-shell`): `STATE_DIR="$HOME/.fno"` hardcoded; all bash-layer writers inherit this.

`HOME` and `FNO_HOME` manipulation is intentionally not attempted: it breaks claude CLI auth and the bash layer ignores `FNO_HOME` anyway. The detective layer is the load-bearing backstop.

A follow-up carveout tracks whether to add config-path awareness to these writers.

### Detective layer

`fno.evals.isolation` runs post-assertions and scans real operator state for eval session ids. It gathers session ids from three sources: `workdir/.fno/target-state.md` frontmatter, `workdir/.fno/events.jsonl`, and transcript files under `~/.claude/projects/<encoded-workdir>/`.

The check is attribution-based, not byte-identity: it looks for the eval session ids, not a diff of the files. A concurrent real session legitimately writing the ledger during an eval run will not trip a false positive because its session id differs from the eval session id.

A violation:
- Sets `isolation: violated` in the history row.
- Exits nonzero.
- Prints the offending file paths, session ids, and a one-line explanation of the suspected escape path (naming the known-escapee writer where applicable).

## Operational notes

**Run from the footnote repo checkout.** `fno evals run` resolves `scripts/run-target-loop.sh` relative to the git repo root of the invocation directory.

**Check `fno doctor` first.** Eval sessions invoke the *installed* `fno`, not the repo source. The run header prints `fno doctor`'s verdict automatically; a `stale` result means the sweep is measuring old code.

**Cost figures are comparative, not invoice-grade.** `session-cost.py` produces estimates; `ccusage` is authoritative for billing. Use cost fields to compare runs against each other, not to predict a bill.

**Single run per invocation.** Each task runs once. LLM nondeterminism can flip one assertion; judge trends across multiple labeled runs, not a single result.

**Workdir location.** `mkdtemp` lands under the OS temp directory (e.g. `/var/folders/...` on macOS). The runner resolves the workdir to its realpath at creation: Claude Code encodes the *resolved* path (`/private/var/...`) in its `~/.claude/projects/` directory naming, and transcript discovery (cost + isolation attribution) depends on the encodings matching. Fixture plans must not contain absolute-path assumptions.

**Assertion interpreter.** Fixture `assert.sh` scripts invoke `"${PYTHON:-python3}"`; the runner injects `PYTHON=<sys.executable>` (the interpreter running the runner, which has pytest) so assertions never depend on the ambient `python3` having pytest installed.

**Concurrency.** Two concurrent `fno evals run` invocations use distinct temp workdirs, and each history write is a single `os.write`, so rows never interleave. Claim keys (`walker:`, `node:`) never collide because the eval workdir is its own project root.

## Locked decisions

1. **Advisory-mode only.** Golden tasks run with `no_ship: true` and terminate `DoneAdvisory`. The ship/review-gate surface stays covered by step 2's deterministic mocked-gh Rust tests. A mocked-gh full-path slice is possible later but is not slice 1.
2. **Manual verb, no CI gating yet.** Results are telemetry first. Ratchet to CI gating only after the suite proves stable.
3. **Deterministic assertions only in slice 1.** `assert.sh` per fixture; no LLM grader. Graders can come later for quality judgments; correctness-of-outcome is checkable mechanically for tiny tasks.
4. **Fixtures live in-repo at `evals/golden/`.** A PR that changes pipeline behavior can update the affected fixture in the same diff.
5. **Isolation is a tested invariant, two layers.** Preventive path overrides plus detective attribution-based post-run check. Violation fails loudly.
6. **Single run per task per invocation.** Nondeterminism is handled by judging trends across history.
7. **`cli/src/fno/evals/` is outside the LOC-ratchet control-plane paths.** The instrument must not itself trip the gate it exists to inform.
8. **History path via `fno.paths`.** `config.paths.evals_history`, default `~/.fno/evals-history.jsonl`, append-only JSONL.

## Implementation

`cli/src/fno/evals/fixtures.py` - fixture loader and `TaskSpec` dataclass.
`cli/src/fno/evals/tap.py` - TAP-lite parser for `assert.sh` output.
`cli/src/fno/evals/history.py` - append-only JSONL writer (`append_row`) and tolerant reader (`iter_rows_tolerant`).
`cli/src/fno/evals/runner.py` - pure orchestration logic (`run_tasks`, `_run_single_task`, `_build_row`).
`cli/src/fno/evals/isolation.py` - detective attribution check (`check_isolation`, `collect_eval_session_ids`).
`cli/src/fno/evals/reporting.py` - report and diff renderers.
`cli/src/fno/evals/cli.py` - Typer sub-app (`fno evals run / report / diff`).
`evals/golden/` - seed fixture suite.
