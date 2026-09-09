# Lane qualification: tuning an operator's model lanes with reproducible evals

The operator runs across several models, harnesses and effort levels. This page is the recipe for qualifying one of them with the existing eval bank, not a new benchmark service. Full mechanics of the lane and cohort machinery live in [docs/evals.md](../evals.md#lanes-and-cohorts); this page is the operator workflow that sits on top of it.

## The paired fixtures

Three bank tasks isolate one axis each, so a lane's weakness shows up on the right axis instead of one blended score:

- `evals/bank/capability-lane-blueprint.yaml` - an ambiguous scope decision. Grades on whether the worker names the correct in-scope file and rules the wrong one out.
- `evals/bank/capability-lane-implementation.yaml` - an unambiguous implementation. Grades on observed functional behavior only, no scope judgment involved.
- `evals/bank/capability-lane-review.yaml` - a seeded review defect. Grades on a finding tied to the specific symbol and the specific wrong value it returns, never a generic "found a bug" match.

Every cohort comparison runs all three against the same fixture revision, the same declared repeat count, and the same stopping rule. A lane that only wins on the implementation task and loses on scope decisions is a different qualification result than one that wins across all three - the split is the point.

## Running a paired trial

1. Declare the cohorts before the first run, in a small JSON file (schema: [docs/evals.md](../evals.md#lanes-and-cohorts)):

   ```json
   {
     "cohorts": [
       {"id": "current-baseline", "repeats": 5, "fixture_rev": "<HEAD sha>"},
       {"id": "astra-trial", "repeats": 5, "fixture_rev": "<HEAD sha>"}
     ],
     "promotion_criteria": {
       "baseline": "current-baseline", "candidate": "astra-trial",
       "min_pass_at_1": 0.9, "require_review": true
     }
   }
   ```

2. Run the baseline lane first, at the existing effort level. Vary one factor at a time - swap the model, or the effort, or the harness, never more than one per cohort. `--task` takes one id per invocation, so run all three:

   ```bash
   for t in capability-lane-blueprint capability-lane-implementation capability-lane-review; do
     fno doctor evals run --task "$t" --lane current-baseline-name --cohort current-baseline --repeat 5
   done
   ```

3. Run the trial lane against the identical fixture revision and repeat count:

   ```bash
   for t in capability-lane-blueprint capability-lane-implementation capability-lane-review; do
     fno doctor evals run --task "$t" --lane astra-high --cohort astra-trial --repeat 5
   done
   ```

4. Compare:

   ```bash
   fno doctor evals report --cohort-spec cohorts.json --json
   ```

## Reading the result

A `lane_status` of `substituted` on any row means capacity served a different harness than requested. Exclude that cohort from a promotion decision until it reruns clean - it is already excluded from `sample_count` automatically, but a cohort built entirely from substituted runs has zero real samples of the lane you meant to qualify. A `lane_status` of `unavailable` means the account could not run the lane at all; this is `unavailable`, never a score of zero on the requested model.

`review_evidence` and `usage` come from whatever the caller attached to a row, not from the eval runner itself (the runner's only success marker is each task's own mechanical grade). A paired trial that only cares about the mechanical pass rate can ignore both fields entirely - they read `unobserved` / `None` and never masquerade as `observed` / `0`.

## One whole-delivery comparison before splitting roles

Run one comparison of a lane doing the FULL delivery (plan, implement, review) against the current baseline before recommending a planner/implementer split for that lane. A lane that is merely weak at blueprint's scope judgment (task 1) may still be a fine implementer (task 2) under a stronger planner - splitting on a single blended score would hide that. The three paired tasks exist precisely so this comparison does not require guessing which axis moved.

## Budget and access

A live trial runs only with an explicit configured budget and an account that can actually reach the lane. No access, no budget, or a lane the account cannot run reports `unavailable` or an empty cohort - never a synthetic pass standing in for a trial that never ran. `promotion_criteria` refuses to recommend promotion when a cohort has zero scored samples, states the reason, and this authorization covers only the experiment contract and the code in this PR - it is not a standing grant to spend an unbudgeted fleet on live model trials.

## What this does not do

- It never edits `config.routing.models`, `agents.profiles`, or any production lane order. Promotion is a recommendation; applying it is a separate, reviewed config change.
- It never claims one model is universally superior. A recommendation is scoped to the fixtures, the cohort, and the predeclared criteria it was measured against.
- It adds no new eval scheduler, benchmark service, or model/effort enum. The lane vocabulary is `config.routing.models`, the same rows `agents.profiles.*.lanes` already reference.
