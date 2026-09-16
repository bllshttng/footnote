---
name: pm-plan-review
description: "Grading lenses the blueprint judge reads to argue against a finished plan. Not for drafting."
disable-model-invocation: true
pack: fno-pm
---

Grading lenses for the blueprint judge. The judge reads `lenses/`: one file per dimension plus `preamble.md`. A source lens argues against the plan from one outside source and must quote that source, verbatim, to fail the plan.

Apply the lenses by hand with a forced run: `fno doctor observer judge --plan <plan path> --node <node id> --force`.

The planner never loads these files. A grader read during drafting turns into a checklist the author writes to. The drafting lenses are the `fno:pm-node` and `fno:pm-epic` skills of this pack.

## Sources

- `lenses/epic_fit.md`, `lenses/mission_fit.md`, `lenses/customer_fit.md` and `lenses/code_truth.md` distill MIT-licensed pm-skills material. Each lens names its source. `NOTICE` at the repo root carries the license terms.

## Known Limitations and Deferred Work

- The lenses are advisory and uncalibrated. A reader sees only its own prompt. Reader cost is untracked. The pack ships in every install with no opt-out. The long form lives in [LIMITATIONS.md](LIMITATIONS.md).
