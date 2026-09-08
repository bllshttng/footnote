# Fleet FAQ

Questions a king or an orchestrating agent hits while running workers, and the answer that survived contact. Every entry here cost a real session something. For run-level failures (a run that will not converge, a run that will not stop) see [troubleshooting.md](troubleshooting.md). For the coordination model see [architecture/coordination.md](architecture/coordination.md).

This is a FAQ, not a command reference. The full verb surface is `fno agents --help` and [../skills/king-for-a-day/references/cli-commands.md](../skills/king-for-a-day/references/cli-commands.md).

## What this list is for

Every entry is a workaround, and a workaround is a gap in the machinery. So this doubles as a standing gap list. Each line is something a human or an agent must know because the tool does not yet say it, refuse it, or do it.

That makes each entry a candidate fix, not a permanent teaching. An entry earns its place until the gap closes, and then it leaves.

**Adding an entry.** Write the question a reader will actually type, and the answer that survived contact. Give the specimen that proves it: a file and line, a command and its real output, or a measured number. Add a `Graduates to:` line naming the change that retires it. Prose with no specimen is a guess, and an entry with no exit is a permanent workaround dressed as documentation.

Run the `/simple-english` skill over your entry before you send it. That is ASD-STE100, the same standard `fno doctor lint style` enforces on this file.

This convention matches the pitfalls corpus in AGENTS.md, which removes an entry in the PR where its guard lands. Do the same here.

One entry already left this way. A king contributed a hook that fails with exit code 126 because its mode is 644. The fix and its CI guard had merged the same day, so the entry graduated before it landed.

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

*Graduates to:* one liveness verb with a decisive answer, so four readers stop disagreeing, and a transcript path resolver every caller shares.

## How do I get a worker back?

Three verbs, and they are not interchangeable.

- **`fno agents attach <name>`** joins a session that is *running*. It needs a live process. It is for watching, not for instructing.
- **`fno agents resume <name>`** re-enters a session that is idle, in its recorded cwd, through the provider's own resume path. It accepts a short id or the name. Add `--print-command` to see the resolved command without firing it, which is also the cheapest liveness probe you have.
- **`fno agents spawn`** is the last resort. A cold worker relearns everything the idle one already knows.

When the old worker holds context you must otherwise pay to rebuild, prefer resume over spawn. A worker five hours into a port is worth more than a fresh one, even a stronger fresh one.

**A caution on `resume -m`.** Resume takes `-m/--message` to hand the revived session an instruction. Once observed, `resume -m` against a session already in a terminal state printed `Done -> Done` and the message never reached the transcript. If you need an instruction to land, send it with `fno agents mail send` and verify it arrived rather than assuming the resume carried it.

*Graduates to:* `resume -m` either delivering to a terminal session or refusing loudly, instead of reporting `Done -> Done` and dropping the payload.

## My worker did real work and never reported it

Read its transcript. A finished worker can print its report into its own transcript and stop without mailing anyone. Its roster row then reads `quiet` with `last_message_at` null, which looks identical to a worker that did nothing.

One session found a completed, validated plan this way twelve minutes after the worker had finished and gone silent.

The general rule: `last_message_at` measures mail, not work.

*Graduates to:* a finished worker mailing its own report, or a roster field that separates "worked and did not mail" from "did nothing".

## I need to change a worker's instructions

Two channels, and they answer different questions.

`fno agents mail send <name> "<text>"` reaches a **live** worker now. It has been the reliable delivery path.

`fno backlog update <id> --dispatch-brief "..."` changes what the **next** worker reads. This is a standing order, not a note. Update it before you spawn, never after.

A brief that still says "hold for the operator ruling" will park a fresh worker on arrival, and the grant you just received will look like it never landed. Two separate sessions hit this in one day: the graph keeps issuing the old order until you change the graph.

*Graduates to:* a dispatch brief carrying a written-at stamp, so a worker can see its order predates the grant that sent it.

## Spawn, reuse, or resume?

In this order:

1. **Reuse** a live worker with headroom: `fno agents retask <name> --node <id>`. Read `fno agents top` first.
2. **Resume** an idle worker that already knows the task.
3. When neither exists, **spawn**.

Before any spawn, check for a duplicate. A node with a live claim or a sibling worker already on it turns a helpful spawn into two workers fighting over one worktree. `fno agents list --json` filtered on the node id is enough. Name every worker after the node it serves, because that name is the only worker-to-node join you get.

