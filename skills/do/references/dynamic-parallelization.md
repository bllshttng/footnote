# Dynamic Parallelization

Automatic optimization that upgrades sequential waves to parallel when task
file sets are provably disjoint. Activated when 00-INDEX.md contains a
`## File Ownership Map` section.

## Activation

Automatic when 00-INDEX.md contains `## File Ownership Map`.
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

## Set Intersection Algorithm

```
For each wave marked sequential in execution strategy:
  tasks = wave.tasks
  For each task, look up file set from ownership map
  If any task has NO entry in map:
    VERDICT: keep sequential (unknown scope, conservative)
    Log: "Task X.Y has no file ownership entry, keeping wave N sequential"
    break

  all_disjoint = true
  For each pair (A, B) in tasks:
    overlap = files_A intersection files_B
    if overlap is not empty:
      all_disjoint = false
      Log: "Tasks A and B share files: {overlap}, keeping wave N sequential"
      break

  If all_disjoint:
    Upgrade wave to parallel
    Log: "Wave N upgraded to parallel: all task file sets disjoint"
```

## Rules

1. **Only upgrade** sequential to parallel, never downgrade parallel to sequential
2. **Tasks missing from map** force the wave to stay sequential (conservative)
3. **Log every decision** for debuggability
4. **File ownership map is the ONLY input** - never infer from task descriptions
5. **Already-parallel waves** are left as-is (no action needed)

## Edge Cases

### Partial overlap in a multi-task wave
If wave has tasks A, B, C where A and B are disjoint but C overlaps with A,
the entire wave remains sequential. Partial parallelization within a single
wave is not supported.

### Malformed or missing map
If the file ownership map section exists but the table cannot be parsed
(malformed markdown, missing columns), log a warning and fall back to the
declared execution strategy.

## Extended Decision Tree

```
Is wave declared sequential?
+-- NO (parallel) -> Leave as-is
+-- YES -> Does 00-INDEX.md have a File Ownership Map?
         +-- NO -> Keep sequential (declared strategy)
         +-- YES -> Are all tasks present in the map?
                  +-- NO -> Keep sequential (unknown scope)
                  +-- YES -> Are all task file sets disjoint?
                           +-- NO -> Keep sequential
                           +-- YES -> Upgrade to parallel
```
