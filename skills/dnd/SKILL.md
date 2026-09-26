---
name: dnd
description: "Do-not-disturb (DND) for this session: hold incoming agent mail on a fixed wall clock while the operator talks to you, then deliver it as one digest. Use when: 'turn on DND', 'do not disturb', 'do not interrupt me', 'hold my mail', 'quiet window', 'I need your time for 20 minutes', or 'DND off'."
argument-hint: "[minutes | off | status | idle <minutes>]"
metadata:
  internal: false
  requires:
    binaries:
      - "fno >= 0.1"
---

# DND

Do-not-disturb for this session. Mail addressed to this session never pastes into the prompt line. It queues durable, and the sender gets a receipt saying so. When the hold lifts, the held mail is delivered as one digest, with no new prompt needed. Your own typing is never muted.

## Route

| The operator says | You run |
|---|---|
| A duration: "20", "for 20 minutes", "I need your time for 20 minutes" | `fno agents mail hold --for <N>` |
| A range: "10-15 minutes" | `fno agents mail hold --for 15` (the upper bound, so the hold cannot lift inside the window) |
| No duration | `fno agents mail hold --for 20` |
| "Until I go idle", with a duration | `fno agents mail hold --minutes <N>` |
| "Off", "release", "I'm done" | `fno agents mail hold --off` |
| "Is DND on?" | `fno agents mail hold --status` |

"Until I go idle" arms the idle clock, not the wall clock. Say in the report that this clock restarts on every prompt and ends at twice the window. Every other route arms the wall clock: a fixed deadline that never moves.

## Report the real receipt

Run the genuine command and report its receipt line verbatim. The CLI calls the hold busy mode. That is DND. The proof is the receipt line, the `fno agents list` DND column, and the mux `[DND]` marker. Relay a nonzero exit or a `hold NOT off` line verbatim, and claim no DND state. Exit 3 is no provable harness identity.

## Scope

The hold applies to the session that runs the command. When the ask arrives by chat or mail, the king runs it on the king's own session. To quiet another session, run the door in that session.

## Not a pause

`fno agents loops pause-all` and the reign halts stop fleet work. They are not the answer to an operator who wants quiet. Codex invokes this skill as `$fno:dnd`.

## Known Limitations and Deferred Work

- No shell verb or help text names DND yet: the hold help says "Busy mode" and never DND. See [LIMITATIONS.md](LIMITATIONS.md).
