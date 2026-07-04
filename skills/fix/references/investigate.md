
# Debug

BDD-first debugging with a scientific-method loop. Define what fixed means, prove the bug exists, then investigate with one hypothesis per iteration.

## Core Principle

No investigation without acceptance criteria and a failing test.

## Reference Materials

Load as needed:

- [iteration-loop.md](iteration-loop.md)
- [verification-patterns.md](verification-patterns.md)
- `track-attempt` skill when repeated approaches start failing

## Mandatory Setup

Before deep investigation:

1. Define acceptance criteria
   - Ask: "What does fixed look like?"
   - Write Given/When/Then criteria in `.fno/debug/YYYY-MM-DD-bug-name.md`
2. Write a failing test or equivalent reproduction
   - The bug must be observable before investigation proceeds

## Interactive Setup

If the user did not give enough detail, gather context in one batched AskUserQuestion call:

1. issue type
2. scope
3. depth
4. auto-fix preference

Pre-scan the codebase first when possible so the choices can include existing failures.
All questions must be asked in a single call.

## Process

### 1. Gather

Collect:

- symptoms
- expected vs actual behavior
- reproduction steps
- environment or recent change context

If no symptom is provided, run available tests, lint, typecheck, or build commands to gather signals.

### 2. Reconnaissance

Map the error surface:

- affected files
- entry points
- call chain
- external dependencies
- recent git history in the area

### 3. Hypothesize

Form one falsifiable hypothesis at a time.

Good hypotheses are:

- specific
- testable
- falsifiable
- prioritized by the strongest evidence so far

Cognitive bias guards:

- confirmation bias: actively seek disconfirming evidence
- anchoring: do not overcommit to the first clue
- sunk cost: abandon a weak line after repeated failures
- availability bias: familiar patterns are not proof

### 4. Test

Run one experiment per iteration. Preferred techniques:

- direct inspection
- trace execution
- minimal reproduction
- binary search
- differential debugging
- pattern search
- working backwards
- rubber duck explanation

### 5. Classify

Record one of:

- confirmed
- disproven
- inconclusive
- new lead

Bug findings must use this format:

```markdown
### [SEVERITY] Bug: [title]
- **Location:** `file:line`
- **Hypothesis:** [what was suspected]
- **Evidence:** [code and experiment result]
- **Root cause:** [why it happens]
- **Suggested fix:** [concrete change]
```

### 6. Log

Append every iteration to `.fno/debug/debug-results.tsv`, including disproven hypotheses. Failed hypotheses still inform the next step.

### 7. Repeat

Use the iteration loop protocol with one hypothesis and one experiment per iteration. Print progress every 5 iterations in bounded mode.

Progress format:

```text
=== Debug Progress (iteration 10/15) ===
Bugs found: 3
Hypotheses tested: 8
Disproven: 4
Coverage gaps: <files or areas still uninvestigated>
```

## `fix` Chain

If `fix` modifier is passed:

1. complete the debug loop
2. append findings to the bug's own file (`.fno/debug/YYYY-MM-DD-bug-name.md`) under a `## Findings` section
3. If a fix skill is installed, invoke it with the debug context: `/fix from-debug`
   If no fix skill is available, present the findings to the user with suggested fixes.

The fix loop (if available) should consume confirmed findings in severity order.

## Tournament Escalation

Preserve tournament mode for stuck bugs:

- if 3 strong hypotheses on the same bug fail
- or 30 minutes of investigation yields no root cause

At that point, escalate to the existing tournament-debugger agent flow instead of grinding on the same line of inquiry.

## State Files

```text
.fno/debug/
├── STATE.md
├── attempts.jsonl
├── debug-results.tsv
├── YYYY-MM-DD-*.md       # active bug reports, flat at root
├── resolved/             # archived bug files (same name, moved here when closed)
├── tournament/           # tournament-debugger state for stuck bugs
└── logs/                 # execution logs
```

All debug artifacts live under `.fno/debug/` so they share the `.fno/` gitignore coverage and stay out of the repo root.

## Rules

- Acceptance criteria and failing reproduction come first
- One hypothesis per iteration
- One experiment per iteration
- Evidence beats intuition
- Disproven hypotheses are valuable and must be logged
- Bounded mode stops exactly at `Iterations: N`

## Composite Metric

For bounded runs:

```text
debug_score = bugs_found * 15
            + hypotheses_tested * 3
            + coverage_ratio * 40
            + techniques_used * 2
```
