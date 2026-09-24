# Product lenses for blueprint drafting

Each drafting lens is one file under `lenses/draft/`, a move you make while you write the plan. Nothing here is read up front. When a row's condition holds for the node in hand, read that one lens file. Each lens names its source, and `NOTICE` at the repo root carries the license terms.

These are the premise lenses, and under lean dispatch they are the only ones drafting keeps: a cheap check that the node is still true before a plan is written. The code lenses moved to the target run's review phase: the canonical table is skills/target/references/review-lenses.md, shared through `skill-bundles.yaml`.

| Read when | Lens |
|---|---|
| The node names no user, or no non-goal | [problem](lenses/draft/problem.md) |
| The node claims something is missing, broken or already added | [intent_vs_main](lenses/draft/intent_vs_main.md) |
| The plan deletes, replaces or retires something | [fence](lenses/draft/fence.md) |

After drafting, write one Context line naming each lens file you read and the condition that pulled it in, or `Lenses: none fired`.

## The grading lenses are not yours

The grading lenses belong to the blueprint judge, which reads them at grading time. This skill never links them.

Never search for, load or read them while drafting: a grader read during drafting turns into a checklist the plan writes to.
