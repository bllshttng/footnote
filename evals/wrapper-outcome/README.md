# JSON-wrapper outcome evaluation

Compare authored `fno_wrap_json` scratch artifacts per matched attempt while checking actual task result JSON. This is an offline evaluation instrument. It neither runs agents nor fixes the CLI. It reuses `fno_agents::scratch::classify` as its shape classifier.

## Run the instrument

```sh
cargo test --manifest-path crates/fno-agents/Cargo.toml --example wrapper_outcome_eval
cargo run --quiet --manifest-path crates/fno-agents/Cargo.toml --example wrapper_outcome_eval -- /absolute/path/to/experiment.json
```

The first command runs synthetic calibration controls. The second reads a captured experiment, writes one JSON report to stdout, and changes no evidence files. Exit 0 means observed improvement or a completed calibration. Exit 1 means regressed or unchanged. Exit 2 means insufficient or invalid evidence. Always read `verdict`, `comparison`, and `kind` together. `calibration_only` never establishes live benefit.

The checked-in `manifest.example.json` declares an incomplete calibration with artificial revision identifiers and no runs. Running it must exit 2 with `missing attempt`. It is an input template, not a result or a completed benchmark.

## Declare the comparison before collecting runs

1. Select representative JSON-reading tasks from the recurring-wrapper discovery corpus. Define their real input, hash its exact bytes, and define the expected JSON result before either arm runs. Keep this oracle outside the agent's workspace. A node's title or shape frequency is not a task definition.
2. Pin the full baseline and candidate Git commit IDs and name the specific intervention. Keep the harness, model, effort, and time budget fixed. Run the evaluator from one fixed checkout so both arms use the same classifier.
3. Declare every task and at least three repetitions, with a declaration timestamp earlier than every run. Use a fresh workspace per attempt and alternate or randomize arm order. Use the same task input for a pair. Prohibit access to prior solutions and the grading oracle.
4. Capture every attempt, including abandoned and failed tasks. A trusted collector must preserve the exact input, raw JSON result, and all newly authored scratch files. Use a separate directory for each attempt. Keep collection metadata outside the worker's writable scope. Record observed runtime and revision, not merely requested settings.
5. Freeze the collected files and their SHA-256 hashes. Run the evaluator and inspect the per-task rows as well as the aggregate. Retain the preregistration and capture packet with the result.

Historical scratch findings select candidate tasks. Their distinct-job counts do not supply the denominator for this comparison. Discovery records can contain several different problems, and the existence of a wrapper does not establish that it was unnecessary.

## Input contract

All fields are required unless explicitly optional. Unknown fields are refused.

| Manifest field | Meaning |
|---|---|
| `schema_version` | `1` |
| `kind` | `calibration` or `experiment` |
| `cohort_id` | Nonempty experiment identity |
| `declared_at` | RFC3339 preregistration time, earlier than every attempt |
| `intervention` | The exact change being compared |
| `baseline_revision`, `candidate_revision` | Different full 40-character lowercase Git SHAs |
| `runtime` | `harness`, `model`, `effort`, and positive `budget_seconds` |
| `repetitions` | At least three, identical for both arms and every task |
| `tasks` | Unique `id`, exact-byte `input_sha256`, and an `expected` JSON value |
| `runs` | Exactly one record for each `(arm, task_id, repeat)` |

| Run field | Meaning |
|---|---|
| `arm`, `task_id`, `repeat` | `baseline` or `candidate`, declared task ID, and repetition starting at 1 |
| `attempt_id` | Unique collector identity for the run; never reuse a prior capture |
| `revision`, `runtime` | Observed values, matching the declared arm and runtime |
| `started_at` | RFC3339 time after preregistration |
| `termination` | `completed`, `task_failed`, `abandoned`, or `infrastructure_error` |
| `exit_code` | Actual process exit code; completed requires zero |
| `input` | `{path, sha256}` for the exact task input |
| `result` | `{path, sha256}` for the raw JSON result; null only for a failed, abandoned, or infrastructure-error attempt |
| `capture_complete` | True only when the trusted collector captured the declared scratch scope completely |
| `scratch_dir` | Existing directory holding this attempt's authored scratch snapshot |
| `scratch` | Complete `{path, sha256}` inventory relative to `scratch_dir`; an empty list is valid only for an existing empty directory |

Input/result paths and scratch directories are relative to the manifest's directory. All paths must be relative normal components: no absolute paths, `..`, symlinks, or reused/overlapping attempt scratch roots. Scratch inventories include every regular file. Only `.py` and `.sh` files contribute to the existing wrapper classifier. Missing files, unlisted files, mismatched hashes, reused results, and invalid JSON make the evidence insufficient.

A successful process is not automatically a completed task. The evaluator parses the actual result artifact and requires exact JSON equality with the preregistered oracle. Wrong but valid JSON is a task failure. A missing or unreadable result on a purportedly completed run is missing evidence. Explicit failed or abandoned attempts remain in the planned denominator. Infrastructure failures make the comparison insufficient.

## Measures and verdicts

The report pins the compiled evaluator and classifier source bytes with SHA-256 hashes. For each task and arm, the report shows attempts, completed tasks, completion rate, and wrapper artifacts. It also reports wrappers per attempt and the share of attempts containing wrappers. Duplicate files count separately: both are authored artifacts. The study must use the same capture policy across arms.

`improved` requires fewer total wrappers and no per-task loss of successful completions. Every candidate attempt must complete correctly. The baseline must contain a successful attempt and at least one wrapper. An increased per-task wrapper count or a completion loss is `regressed`. Equal wrapper counts with the completion floor met are `unchanged`. Missing evidence, an unmet completion floor, no successful baseline, or no baseline wrapper signal is `insufficient`. Calibration always has outer `verdict: calibration_only`. Its inner comparison can still be improved.

This is an observed comparison over the supplied task set, not statistical proof of causality or a business-value estimate. It does not infer time or money saved. Report task mix, failed runs, capture coverage, and the number of repetitions alongside any claimed effect.

## Scope and limits

The collector is a trust boundary, not an agent-written self-certification. Hash checks detect changed or omitted files within a frozen packet. They cannot prove the collector recorded the true runtime, every action, or every scratch write. Inline shell wrappers, other languages, deleted artifacts, and writes outside the declared capture scope are not measured. If those paths matter, classify the capture as incomplete rather than calling it zero. A baseline and candidate with different capture rules are not comparable.

No live baseline/candidate remedy comparison ships with this eval. The synthetic controls establish that the grader accepts a valid improvement and refuses misleading results. The existing regression bank entry tests that instrument without spawning a model, granting authority, filing nodes, or publishing effects.
