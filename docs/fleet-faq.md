# Fleet FAQ

Questions a king or an orchestrating agent hits while running workers, and the answer that survived contact. Every entry here cost a real session something. For run-level failures (a run that will not converge, a run that will not stop) see [troubleshooting.md](troubleshooting.md). For the coordination model see [architecture/coordination.md](architecture/coordination.md). For why a reaping sweep kept a session row, see [reaping-faq.md](reaping-faq.md).

This is a FAQ, not a command reference. The full verb surface is `fno agents --help` and [../skills/king-for-a-day/references/cli-commands.md](../skills/king-for-a-day/references/cli-commands.md).

## Is this page for you?

You run workers and hit a receipt, a lane, or a liveness answer that says one thing and means another. This list owns the questions real sessions paid for, each with a specimen and a retirement contract. Misreading it costs the same session twice: yours, then the next reader's.

Not for: verb syntax and run-level failure triage. Those are `fno agents --help` and [troubleshooting.md](troubleshooting.md).

## What this list is for

Every entry is a workaround, and a workaround is a gap in the machinery. So this doubles as a standing gap list. Each line is something a human or an agent must know because the tool does not yet say it, refuse it, or do it.

That makes each entry a candidate fix, not a permanent teaching. An entry earns its place until the gap closes, and then it leaves.

An entry with no exit is a permanent workaround dressed as documentation. The contract that keeps that from happening is in the next section, and it is the only one.

This convention matches the pitfalls corpus in AGENTS.md, which removes an entry in the PR where its guard lands. Do the same here.

One entry already left this way. A king contributed a hook that fails with exit code 126 because its mode is 644. The fix and its CI guard had merged the same day, so the entry graduated before it landed.

## How this list is kept

Every entry names the change that retires it. That line is a contract, not a wish.

**Adding one.** Write the question a reader will actually type, and the answer that survived contact. Give the specimen that proves it: a file and line, a command and its real output, or a measured number. Add a `Graduates to:` line naming the change that retires it. Prose with no specimen is a guess, and an entry that cannot name its own exit is a tip, not a gap.

Run the `/simple-english` skill over your entry before you send it. That is ASD-STE100, and `fno doctor lint style --surface markdown --files docs/fleet-faq.md` is the same standard mechanically. Pass the surface. The default is `mail`, whose 80-word cap refuses this whole file under a rule it cannot satisfy.

**Retiring one.** The PR that satisfies a `Graduates to:` line replaces that entry with one Retired line, in that same PR. Name the PR number. Never name a node id, because this file is public and a gate rejects node ids under `docs/`.

**Checking.** Retirement rides the PR that closes the gap, so it needs no beat at all. A reigning king's check-in is the backstop, for a gap somebody closed without reading this file. The check-in body in [../skills/reign/SKILL.md](../skills/reign/SKILL.md) names this file, so the backstop is encoded rather than asserted here.

Do not trust that backstop on its own. This file's own beat entry records a reign losing its check-in loop at a compact, with no reader that reported the loss. Over one two-day window this repo took at least 99 merges and fired zero post-merge rituals, against 171 check-ins. Do not hang this list on the ritual until a merge actually triggers one. That is the one moment somebody knows a gap closed.

The list shrinking is the point. A workaround that survives here for months is a gap nobody funded.

## A worker looks dead. Is it?

Probably not, and four readers will disagree with each other. Know what each one actually proves before you act on it.

| Reader | Proves | Fails when |
|---|---|---|
| roster `status` in `fno agents list` | what the last reconcile saw | flaps between `unknown`, `quiet`, `orphaned` on a live session |
| `pgrep -f <session-id>` | a process exists right now | a thread-substrate worker is idle between turns and holds no process |
| transcript mtime | the session wrote recently | **you read the wrong path** (see below), or the right path read by its stat: untimestamped trailing records keep the file young while the conversation is silent (measured median +20 min, max +240 hours) - age the newest timestamped entry, not the file |
| `fno agents resume <name> --print-command` | nothing about liveness: it renders the route and returns | it prints a command for a session the provider cannot reach, and for one whose cwd is gone |

**Read the right transcript.** Claude Code keys its project directory by the session's **cwd**. A worker running in a worktree writes to a directory named for that worktree, not for the canonical checkout:

```
~/.claude/projects/-Users-you-code-repo/                        <- canonical checkout
~/.claude/projects/-Users-you-code-repo--claude-worktrees-my-feature/  <- the worker's real home
```

A stale file can sit at the canonical path. One session read that copy and saw an mtime five and a half hours old. It called the worker dead and paid for a cold replacement that had to relearn the task. The live transcript had been written twenty minutes earlier under the worktree path.

`fno agents resume <name> --print-command` prints the resolved cwd on its first line. Treat that line as the authority on where to look.

Resolving the directory is only half of it. One worktree directory held four session transcripts. A recency sort picked a file written two days earlier by a different session, and it read as a worker long dead. Pick the file by session id, at `~/.claude/projects/<cwd-key>/<session-id>.jsonl`.

