# Mail push-first delivery (turn-boundary drain + undelivered escalation)

The durable mail bus (`~/.fno/bus/messages.jsonl`) delivers on a pull model whose only drain point was `SessionStart`.
A long-lived session never restarts, so mail addressed to its handle sat unread for the life of that session (a 13.5h run never hit another `SessionStart`; mail queued the whole time).
This makes delivery push instead of pull, and closes the matching sender-side honesty gap.

## Two changes, one node

**Receive (push).** A new `UserPromptSubmit` hook, `hooks/inject-mail-notify.sh`, runs `fno mail notify-self` every turn and injects a one-line nudge as `additionalContext` when there is unread inbound mail.
`UserPromptSubmit` already fires every turn, so this is the retired interval-drain reborn at a boundary that already exists, with no daemon.

**Send (honesty).** The same `notify-self` invocation also surfaces the session's *own* sent mail that no recipient has claimed past a TTL, both as a turn-boundary line and as `sent unclaimed: N` in `fno mail status`.
Before this, `queued (durable)` was the last thing a sender ever heard, so silence read as delivered.

## `fno mail notify-self` (hidden verb)

Stat-only. It reuses `drain-self`'s identity path (`resolve_harness_identity` -> `canonical_handle` -> `scan_unread`) but **never advances the consume cursor** - the load-bearing invariant.
A nudge is a notice, not a consume: `SessionStart`'s `drain-self` and the sender-side check must still see un-acted mail.
There is no notify-cursor; the consume cursor is the sole delivery marker, and its non-advancement is exactly what keeps the nudge persistent (re-injecting each turn while unread) and self-clearing the instant the agent drains.

- **Inbound:** unread envelopes addressed to my handle -> `N unread fno mail from <senders>: run \`fno mail unread\``. Senders are deterministic (first-seen), defanged, bounded (`X, Y, Z, +K more`).
- **Sent-unclaimed:** my sent mail still returned by `scan_unread(recipient)` (recipient's cursor has not passed it) AND strictly older than `config.inbox.unclaimed_ttl` (default 1800s) -> `N sent fno mail unclaimed (to <recipients>, >30m): recipient has not picked it up`. Computed live every call, so a just-consumed message stops being flagged immediately.

## Failure posture

Every path degrades to silence, never to a blocked turn: no harness identity -> no-op; `fno`/`jq` missing -> hook no-op; a recipient name `scan_unread` rejects (path-traversal guard) is skipped, never crashing the verb.
The `</system-reminder>` delimiter is defanged in every interpolated field before embedding.
The hook carries a portable 2s timeout and always exits 0.

## Scope

Bus/handle lane only. Project-inbox markdown delivery honesty and liveness detection (a non-mesh session invisible to the bus) are out of scope.