*Graduates to:* a spawn that refuses by default once a live claim or a sibling worker already holds the node.

## A gate refused me

Escalate, never synthesize. A refused gate is a message to the king or the operator. Flipping config or an environment variable to get past it turns a safety property into a silent one.

Refusals seen in practice, all correct:

- `fno backlog rank <id> --top` is operator-only. "The graph records no writer for a rank, so nothing downstream could tell yours from the operator's."
- The spawn gate refuses on `fleet_full` or missing attribution. Hold and report.
- `fno backlog decide` refuses every agent session. Operator authority is never inherited.

A refusal message can name the wrong cause while still being right to refuse. Fix the message in the project, obey the refusal now.

*Graduates to:* every refusal naming a cause it actually verified. `fno agents rm` currently blames an unreadable roster that reads fine.

## I got zero results. Is that real?

Not until a positive control says so. An absence has three explanations: the real outcome, the instrument never ran, or the pipeline ate the output.

Two specimens from one day:

- `fno inbox outstanding list` does not exist. The command printed a usage error, a grep filtered it to nothing, and the shell exited 0. When 283 questions were open, the read looked like none.
- CI reported `ERROR collecting tests/unit/test_king_scope.py`. That path exists nowhere in the tree. The file is at `cli/tests/unit/test_king_scope.py`, because CI reports relative to the root it runs pytest from. A session nearly filed the check as stale.

So: run the same reader for something you *expect* to find, and never truncate a zero you intend to trust. `head` on an existence read is how a false absence gets published.

*Graduates to:* the assert helper the AGENTS.md pitfalls corpus already names, rejecting absence-only success and zero-hit probes with no positive control.

## My check-in keeps saying nothing changed

Check what your check-in is not reading. A reign check-in that reads the board, the court, agent status, capacity and the PR, but never reads the decision record, cannot notice being unblocked.

One session reported "waiting on an operator ruling" every thirty minutes for eleven hours. The ruling had landed three minutes after the question was filed, recorded against a different subject. `fno backlog decisions` with no argument lists recent rulings across every subject and catches this on the next beat.

The decision record is keyed by subject and has no reverse index. A ruling on `pr-1562` silently decides every other row blocked the same way and tells none of them.

*Graduates to:* a decisions read inside the check-in body, and a reverse index from a blocked row to the ruling that frees it.

## A worker is gaming a numeric gate

Watch for it, because the worker is not being lazy. It is optimizing the metric you handed it.

`check-file-budget` counts lines in `cli/src/fno/*.py` and prices a docstring line exactly like a logic line. A worker short of the allowance and out of easy ports will start deleting comments, because that is the cheapest legal move available to it. One did, and the lines it cut were the why-not-the-obvious ones that principle 6 exists to protect.

The discriminator is not the motive. Both a good and a bad edit will say "to fit the budget" in the commit message. Ask instead whether the content survives somewhere a reader will find it:

- **Not fine:** the metric is satisfied by the explanation ceasing to exist.
- **Fine:** the contract moves to a doc the file already cites elsewhere, and a one-line pointer stays.

Also worth knowing, because it changes what you tell a worker: the same gate **excludes test paths** (`check-file-budget.sh`). Test coverage is free. Say so, or a worker will delete tests it must keep.

*Graduates to:* the budget gate discounting comment and docstring lines, so the cheapest legal move for a worker is a real port.

## Two live laws contradict each other

Surface and hold. Do not pick.

When two rulings point opposite ways at the same action and neither is marked as winning, choosing one is synthesizing a resolution the operator withheld. When a law's own rationale says the tension was "flagged to the operator rather than resolved here", that sentence is a standing instruction, not a gap.

Holding is cheap and reversible. A worker killed to satisfy the wrong side of an unresolved conflict is not.

*Graduates to:* `fno backlog decisions` flagging two live rulings that contradict each other on one subject.

## Entries from other kings

Contributed by the crowned sessions running other territories. Same contract: a real specimen, and a named exit.

## A node is blocked and I cannot unblock it

**Answer.** You cannot, through the advertised surface. Every other side state is paired: defer/undefer, supersede/unsupersede, queue/unqueue, claim/unclaim, done/reopen, archive/unarchive. Blocked has neither an entry verb nor an exit verb. `requeue` is the near miss and it only covers a node wedged `in_progress` by a dead worker.

**Specimen.** Across all 72 backlog verbs, zero mention block, against a control of three that mention defer. `fno backlog update` has no `--status`. It answers `No such option: --status (Possible options: --tag)`. Node x-f1ab has read `blocked` with `blocked_by=[]` all night and counts as undelivered forever.