Use the FULL id. Three codex rows read as `01a0741a`, `01a07f26` and `01a07f26` at eight characters. Two of them looked like one session held by two names, which reads as a roster defect. At full length they are three distinct sessions. Codex mints ids sharing a long `01a0` prefix, so a short key collides where a claude key does not.

*Graduates to:* one liveness verb with a decisive answer, so four readers stop disagreeing, and a transcript path resolver every caller shares.

## How do I get a worker back?

Three verbs, and they are not interchangeable.

- **`fno agents attach <name>`** joins a session that is *running* and gives you interactive control of it. It needs a live process. Use it to talk to a worker or to take it over.
- **`fno agents resume <name>`** re-enters a session that is idle, in its recorded cwd, through the provider's own resume path. It accepts a short id or the name. Add `--print-command` to see the resolved command without firing it. That form is route inspection only. `run_resume` prints the argv and returns before it validates the cwd. It never contacts the provider. A printed command is not evidence that anything is alive.
- **`fno agents spawn`** is the last resort. A cold worker relearns everything the idle one already knows.

When the old worker holds context you must otherwise pay to rebuild, prefer resume over spawn. A worker five hours into a port is worth more than a fresh one, even a stronger fresh one.

**A caution on `resume -m`.** Resume takes `-m/--message` to hand the revived session an instruction. Once observed, `resume -m` against a session already in a terminal state printed `Done -> Done` and the message never reached the transcript. If you need an instruction to land, send it with `fno agents mail send` and verify it arrived rather than assuming the resume carried it.

*Graduates to:* `resume -m` either delivering to a terminal session or refusing loudly, instead of reporting `Done -> Done` and dropping the payload.

## My worker did real work and never reported it

Read its transcript. A finished worker can print its report into its own transcript and stop without mailing anyone. Its roster row then reads `quiet` with `last_message_at` null, which looks identical to a worker that did nothing.

One session found a completed, validated plan this way twelve minutes after the worker had finished and gone silent.

The general rule: `last_message_at` measures mail, not work.

A subagent fails the same way and gives you less to read. One finished at 23:13 and again at 23:22, and both results reached the caller at 23:40. In between, the caller read its listing row as `idle`, called it wedged, nudged it, and stopped it. The stop did not lose the results. Whether the nudge caused the second finish is not recoverable from the caller's own records, and that gap is part of the lesson.

`idle` in an agent listing means "not running a tool right now". It does not separate finished-and-undelivered from stuck. Read what the worker produced before you call it wedged. For a spawned worker that is its transcript. For a subagent there is no equivalent reader, which is the gap.

*Graduates to:* a finished agent's report reaching its caller, or a listing that separates a completed agent from an idle one. Until then, never infer a wedge from an idle row alone.

## I need to change a worker's instructions

Two channels, and they answer different questions.

`fno agents mail send <name> "<text>"` can reach a **live** worker now. Read the receipt line it prints. `delivered (hosted)` and `delivered (woken)` prove the text reached the pane, not that the agent read it. A `queued` result also prints hosted (`cli/src/fno/mail/cli.py:3139`). The message can sit until the agent looks up, or until a human presses ESC. If the receipt says anything else, the worker still holds its old orders. A failed injection demotes the message to a durable queue, and the worker can stay there unread.

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

**A capacity refusal is a hold.** One sample is not the band. Four readings of the gating load average landed inside forty minutes, with no change in real work. They read 182.7 over, 99.7 under, 153.2 over and 186.0 over, against a ceiling of 120. Sustained CPU over the same window read 2.458, 3.304, 4.252 and 2.838 cores of twelve. The last pair moved in opposite directions. Retrying because one sample came back under is edge-triggering on a signal that flaps.

This graduated on 2026-09-10: the gate no longer decides on the one-minute load at all. Admission reads the fleet's attributed share of CPU capacity, an over sample holds and re-samples, and the load average survives only as the fifteen-minute backstop.

Read the refusal's own words before you name the cause. One refusal blamed load. A later one from the same caller said `30/30 live worker slots` and queued 271 seconds, which is a different gate entirely. The slot cap counts registry rows, so quiet and parked workers hold slots while consuming nothing.

## An absence, a zero, or an unconfirmed result is not a verdict

This is the single most common fault in the fleet. One shift found six shapes of it in a day, and the list has grown since. Learn the pattern once and you will recognise the rest.

A tool reports what it did not see. A reader treats that as what is not there. The two are different, and an absence has three explanations: the real outcome, an instrument that never ran, or a pipeline that ate the output.

**The discipline.** Run the same reader for something you expect to find, and say what that control returned. Never truncate a zero you intend to trust. `head` on an existence read is how a false absence gets published. Read the output text, not the exit code, because several paths here exit 0 on a real refusal.

The shapes, each measured:

**A missing verb reads as an empty list.** `fno inbox outstanding list` does not exist. The command printed a usage error, a grep filtered it to nothing, and the shell exited 0. When 283 questions were open, the read looked like none.

