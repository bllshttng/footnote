# Review lenses for the target run

The code lenses that moved out of blueprint drafting (lean dispatch: the plan is no longer the upstream quality gate) land in the review phase, bundled from their canonical source under `skills/blueprint/references/lenses/code/` via `skill-bundles.yaml`. Nothing here is read up front. When a row's condition holds for the diff under review, read that one lens file and apply its question to the built code. Each lens names its source, and `NOTICE` at the repo root carries the license terms.

The `Feeds:` line inside each lens file names the plan section the lens fed when it lived in blueprint drafting. It is metadata from that era, not an instruction to read a plan; the review phase reads the code.

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
