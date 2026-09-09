# Lane qualification: tuning an operator's model lanes with reproducible evals

The operator runs across several models, harnesses and effort levels. This page is the recipe for qualifying one of them with the existing eval bank, not a new benchmark service. Full mechanics of the lane machinery live in [docs/evals.md](../evals.md#lanes). This page is the operator workflow that sits on top of it.

## The paired fixtures

Three bank tasks isolate one axis each, so a lane's weakness shows up on the right axis instead of one blended score:

- `evals/bank/capability-lane-blueprint.yaml` - an ambiguous scope decision. It grades whether the worker names the correct in-scope file and rules the wrong one out.
- `evals/bank/capability-lane-implementation.yaml` - an unambiguous implementation. It grades observed functional behavior only, with no scope judgment involved.
- `evals/bank/capability-lane-review.yaml` - a seeded review defect. It grades a finding tied to the specific symbol and the specific wrong return value, never a generic "found a bug" match.

Run all three against the same fixture revision and the same repeat count. A lane that wins only on implementation, and loses on scope decisions, is a different qualification result than one that wins across all three. The split is the point.

## Running a paired trial

Run the baseline lane first, at the existing effort level. Vary one factor at a time. Swap only the model, or only the effort, or only the harness. `--task` takes one id per invocation. Run all three:

```bash
for t in capability-lane-blueprint capability-lane-implementation capability-lane-review; do
  fno doctor evals run --task "$t" --lane current-baseline-name --cohort current-baseline --repeat 5
done
```

Then run the trial lane against the identical fixture revision and repeat count:

```bash
for t in capability-lane-blueprint capability-lane-implementation capability-lane-review; do
  fno doctor evals run --task "$t" --lane astra-high --cohort astra-trial --repeat 5
done
```

Each run appends one history row per task-run to `~/.fno/evals-history.jsonl`, carrying `experiment_id` (the `--cohort` id), `requested_lane`, and the `lane_status`/observed fields ([docs/evals.md](../evals.md#lanes)).

## Reading the result

There is no cohort-comparison command yet. Read `~/.fno/evals-history.jsonl` directly (`--experiment_id` filters to one cohort) and compare pass rates by hand across the two cohort ids. Folding this into one `fno doctor evals report` view, with a promotion recommendation, is deferred follow-up work.

A `lane_status` of `substituted` on any row means capacity served a different harness than requested. Exclude that row before comparing lanes. It is not a real sample of the lane you meant to qualify.

A `lane_status` of `unavailable` means the account cannot reach the lane at all. Read this as `unavailable`. Never read it as a score of zero on the requested model.

## One whole-delivery comparison before splitting roles

Run one comparison of a lane doing the full delivery (plan, implement, review) against the current baseline before recommending a planner/implementer split for that lane. A lane can be weak at blueprint's scope judgment (task 1) and still be a fine implementer (task 2) under a stronger planner. Splitting on a single blended score hides that difference. The three paired tasks exist precisely so this comparison does not require guessing which axis moved.

## Budget and access

A live trial runs only with an explicit configured budget and an account that can actually reach the lane. No access, no budget, or a lane the account cannot run reports `unavailable`. It never reports a synthetic pass standing in for a trial that never ran.

## What this does not do

- It never edits `config.routing.models`, `agents.profiles`, or any production lane order. Applying a qualification result is a separate, reviewed config change.
- It never claims one model is universally superior. A result is scoped to the fixtures and the cohort it was measured against.
- It adds no new eval scheduler, benchmark service, or model/effort enum. The lane vocabulary is `config.routing.models`, the same rows `agents.profiles.*.lanes` already reference.
- It does not yet fold cohort rows into one comparison report or a promotion recommendation. That is future work, not part of this delivery.
