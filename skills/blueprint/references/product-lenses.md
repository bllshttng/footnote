# Product lenses for blueprint drafting

The drafting lenses live in the fno-pm pack: `fno:pm-node` for node plans and `fno:pm-epic` for epic plans.

At step 2a-bis, load `fno:pm-node` with the Skill tool for every node. When the node has children, carries `scope: epic`, or has an epic for a parent, also load `fno:pm-epic`.

## The grading lenses are not yours

The grading lenses live in `skills/pm-plan-review/lenses/` (pack source: `plugins/fno-pm/skills/pm-plan-review/lenses/`). They belong to the blueprint judge, which reads them at grading time.

Never load or read them while drafting: a grader read during drafting turns into a checklist the plan writes to.
