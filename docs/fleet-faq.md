# Fleet FAQ

Questions a king or an orchestrating agent hits while running workers, and the answer that survived contact. Every entry here cost a real session something. For run-level failures (a run that will not converge, a run that will not stop) see [troubleshooting.md](troubleshooting.md). For the coordination model see [architecture/coordination.md](architecture/coordination.md).

This is a FAQ, not a command reference. The full verb surface is `fno agents --help` and [../skills/king-for-a-day/references/cli-commands.md](../skills/king-for-a-day/references/cli-commands.md).

## A worker looks dead. Is it?

Probably not, and four readers will disagree with each other. Know what each one actually proves before you act on it.

| Reader | Proves | Fails when |
|---|---|---|
| roster `status` in `fno agents list` | what the last reconcile saw | flaps between `unknown`, `quiet`, `orphaned` on a live session |
| `pgrep -f <session-id>` | a process exists right now | a thread-substrate worker is idle between turns and holds no process |
| transcript mtime | the session wrote recently | **you read the wrong path** (see below) |
| `fno agents resume <name> --print-command` | the provider can still reach it | prints `is live` for a session that is idle, which is the useful answer |

**Read the right transcript.** Claude Code keys its project directory by the session's **cwd**. A worker running in a worktree writes to a directory named for that worktree, not for the canonical checkout:

```
~/.claude/projects/-Users-you-code-repo/                        <- canonical checkout
~/.claude/projects/-Users-you-code-repo--claude-worktrees-x-1234/  <- the worker's real home
```

A stale file can sit at the canonical path. One session read that copy and saw an mtime five and a half hours old. It called the worker dead and paid for a cold replacement that had to relearn the task. The live transcript had been written twenty minutes earlier under the worktree path.

`fno agents resume <name> --print-command` prints the resolved cwd on its first line. Treat that line as the authority on where to look.

## How do I get a worker back?

Three verbs, and they are not interchangeable.

- **`fno agents attach <name>`** joins a session that is *running*. It needs a live process. It is for watching, not for instructing.
- **`fno agents resume <name>`** re-enters a session that is idle, in its recorded cwd, through the provider's own resume path. It accepts a short id or the name. Add `--print-command` to see the resolved command without firing it, which is also the cheapest liveness probe you have.
- **`fno agents spawn`** is the last resort. A cold worker relearns everything the idle one already knows.

Prefer resume over spawn when the old worker holds context you must otherwise pay to rebuild. A worker five hours into a port is worth more than a fresh one, even a stronger fresh one.

**A caution on `resume -m`.** Resume takes `-m/--message` to hand the revived session an instruction. Once observed, `resume -m` against a session already in a terminal state printed `Done -> Done` and the message never reached the transcript. If you need an instruction to land, send it with `fno agents mail send` and verify it arrived rather than assuming the resume carried it.

## My worker did real work and never reported it

Read its transcript. A finished worker can print its report into its own transcript and stop without mailing anyone. Its roster row then reads `quiet` with `last_message_at` null, which looks identical to a worker that did nothing.

One session found a completed, validated plan this way twelve minutes after the worker had finished and gone silent.

The general rule: `last_message_at` measures mail, not work.

## I need to change a worker's instructions

Two channels, and they answer different questions.

`fno agents mail send <name> "<text>"` reaches a **live** worker now. It has been the reliable delivery path.

`fno backlog update <id> --dispatch-brief "..."` changes what the **next** worker reads. This is a standing order, not a note. Update it before you spawn, never after.

A brief that still says "hold for the operator ruling" will park a fresh worker on arrival, and the grant you just received will look like it never landed. Two separate sessions hit this in one day: the graph keeps issuing the old order until you change the graph.

## Spawn, reuse, or resume?

In this order:

1. **Reuse** a live worker with headroom: `fno agents retask <name> --node <id>`. Read `fno agents top` first.
2. **Resume** an idle worker that already knows the task.
3. When neither exists, **spawn**.

