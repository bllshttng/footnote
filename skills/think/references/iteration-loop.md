# Iteration Loop

Adapted from [autoresearch](https://github.com/arakoodev/autoresearch) and Karpathy's autonomous iteration principles.

## Purpose

Use this protocol when a skill needs bounded, repeatable exploration or repair without inventing a bespoke script. The loop is protocol, not code.

## Core Principles

1. Constraint enables autonomy. Keep scope, metric, and iteration count explicit.
2. Humans set direction. The agent iterates on tactics.
3. Verification must be mechanical. No "looks better" or "seems fixed".
4. Fast verification beats perfect verification inside the loop.
5. Iteration cost shapes behavior. Prefer cheap checks that keep the loop moving.
6. Git is memory. Commit or log every kept change so later iterations can learn.
7. State honest limits. Block or stop when a required command or permission is missing.

## Required Seed

Before iteration 1, define:

- Goal: what outcome the loop is optimizing for
- Scope: files, systems, or dimensions in bounds
- Metric: one mechanical success criterion
- Verify command: extracts pass/fail or a numeric delta
- Guard command: optional regression check that must always pass
- Iterations: exact count for bounded mode, defaulting to the calling skill's standard

## Universal Loop

Each iteration follows the same sequence:

1. Seed
   - Re-state the current target item, dimension, or error.
   - Confirm the verify and guard commands for this iteration.
2. Do ONE thing
   - Make one atomic change, generate one scenario, or test one hypothesis.
   - Do not batch multiple fixes or multiple scenarios into one iteration.
3. Verify
   - Run the mechanical verify command.
   - Compute a delta against the previous baseline or classify the result mechanically.
4. Guard
   - Run the guard command if one exists.
   - Any regression fails the iteration, even if the main metric improved.
5. Decide
   - Keep, discard, or rework using the decision matrix below.
6. Log
   - Append the result to the skill-specific TSV log.
7. Repeat
   - Increment `current_iteration`.
   - Stop exactly at the configured iteration count in bounded mode.

## Decision Matrix

| Delta / Result | Guard | Action |
|----------------|-------|--------|
| Improved (`delta > 0`) or confirmed useful result | pass | Keep |
| Improved (`delta > 0`) | fail | Rework or discard |
| No improvement (`delta = 0`) | any | Discard |
| Worse (`delta < 0`) | any | Discard immediately |
| Verification crashed or returned invalid output | fail | Rework once, then discard |

## Keep / Discard Rules

- Keep only when the verify command shows objective improvement or useful net-new output.
- Discard when the verify result is flat, worse, duplicated, or unverifiable.
- For code changes, revert discarded work with `git revert HEAD --no-edit` if the iteration was committed first.
- Rework is allowed for at most 2 attempts on the same target. After that, mark it blocked and move on.

## Mechanical Verification Rules

Allowed:

- exit code checks
- error-count deltas
- grep/awk extracted numeric metrics
- duplicate/new classification from a logged inventory
- bounded counters such as covered dimensions or bugs confirmed

Forbidden:

- "looks cleaner"
- "probably fixed"
- "seems more robust"
- any subjective aesthetic judgment as the loop decision function

## Progress Reporting

Print progress every 5 iterations:

```text
=== Iteration Progress (iteration 10/15) ===
Kept: 6 | Discarded: 3 | Reworked: 1
Current metric: <value>
Coverage or remaining work: <summary>
Next focus: <target>
```

## Composite Metric Template

Skills may define a weighted score when one number helps summarize progress:

```text
score = primary_outcome * 0.60
      + guard_health * 0.25
      + quality_factor * 0.15
```

Use only mechanical inputs. Do not hide subjective judgment inside the weights.

## Stop Conditions

- Bounded mode: stop exactly at `Iterations: N`
- Unbounded mode: continue until interrupted or until the calling skill's completion rule is met
- Early stop: stop and report `BLOCKED` if verify or guard cannot be run safely
