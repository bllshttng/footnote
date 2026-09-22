# Product lenses for blueprint drafting

Each drafting lens is one file under `lenses/draft/`, a move you make while you write the plan. Nothing here is read up front. When a row's condition holds for the node in hand, read that one lens file. Each lens names its source, and `NOTICE` at the repo root carries the license terms.

| Read when | Lens |
|---|---|
| The node names no user, or no non-goal | [problem](lenses/draft/problem.md) |
| The plan has more than one change | [story_split](lenses/draft/story_split.md) |
| The plan changes behavior a person or agent can observe | [test_scenarios](lenses/draft/test_scenarios.md) |
| Verification checks only that code runs, not that the need is met | [outcome](lenses/draft/outcome.md) |
| The node claims something is missing, broken or already added | [intent_vs_main](lenses/draft/intent_vs_main.md) |
| The plan deletes, replaces or retires something | [fence](lenses/draft/fence.md) |
| A changed file has callers outside the plan, or a CI gate binds it | [blast_radius](lenses/draft/blast_radius.md) |
| The change touches a CLI flag, config key, schema, shipped file name, or a skill or agent name | [door_type](lenses/draft/door_type.md) |
| `difficulty: high`, or three or more tasks | [inversion](lenses/draft/inversion.md) |
| The change adds a module or crosses a crate or package boundary | [boundaries](lenses/draft/boundaries.md) |
| The change adds an action a person takes | [agent_user](lenses/draft/agent_user.md) |
| The node has children, has `scope: epic`, or its parent is an epic | [epic_outcome](lenses/draft/epic_outcome.md) |
| The plan creates or reorders child nodes (`group N`, `scope: epic`) | [unlock_order](lenses/draft/unlock_order.md) |

After drafting, write one Context line naming each lens file you read and the condition that pulled it in, or `Lenses: none fired`.

## The grading lenses are not yours

The grading lenses belong to the blueprint judge, which reads them at grading time. This skill never links them.

Never search for, load or read them while drafting: a grader read during drafting turns into a checklist the plan writes to.