Before any spawn, check for a duplicate. A node with a live claim or a sibling worker already on it turns a helpful spawn into two workers fighting over one worktree. `fno agents list --json` filtered on the node id is enough. Name every worker after the node it serves, because that name is the only worker-to-node join you get.

## A gate refused me

Escalate, never synthesize. A refused gate is a message to the king or the operator. Flipping config or an environment variable to get past it turns a safety property into a silent one.

Refusals seen in practice, all correct:

- `fno backlog rank <id> --top` is operator-only. "The graph records no writer for a rank, so nothing downstream could tell yours from the operator's."
- The spawn gate refuses on `fleet_full` or missing attribution. Hold and report.
- `fno backlog decide` refuses every agent session. Operator authority is never inherited.

A refusal message can name the wrong cause while still being right to refuse. Fix the message in the project, obey the refusal now.

## I got zero results. Is that real?

Not until a positive control says so. An absence has three explanations: the real outcome, the instrument never ran, or the pipeline ate the output.

Two specimens from one day:

- `fno inbox outstanding list` does not exist. The command printed a usage error, a grep filtered it to nothing, and the shell exited 0. When 283 questions were open, the read looked like none.
- CI reported `ERROR collecting tests/unit/test_king_scope.py`. That path exists nowhere in the tree. The file is at `cli/tests/unit/test_king_scope.py`, because CI reports relative to the root it runs pytest from. A session nearly filed the check as stale.

So: run the same reader for something you *expect* to find, and never truncate a zero you intend to trust. `head` on an existence read is how a false absence gets published.

## My check-in keeps saying nothing changed

Check what your check-in is not reading. A reign check-in that reads the board, the court, agent status, capacity and the PR, but never reads the decision record, cannot notice being unblocked.

One session reported "waiting on an operator ruling" every thirty minutes for eleven hours. The ruling had landed three minutes after the question was filed, recorded against a different subject. `fno backlog decisions` with no argument lists recent rulings across every subject and catches this on the next beat.

The decision record is keyed by subject and has no reverse index. A ruling on `pr-1562` silently decides every other row blocked the same way and tells none of them.

## A worker is gaming a numeric gate

Watch for it, because the worker is not being lazy. It is optimizing the metric you handed it.

`check-file-budget` counts lines in `cli/src/fno/*.py` and prices a docstring line exactly like a logic line. A worker short of the allowance and out of easy ports will start deleting comments, because that is the cheapest legal move available to it. One did, and the lines it cut were the why-not-the-obvious ones that principle 6 exists to protect.

The discriminator is not the motive. Both a good and a bad edit will say "to fit the budget" in the commit message. Ask instead whether the content survives somewhere a reader will find it:

- **Not fine:** the metric is satisfied by the explanation ceasing to exist.
- **Fine:** the contract moves to a doc the file already cites elsewhere, and a one-line pointer stays.

Also worth knowing, because it changes what you tell a worker: the same gate **excludes test paths** (`check-file-budget.sh`). Test coverage is free. Say so, or a worker will delete tests it must keep.

## Two live laws contradict each other

Surface and hold. Do not pick.

When two rulings point opposite ways at the same action and neither is marked as winning, choosing one is synthesizing a resolution the operator withheld. When a law's own rationale says the tension was "flagged to the operator rather than resolved here", that sentence is a standing instruction, not a gap.

Holding is cheap and reversible. A worker killed to satisfy the wrong side of an unresolved conflict is not.

## Related

- [troubleshooting.md](troubleshooting.md) for run-level failures
- [architecture/coordination.md](architecture/coordination.md) for claims and the work-claim primitive
- [architecture/fleet-watchdog.md](architecture/fleet-watchdog.md) for automated wake, reroute and reap
- [../skills/king-for-a-day/references/court-operations.md](../skills/king-for-a-day/references/court-operations.md) for the court primitives
