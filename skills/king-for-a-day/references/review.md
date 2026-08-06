# Answering a review request

The minion clause tells a teammate to mail you for a code review and stop.
This is the other half of that exchange: what you mail back, in the teammate's own harness vocabulary, and what it must do when the verb refuses.

Load it when any of these lands in your inbox: a done report (`RESULT: resolved`, which the minion clause makes a review request), a `RESULT: blocked`, or a bare "I need a review on <branch-or-PR>".

Match the vocabulary the canonical clause actually emits - `RESULT: <resolved|blocked|failed>` ([minion-clause.md](minion-clause.md)) - not the `SUCCESS`/`DONE_WITH_CONCERNS` set from the execution-agent return contract in `AGENTS.md`. Two vocabularies share the `RESULT:` prefix, and a king matching on the wrong one never fires on a report its own spawn payload produced.

## Why the king serves the verb at all

A harness-native review verb is user-triggered: the session that wrote the diff cannot launch it for itself.
`fno mail send` injects as user-shaped text in the recipient's pane, so **your reply is the user invocation**, and the review runs in the teammate's OWN harness against the tree it actually built.

Two consequences fall straight out of that, and both are load-bearing:

- **Serve the verb, never run it.** Ordering the review is your job; running it in your own session reviews the wrong tree. Do not fan out a sigma panel the worker never configured, and do not review the diff yourself as a substitute.
- **A capability probe sent this way can only ever answer yes.** Mailing "can you run X unprompted?" tests the user-triggered path by construction. See the pitfalls corpus entry of the same name in `AGENTS.md`; it is the trap this exact mechanism sets.

## The verb, per harness

Mail the teammate the verb ITS harness serves. A claude verb mailed to a codex worker is an unknown command, and the worker will either stall or improvise.

### claude

```
/code-review [low|medium|high|xhigh|max|ultra] [--fix] [--comment] [target]
```

The effort level, the flags, and the target are all optional; bare `/code-review` reviews the current diff and is a complete order.
`--fix` applies the findings it finds. `--comment` posts them as inline GitHub PR comments.

**Never order `ultra`.** It is a deep cloud review, billed, and reserved for explicit human invocation. It is legal grammar and still not yours to spend.

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

Name the verb, name the diff, and state the two rules that keep the answer honest:

```
fno mail send <worker-handle> "REVIEW GRANTED for <branch-or-PR>. Invoke this in your session, as a command, against your diff:

  /code-review

Invoke it for real. If it refuses, retry; if it still refuses, report the LITERAL refusal string and STOP. Do not substitute another reviewer, a subagent, or a Bash approximation.

Verify each finding against source before accepting it. The reviewer is advisory, not authoritative - push back with a file:line argument where you disagree."
```

## When the verb refuses

The invocation can hit a `disable-model-invocation` refusal. The discriminator is **whether a user-shaped directive naming the verb is present in the session** - which is exactly what a mailed order is, and exactly what a worker's own decision to review is not.

The cleanest control came from one session, one model lane, one afternoon:

| what preceded the call | outcome |
|---|---|
| a mailed order carrying `/code-review` | fired, 5 of 5 |
| the session deciding on its own to review | refused, 2 of 2 |

Same session, same harness, same model, minutes apart. That is why the king serves the verb: the mail IS the invocation, and without it there is nothing for the session to act on.

An earlier reading blamed the session's model lane, on a paired control where a claude-lane session fired and a zai/glm-routed one refused. Do not carry that: it does not survive the table above, where one claude-lane session both fired and refused depending only on whether an order preceded the call. The glm-lane refusal remains **unexplained** - it had a mailed order and still refused, so something else is in play there. Record that as unknown rather than reaching for a third theory; four have already been built on real observations and retracted.

Three rules follow:

- **A worker cannot self-serve this.** If a teammate decides on its own that it should review, the call refuses. It must mail you and wait. That is not ceremony; it is the only path that works.
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
