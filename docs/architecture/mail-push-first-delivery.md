# Mail push-first delivery (active-turn drain + undelivered escalation)

The durable mail bus (`~/.fno/bus/messages.jsonl`) delivers on a pull model whose only drain point was `SessionStart`.
A long-lived session never restarts, so mail addressed to its handle sat unread for the life of that session (a 13.5h run never hit another `SessionStart`; mail queued the whole time).
This makes delivery push instead of pull, and closes the matching sender-side honesty gap.

## Two changes, one node

**Receive (push).** The `UserPromptSubmit` hook, `hooks/inject-mail-notify.sh`, runs `fno agents mail notify-self` every turn and relays the complete framed durable messages as `additionalContext`.
The payload includes each message id and `fno agents mail reply --to <id>` guidance, so the recipient sees the mail without discovering or running a drain command.
`UserPromptSubmit` already fires every turn, so delivery uses an active boundary that already exists and adds no daemon or poll loop.

**Send (honesty).** The same `notify-self` invocation also surfaces the session's *own* sent mail that no recipient has claimed past a TTL, both as a turn-boundary line and as `sent unclaimed: N` in `fno agents mail status`.
Before this, `queued (durable)` was the last thing a sender ever heard, so silence read as delivered.

## `fno agents mail notify-self` (hidden hook-output verb)

The verb reuses `drain-self`'s identity path (`resolve_harness_identity` -> `canonical_handle` -> `scan_unread`) and the same forward-only consume cursor.
It renders the complete `UserPromptSubmit` JSON envelope in the CLI, writes and flushes that envelope, and only then advances the cursor through the last rendered message.
The shell hook relays the already-valid JSON directly, so no command substitution or second serializer can acknowledge mail before the final hook payload exists.
There is no notify cursor: `SessionStart` and `UserPromptSubmit` race on the one canonical cursor, and whichever successfully drains first makes the other silent.

- **Inbound:** unread envelopes addressed to the canonical session handle -> complete bodies, ids, and reply guidance inside a hook-owned `<system-reminder>` frame, followed by acknowledgement after flush.
- **Sent-unclaimed:** my sent mail still returned by `scan_unread(recipient)` (recipient's cursor has not passed it) AND strictly older than `config.inbox.unclaimed_ttl` (default 1800s) -> `N sent fno agents mail unclaimed (to <recipients>, >30m): recipient has not picked it up`. Computed live every call, so a just-consumed message stops being flagged immediately.

Sent-unclaimed reporting remains stat-only and advances no recipient cursor.

## Failure posture

Every path degrades to silence, never to a blocked turn: no harness identity -> no-op; `fno` missing -> hook no-op; a recipient name rejected by the cursor path guard is skipped instead of crashing the verb.
The `</system-reminder>` delimiter is defanged across the complete untrusted mail render before embedding.
The hook carries a portable 2s timeout and always exits 0.
A rendering, serialization, write, flush, or process failure before acknowledgement leaves the cursor unchanged, so the next active-turn or SessionStart boundary can repeat the message instead of losing it.
The achievable guarantee is therefore at-least-once display around process failure: a crash may repeat mail, but successful output-before-ack prevents permanent loss.

## Bus-only recipients

Live injection is a bracketed paste into the recipient's input buffer. For a worker that is correct: a BUSY recipient records the paste as a submit-time queue-operation row. It reads the row at its next turn boundary (pinned at `crates/fno-agents/src/mail_inject.rs`). For a session with a human at the keyboard it is a defect. The paste lands mid-sentence in the box the operator is typing into. Two live specimens on 2026-08-14, both inside the operator's own reports of the bug.

The fix is a recipient-level delivery policy, not a heuristic: `delivery_policy: bus-only` on the agents registry row.

- **Who sets it:** the session itself, once: `fno agents register --delivery-policy bus-only` (in the human-attended session). `--delivery-policy off` clears it. A later flagless re-register preserves the stamp. The re-firing SessionStart hook cannot silently revert the recipient to injectable.
- **What senders see:** `queued (durable) for <handle> [bus-only: recipient polls the bus at each turn boundary]` on the name, job, and registered-agent lanes. No recovery warning, no send-time escalation. The queue is designed, not stranded, and it drains through this doc's own `notify-self` push at each turn boundary.
- **What never happens:** a prompt-line paste, on any lane. The gate lives inside the three shared injectors (`_mail_inject_claude`, `_mail_inject_codex`, `_mux_pane_send` in `cli/src/fno/agents/dispatch.py`). Name, reply, job, project, raw, dispatch, ask, and annotate lanes inherit it rather than remembering to check.
- **The raw lane:** `--raw` never queues durable, so a raw send to a bus-only recipient refuses non-zero (`refused: ... has delivery-policy bus-only`). `--check` answers `not-injectable` naming the policy.
- **The naming rule:** bus-only is a DELIVERY-POLICY fact, never a liveness verdict. A bus-only session can be alive and mid-turn. It just belongs on the bus. This is the same distinction that renamed `NOT_INJECTABLE` off "not-live" (see `mail_inject.rs`).

## Scope

Bus/handle lane only. Project-inbox markdown delivery honesty and liveness detection (a non-mesh session invisible to the bus) are out of scope.
