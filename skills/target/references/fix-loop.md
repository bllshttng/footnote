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
3. name the root cause (below) before you edit
4. make one focused fix at the root cause
5. `git commit` before verify
6. re-run detection to compute `delta`
7. if a guard command is set, run it
8. keep, revert, or rework
9. log to `fix-results.tsv`, with the cause and the state verdict from step 3
10. emit ONE `builder_step` crumb (below)

Maximum rework attempts per item: 2. After that, add it to the blocked list and continue.

#### Name the root cause before the fix

The error line is where the bad state was noticed, not where it was made. Trace the bad value back one hop at a time until you reach the code that produced it. Write that site down before you edit. For a state the code made, the line is `cause: <file:line> <what it does wrong>`. For a state the world hands in, no code made it. Then the line names where it enters: `cause: <file:line> takes <external source> unhandled`.

Then sort the bad state into one of two kinds. Ask whether the system must expect this state from outside its own code.

- **legitimate**: the world can hand the code this state. Examples: user input, a network failure, a missing optional config, a race the design allows. Here a defensive check is the fix. Kinds: early return, null check, skip, retry, raised timeout, fallback value, catch. Put the check where the state enters, not at the place that crashed.
- **illegitimate**: the code made this state itself, and no correct path makes it. Examples: a writer stored a bad row. A reaped path was read as truth. A cache served a stale answer as fact. Fix the producer. A defensive check at the consumer hides the defect and lets the next reader hit it. Treat it like a suppression: discard it and re-queue the item.

When the state a defensive check handles is legitimate, the check is a fix. Otherwise it is not. This check lives in the code. It is not the `--guard` regression command.

When the producer is out of reach (another repo, or out of scope), block the item and name the producer. Never keep a consumer-side check as the repair.

Then look for the pattern, not the instance. Grep for every other site with the same fault. Fix each one in this loop, or block each one by name. A class fixed at one site comes back from the next one.

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
- for each kept fix, its `cause:` line and its state verdict (`legitimate` or `illegitimate`)
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
- a defensive check over a state that no correct path makes (see "Name the root cause before the fix")

### Composite Metric

```text
fix_score = reduction * 0.60 + guard_health * 0.25 + quality * 0.15
```

The score ranks candidate keeps when several fixes touch the same area; the decision rules above (delta and guard) stay the keep/revert authority. The per-class delta is trusted where the category has a mechanical counter (build, type, lint, test); for `bug` classes the delta is evidence, and the guard plus reproduction decide.