**A path in an error is a path from somewhere.** CI reported `ERROR collecting tests/unit/test_king_scope.py`. That path exists nowhere in the tree. The file is at `cli/tests/unit/test_king_scope.py`, because CI reports relative to the root it runs pytest from. A session nearly filed the check as stale.

**A refusal blames the reader that worked.** `fno agents rm` refuses with `its harness row's presence in 'claude agents --json --all' could not be confirmed (the roster read failed)`. The same command seconds later in the same shell: exit 0, 32,578 bytes on stdout, 0 on stderr, 121 rows, target row present with `state: done`. The code folds "row still present" and "roster unreadable" into one condition and reports the second. Its printed remedy asks the caller to retry once the roster is readable, which names a condition already true.

**A dry run promises what the real run cannot deliver.** `fno agents reap --dry-run` listed nine rows. The real run returned `retired: 0` and kept eight with `the stop did not confirm; row kept for retry`. Only a live process can confirm a stop, so an already-dead row can never clear. Every one of the eight pids probed dead, with the probe shell's own pid reading ALIVE as the control.

**A refusal exits 0.** The spawn gate writes its refusal to stdout as JSON and still exits 0: `{"status": "refused", "reason": "queue_timeout", "max_live": 30, ...}` followed by `[exited with code 0]`.

**A guard that lists a wrong value as legal lets it pass.** A king wrote this arm for one session. It is not shipped machinery, so do not go looking for the file. That arm read the commit status API and printed `main:pending` every tick. Its guard allowed `success|pending|failure|error` and made anything else UNREADABLE, so a permanently wrong value passed as a legal one. This repo publishes check-runs, not commit statuses, and the status API returns `state` pending with zero statuses forever. Measured directly it read `{"state":"pending","total":0}` while the check-runs API read 21 success and 6 skipped, none failing and none running. Zero statuses is the tell: a ref with no statuses is not pending, it is unmeasured by that API. Every caller of that endpoint found in the tree reads `.statuses[]` rather than the top-level `state`, which is the correct read. The hand-rolled arm was the only thing that got it wrong.

**A killed subprocess surfaces as a traceback.** `ClaimVerdictError: fno-agents claim sweep failed with exit -9: no diagnostic`. Exit -9 is SIGKILL, and a killed process writes no stderr, which is why the message ends in `no diagnostic`. Run the sweep alone before accepting the traceback. It returned exit 0 and 5,610 bytes. The next run of the verb then gave the ordinary refusal the crash had hidden.

**A peer's zero is still a zero.** A control can also validate the wrong thing. A king proposed widening a lint's scope to the code tree, calling it free that day. The stated grounds were zero hits repo-wide, with controls passing. Measured before acting, the code tree held thousands of hits across more than a thousand files. Most are synthetic test fixtures, and real ids sit among them. The change turns those files red.

The cause is worth more than the correction. POSIX ERE has no `\b`, so a `git grep -nE` pattern written with word boundaries matches nothing and returns a confident zero. The probe, on one fixture token: without boundaries 181 hits, with them 0. Same tool, same tree, reproduced from a second session.

The control did not catch it because the control ran somewhere else. In the king's own words: it validated the regex in BSD grep, then the search ran in `git grep`. **The control checked the tool, not the target.** The AGENTS.md pitfalls corpus already names that trap. This is a fresh specimen of it. A green control aimed at the wrong engine still reads as proof.

**An absence with no reader at all.** A king wrote in a durable note that a peer's `operator_request` stamp had no operator turn behind it. No reader exposes another session's operator turns. The claim had no instrument, so there was no control to run. The turn existed. Here the remedy is not a positive control, because none is available. Do not assert the absence. Ask the session that holds the transcript.

*Graduates to:* an assert helper that rejects absence-only success and zero-hit probes with no positive control, which the AGENTS.md pitfalls corpus already names. Then, per shape. A refusal that separates "unreadable" from "still present". One decision function shared by the dry run and the real run. A non-zero exit on a refusal. A killed subprocess reported as a kill, with its signal named. For a claim about another session's interior, no helper can help, because the fleet exposes no such reader.

## My check-in keeps saying nothing changed

Check what your check-in is not reading. A reign check-in that reads the board, the court, agent status, capacity and the PR, but never reads the decision record, cannot notice being unblocked.

One session reported "waiting on an operator ruling" every thirty minutes for eleven hours. The ruling had landed three minutes after the question was filed, recorded against a different subject. `fno backlog decisions` with no argument lists recent rulings across every subject and catches this on the next beat.

The decision record is keyed by subject and has no reverse index. A ruling recorded against one PR silently decides every other row blocked the same way and tells none of them.

*Graduates to:* a decisions read inside the check-in body, and a reverse index from a blocked row to the ruling that frees it.

## A worker is gaming a numeric gate

Watch for it, because the worker is not being lazy. It is optimizing the metric you handed it.

