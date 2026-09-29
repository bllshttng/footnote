# GLM harness-fit study (x-632f)

Compare how well the fleet's production model (`glm-5.3-flash` through the
z.ai endpoint) is served by the harnesses the fleet actually runs it on. One
attempt = one bank task in a fresh disposable worktree, one lane (harness +
model + effort), one grade. This is an observed comparison, not proof of
causality.

## Arms

**Run 0 (Terminal-Bench 2 through Harbor, external tooling).** Five arms:
claude-code with the fleet's zai env, opencode with a z.ai provider, zcode
(model in its own config; no --model flag), pi with a z.ai provider, and
Terminus 2 as the neutral reference. A one-task smoke per arm proves each CLI
reaches the endpoint before the 89-task pass. An arm whose smoke fails is
recorded `unavailable` with the error; the other arms still run.

**Run 1 (the replay bank, this repo's eval runner).** The same four harness
lanes through `fno doctor evals run --lane <lane> --cohort harness-fit-<lane>
--repeat 3`. A zcode lane the spawn refuses is recorded `unavailable`, never
scored, and zcode is judged on Run 0 only.

## Fixed per comparison

Model string and effort (glm-5.3-flash, high), provider endpoint
(`https://api.z.ai/api/manifest.json` name 'zai', base url
`https://api.z.ai/api/anthropic` for the claude harness), permission mode
(yolo or bypassPermissions on every arm), 45-minute wall budget per attempt,
this machine, harness versions and the footnote plugin sha recorded per run,
fresh disposable worktree per attempt, randomized arm order, 3 repeats, and
the price table in `manifest.json` filled before Run 0.

## Sampling rule (declared before any node is picked)

The 10 replay nodes are every footnote node whose PR merged 2026-09-01 through
2026-09-28, whose PR added at least one NEW test file, and whose tests run in
under 10 minutes, ordered by merge time and taking a systematic sample of 10
(systematic: every k-th of the eligible population, k = round(N/10), offset
k/2), excluding epic/meta nodes (no test files). Contamination check: the task
prompt carries no merge sha and no PR number; an attempt whose transcript shows
it read commits past its fixture (`git log origin/main`, `git show` of a later
sha) is excluded as contaminated.

## Metrics

Accepted change (the replay bank's hidden tests pass from the merge commit and
the scoped CI subset is green), stalls and BLOCKED results, tokens (input,
output, cache read, cache write) and dollars per attempt and per accepted
change (priced from the manifest price table via `evals-trend --by-cohort
--prices`), median wall time.

## Exclusions

`substituted` rows (the door observed a different harness or model than the
lane requested), `unavailable` and infrastructure failures, and contaminated
attempts (`excluded_reason: contaminated` on the row). Each is counted by
reason (AC3-EDGE), never scored.

## Dollar ceiling and stop rule

**$200.00 total across Run 0 and Run 1.** The worker stops and reports when
the next attempt would take the cumulative spend past the ceiling; it never
raises it. The ceiling value is part of this preregistration; changing it
requires a new preregistration commit dated before the first results commit.
The operator reviews this ceiling at PR review, before any paid run starts.

## Decision rules (applied in order)

1. If opencode or zcode beats Claude Code on accepted changes, or matches it
   at 30% lower cost per accepted change, recommend a GLM routing change in
   `config.routing.models`. A routing change is a recommendation for the
   operator, never an edit.
2. If every external harness shares one named failure on GLM that a loop
   change would fix (tool-call format, context handling, write corruption),
   name it as the deficiency child 3 (`-H footnote` Stage 1) targets.
3. If no arm differs beyond noise (about 15 points at this n), record that and
   recommend closing the native branch under AC4-EDGE.

Report per-task paired results with bootstrap intervals
(`fno doctor evals report --by-cohort`).

## Scope and limits

One machine, one model, 10 replay tasks, 3 repeats: n is small. Bootstrap
intervals at this n are wide; a difference inside the interval is noise, and
an arm under 20 graded attempts is underpowered and fires no rule alone. Run 0
and Run 1 grade different task distributions (Terminal-Bench 2 is generic;
the replay bank is footnote's own), so arms can differ across runs. The
Terminus 2 reference tells a harness effect from a model effect; it does not
measure footnote's own loop. The collector is the runner's own history rows;
the observe door reads the attempt's own transcript for identity and usage
(`usage` null, never zero, when unreadable), and a row whose identity reads
`unverified` still counts toward attempts but never toward a rule.
