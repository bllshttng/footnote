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
~/.claude/projects/-Users-you-code-repo--claude-worktrees-my-feature/  <- the worker's real home
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

*Graduates to:* every refusal naming a cause it actually verified.

## An absence, a zero, or an unconfirmed result is not a verdict

This is the single most common fault in the fleet, and one shift found six shapes of it. Learn the pattern once and you will recognise the rest.

A tool reports what it did not see. A reader treats that as what is not there. The two are different, and an absence has three explanations: the real outcome, an instrument that never ran, or a pipeline that ate the output.

**The discipline.** Run the same reader for something you expect to find, and say what that control returned. Never truncate a zero you intend to trust. `head` on an existence read is how a false absence gets published. Read the output text, not the exit code, because several paths here exit 0 on a real refusal.

The shapes, each measured:

**A missing verb reads as an empty list.** `fno inbox outstanding list` does not exist. The command printed a usage error, a grep filtered it to nothing, and the shell exited 0. When 283 questions were open, the read looked like none.

**A path in an error is a path from somewhere.** CI reported `ERROR collecting tests/unit/test_king_scope.py`. That path exists nowhere in the tree. The file is at `cli/tests/unit/test_king_scope.py`, because CI reports relative to the root it runs pytest from. A session nearly filed the check as stale.

**A refusal blames the reader that worked.** `fno agents rm` refuses with `its harness row's presence in 'claude agents --json --all' could not be confirmed (the roster read failed)`. The same command seconds later in the same shell: exit 0, 32,578 bytes on stdout, 0 on stderr, 121 rows, target row present with `state: done`. The code folds "row still present" and "roster unreadable" into one condition and reports the second. Its printed remedy asks the caller to retry once the roster is readable, which names a condition already true.

**A dry run promises what the real run cannot deliver.** `fno agents reap --dry-run` listed nine rows. The real run returned `retired: 0` and kept eight with `the stop did not confirm; row kept for retry`. Only a live process can confirm a stop, so an already-dead row can never clear. Every one of the eight pids probed dead, with the probe shell's own pid reading ALIVE as the control.

**A refusal exits 0.** The spawn gate writes its refusal to stdout as JSON and still exits 0: `{"status": "refused", "reason": "queue_timeout", "max_live": 30, ...}` followed by `[exited with code 0]`.

**A killed subprocess surfaces as a traceback.** `ClaimVerdictError: fno-agents claim sweep failed with exit -9: no diagnostic`. Exit -9 is SIGKILL, and a killed process writes no stderr, which is why the message ends in `no diagnostic`. Run the sweep alone before accepting the traceback. It returned exit 0 and 5,610 bytes. The next run of the verb then gave the ordinary refusal the crash had hidden.

**A peer's zero is still a zero.** A control can also validate the wrong thing. A king proposed widening a lint's scope to the code tree, calling it free that day. The stated grounds were zero hits repo-wide, with controls passing. Measured before acting, the code tree held thousands of hits across more than a thousand files. Most are synthetic test fixtures, and real ids sit among them. The change turns those files red.

The cause is worth more than the correction. POSIX ERE has no `\b`, so a `git grep -nE` pattern written with word boundaries matches nothing and returns a confident zero. The probe, on one fixture token: without boundaries 181 hits, with them 0. Same tool, same tree, reproduced from a second session.

The control did not catch it because the control ran somewhere else. In the king's own words: it validated the regex in BSD grep, then the search ran in `git grep`. **The control checked the tool, not the target.** The AGENTS.md pitfalls corpus already names that trap. This is a fresh specimen of it. A green control aimed at the wrong engine still reads as proof.

*Graduates to:* an assert helper that rejects absence-only success and zero-hit probes with no positive control, which the AGENTS.md pitfalls corpus already names. Then, per shape. A refusal that separates "unreadable" from "still present". One decision function shared by the dry run and the real run. A non-zero exit on a refusal. A killed subprocess reported as a kill, with its signal named.

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

*Graduates to:* a lane released on delivery, rather than on an exit event that never arrives.

## Must I rebase onto main first?

**Answer.** Staleness is the only blocker a rebase clears. Read the blocker before you name the remedy.

A rebase is destructive on a branch carrying attestations, and it does nothing against a content gate.

**Specimen.** One king told two workers to rebase. On the first PR the worker merged `origin/main` instead, which was correct. Six of its commits carry head-pinned review attestations. A rebase rewrites those commits, and a rewrite voids every attestation and stops the `attestation_in_scope` arm. On the second PR the red came from the file-budget gate at +899 against an allowance of 100, and no rebase touches that gate. That branch was 68 commits behind, and 25 behind one hour earlier.

*Graduates to:* nothing. This entry is documentation, and it stays until the reflex does.

## The merge gate refuses a real cross-model review

**Answer.** With no review configuration, the posture resolves to `self_review`, whose only component matches a local attestation. A GitHub App review satisfies nothing.

**Specimen.** A PR was green, at rounds 2 of 2, coverage covered. A second model reviewed it and found three real P2 findings. The merge refused. `posture_verdict` in `crates/fno-agents/src/loopcheck.rs` matches `"self"` against `CoverageProducer::LocalAttestation` only. `github_apps`, `required_bots`, `peers` and `reviewers` are all unset, and `self_review_required` is true. So `resolve_posture_config` finds no signal and falls to its final `else`. It returns `self_review`, components `["self"]`, source `default`.

So by default the gate rewards the author's own lane and discards the more independent review.

*Graduates to:* a default posture that counts an external review. The fix has a node and an open PR, which the file-budget gate is currently blocking. The gate blocks the fix for the gate.

## A rule cites a gate that does not enforce it

**Answer.** Read the gate, not the sentence that names it. A citation is a claim about behavior, and it drifts from behavior for free.

**Specimen.** Principle 6 in AGENTS.md read "Never ticket/PR/node IDs (`scripts/ci/check-no-internal-refs.sh` fails on them)". That script carries two patterns: `NODE_ID_RE` for `x-` plus four hex or `ab-` plus eight, and `SESSION_URL_RE` for a session link. It has no ticket pattern and no PR pattern. Its own help also says the code tree is not scanned, and principle 6 governs code comments. So the rule named a gate that catches one of its three targets, in the one place the rule does not apply.

Two kings acted on the false half within an hour. One warned the other that PR numbers fail the gate. The other stripped real PR numbers out of quoted specimens on that warning, then had to be told to keep them.

*Graduates to:* a citation that names what the gate matches, or a gate that matches what the rule says. This entry corrects the sentence, and the gate is unchanged.

## Related

- [troubleshooting.md](troubleshooting.md) for run-level failures
- [architecture/coordination.md](architecture/coordination.md) for claims and the work-claim primitive
- [architecture/fleet-watchdog.md](architecture/fleet-watchdog.md) for automated wake, reroute and reap
- [../skills/king-for-a-day/references/court-operations.md](../skills/king-for-a-day/references/court-operations.md) for the court primitives