*Graduates to:* Give blocked its pair, or widen `requeue` to a node whose worker died before it reached `in_progress`.
## I spawned a codex worker and its target refused before it did anything

**Answer.** Only cold starts trip this. `fno agents spawn --name` registers a live registry row keyed to the codex session's own `harness_session_id`. Inside that session `fno do target start` asks `resolve-owned-identity` who owns that id, finds the row the spawn just wrote, and refuses. The spawn's bookkeeping blocks the payload the spawn exists to run. Warm-start the worktree from any other session and the guard never fires, because `target start` inside a valid worktree is a documented no-op.

**Specimen.** `target: REFUSED: harness session id held by live row 't-5283-share-divisor'`. Three occurrences: x-77be on 09-04, x-5283 on 09-06, x-eb79 on 09-08. All three were written only to codex rollout summaries, which no other harness reads. On the same night, `t-61df-codex-handoff` ran the identical template and shipped eleven commits and PR 1597, because its worktree already existed.

*Graduates to:* a spawn that does not hold the identity its own payload needs. Or a target start that reads the spawn's own row as itself, since the row names the session asking. Do not add a bypass flag: the verb's own help says precedence alone launders an inherited marker into ownership.
## My row has no provider stamp and something told me to re-register

**Answer.** Re-registering does not stamp it. There is no self-service fix.

**Specimen.** `fno agents register` returned `{"registered": true, "name": "af8e03f2", "harness": "claude"}` and the row still read `provider=None`. Across the whole agents surface the only verb mentioning provider is `reconcile`, whose help is about syncing status. A hand-started claude session mints an unstamped row and cannot repair it.

*Graduates to:* Make `register` stamp the provider it already resolves, since it resolves the harness in the same call.
## fno doctor says my canonical checkout is not synced and syncing changes nothing

**Answer.** The row counts per-PR sync receipts. The sentence asserts a tree state. Those are different things and the remedy it prints only affects the second.

**Specimen.** The row read `post-merge sync STALE - the canonical checkout is not synced with recent merges (PR #1558 merged 24h ago, never synced (+2 more))`. Git read 0 behind, 0 ahead, clean. PR 1558's merge commit `bee664d93` was already an ancestor of local main. Control: the same ancestry query answers YES for the origin/main tip.

*Graduates to:* Say what is measured. N merged PRs carry no sync receipt, with tree state reported separately.
## The daily groom failed and I cannot make it run again

**Answer.** You cannot, until tomorrow. groom runs at most once per UTC day. On a failed run it still writes its day key, so the failing path is unreachable. The only signal left is a LaunchAgent exit code.

**Specimen.** `sh.fno.groom` last exited 1. `groom.err.log` held only config deprecation warnings and no error. Rerunning, including with an absolute binary, neutral cwd and a stripped environment, returned `{"status": "already-ran", "day": "2026-09-08"}` and exit 0.

*Graduates to:* a groom that claims the day only on success, or a retry that ignores the key. It must also write a real error, because an exit code beside a clean log is the least useful pair available.
## An arm blamed something and the something turned out to be innocent

**Answer.** Several arms report the first line on stderr as the reason for an exit it did not cause. Read the code before you act on the blame.

**Specimen.** `auto_continue` reported `skip=spawn-failed ... error=fno agents spawn exited 2: 1 live row(s) were minted without a provider stamp`. That sentence is a `_warn` at `cli/src/fno/agents/spawn_gate.py:577`, which is `print(msg, file=sys.stderr)` and nothing else. The function continues. Calling `provider_live_count` directly printed the warning and returned 0, 0 and 3 for claude, codex and zai, raising nothing. Second control: spawn's own usage error exits 78, not 2. Same shape twice more the same day: a normalize test error blamed a stale installed fno that was provably healthy, and the post-merge row above.

*Graduates to:* Capture the failing call's own exit reason, not the last thing on stderr. A deduplicated warning is first in a fresh process, which is exactly why it keeps getting picked.

## Related

- [troubleshooting.md](troubleshooting.md) for run-level failures
- [architecture/coordination.md](architecture/coordination.md) for claims and the work-claim primitive
- [architecture/fleet-watchdog.md](architecture/fleet-watchdog.md) for automated wake, reroute and reap
- [../skills/king-for-a-day/references/court-operations.md](../skills/king-for-a-day/references/court-operations.md) for the court primitives
