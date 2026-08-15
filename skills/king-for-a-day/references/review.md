# Answering a review request

The minion clause tells a teammate to mail you for a code review and stop.
This is the other half of that exchange: what you mail back, in the teammate's own harness vocabulary, and what it must do when the verb refuses.

Load it when a `RESULT: resolved` report or a bare "I need a review on <branch-or-PR>" lands in your inbox.

**A `RESULT: resolved` report on a phase that produced a diff IS the review request.** The canonical clause has the teammate send the report and the request as two separate mails, so do not wait for the second one - a worker that reported and stopped will wait forever on an order you were holding for a mail it never sent. When the explicit request does arrive as well, it is the SAME request: order the review once. Two mails describing one finished unit of work are not two review orders, and sending two burns a second pass on an unchanged diff.

A `think` or `blueprint` phase resolves without a diff. There is nothing for a review verb to read there, so answer it - do not order a review of a design document with a code reviewer.

**A `RESULT: blocked` or `RESULT: failed` report is NOT.** That work is unfinished by the teammate's own account, and ordering a review of it spends a review pass on a diff its author already says is not done. Those reports want an answer to the blocker, or a decision. Review them only when the teammate explicitly asks.

Match the vocabulary the canonical clause actually emits - `RESULT: <resolved|blocked|failed>` ([minion-clause.md](minion-clause.md)) - not the `SUCCESS`/`DONE_WITH_CONCERNS` set from the execution-agent return contract in `AGENTS.md`. Two vocabularies share the `RESULT:` prefix, and a king matching on the wrong one never fires on a report its own spawn payload produced.

## Why the king fires the verb at all

A harness-native review verb can be self-invoked via the Skill tool but is often refused (cause unknown; see `docs/architecture/review-lanes.md`), so the king fires it directly.
`fno mail send <teammate> --raw '/<review-verb>'` injects the verb unwrapped at the teammate's prompt line, so the REPL's slash parser runs it - **--raw IS the user invocation**, and the review runs in the teammate's OWN harness against the tree it actually built. A wrapped `fno mail send` does NOT fire the verb; it relies on the teammate pulling its own trigger. You must not have authored the diff - a king reviewing its own diff is self-review even through --raw.

Two consequences fall straight out of that, and both are load-bearing:

- **Serve the verb, never run it.** Ordering the review is your job; running it in your own session reviews the wrong tree. Do not fan out a sigma panel the worker never configured, and do not review the diff yourself as a substitute.
- **A capability probe sent this way can only ever answer yes.** Mailing "can you run X unprompted?" tests the user-triggered path by construction. See the pitfalls corpus entry "A capability probe delivered over the mail bus can only ever return yes" in `AGENTS.md`; it is the trap this exact mechanism sets.

## The verb, per harness

Mail the teammate the verb ITS harness serves. A claude verb mailed to a codex worker is an unknown command, and the worker will either stall or improvise.

### claude

```
/code-review [low|medium|high|xhigh|max] [--fix] [--comment] [target]
```

The effort level, the flags, and the target are all optional; bare `/code-review` reviews the current diff and is a complete order.
`--fix` applies the findings it finds. `--comment` posts them as inline GitHub PR comments.

**`ultra` is not in the orderable grammar.** It is a deep cloud review, billed separately, and reserved for explicit human invocation. This is not a prose ban to weigh. The builder (`self_review_invocation` in `cli/src/fno/review_capability.py`) rejects it structurally. A CI check also fails any surface that spells a concrete review level outside that builder.

Source: the Claude Code command reference (`/code-review` row).

### codex

In session, `/review` opens an interactive picker over four targets: base branch, uncommitted changes, a specific commit, or custom instructions.
It also takes inline free-form instructions, which are prose and NOT flag syntax:

```
/review focus on concurrency and missing tests
```

Use the picker when you need exact branch or commit targeting; inline text cannot express it.

Non-interactively, `codex review` takes real parameters:

