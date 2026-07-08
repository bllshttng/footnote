# Single-Doc Plan Detection and Dispatch

Reference for how `/do waves` reads plans. Architectural overview: `docs/architecture/lean-blueprint.md`.

A plan is always a single `.md` file (G1 blocks authoring new folder plans; G3 removed folder-plan *reading* entirely). There is no shape detection step anymore - callers pass a file path directly.

## `load_plan_strategy(plan_input: str)` API

`skills/do/orchestrator.py`'s `load_plan_strategy` is the dispatcher `/do waves` calls. It:

1. Calls `fno.plan._doc.load_plan(Path(plan_input))` to parse frontmatter and sections.
2. Extracts the `## Execution Strategy` section via `doc.get_section("Execution Strategy")`.
3. Delegates the fenced YAML body to `fno.plan.brief.parse_execution_strategy` (the canonical Execution Strategy parser - single source of truth, shared with `fno plan brief`) to get `execution_mode` / `scope` / `projects` / `waves`.
4. Builds and returns an `ExecutionStrategy` dataclass (`waves: List[Wave]`, `scope`, `project_tasks`).

`orchestrator.py` also has a lower-level `parse_execution_strategy(index_path: str)` - a self-contained, stdlib-only regex parser used directly by `main()`'s CLI entry point (`orchestrator.py <path-to-plan.md>`). It reads the same `## Execution Strategy` fenced YAML block from any single file; it does not go through `fno.plan._doc`.

## Failure surfaces (`load_plan_strategy`)

| Condition | Result | Detail |
|-----------|--------|--------|
| `fno.plan._doc` not importable | Returns `None` | Warning to stderr |
| Plan file missing / unreadable (`OSError`) | Returns `None` | `BLOCKED blocked_reason=plan_unreadable: <exc>` to stderr |
| Plan doc malformed (frontmatter parse failure) | Returns `None` | `Error: malformed plan doc (Execution Strategy YAML): <exc>` to stderr |
| `## Execution Strategy` section absent | Returns `None` | `Warning: No execution strategy section found in <path>` to stderr |
| `fno.plan.brief` not importable | Returns `None` | Warning to stderr |
| Execution Strategy YAML malformed (`BriefParseError`) | Returns `None` | `Error: malformed Execution Strategy YAML in <path>: <exc>` to stderr |
| No valid waves parsed | Returns `None` | `Error: No valid waves found in Execution Strategy of <path>` |

Every failure mode returns `None` and prints a diagnostic to stderr - never raises past this function, never returns a partially-built `ExecutionStrategy`.

## Task-scoped file targets

`get_task_file_targets(plan_path, task_id)` extracts a single task's `Files:` section for parallel-wave conflict detection (`detect_hidden_output_conflicts`, `resolve_wave_execution_mode`). Since a plan is one file, this scans that file for the `### Task <task_id>` heading via `_extract_task_section` - no directory globbing.

## See also

- `cli/src/fno/plan/_doc.py` - `load_plan`, `PlanDoc`
- `cli/src/fno/plan/brief.py` - `parse_execution_strategy` (the canonical Execution Strategy YAML parser)
- `skills/do/orchestrator.py` - `load_plan_strategy`, `parse_execution_strategy`, `get_task_file_targets`
- `skills/blueprint/references/single-doc-spec.md` - mutation contract and section ownership
