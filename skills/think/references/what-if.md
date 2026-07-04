
# What-If

Stress-test an idea before committing to implementation. This skill adapts autoresearch's scenario exploration into an footnote-native workflow.

## Invoked from /think (deep-dive mode)

`/think` runs a compact inline failure-mode pass in its Step 6b for every
design doc. When the feature is large or risky enough to exceed that inline
budget, `/think` hands off to `/think what-if` instead of trying to enumerate every
angle on its own.

**Trigger conditions that cause `/think` to recommend `/think what-if`:**

- >=3 external dependencies (APIs, webhooks, third-party SDKs)
- Touches auth, payments, or money movement
- Has concurrent / distributed / queued state (workers, cron, locks)
- Multi-tenant or permission-sensitive surface

**Hand-off contract (one-line prompt emitted by `/think`):**

```
Run `/think what-if <domain> <depth> failure-modes "<scope>"` to stress-test: <categories>
```

Where:
- `<domain>`: the `/think what-if` domain (`software`, `product`, `business`, `security`)
- `<depth>`: typically `standard`; `deep` for high-risk features
- `<scope>`: one-sentence scope the user will stress-test
- `<categories>`: the dimensions `/think` wants explored (e.g. `error_path, concurrent, recovery`)

**Output contract back to `/think`:**

When `/think what-if` runs in service of a subsequent `/think` pass, its
`what-ifs.md` file includes a top-level `## Failure Modes` section using the
same sub-bullet vocabulary `/think` requires (Boundaries, Errors, Invariants,
Concurrency). `/think` folds these findings into its own Step 6b without
duplicating items already covered.

**Standalone mode is unchanged.** `/think what-if` still works on its own: no
design doc required, no mandatory Failure Modes summary. The `## Failure
Modes` block is only added when the user passes the `failure-modes`
positional modifier (see Argument Parsing below). This keeps `/think what-if`
usable for non-design exploration (incident postmortems, red-team
exercises, roadmap pre-mortem) while giving `/think` a mechanical way to
request the block when it is handing off.

## Reference Materials

Load these references as needed:

- [iteration-loop.md](iteration-loop.md)
- [verification-patterns.md](verification-patterns.md)

## Defaults

- Default depth: `standard`
- Default bounded run: `Iterations: 15`
- Default output root: `.fno/what-if/{YYYYMMDDHHMM}-{slug}/` (keeps output out of the main repo tree; `.fno/` is the conventional ephemeral workspace)

## Argument Parsing

**Positional modifiers are keyword-aware, not strictly position-bound.**
Scan the argument list token-by-token and classify each token by its
literal value rather than its index. A token that does not match any
known keyword or `Iterations:` pattern falls through to the scenario seed.
Order of preceding tokens is irrelevant:

1. **Domain** (token in `{software, product, business, security}`)
2. **Depth** (token in `{shallow, standard, deep}`)
3. **`from-scenarios`** (literal token): read from prior scenarios
4. **`failure-modes`** (literal token): emit a top-level `## Failure Modes`
   section in `what-ifs.md` using the Boundaries / Errors / Invariants /
   Concurrency sub-section vocabulary that `/think` consumes. Off by
   default; `/think` includes this literal in the hand-off line it
   generates.
5. **`Iterations: N`** (literal prefix `Iterations:` followed by integer)
6. **Remaining tokens** concatenate into the scenario seed

Because classification is keyword-based, any of these invocations are
equivalent and all enable failure-mode output:

```
/think what-if software standard failure-modes "checkout flow"
/think what-if failure-modes software standard "checkout flow"
/think what-if standard failure-modes "checkout flow"
```

A quoted scenario string (standard shell quoting) is treated as one
token so modifier keywords inside the quoted text are NOT interpreted.

If the scenario is clear and either domain or depth is provided, skip setup and proceed directly to seed analysis.

## Interactive Setup

If the user gives no scenario or the intent is too vague, gather context in one batched AskUserQuestion call:

1. scenario description
2. domain
3. depth
4. output format

Do not ask one question at a time.

## Process

### 1. Seed

Parse the scenario into:

- actors
- goals
- components
- preconditions
- expected outcomes

### 2. Decompose

Map the scenario into these 12 dimensions:

1. happy_path
2. error_path
3. edge_case
4. abuse_misuse
5. scale
6. concurrent
7. temporal
8. data_variation
9. permission
10. integration
11. recovery
12. state_transition

Domain priorities:

- software: error_path, edge_case, concurrent, integration
- product: happy_path, error_path, permission, state_transition
- business: state_transition, permission, recovery, integration
- security: abuse_misuse, permission, integration, recovery
- footnote-specific: scale, concurrent, recovery, state_transition

### 3. Iterate

Load `iteration-loop.md` and run one situation per iteration:

1. pick the highest-priority unexplored dimension or combination
2. generate exactly one situation
3. classify it as `new`, `variant`, or `duplicate`
4. expand it with boundary, interruption, ordering, and missing-data checks
5. log it to `what-ifs-results.md`

### 4. Classify

Keep:

- `new`
- `variant`

Discard:

- `duplicate`
- out-of-scope situations
- unrealistic low-value scenarios

### 5. Output

Write exactly two files:

- `.fno/what-if/{YYYYMMDDHHMM}-{slug}/what-ifs.md` - the narrative: seed recap, every kept situation grouped by dimension, then a summary section covering severity breakdown, coverage gaps, and actionable findings
- `.fno/what-if/{YYYYMMDDHHMM}-{slug}/what-ifs-results.md` - a compact markdown table logging every iteration with columns: `iteration | arc | dimension | title | severity | classification`

Create the directory with `mkdir -p .fno/what-if/{YYYYMMDDHHMM}-{slug}` before writing. If `.fno/` is not already gitignored in the host project, mention it at the end of the run so the user can add it.

`what-ifs.md` must include:

- scenarios grouped by dimension
- severity breakdown
- coverage gaps
- actionable findings

## Situation Format

```markdown
### [DIMENSION] Situation: [title]
- Actors: [who]
- Preconditions: [what must already be true]
- Trigger: [action]
- Expected outcome: [verifiable result]
- What could go wrong: [risk]
- Severity: [Critical/High/Medium/Low]
```

## Roadmap Integration

At the end of the run, suggest:

`Run /do-roadmap from-scenarios to convert high-severity findings into implementation tasks.`

Priority mapping:

- critical -> high
- high -> medium
- medium -> low

## Rules

- One situation per iteration
- Bounded mode stops exactly at `Iterations: N`
- Use mechanical classification for keep/discard decisions
- Do not bundle multiple scenarios into one iteration
