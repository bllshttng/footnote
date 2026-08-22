# Eval task bank (`fno doctor evals`)

A live-execution eval harness: give the pipeline a known task, run it, grade the result mechanically, and score how reliably it passes. Distinct from the observer (offline corpus replay) and skill-diff eval-after-merge (prompt-diff scoring) - this is live task execution with a pass/fail verdict.

## The two disciplines

**Capability vs regression tiers.** Every bank task declares `tier: capability | regression`.

- `capability` - a hill to climb: a hard task the pipeline currently fails. Failures are informational, not alarms.
- `regression` - must stay ~100%: a task that used to pass and must keep passing (e.g. the CLI starts, a known-flaky suite is green). Any regression-tier task below 100% fires the **regression alarm**.

A capability task that passes its last N consecutive full runs (default 3) becomes graduation-eligible. `fno doctor evals graduate <id>` retags its YAML to `regression`. Graduation is a reviewed edit: the verb rewrites the file, and a human ships the PR. It is never a silent runtime flip.

**pass^k reliability.** `--repeat K` runs a task K times. The report shows `pass@1` (single-run success rate) and `pass^k` (every run passed) per task, plus a flake list (tasks that passed sometimes but not always). This turns "we re-run CI and it clears" folklore into a graded flake rate - the CI-flake regression tasks (e.g. the `loop_check` suite) are the first targets.

## Bank format

One file per task under `evals/bank/*.yaml`:

```yaml
id: capability-add-hello-verb
tier: capability            # capability | regression
prompt: |                   # optional: omit for a grade-only task (no worker step)
  <the task the worker is asked to perform>
repo_fixture: HEAD          # git ref or fixture dir (default HEAD)
grade:                      # >=1 mechanical check; a gradeless task is rejected at load
  - kind: exit              # a shell command's exit code must equal `expect` (default 0)
    command: "cd cli && uv run fno-py hello"
  - kind: grep              # a workdir-relative file must contain a substring
    path: "cli/src/fno/cli.py"
    pattern: "hello"
  - kind: file-exists       # a workdir-relative path must exist
    path: "out/report.md"
timeout_minutes: 20
tags: [cli, greenfield]
```

Success criteria must be **mechanical** (develop-tests discipline): a task with no runnable `grade` is invalid at load time, and an all-trivial grade (`command: true`) warns. A **grade-only** task (no `prompt`) skips the worker step and grades the fixture directly - the honest model for a CI-flake regression task where there is no agent work, only a flake to measure.

## Verbs

| Command | What it does |
|---|---|
| `fno doctor evals run [--task ID] [--tier T] [--repeat K] [--provider P]` | Run bank tasks in disposable worktrees, grade mechanically, append one history line per task-run. Confirms above 20 total runs (`--yes` skips). |
| `fno doctor evals report [--since N] [--graduate] [--json]` | Fold history: per-tier pass rates, pass@1, pass^k, flake list, regression alarm (exit 4 on alarm). `--graduate` lists eligible capability tasks. |
| `fno doctor evals graduate <id>` | Retag a capability task's YAML to regression. |

Each run executes the task in a disposable worktree via the headless spawn substrate (`fno agents spawn --substrate headless` - never bare `claude -p`, keeping provider rotation and the spawn cap in play), then removes the worktree after grading. A bank task never runs in your working copy. History appends to `~/.fno/evals-history.jsonl` (override via `config.paths.evals_history`).

## Run cadence

**Manual or explicitly scheduled - never auto-on-merge in v1.** Live execution spends real money (each run spawns a worker). Automation is a separate opt-in once the baseline cost per sweep is known. Run it:

- **By hand** during development: `fno doctor evals run --tier regression` before a risky change, or `--repeat 5` on a suspect flaky task.
- **On a schedule** you own (cron / `fno schedule`) for a periodic regression sweep.

The report has two wired consumers from day one, so the harness is not a write-only artifact:

1. `fno backlog triage health` shows an `evals` line (regression pass rate + flake count) whenever history exists.
2. CI-flake suites live as regression tasks, making the flake ledger a graded artifact instead of memory notes.

## Context-to-outcome trace

`fno whoami scoreboard --plan-fidelity` derives a context-to-outcome trace from the ledger, backlog graph, and canonical event journal.
It does not persist a second graph or treat a terminal label as downstream evidence.
A context-pruning or graph-widening comparison is labeled an improvement only when its event-carried contract declares the cohort members, observation window, exclusions, budgets, and declaration time before the window starts.
Every selected trace must also carry a falsifiable CI, review, merge, revert, recovery, latency, or spend observation.

The table below is generated by `fno.scoreboard.fold.render_context_trace_field_docs`; update the parser-owned field contract and regenerate this block instead of duplicating field prose.

<!-- context-outcome-fields:start -->
| Field | Derived from | Meaning |
|---|---|---|
| `objective` | graph, ledger | Objective or node title attributed to the delivery. |
| `plan_node` | ledger, graph | Canonical backlog node identifier. |
| `worker_session` | ledger, events | Session that produced the delivered commit. |
| `claim` | claim events | Historical claim observations; never current ownership truth. |
| `commit` | ledger, loop_check | Exact delivered commit when observed. |
| `evidence_receipt` | ledger | Existing receipt reference; the trace never invents one. |
| `pr` | ledger | Pull request number and URL. |
| `context` | context_snapshot event | Harness, entry state, delivered bytes, tokens, source hashes, and measurement completeness. |
| `outcomes.ci` | loop_check events | Latest observed CI state. |
| `outcomes.review` | review events, loop_check | Review verdict and finding observations. |
| `outcomes.merge` | events, graph | Observed merge outcome. |
| `outcomes.revert` | events, graph | Observed revert outcome. |
| `outcomes.recovery` | recovery and handoff events | Recovery observations without changing resume behavior. |
| `outcomes.latency_minutes` | ledger | Recorded delivery latency. |
| `outcomes.spend_usd` | ledger | Recorded delivery spend. |
| `falsifiable` | derived | True only when at least one downstream observation can disprove a claim. |
| `provenance` | derived | Canonical inputs that contributed to the trace. |
<!-- context-outcome-fields:end -->