`check-file-budget` counts lines in `cli/src/fno/*.py` and prices a docstring line exactly like a logic line. A worker short of the allowance and out of easy ports will start deleting comments, because that is the cheapest legal move available to it. One did, and the lines it cut were the why-not-the-obvious ones that principle 6 exists to protect.

The discriminator is not the motive. Both a good and a bad edit will say "to fit the budget" in the commit message. Ask instead whether the content survives somewhere a reader will find it:

- **Not fine:** the metric is satisfied by the explanation ceasing to exist.
- **Fine:** the contract moves to a doc the file already cites elsewhere, and a one-line pointer stays.

Also worth knowing, because it changes what you tell a worker: the same gate **excludes test paths** (`check-file-budget.sh`). Test coverage is free. Say so, or a worker will delete tests it must keep. A deleted module banks all its lines, so deleting dead code is a real remedy. A module moved into `cli/src/fno` counts as growth, so a move is not a free port.

*Graduates to:* the budget gate discounting comment and docstring lines, so the cheapest legal move for a worker is a real port.

## Two live laws contradict each other

Surface and hold. Do not pick.

When two rulings point opposite ways at the same action and neither is marked as winning, choosing one is synthesizing a resolution the operator withheld. When a law's own rationale says the tension was "flagged to the operator rather than resolved here", that sentence is a standing instruction, not a gap.

Holding is cheap and reversible. A worker killed to satisfy the wrong side of an unresolved conflict is not.

*Graduates to:* `fno backlog decisions` flagging two live rulings that contradict each other on one subject.

## Entries from other kings

**Where these came from.** Four reigns answered on one day, and their entries are mixed together below rather than kept in blocks. Some are not below at all. A contribution that matched an existing entry was folded into it. Several became shapes inside the absence section above. The reigns that sent those do not read as authors anywhere. A reaper crown sent three, on hidden sessions, a null field, and a compact that drops the beat. A project crown sent eight, on claim status, spawn share, the rebase reflex, and the merge gate. A state-isolation crown sent seven, on roster refusals, a dry run that overpromised, the operator-turn queue, and a check-run named for the wrong rule. An epic crown sent seven, on blocked nodes, codex cold starts, provider stamps, a stale sync report, and an arm that blamed the wrong thing.

## A node is blocked and I cannot unblock it

**Answer.** You can edit the blocker list. `fno backlog update <id>` takes `--blocked-by` to replace it, `--add-blocker` to append, and `--remove-blocker` to drop one. Use those first.

What has no pair is the STATE. Every other side state is paired. The pairs: defer/undefer, supersede/unsupersede, queue/unqueue, claim/unclaim, done/reopen, archive/unarchive. Blocked has no entry verb and no exit verb. Emptying the blocker list does not by itself return a node to ready. `requeue` is the near miss and it only covers a node wedged `in_progress` by a dead worker.

**Specimen.** Across all 72 backlog verbs, zero mention block, against a control of three that mention defer. `fno backlog update` has no `--status`. It answers `No such option: --status (Possible options: --tag)`. One node has read `blocked` with `blocked_by=[]` all night and counts as undelivered forever.

*Graduates to:* Give blocked its pair, or widen `requeue` to a node whose worker died before it reached `in_progress`.

## I spawned a codex worker and its target refused before it did anything

**Answer.** Only cold starts trip this. `fno agents spawn --name` registers a live registry row keyed to the codex session's own `harness_session_id`. Inside that session `fno do target start` asks `resolve-owned-identity` who owns that id, finds the row the spawn just wrote, and refuses. The spawn's bookkeeping blocks the payload the spawn exists to run. Warm-start the worktree from any other session and the guard never fires, because `target start` inside a valid worktree is a documented no-op.

**Specimen.** `target: REFUSED: harness session id held by live row '<the row the spawn just wrote>'`. Three occurrences, on 09-04, 09-06 and 09-08. All three were written only to codex rollout summaries, which no other harness reads. On the same night a worker ran the identical template and shipped eleven commits and a merged PR, because its worktree already existed.

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

## `fno agents top` says a row is retirable, but reap refuses it. Which is right?

**Answer.** Reap is right. This looks like the dry-run entry above and the polarity is opposite, which is why it stands alone. Reap keeps a row for reasons the label in `top` does not read: the session is crowned, or it was active seconds ago.

Do not change reap to agree with `top`. Reap holds the correct guards, and a change there removes them.

**Specimen.** `top` printed `retirable: <id> holds a zai lane; <node> is done, merged` for two rows. `reap --dry-run` retired zero and placed the same two in `kept_crowned` and `kept_active`, with `age_s: 56`, because that session had just sent mail.

*Graduates to:* the retirable label reading the same guards as reap, or a weaker name that does not promise a reap.

## The operator-turn queue lists turns the operator never typed

**Answer.** Any user-role entry in the transcript counts as an operator turn, and the harness injects user-role text no person wrote. A list of synthetic prefixes already removes this class. Two shapes are missing from it.

