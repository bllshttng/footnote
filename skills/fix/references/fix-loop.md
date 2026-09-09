<!-- style-exception: verbatim move of the repair itinerary from the style-exception'd fix/SKILL.md root; category strategies and the metric travel intact -->

# Fix mode body (the repair loop)

Load only when mode `fix` is selected. The SKILL.md root carries the mode/target/guard/completion contract; this file is the itinerary that fulfills it.

### Reference Materials

Load these references as needed:

- [references/iteration-loop.md](iteration-loop.md)
- [references/verification-patterns.md](verification-patterns.md)

### Process

#### 1. Detect

Use `verification-patterns.md` to identify:

- build failures
- critical/high debug findings
- type errors
- test failures
- lint errors
- warnings

If `from-debug` modifier is set, read the latest `debug/*/findings.md` (or `.fno/debug/*.md`) and populate the queue from confirmed bugs first.

**Nothing-to-fix exit (EDGE).** If detection finds zero failures across every category (clean working tree, no failing test, no debug findings), report `nothing to fix` and exit cleanly. Do NOT enter the iteration loop on an empty queue.

#### 2. Prioritize

Fix order:

1. build
2. critical/high bugs
3. type
4. test
5. medium/low bugs
6. lint
7. warnings

#### 3. Iterate

Load `iteration-loop.md` and run this atomic loop:

1. pick the highest-priority unfixed item
2. read the relevant code and error context
3. make one focused fix
4. `git commit` before verify
5. re-run detection to compute `delta`
6. run the guard command if provided
7. keep, revert, or rework
8. log to `fix-results.tsv`
9. emit ONE `builder_step` crumb (below)

Maximum rework attempts per item: 2. After that, add it to the blocked list and continue.

#### Per-iteration crumb (builder trail)

After the keep/revert/rework decision, append one `builder_step` to `.fno/events.jsonl` so a resume or self-handoff successor picks up from the attempt trail instead of repeating a failed approach. One crumb per iteration, at the boundary - never per tool call. Map the loop's own result: kept -> `worked`, reverted -> `failed`, blocked-after-rework -> `abandoned`.

```bash
fno doctor event emit --type builder_step \
  -d '{"node_id":"<current node id>","tried":"<the fix attempted>","found":"<what detection/guard showed>","fix":"<the change made>","outcome":"worked|failed|abandoned"}' \
  || echo "warning: builder_step crumb not recorded (continuing)" >&2
```

Truncate `tried`/`found`/`fix` to ~500 chars each (a crumb is a pointer, not a transcript). `found`/`fix` are optional. `node_id`, `tried`, and `outcome` are required. Degrade, never block: a failed emit prints exactly the one warning above and the loop continues - no retry.

#### 4. Summary

Write:

- `fix/{YYMMDD}-{HHMM}-{slug}/fix-results.tsv`
- `fix/{YYMMDD}-{HHMM}-{slug}/summary.md`

`summary.md` must include:

- baseline error count
- fixed count by category
- remaining errors
- blocked items
- every reverted fix and why it reverted (regression / non-positive delta), so an auto-reverted attempt is reported, not silently dropped
- suggestion to run a code review skill if available (e.g., `/review`), or run the project's test/lint/build commands to verify fixes

### Category Strategies

| Category | Strategy |
|----------|----------|
| build | fix the exact import, syntax, or config break |
| type | proper types, null handling, generics, explicit narrowing |
| test | fix implementation, not the test, unless the test is provably wrong |
| lint | satisfy the rule, do not suppress it |
| bug | apply the concrete fix implied by the debug evidence |

### Anti-Pattern Blocklist

Discard and re-queue any fix that uses:

- `@ts-ignore`
- `eslint-disable`
- `# type: ignore`
- `# noqa`
- `any` used only to silence type errors
- deleted or skipped failing tests
- empty catch blocks
- hardcoded values chosen only to satisfy one test

### Composite Metric

```text
fix_score = reduction * 0.60 + guard_health * 0.25 + quality * 0.15
```

The score ranks candidate keeps when several fixes touch the same area; the decision rules above (delta and guard) stay the keep/revert authority. The per-class delta is trusted where the category has a mechanical counter (build, type, lint, test); for `bug` classes the delta is evidence, and the guard plus reproduction decide.
