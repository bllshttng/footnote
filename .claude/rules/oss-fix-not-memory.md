# Fix in the project, never memory-only

The single rule that keeps footnote's hard-won knowledge where users can reach it. Loaded every session via `AGENTS.md` (working principle #3). Overrides any habit of parking a finding in private agent memory.

## Why this rule exists

footnote is open source. Other projects build on it, and they inherit exactly three things: the **code**, the **docs**, and the **runtime behavior** (a verb's output, a `--help` string, a gate's refusal message). They inherit **none** of your private agent memory. A maintainer running footnote a year from now, or a contributor reading the repo for the first time, sees only what shipped in the project.

So a load-bearing fact kept only in memory is a fix you mailed to yourself. It ships to nobody. Worse, it *feels* like progress - you recorded the lesson - which is why the gap never gets closed. We have caught ourselves adding the same discovery to memory across many sessions instead of fixing it once in the project.

## The trigger

The moment you reach for memory to record something that a future contributor would need, **stop**. That reach is the signal to write a project artifact instead. Watch for:

- a **workaround** ("you have to run X before Y or it fails")
- a **non-obvious invariant** ("this claim must be held before that write")
- a **gap or gotcha** in how the tool behaves ("the receipt says success even when it queued")
- a **trap** ("this flag silently reroutes to the durable lane")
- a **"next time do X"** note to your future self

If a teammate would hit the same wall, it belongs in the project, not your memory.

## Where it lands (most durable first)

1. **The fix itself.** The best artifact is code that makes the trap impossible: a guard, a gate, a corrected default, a verb that encodes the safe path. A gotcha you can delete is better than one you document.
2. **Self-teaching runtime text.** A refusal message, a receipt line, a `--help` string that hands the reader the right verb inline. This cannot drift out of sync with behavior because it *is* the behavior.
3. **A doc or a rule.** `docs/` for subsystem mechanics; `.claude/rules/` for behavioral guidelines; a one-line pointer in `AGENTS.md` if it's load-bearing enough to load every session.
4. **A test.** The invariant becomes executable and fails loudly when someone breaks it.
5. **A filed node whose details name the concrete fix path.** When the fix is genuinely a larger effort, `fno backlog idea` with details that say exactly what to change (the verb, the file, the gate) - not a vague "improve X."

Any of these is a real artifact. A memory entry is not.

## What memory IS for

Memory is not banned - it is scoped. Keep in memory what is genuinely local and non-shippable:

- **user preferences** (how this user likes PRs opened, prose written, work paced)
- **session continuity** (what this run is mid-way through, so a compaction or a fresh session can resume)
- **who's who** (a peer session's handle, a coordinator's role for this effort)

The discriminator: *would a stranger cloning the repo need this to work in the codebase?* If yes, it's a project artifact. If it only matters to this user or this run, memory is right.

## The self-check

Before saving a memory, ask: "Am I recording how footnote *works*, or how this *user/session* works?" The first is a project artifact you haven't written yet. The second is a memory. When you're unsure, write the project artifact - the cost of a doc nobody needed is far smaller than the cost of a fix that shipped to nobody.