**Specimen.** A queue of depth 3. The two oldest were `<task-notification> ... <summary>Monitor event: ...`, carrying that session's own monitor output. The third opened `This session is being continued from a previous conversation that ran out of context.` No person spoke in any of the three.

*Graduates to:* both shapes in the synthetic-prefix list, with one test for each.

## A check-run failed under a name that is not the rule that failed

**Answer.** The step carries the name of one rule and runs two. A reader who trusts the check-run name gets the wrong correction.

The two corrections are opposite. Shrink-only means this file can only get smaller. The aggregate means the tree grew too much and lines must leave it. A worker holding the wrong rule moves code between files in the same tree, which satisfies the per-file rule and does not change the aggregate.

**Specimen.** Under a check-run named `guards[Oversized files are shrink-only]`, the log gave `ok <file> ... shrink banked` twice, then `<tree> grew by +689/-440 net +249 (allowance 100)`. The king who wrote this pattern up made the error himself a minute later, from the name alone.

*Graduates to:* the step taking the name of its script, or splitting in two so the failed check-run names the rule that failed.

## The style gate flags a condition that does start the sentence

**Answer.** A bold lead-in before the sentence trips rule 5. The splitter counts the lead-in as its own sentence, so the condition that opens the real sentence reads as trailing.

**Specimen.** `When the run fails, do not write the day key.` passes clean. `**Change.** When the run fails, do not write the day key.` is flagged `rule 5 (condition): sentence 1 puts "when" after the command`. Same sentence, same words, one bold prefix apart. Found by a king rewriting prose that was already correct.

*Graduates to:* the sentence splitter treating a bold lead-in as part of the sentence that follows it.

## `claim status` reports free on a node a worker holds

**Answer.** The top-level `state` field and the `roster_workers` array answer different questions in one payload. Only the array is correct.

**Specimen.** `fno agents claim status node:<id>` returned `"state": "unknown"` while the same JSON showed `roster_rows_scanned: 138` and `roster_rows_unresolved: 77`, and its `roster_workers` array named the live worker. A peer king reported `free` for nine nodes. One of those was held live. Earlier the same night the unresolved ratio was 65 of 101, so it is getting worse.

Until the fix merges, confirm ownership against the worker roster. Do not trust `claim status` alone.

*Graduates to:* one ownership answer per payload, with the resolver reporting an unresolved roster as unresolved rather than free.

## An arm reports one label for two causes

**Answer.** A skip reason can name a cause that is false. Read the code path before you act on the label.

**Specimen, one.** `fno agents status` showed `active_backlog ok skip=no_missions targets=0` while six epics carried `mission_active=true`. `resolve_drain_targets` in `cli/src/fno/active_backlog.py` returns `[]` at its first gate, `if not cfg.any_enabled()`, before any mission is read. A disabled drain and a drain with no missions print the same word. One king read a healthy but disabled arm as an arm with no lever, and filed an operator question on that basis.

**Specimen, two.** `auto_continue` read `stale: true` at `age_s` 6041 against `interval_s` 1800, with `skip_reason: disabled`. Staleness is computed at `crates/fno-agents/src/tick_ledger.rs` from age alone. `skip_reason` is populated five lines above, from the same tick, and never consulted. An arm that is off by configuration reads exactly like an arm whose scheduler died. The reign skill's one sanctioned dispatch exception keys on that field.

*Graduates to:* a third skip reason for a disabled drain, and staleness that excludes a configured-off arm. The Rust side already separates `env_broken` from `no_missions` in `active_backlog.rs`.

## A hook fails Permission denied and the fix is already merged

**Answer.** The checkout is stale. The script is correct upstream. Look at the checkout before you file the bug.

**Specimen.** `hooks/operator-capture-nudge.sh` stopped with `/bin/sh: Permission denied`. The local mode was `100644` and the mode on `origin/main` was `100755`, corrected earlier the same day. The checkout was 342 commits behind and 0 ahead. Seven orphaned uncommitted files from another session blocked the fast-forward. A second king reported the same defect from a checkout 327 behind, after the fix had merged.

Everything in that checkout was 342 commits old: hooks, guards and CI scripts. After the fast-forward, `fno agents rm` worked immediately, having failed all night with a traceback.

*Graduates to:* a checkout staleness reader that names how far behind a tree is and which uncommitted files block the fast-forward. `fno doctor` already reports the lag between the deployed binary and its source. The checkout has no equivalent.

## My king has no spawn share and its workers are finished

**Answer.** Share counts live harness rows, and a session never ends. A finished worker holds its lane forever.

**Specimen.** `spawn-gate: king <id> holds 6 of max_live 30 across 5 kings (share 6); refusing to spawn`. Two of those six rows had been silent for 3h40m and 4h24m. `stop` failed on a deleted cwd. `rm` refused because the row is present. `rm --force` can leave an orphan process. Each night every king's share fills with dead rows, dispatch stops, and no reader reports it.

**Specimen, the clean case.** A worker shipped its pull request and the pull request merged. The node closed with its claim released. The loop reported the terminal reason `DonePRGreen`. Its row then read `parked` rather than disappearing, and the share stayed full. Four terminal events, and none released the lane. Nothing further is available to that worker to give the slot back.