```
codex review --uncommitted
codex review --base main
codex review --commit <SHA> --title "Commit title"
codex review "Focus on concurrency and missing tests"
```

A target flag and a custom prompt cannot be combined in one invocation.

The review runs on the session's current model unless `review_model` is set in `~/.codex/config.toml`.

Source: codex `slash_command.rs` (the in-session verb) and `exec/src/cli.rs` (the non-interactive one).

### opencode

**There is no review verb.** Measured twice independently on 1.14.50: 22 commands, none of them `review`. The nearest thing is `opencode run "<prompt>"`, which is a general prompt runner, not a review lane.

This is a UX defect, not a gating one, and the distinction matters because the two get conflated. `resolved_local_peer_reviewers_for_author` returns a SINGLE composite `peer` entry, not one per configured peer, so **any one** peer pass clears the whole gate. A repo with `peers = ["codex", "opencode"]` is perfectly clearable through codex.

What actually goes wrong: a teammate handed `opencode` as its peer has no verb to run and nothing telling it to pick the other one. Name the peer you mean, rather than letting a worker choose from a list where one entry is a dead end.

## What you mail

Send this shape. It is not a style preference: every observed firing used it, and the two attempts that were self-initiated rather than ordered were both refused.

```
fno mail send <worker-handle> "REVIEW GRANTED for <branch-or-PR>. Invoke this in your session, as a command, against your diff:

    /code-review <level> --comment [--fix]

INVOKE IT FOR REAL. Type the verb. Do not substitute fno:code-reviewer, /fno:review, or a Bash approximation.

If it refuses, retry; if it still refuses, report the LITERAL refusal string and STOP.

Verify each finding against source before accepting it. The reviewer is advisory, not authoritative - push back with a file:line argument where you disagree."
```

Three details in that template are load-bearing, so do not smooth them out:

- **The verb sits alone on its own indented line.** Nothing before it on the line, nothing after.
- **"INVOKE IT FOR REAL. Type the verb."** A bare verb as the entire message body has been observed failing where this framing fired. That is one observation each way, so it is a lead rather than a mechanism - but it costs nothing to keep and the shape with it has never failed.
- **Name the substitutes.** `fno:code-reviewer`, `/fno:review`, a Bash approximation - these exactly, not "do not substitute another reviewer". The refusal text forbids these by name, a worker DID quietly take one of them and report success, and a generic prohibition leaves the worker deciding what counts as a substitute.

Pick `<level>` from the diff (`low|medium|high|xhigh|max`), never `ultra`. The sizing is codified, not judged. `level_for_diff` in `cli/src/fno/review_capability.py` tiers the level from the changed-file and line counts against the merge base. Add `--fix` only on a branch the worker can commit to. An auto-applied fix from the wrong worktree writes into the wrong tree.

## When the verb refuses

The invocation can hit a `disable-model-invocation` refusal. **What distinguishes a firing attempt from a refused one is not known.** Here is the whole evidence base, because every compact explanation offered so far has been retracted.

A mailed order appears **necessary**. One session, one lane, one afternoon:

| what preceded the call | outcome |
|---|---|
| a mailed order carrying `/code-review` | fired, 5 of 5 |
| the session deciding on its own to review | refused, 2 of 2 |

Same session, same harness, same model, minutes apart. That is the argument for the king serving the verb: a worker that decides on its own to review gets nothing.

It is **not sufficient**, and do not write it up as though it were: a different session was refused twice *with* an order in hand.

The model lane is **not** the variable, though it was ruled so twice. A glm-5.2 worker ran `/code-review` to completion and produced a P1 with a file:line that landed as a real fix. A rejection cannot produce that. So no lane is structurally barred, and a king must not tell a worker its model disqualifies it.

Beyond that, stop. The framing of the order is a live candidate - a bare verb as the entire message body failed where an explicit "invoke this as a command, for real" fired - but that is one observation each and it is offered here as a lead, not a mechanism. Four explanations have been built on real observations today and all four were retracted. A fifth is not what anyone needs.

Three rules follow:

