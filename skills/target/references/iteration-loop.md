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

## Async-wait idling (the `<watching>` contract)

When your only outstanding work is an async external check - CI still running, or a bot review not yet posted - with nothing to do until it settles, do NOT keep waking every stop tick to re-check. Each wake is a full model invocation that produces zero progress. Instead, idle the session to ZERO invocations until the watched state changes:

1. **Arm a harness-tracked watcher with a hard timeout.** Use a background task the harness re-invokes the model on when it exits - background `Bash` (`run_in_background`) or a `Monitor` - whose command embeds a hard timeout, e.g.:

   ```bash
   fno do pr wait <PR> --until settled --timeout 30m
   ```

   The verb is the heartbeat. It ends on its own after the bound (default 30m), so a wedged read still wakes the session. Its exit code is the status verb's own alphabet: 0 green, 1 red, 2 still-unsettled at the bound. Every tick rides the coalescing cache: N waiters on one PR cost one network read per TTL. A rate-limit backoff window is ridden out, not hammered. The gh-call count prints at exit. The verb exits on the POSITIVE settled marker internally, never an absence test. An absence test reads a rate-limited read as settled. The verb does not. Spell the bound in `--timeout`, never `timeout(1)`. That is GNU coreutils, and a stock macOS ships neither it nor `gtimeout`. A command naming it dies with `command not found` before the read runs, so the watcher no-ops. Detaching the task itself (`nohup`, `disown`, a trailing `&`) is FORBIDDEN - it exits without re-invoking anyone, so the session idles forever. The CI read inside is REST, so this spends none of the per-user GraphQL quota every session on the machine shares. Do NOT reach for `gh pr checks --watch` or a `gh pr view` poll: `hooks/git-protection.py` DENIES both, so that recipe cannot run at all. Never hand-roll a `while/sleep/grep` replacement: every such loop is an uncoordinated poll against the shared quota, the exact fleet condition the verb exists to prevent.

2. **End your turn with the tag, and nothing else.** After arming the watcher, close the turn with:

   ```
   <watching reason="ci" pr="<PR>" timeout="30m">
   ```

   `reason` is `ci` or `review`; the attributes are advisory (used for the event and the claim-lease math). loop-check verifies against external truth that the only blocker is the async class (PR open, HEAD pushed, no unaddressed findings), extends your node claim to cover the window, and idles the session non-terminally. If any real blocker exists (CI red, unpushed HEAD, an unaddressed finding) the tag is ignored and you are told the real reason.

3. **On wake, re-check and either proceed or re-arm.** The harness re-invokes you when the watcher exits (settle, timeout, or kill). If the state settled, proceed. If the timeout fired and it is still pending, re-arm the watcher and re-emit `<watching>` - one cheap turn per ~30 min instead of one per ~90 s.

**Residual-turn austerity.** The few turns that remain (the initial arm-and-tag, a timeout re-arm) must be near-empty: the tag plus at most one short line. No status recap, no "waiting for it to settle" narration, no restating what you armed. The transcript is the operator's review artifact; the wait machinery's job is to be invisible in it.
