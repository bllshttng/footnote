---
name: dnd
description: Hold incoming agent mail during a quiet window, then deliver one digest.
argument-hint: "[minutes | off | cancel | status | idle <minutes>]"
metadata:
  internal: false
  requires:
    binaries:
      - "fno >= 0.1"
---

# DND

Do-not-disturb for this session. Mail addressed to this session never pastes into the prompt line. It queues durable, and the sender gets a receipt saying so. When the hold lifts, the held mail is delivered as one digest, with no new prompt needed. Your own typing is never muted.

## Route

| The user says | You run |
|---|---|
| A duration: "20", "for 20 minutes", "I need your time for 20 minutes" | `fno agents mail hold --for <N>` |
| A range: "10-15 minutes" | `fno agents mail hold --for 15` (the upper bound, so the hold cannot lift inside the window) |
| No duration | `fno agents mail hold --for 20` |
| "Until I go idle", with a duration | `fno agents mail hold --minutes <N>` |
| "Off", "cancel", "stop", "release", "done", "allow mail" | `fno agents mail hold --off` |
| "Is DND on?" | `fno agents mail hold --status` |

"Until I go idle" arms the idle clock, not the wall clock. Say in the report that this clock restarts on every prompt and ends at twice the window. Every other route arms the wall clock: a fixed deadline that never moves.

A real message (not a slash command, a `!` line, or a raw send) starts the conversation hold by itself. It lasts while the session answers plus the configured grace (2 minutes by default) and restarts with each message. `Off` or `cancel` ends it now. A DND you set with a duration keeps its full length. The conversation rules never shorten or replace it.

While a hold is live: `control:` mail and the session's own sends pass, so you can always reach yourself. When the hold ends, a `--raw` send parked at the gate runs once through the raw door. Mail never lands while you are typing in the pane. It also waits out a question the session is asking. When you go quiet, it delivers. The sender's receipt names the time the hold ends. Relay the receipt verbatim.

## Report the real receipt

Run the genuine command and report its receipt line verbatim. The CLI calls the hold busy mode. That is DND. The proof is the receipt line, the `fno agents list` DND column, and the mux `[DND]` marker. Relay a nonzero exit or a `hold NOT off` line verbatim, and claim no DND state. Exit 3 is no provable harness identity.

## Scope

The hold applies to the session that runs the command. When the ask arrives by chat or mail, the lead runs it on the lead's own session. To quiet another session, run the door in that session.

## Not a pause

`fno agents loops pause-all` and the lead halts stop fleet work. They are not the answer to a user who wants quiet. Codex invokes this skill as `$fno:dnd`.

## Known Limitations and Deferred Work

- No shell verb or help text names DND yet: the hold help says "Busy mode" and never DND. See [LIMITATIONS.md](LIMITATIONS.md).
