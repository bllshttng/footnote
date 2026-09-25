# Review lenses for the target run

The code lenses of blueprint drafting moved to the review phase under lean dispatch: the plan is no longer the upstream quality gate. The canonical files live under `skills/blueprint/references/lenses/code/`. `skill-bundles.yaml` copies them into this skill. Read nothing up front. When a row's condition holds for the diff under review, read that one lens file and apply its question to the built code. Each lens names its source. `NOTICE` at the repo root carries the license terms.

Each lens file carries a `Feeds:` line. It names the plan section the lens fed during its drafting era. Treat it as metadata from that era. The review phase reads the code, not a plan.

| Read when | Lens |
|---|---|
| The change adds an action a person takes | [agent_user](lenses/agent_user.md) |
| A changed file has callers outside the diff, or a CI gate binds it | [blast_radius](lenses/blast_radius.md) |
| The change adds a module or crosses a crate or package boundary | [boundaries](lenses/boundaries.md) |
| The change touches a CLI flag, config key, schema, shipped file name, or a skill or agent name | [door_type](lenses/door_type.md) |
| The node has children, has `scope: epic`, or its parent is an epic | [epic_outcome](lenses/epic_outcome.md) |
| `difficulty: high`, or three or more tasks | [inversion](lenses/inversion.md) |
| Verification checks only that code runs, not that the need is met | [outcome](lenses/outcome.md) |
| The diff carries more than one independent change | [story_split](lenses/story_split.md) |
| For each observable behavior change, happy path, error path, edge case | [test_scenarios](lenses/test_scenarios.md) |
| The plan creates or reorders child nodes | [unlock_order](lenses/unlock_order.md) |

After the review round, write one Context line naming each lens you applied and the condition that pulled it in, or `Lenses: none fired`.