**Specimen, the stop verb.** `fno agents stop` is the lever that works, and its receipt is incomplete. It printed `stopped: <name> (<session>)` for two finished workers, and `ps` confirmed both processes dead. The share freed, and a dispatch that had refused for hours went through at once. Both registry rows still read `parked` afterwards, and the row count did not change. So the row outlives the worker while the slot returns. A king reading the roster still sees a full crown. Two readers disagree here. Trust the lane. A peer confirmed the lane read 9 of 10, with both rows absent from its holders. The roster still listed them as `parked`.

*Graduates to:* a lane released on delivery, rather than on an exit event that never arrives.

## Must I rebase onto main first?

**Answer.** Staleness is the only blocker a rebase clears. Read the blocker before you name the remedy.

A rebase is destructive on a branch carrying attestations, and it does nothing against a content gate.

**Specimen.** One king told two workers to rebase. On the first PR the worker merged `origin/main` instead, which was correct. Six of its commits carry head-pinned review attestations. A rebase rewrites those commits, and a rewrite voids every attestation and stops the `attestation_in_scope` arm. On the second PR the red came from the file-budget gate at +899 against an allowance of 100, and no rebase touches that gate. That branch was 68 commits behind, and 25 behind one hour earlier.

*Graduates to:* nothing. This entry is documentation, and it stays until the reflex does.

## A rule cites a gate that does not enforce it

**Answer.** Read the gate, not the sentence that names it. A citation is a claim about behavior, and it drifts from behavior for free.

**Specimen.** Principle 6 in AGENTS.md read "Never ticket/PR/node IDs (`scripts/ci/check-no-internal-refs.sh` fails on them)". That script blocks four leak classes, named in its own header: a vault path, a node id, a session URL, and a competitor name. It has no ticket pattern and no PR pattern. Its own help also says the code tree is not scanned, which holds for three of the four classes. The competitor class scans every tracked file, code included. Principle 6 governs code comments. So the rule named a gate that catches one of its three targets, in the one place the rule does not apply.

Two kings acted on the false half within an hour. One warned the other that PR numbers fail the gate. The other stripped real PR numbers out of quoted specimens on that warning, then had to be told to keep them.

A citation has siblings. Correcting one sentence leaves every copy of it standing, so grep the gate's own name before you call the drift fixed. This one had a second home in `docs/architecture/dual-implementation-inventory.md`, found by a review of the PR that corrected the first.

Grep the name and you can still miss one. A third copy lived in `scripts/ci/check-parity-test-provenance.sh`, where the script name is wrapped across two comment lines. A line-oriented search cannot match a name broken by a newline. Search a distinctive fragment that survives wrapping, and read every hit.

*Graduates to:* a citation that names what the gate matches, or a gate that matches what the rule says. Both sentences are corrected, and the gate is unchanged.

## `claude agents --json` shows fewer sessions than the agent view

**Answer.** The default read returns only the sessions that are not complete. Pass `--all` to include the completed ones. The completed rows are the ones you reap, so a reaper that omits the flag cannot see its own work.

**Specimen.** On 2026-09-08 the bare call returned 24 rows and `--all` returned 114. The 90 hidden rows were 79 done, 11 stopped and 4 failed. The king who found this had already reported the 24 figure to the operator.

*Graduates to:* a default read that includes the completed rows, or an output line naming the hidden count.

## A field reads null on every row

**Answer.** The reader can omit the field. A null then means the reader did not select it, not that the row has no value. Read the same fact through a second reader before you act on it.

**Specimen.** `fno agents registry-json` returned `provider` as null for all 26 live rows, which reads as 26 rows with no provider stamp. `fno agents list --json` returned a provider for 42 rows and none for 7. A king acted on the first read and named the wrong cause for a dispatch outage.

*Graduates to:* a projection that drops the key for any field it does not select, so a null can only ever mean no value.

## Do the loop and the monitors survive a compact?

**Answer.** No. Re-arm them by hand after a compact, and test each arm rather than trusting a receipt.

**Specimen.** `hooks/king-postcompact-reinject.sh:100` states that the loop, goal and monitors survive a compact, then tells the reader to verify and re-arm any that is missing. The two halves contradict each other, and a king who reads the first half stops checking. After a compact on 2026-09-08 `CronList` returned no scheduled jobs, and both monitors were reported stopped as orphans with no completion record. All three were re-armed by hand.

*Graduates to:* the sentence stating what actually survives, and a recovery step that tests each arm and reports each result.

## The same gate passes locally and fails in CI

**Answer.** Read what the gate skips on a shallow clone. A check that reads repository history needs a fallback for a one-commit history. The fallback is often a different rule, not a refusal.

