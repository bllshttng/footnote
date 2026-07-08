# Single-Doc Plan Detection and Dispatch

Reference for how `/do waves` reads single-doc plans. Architectural overview: `docs/architecture/lean-blueprint.md`.

## `locate_plan(input)` API

`fno.plan._locate.locate_plan(input: str) -> ResolvedPlan` resolves the user's argument to a typed plan descriptor.

```python
@dataclass
class ResolvedPlan:
    kind: Literal["folder", "single"]
    root_path: Path       # directory (folder) or parent dir (single)
    index_path: Path      # 00-INDEX.md (folder) or the .md file itself (single)
```

Detection priority:

1. If `input` resolves to a directory containing `00-INDEX.md`: `kind="folder"`, `root_path=input`, `index_path=<dir>/00-INDEX.md`.
2. If `input` resolves to a `.md` file: `kind="single"`, `root_path=<file>.parent`, `index_path=<file>`.
3. Otherwise: raises `PlanNotFound` with the input and a suggestion to check the path.

`load_plan_strategy(input: str)` is the higher-level dispatcher. It calls `locate_plan`, then routes to the appropriate reader:

- `kind="folder"`: reads `00-INDEX.md` for frontmatter and wave/task structure (existing behavior, unchanged).
- `kind="single"`: calls `fno.plan._doc.load_plan(index_path)` to parse frontmatter and sections, then extracts the `## Execution Strategy` fenced YAML block to build the wave/task structure.

## Folder deprecation warning

When `load_plan_strategy` resolves a folder plan, it emits a deprecation warning to stderr:

```
folder plan format deprecated; run `fno plan migrate-folder` to convert
```

Behavior is otherwise unchanged. The warning is active starting in PR2. In PR1, folder plans load silently with no warning.

## Failure surfaces

| Condition | Exception / exit | Detail |
|-----------|-----------------|--------|
| Input path does not exist | `PlanNotFound` | Includes input string and path-check suggestion |
| `00-INDEX.md` missing from directory | `PlanNotFound` | Folder exists but is not a valid folder plan |
| `.md` file unreadable (permissions) | `PlanNotFound` | OS error wrapped; original error in `__cause__` |
| Frontmatter YAML invalid | `MalformedPlan` (exit 3) | Line number and offending field in stderr |
| `## Execution Strategy` section absent | `MalformedPlan` (exit 2) | Names the missing section; suggests re-running `/blueprint` |
| Execution Strategy YAML block invalid | `MalformedPlan` (exit 3) | Line number within the fenced block |

`PlanNotFound` and `MalformedPlan` both carry a `hint` field that the operator prints to stderr before propagating the error to the caller.

## See also

- `cli/src/fno/plan/_locate.py` - `locate_plan`, `ResolvedPlan`, `load_plan_strategy`
- `cli/src/fno/plan/_doc.py` - `load_plan`, `PlanDoc`
- `skills/do/orchestrator.py` - caller site for `load_plan_strategy`
- `skills/blueprint/references/single-doc-spec.md` - mutation contract and section ownership
