# Product lenses for blueprint drafting

The lens text lives in the fno-pm pack skill `pm-plan-draft`. Nothing here is read up front: read a lens file only when its row's condition holds for the node in hand.

| Read when | Lens |
|---|---|
| The node names no user, or no non-goal | [problem](../../pm-plan-draft/lenses/problem.md) |
| The plan has more than one change | [story_split](../../pm-plan-draft/lenses/story_split.md) |
| The plan changes behavior a person or agent can observe | [test_scenarios](../../pm-plan-draft/lenses/test_scenarios.md) |
| Verification checks only that code runs, not that the need is met | [outcome](../../pm-plan-draft/lenses/outcome.md) |
| The node claims something is missing, broken or already added | [intent_vs_main](../../pm-plan-draft/lenses/intent_vs_main.md) |
| The plan deletes, replaces or retires something | [fence](../../pm-plan-draft/lenses/fence.md) |
| A changed file has callers outside the plan, or a CI gate binds it | [blast_radius](../../pm-plan-draft/lenses/blast_radius.md) |
| The change touches a CLI flag, config key, schema, shipped file name, or a skill or agent name | [door_type](../../pm-plan-draft/lenses/door_type.md) |
| `difficulty: high`, or three or more tasks | [inversion](../../pm-plan-draft/lenses/inversion.md) |
| The change adds a module or crosses a crate or package boundary | [boundaries](../../pm-plan-draft/lenses/boundaries.md) |
| The change adds an action a person takes | [agent_user](../../pm-plan-draft/lenses/agent_user.md) |
| The node has children, has `scope: epic`, or its parent is an epic | [epic_outcome](../../pm-plan-draft/lenses/epic_outcome.md) |
| The plan creates or reorders child nodes (`group N`, `scope: epic`) | [unlock_order](../../pm-plan-draft/lenses/unlock_order.md) |

After drafting, write one Context line naming each lens file you read and the condition that pulled it in, or `Lenses: none fired`.

## The grading lenses are not yours

The grading lenses live in `skills/pm-plan-review/lenses/` (pack source: `plugins/fno-pm/skills/pm-plan-review/lenses/`). They belong to the blueprint judge, which reads them at grading time.

Never load or read them while drafting: a grader read during drafting turns into a checklist the plan writes to.
