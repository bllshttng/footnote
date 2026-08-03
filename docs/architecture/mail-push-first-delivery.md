# Mail push-first delivery (active-turn drain + undelivered escalation)

The durable mail bus (`~/.fno/bus/messages.jsonl`) delivers on a pull model whose only drain point was `SessionStart`.
A long-lived session never restarts, so mail addressed to its handle sat unread for the life of that session (a 13.5h run never hit another `SessionStart`; mail queued the whole time).
This makes delivery push instead of pull, and closes the matching sender-side honesty gap.

## Two changes, one node

**Receive (push).** The `UserPromptSubmit` hook, `hooks/inject-mail-notify.sh`, runs `fno mail notify-self` every turn and relays the complete framed durable messages as `additionalContext`.
The payload includes each message id and `fno mail reply --to <id>` guidance, so the recipient sees the mail without discovering or running a drain command.
`UserPromptSubmit` already fires every turn, so delivery uses an active boundary that already exists and adds no daemon or poll loop.

**Send (honesty).** The same `notify-self` invocation also surfaces the session's *own* sent mail that no recipient has claimed past a TTL, both as a turn-boundary line and as `sent unclaimed: N` in `fno mail status`.
Before this, `queued (durable)` was the last thing a sender ever heard, so silence read as delivered.

## `fno mail notify-self` (hidden hook-output verb)

The verb reuses `drain-self`'s identity path (`resolve_harness_identity` -> `canonical_handle` -> `scan_unread`) and the same forward-only consume cursor.
It renders the complete `UserPromptSubmit` JSON envelope in the CLI, writes and flushes that envelope, and only then advances the cursor through the last rendered message.
The shell hook relays the already-valid JSON directly, so no command substitution or second serializer can acknowledge mail before the final hook payload exists.
There is no notify cursor: `SessionStart` and `UserPromptSubmit` race on the one canonical cursor, and whichever successfully drains first makes the other silent.

- **Inbound:** unread envelopes addressed to the canonical session handle -> complete bodies, ids, and reply guidance inside a hook-owned `<system-reminder>` frame, followed by acknowledgement after flush.
- **Sent-unclaimed:** my sent mail still returned by `scan_unread(recipient)` (recipient's cursor has not passed it) AND strictly older than `config.inbox.unclaimed_ttl` (default 1800s) -> `N sent fno mail unclaimed (to <recipients>, >30m): recipient has not picked it up`. Computed live every call, so a just-consumed message stops being flagged immediately.

Sent-unclaimed reporting remains stat-only and advances no recipient cursor.

## Failure posture

Every path degrades to silence, never to a blocked turn: no harness identity -> no-op; `fno` missing -> hook no-op; a recipient name rejected by the cursor path guard is skipped instead of crashing the verb.
The `</system-reminder>` delimiter is defanged across the complete untrusted mail render before embedding.
The hook carries a portable 2s timeout and always exits 0.
A rendering, serialization, write, flush, or process failure before acknowledgement leaves the cursor unchanged, so the next active-turn or SessionStart boundary can repeat the message instead of losing it.
The achievable guarantee is therefore at-least-once display around process failure: a crash may repeat mail, but successful output-before-ack prevents permanent loss.

## Scope

Bus/handle lane only. Project-inbox markdown delivery honesty and liveness detection (a non-mesh session invisible to the bus) are out of scope.
