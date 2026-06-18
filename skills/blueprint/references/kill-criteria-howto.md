# Kill Criteria: How-To

Plan-declared abort conditions. When a predicate fires, target/do
emit `<aborted reason="{name}">`, the stop hook exits clean, and the ledger
records `status: aborted` with the reason. Use this to short-circuit burn
loops when the plan itself is the problem.

See `skills/target/references/kill-criteria.md` for the architecture and
predicate vocabulary.

## When Defaults Are Enough

The defaults `/blueprint` writes catch the two most common burn-loop shapes:

```yaml
kill_criteria:
  - name: iteration_ceiling
    predicate: iteration > 15
    reason: "Too many iterations - planning likely wrong"
  - name: stuck_test
    predicate: same_test_failing_for >= 3
    reason: "Same test failing 3+ iterations - root cause unclear"
```

Ship as-is when the plan fits in ~15 iterations, tests drive feedback,
and the file surface is modest. Customize when the plan is complex
enough to justify a higher ceiling, narrow enough to catch scope creep,
or test-heavy enough that a deleted test file should fail loud.

## Declaring in Full Mode

Put the block in `00-INDEX.md` frontmatter, before any stamp fields
(`status:`, `shipped_at:`, `urls:`, `session_ids:`). The `/blueprint`
template already does this:

```yaml
---
created: 2026-04-23T08:00
kill_criteria:
  - name: iteration_ceiling
    predicate: iteration > 15
    reason: "Too many iterations - planning likely wrong"
  - name: stuck_test
    predicate: same_test_failing_for >= 3
    reason: "Same test failing 3+ iterations - root cause unclear"
---
```

## Declaring in Quick Mode

Quick-mode plans are single files. Put a `## Kill Criteria` heading
right after the title with a fenced YAML block containing the same
`kill_criteria:` list shown above. Omit the section entirely to inherit
engine defaults - the evaluator returns exit 0 when the block is absent.

## Example: Adding Scope-Creep Guard

Narrow-scope plans (single-file refactor, targeted bug fix) benefit from
catching drift early. Add `files_outside(plan_path)` to abort when the
session touches more than N files outside the plan folder:

```yaml
kill_criteria:
  - name: iteration_ceiling
    predicate: iteration > 15
    reason: "Too many iterations - planning likely wrong"
  - name: stuck_test
    predicate: same_test_failing_for >= 3
    reason: "Same test failing 3+ iterations - root cause unclear"
  - name: scope_creep
    predicate: files_outside(plan_path) > 5
    reason: "Touching too many files outside the declared scope"
```

The evaluator counts files from `git diff --name-only` (committed +
staged + unstaged) that are not prefixed by the plan folder. Pick N
based on the plan's real surface: 3-5 for a single-file fix, 10-15 for
a feature touching a module, much higher if the plan is cross-cutting.

## Example: Lifting Iteration Ceiling

Plans with heavy test suites, integration waves, or a long verify cycle
can legitimately exceed 15 iterations. Raise the ceiling to match:

```yaml
kill_criteria:
  - name: iteration_ceiling
    predicate: iteration > 25
    reason: "Exceeded planned iteration budget for this complex feature"
  - name: stuck_test
    predicate: same_test_failing_for >= 3
    reason: "Same test failing 3+ iterations - root cause unclear"
```

Rule of thumb: estimate the tasks, add a 2x buffer, round up to the
nearest 5. Going above ~30 usually means the plan should be split.

## Troubleshooting

### Validator warns about an unknown predicate

```
WARN: 00-INDEX.md: kill_criteria entry scope_creep: predicate
`files_outside(src) > 5` not in known vocabulary (engine will log
WARN and skip at runtime)
```

The validator's known vocabulary is `iteration [>=] N`,
`same_test_failing_for [>=] N`, `files_outside(plan_path) > N`, and
`any_test_file_deleted`. Two common causes:

- Typo in the predicate (`files_outside(src)` instead of
  `files_outside(plan_path)`). The argument is literally `plan_path`.
- Using a predicate that was added to the engine after this validator
  shipped. Safe to ignore - the engine logs WARN and skips once per
  iteration, and the rest of the block still evaluates.

### Validator errors

```
ERROR: 00-INDEX.md: kill_criteria entry 2 missing required field `predicate`
```

Every entry needs all three fields: `name`, `predicate`, `reason`. The
validator fails the plan when any is missing. Check for YAML indent
mistakes - the fields must be indented under the list marker:

```yaml
kill_criteria:
  - name: stuck_test
    predicate: same_test_failing_for >= 3
    reason: "Same test failing 3+ iterations - root cause unclear"
```

### Seeing why an abort fired

Check `.fno/.aborted-archive/` for the archived state file:

```bash
ls -lt .fno/.aborted-archive/
# target-state.20260423T081234Z.md  (most recent)

grep -E 'iteration|status|abort_reason' \
  .fno/.aborted-archive/target-state.20260423T081234Z.md
```

The archived file is the exact target-state.md at the moment of abort,
with `status: ABORTED` and `abort_reason:` set. The ledger entry in
`.fno/ledger.json` carries the same reason:

```bash
jq '.entries[] | select(.status == "aborted")' .fno/ledger.json
```

If the reason is `unspecified`, the engine emitted `<aborted>` without
a `reason="..."` attribute - the stop hook logs a WARN for that case
to the hook log. The archived target-state file shows the iteration
value at the moment the predicate fired, which is always one past the
threshold for `>` or equal to it for `>=`.
