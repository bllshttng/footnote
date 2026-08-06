# Answering a review request

The minion clause tells a teammate to mail you for a code review and stop.
This is the other half of that exchange: what you mail back, in the teammate's own harness vocabulary, and what it must do when the verb refuses.

Load it when a `RESULT: blocked` or a bare "I need a review on <branch-or-PR>" lands in your inbox.

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

## What you mail

Name the verb, name the diff, and state the two rules that keep the answer honest:

```
fno mail send <worker-handle> "REVIEW GRANTED for <branch-or-PR>. Invoke this in your session, as a command, against your diff:

  /code-review

Invoke it for real. If it refuses, retry; if it still refuses, report the LITERAL refusal string and STOP. Do not substitute another reviewer, a subagent, or a Bash approximation.

Verify each finding against source before accepting it. The reviewer is advisory, not authoritative - push back with a file:line argument where you disagree."
```

## When the verb refuses

The invocation has been reported to hit a `disable-model-invocation` refusal intermittently: one teammate was refused on attempts one and two, and a byte-identical third attempt launched.
Another session reported it launching on the first attempt with no refusal at all.

**The cause of that variance is unknown.** Do not offer one, and do not let a refusal become an impossibility claim - that inference has been drawn and retracted repeatedly, always from a real observation and always wrong.

So: **retry is legitimate.** An identical retry has been observed to succeed after a refusal.
After a few attempts, the teammate reports the refusal string verbatim and stops.

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
