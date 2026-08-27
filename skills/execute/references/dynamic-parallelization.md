# Dynamic Parallelization

Automatic optimization that upgrades sequential waves to parallel, with
`collision.partition` serializing any tasks whose file sets overlap. Activated
when the plan contains a `## File Ownership Map` section.

## Activation

Automatic when the plan contains `## File Ownership Map`.
Skip if no file ownership map found - use declared execution strategy as-is.

## Parsing the File Ownership Map

Locate the markdown table under `## File Ownership Map`:

| File | Phase | Action |
|------|-------|--------|
| `src/auth.ts` | 1.1 | Modify |
| `src/billing.ts` | 1.2 | Create |

Build mapping: task_id -> set of file paths

**Phase column formats:**
- Single: `1.1`
- Comma-separated: `1.1, 2.2` (split and assign each)

## The Partition Rule

`collision.partition` (cli/src/fno/graph/collision.py) groups a wave's tasks
by shared normalized path; two tasks writing under one hidden shared output
root (`.fno/`, `docs/`, ...) land in one group too. A task with no parseable
file list is `unevaluated` - its own verdict, never a silent pass.

A wave's tasks run as soon as their blockers are complete, whether those
blockers are declared in `blocked_by` or derived from file overlap:

- Tasks in disjoint groups dispatch concurrently - the wave stays parallel.
- A group of overlapping tasks runs in id order: each task's derived edge
  names the group mate before it.
- An unevaluated task runs last: it waits for every evaluated task in the
  wave.

The overlap never downgrades the wave. The `--ready` query unions the
derived edges with the declared ones, so `/execute waves` dispatches the
ready set concurrently while the edges hold the overlapping tasks back.

## Rules

1. **Only upgrade** sequential to parallel, never downgrade parallel to sequential
2. **Tasks missing from map** are `unevaluated`: they run after every evaluated task in the wave
3. **Log every decision** for debuggability
4. **File ownership map is the ONLY input** - never infer from task descriptions
5. **Already-parallel waves** are left as-is (no action needed)

## Edge Cases

### Per-task readiness partitions file overlap
Per-task readiness controls dependency availability, and derived edges carry
file overlap into that same query. If A, B, and C share files, A and B may
still dispatch while C waits on its group mate - partial parallelization
within one wave is the supported path.

### Malformed or missing map
If the ownership map is present but malformed, log a warning and use the declared strategy.

## Extended Decision Tree

```
Is wave declared sequential?
+-- NO (parallel) -> Leave as-is
+-- YES -> Does the plan have a File Ownership Map?
         +-- NO -> Keep sequential (declared strategy)
         +-- YES -> Do any two tasks share a file or a hidden output root?
                  +-- NO -> Upgrade to parallel
                  +-- YES -> Upgrade to parallel; partition edges
                             serialize the overlapping tasks
```