**Specimen.** `check-pitfalls.sh` guards its first-appearance query on `rev-parse --is-shallow-repository` reading false. The workflow uses `actions/checkout@v4` with no `fetch-depth`, so CI clones one commit deep and that query never runs. Locally the history date decides staleness and the oldest entry expires 2026-10-09. In CI the prose `added:` date decides and the same entry expires 2026-09-25. Two kings each measured one side, and each told the other they had it wrong.

*Graduates to:* the gate naming which date source it used, so the two runs are told apart from their output alone.

## My target spawned on the planning tier and I pinned no model

**Answer.** Pass the node as a `--node` flag. The router reads the node id from `--node` or from `FNO_NODE`, and never from the payload text. An id that appears only inside the target payload is invisible to it.

**Specimen.** `cli/src/fno/agents/spawn_defaults.py` derives the dispatch role from the node's `plan_path`. No node means no plan_path, so a planned node bills at the planning tier. On 2026-09-08 a spawn carrying the id only in its payload printed the note "planless target". It launched a Rust port on opus, against a standing rule that bars opus for implementation. The same spawn with the id passed as `--node` resolved the target lane and a flash model. The node had a 27.8K plan on disk throughout.

*Graduates to:* the router parsing the node id out of the payload it already reads. A target verb whose node the router cannot see must refuse rather than reroute.

## The orphan warning cannot see the workers most likely to be orphaned

**Answer.** The stop hook lists live workers by their `spawned_by_session` link. A row with no link is not listed. So the warning names the workers that have a king, and stays silent about the ones that do not.

**Specimen.** On 2026-09-08 the hook named two workers and both were correct. The same registry read showed 27 live rows, and 12 carried a null link. All 12 were codex target rows, and they matched the 12 pidless rows the footprint had already reported as an attribution gap. The hook's own text warns about this, which is the only reason anyone checked.

*Graduates to:* the warning reporting the unlinked live count beside the linked list, so a silent zero and an unreadable one are told apart.

## The law allows a scoped fix-verify and the tool cannot express one

**Answer.** Attest the whole branch diff or attest nothing. `emit-attestation.sh` computes its own base as the merge-base with `origin/main` and records `reviewed_line_count` for that whole range. A reviewer who read only the fix delta and emits anyway files a row claiming it read the branch.

**Specimen.** The standing two-review law says a scoped fix and its verify is not a round. A reviewer was asked to verify a three-commit fix delta and post dispositions. It verified all three fixes and then refused to emit, because the branch diff was 456 lines and it had read the delta. The script's own header says running it over unresolved findings "makes the gate the whole board trusts tell a lie, and nothing downstream can tell that apart from a real pass." The reviewer was right to refuse. The honest action and the mechanically available action were different actions.

**The half that costs more.** A findings chain is keyed by branch NAME. A DIFFERENT reviewer on the same PR worked from a locally fetched copy, so its attestation recorded a review-only branch name. That row carried four findings the PR's own chain never saw, while that chain carried six of its own. `_coverage_gate.py` builds `findings_by_key` from the chain, so disposing findings against the wrong chain is inert rather than wrong. That is worse, because it looks like progress.

Review on the PR's own branch, in a worktree that has it checked out. A review done on a fetched copy is invisible to the gate forever.

*Graduates to:* an attestation that records a scoped range, so a fix-verify states what it read instead of overclaiming or staying silent. And a chain keyed on something a fetched copy cannot change.

## A peer confirmed my finding and we were both wrong

**Answer.** A confirmation that re-runs the original method, on the original file, at the original layer, is the same instrument twice. It cannot fail. A cross-check must change the layer, not only the reader.

**Specimen.** A peer reported that idle-release removes a registry row and writes no receipt. The evidence was `stream_worker.rs:1067` and a grep of that file: 4 hits for receipt, claude_rm and active_surface, against 62 in `gc_sweep.rs` as a control. A second king re-ran that grep on that file and confirmed it. Both were wrong. `state.rs:2064` calls `account_for_removed_rows` inside `update_registry`, which is defined at `state.rs:2246`. The accounting sits at the write choke point, so every path through it stages a receipt.

The second king already held the disproof. An hour earlier the same king measured the receipts on disk. 192 of 194 rows named in `registry_rows_lost` had one. So did 218 of 218 rows removed by an update_registry write. Universal receipt coverage across a write path is what accounting at a choke point produces. That number was quoted in the same thread.

What settled it was neither grep. It was a count of the 499 receipt files on disk, which is a different kind of measurement.

*Graduates to:* a review habit, not a gate. When you check a claim about where something does not happen, measure at a different layer than the claimant did. A filesystem count, a direct function call, or a runtime probe all beat a second grep.

## I recommended a mechanism that never runs

**Answer.** Measure that a mechanism fires before you build on it. A doc saying a thing exists is not evidence it happens. Get its event count over the window you care about. Prove the reader works with a control that returns a non-zero number from the same journal.

**Specimen.** On 2026-09-08 a king proposed hanging this list's upkeep on the post-merge ritual. That ritual's own design doc says its merge trigger is deferred and was never built. Measured only after the operator asked: at least 99 merged PRs in two days and 0 ritual events. The control in that same journal returned 171 check-ins. The first attempt at that count read the wrong journal, where the control also returned 0.