- **Mail is the most reliable path.** A teammate that decides on its own to review is often refused (cause unknown); self-invocation has worked for some workers too, so it is not impossible, just less reliable than a mailed order. It mails you and waits. That is not ceremony; it is the path that most reliably works.
- **Retry is legitimate, but do not expect much.** An identical retry has been observed to succeed after a refusal once. Two identical retries with no order present were refused both times.
- **A refusal is never an impossibility claim.** That inference has been drawn from real observations and retracted at least four times in a single day. After a few attempts, the teammate reports the refusal string verbatim and stops - it does NOT substitute a subagent or a Bash approximation, which the refusal text forbids by name.

## A substitute is never silent

This is the rule with a receipt behind it.
A teammate that could not run the ordered verb quietly ran a review subagent instead, reported success, and the king merged that PR believing it had been reviewed.
The refusal text itself forbids exactly that route.

So the contract is: **run the ordered verb, or report the literal refusal and stop.**
Never `fno:code-reviewer`, never `/fno:review` standing in for a native verb, never a Bash approximation.
A review the king believes happened and did not is strictly worse than no review, because it retires the question.

## Two different things are named codex

They share a name, separate quotas, separate channels:

| | what it is | how it fails |
|---|---|---|
| `chatgpt-codex-connector` | a GitHub App bot that posts reviews on the PR | refuses on quota, with a comment saying so |
| `codex` | a local CLI a teammate runs in its own session | independent of the App's quota entirely |

A bot refusal (`You have reached your Codex usage limits for code reviews.`) does NOT mean no codex review is possible.
A teammate that waits on the App after it has already refused is waiting on a review that is never coming; order it the local verb instead.

## Confirm the order actually landed

A review order that never arrives looks exactly like a teammate ignoring you.
Read the receipt literally: only `delivered (hosted)` / `delivered (woken)` is arrival.

`fno mail send <name>` is the form proven to carry an order.
Delivery failures observed in the field include a `reply --to <id>` that printed no receipt at all and never arrived, and a `peek <handle>` that returned the CALLER's own transcript rather than the target's, which silently misleads anyone using it to verify a peer's state.

When a send will not land, write the order to a file and mail the path.
Reading it off disk is the fallback that works.

## When a lane cannot be satisfied, only restated

The `peer` lane gates on a machine verdict: `consume-peer-verdict.sh` emits a passing attestation only for a record declaring zero blocking findings. So a peer that keeps flagging the same point holds the gate shut, however the author responds.

That is usually correct and is the whole point of a fail-closed gate. It has one failure mode worth recognising, because it does not look like a failure while it is happening.

**A gate can assert on computation rather than on honesty.** Some limitations are not defects to be fixed but facts about where the code sits: a config-only renderer cannot resolve which peer is the author's own model, because that needs the session's harness. The honest response is to state the condition - name it, say why it cannot be decided here - and a reviewer that will not accept "I do not know, and here is why I cannot know from here" leaves exactly one way to clear the gate: take the dependency the file was designed to avoid. The reviewer is then pushing an architecture change through a review lane, which is not what a review lane is for.

Five consecutive peer passes on one PR each found something real, and one point survived every round: a stated-not-computed limitation the peer would not accept. Five real findings is not diminishing returns and is not a reason to stop. But the surviving point stopped being evidence about the diff and became evidence about the gate.

So, as a king: **count the restatements, not the rounds.** A finding restated a third time against a correctly-narrowed claim is a design signal, not a review outcome. Rule on it rather than ordering another pass. There are exactly two honest rulings, and neither is a quiet attestation:

- **Authorize the dependency**, as its own PR. Never folded into the one under review - a PR five rounds deep and green on everything else is the worst place to land an architecture change.
- **Rule the expectation unsatisfiable from that file**, ship with the lane OPENLY unmet, and put the reasoning in the PR body.

An unmet gate with a stated reason is honest. A gate cleared by a claim nobody could verify is the thing every rule in this file exists to prevent.