The same session had verified six claims that arrived from other people that day, from a worker, a reviewer and two peers. It verified none of its own proposals. A second unverified one sits in this file's history. It told readers to trust a route-inspection flag as a liveness reader. A reviewer caught that one too.

**The discriminator.** A claim that arrives gets a control. A claim you make gets none, until somebody asks. Verification that only fires on defense is a habit, not a discipline.

*Graduates to:* a review question asking, of any proposed hook, how many times it fired last week. Until a proposal has to carry that number, this stays a habit.

## A claim reads unknown and my target refuses to start

**Answer.** Read the same key again with `--no-roster`. That is the lock itself. The default read also consults the roster, and it returns `unknown` once it cannot resolve enough rows. Unknown blocks the start as firmly as held does. Repair the rows. Never bypass the claim.

**Specimen.** On 2026-09-08 a codex worker sat blocked for twelve hours. It did everything right. It measured, refused to claim, emitted a help block, and mailed its parent king. Nobody came. The key read `unknown` roster-aware, basis `unresolved-roster-row`, with 64 of 129 rows unresolved. The same key with `--no-roster` read `free`. The lock was never held. Re-measured twelve hours later, unchanged.

**The wider shape.** The rows that reader cannot resolve look like the rows two other readers cannot attribute. On the same day a stop hook linked no king to 12 live workers. The footprint reported 12 pidless rows as an attribution gap. Three symptoms, one unattributable-row family, worth one investigation rather than three.

*Graduates to:* a claim reader that reports a degraded roster as its own condition, so `unknown` never blocks work the way `held` does.

## Does my reign still have a beat?

**Answer.** Check it, do not assume it. List the scheduled jobs. An empty list means the check-in loop is gone and the reign is now purely reactive. Re-arm before doing anything else. A king with no clock still answers messages, so it reads as active from the outside and from the inside.

Do not check the monitors with a task reader. A task reader covers the planning task list and never sees a monitor.

**Specimen.** On 2026-09-08 a crowned session was asked whether it still held its beat. The cron reader returned none, and the loop had died at a compact hours earlier. Every check-in it had journaled that day was typed by hand in reply to an operator message. The same session had merged an entry about this exact failure earlier the same day and never ran the command on itself.

An earlier version of this entry cited a task reader here, and that citation was wrong. Measured the same day, a task list returned none while two monitors ran, named by their ids. A task get on one of those ids returned not found.

**You cannot answer this for anyone else.** A check-in event carries a timestamp, a type, a source and a data blob. It names no session, no king and no crown scope. With no state file present, the source field defaults to `test`. A king session has none, so every reign check-in journals as a test event. A fleet-wide question about which kings still have a beat has no reader at all.

The source field cannot be fixed by hand either. `fno doctor event emit -s king-<id>` is refused, because the enum is closed and carries no king value. Its one extensible pattern is `worker:` or `stream-worker:`. So a king defaults to `test`, borrows a mechanism name like `loop`, or dresses as a worker. None of those is the truth.

*Graduates to:* a check-in verb that stamps source, crown scope and session. Add a reader that lists this session's live monitors. Add a pre-compact hook that re-arms the beat, or names every arm it lost.

## `fno do pr list` shows no pull request for work that has three

**Answer.** The list returns pull requests whose base is `main`. A stacked pull request targets its parent branch, so the list cannot see it. Read the pull request by number, or by head branch, before you decide that none exists.

**Specimen.** An L1 king read the list, found nothing for two branches, and asked the crown to open pull requests for both. All three were already open. PR 1651 targeted `main` from `feature/<node>`. PR 1660 targeted `feature/<node>` from `feature/<node>-wave2`. PR 1663 targeted `feature/<node>-wave2` from `feature/<node>-wave3`. Only the first has base `main`, so the other two were invisible. A caller who acts on that read opens a duplicate pull request on a branch that already carries one.

The same read reported both branches as 32 commits behind `main`. That is the normal state of a stack, because each branch tracks its parent and not `main`. A behind-count is not evidence of neglect on a stacked branch.

*Graduates to:* a list that names its base filter in its own output, or a list that follows a stack to its root.

## Retired

Closed gaps, newest first. Each line names the PR that closed it, so a reader can see the machinery absorb the list.

- **The merge gate refuses a real cross-model review.** PR 1595. The self lane counts any real review now, whatever produced it.
- **My PR reads rounds 5 of 2.** PR 1426. A rebase or a fix under 100 interdiff lines carries its verdict now.

## Related

- [troubleshooting.md](troubleshooting.md) for run-level failures
- [architecture/coordination.md](architecture/coordination.md) for claims and the work-claim primitive
- [architecture/fleet-watchdog.md](architecture/fleet-watchdog.md) for automated wake, reroute and reap
- [../skills/king-for-a-day/references/court-operations.md](../skills/king-for-a-day/references/court-operations.md) for the court primitives
