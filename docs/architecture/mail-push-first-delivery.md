# Mail push-first delivery (active-turn drain + undelivered escalation)

The durable mail bus (`~/.fno/bus/messages.jsonl`) delivers on a pull model whose only drain point was `SessionStart`.
A long-lived session never restarts, so mail addressed to its handle sat unread for the life of that session (a 13.5h run never hit another `SessionStart`; mail queued the whole time).
This makes delivery push instead of pull, and closes the matching sender-side honesty gap.

## Two changes, one node

**Receive (push).** The `UserPromptSubmit` hook, `hooks/inject-mail-notify.sh`, gates cheaply (session identity, bus-log stat, binary on PATH, the one load-aware hook budget) and then relays the envelope the native verb `fno-agents mail-notify-self --bus-dir <bus> --session <sid>` prints, through fd 3 and byte-for-byte, as `additionalContext`.
The payload includes each message id and `fno agents mail reply --to <id>` guidance, so the recipient sees the mail without discovering or running a drain command.
`UserPromptSubmit` already fires every turn, so delivery uses an active boundary that already exists and adds no daemon or poll loop.

**Send (honesty).** The turn-boundary render no longer computes the sent-unclaimed nag: the cancelled Python run never reached that scan once, so the native port drops it and `fno agents mail status` keeps the signal (`sent unclaimed: N`).
Before the push boundary existed, `queued (durable)` was the last thing a sender ever heard, so silence read as delivered; the turn-boundary delivery is what closed that, not the nag line.

## `fno-agents mail-notify-self` (native hook-output verb)

The first implementation shelled from the hook to the Python `fno agents mail notify-self`.
The interpreter start alone outran the hook budget on a loaded box: 46 of 46 recorded runs were cancelled at the budget and nothing ever delivered.
The verb is native Rust now (`crates/fno-agents/src/mail_notify_self.rs`), dispatched from `client.rs` before the tokio runtime builds, so a fire answers in microseconds; the shell layer is a cheap gate around it.
The Python leg is deleted with the boundary it served: `fno agents mail notify-self` is no longer a verb, so the Rust verb is the only renderer for this payload and the two can no longer drift.

The verb reuses `drain-self`'s identity path (`canonical_handle` -> unread scan) and the same forward-only consume cursor.
It renders the complete `UserPromptSubmit` JSON envelope in Rust, writes and flushes that envelope, and only then advances the cursor through the last rendered message.
The shell hook relays the already-valid JSON directly, so no command substitution or second serializer can acknowledge mail before the final hook payload exists.
A live hold (idle or wall) short-circuits before any render or ack, so a busy turn's mail stays pending for the next boundary.
There is no notify cursor: `SessionStart` and `UserPromptSubmit` race on the one canonical cursor, and whichever successfully drains first makes the other silent.

- **Inbound:** unread envelopes addressed to the canonical session handle -> complete bodies, ids, and reply guidance inside a hook-owned `<system-reminder>` frame, followed by acknowledgement after flush.
- **Sent-unclaimed:** no longer part of the boundary path. The status verb still computes it (hosted and durable sends not yet proven `landed`, aged past `config.inbox.unclaimed_ttl`, default 1800s) and reports `sent unclaimed: N`. Stat-only; advances no recipient cursor. Porting the scan back into the boundary render is open follow-up work.

## Failure posture

Every path degrades to silence, never to a blocked turn: no harness identity -> no-op; `fno-agents` missing -> hook no-op; a recipient name rejected by the cursor path guard is skipped instead of crashing the verb.
The `</system-reminder>` delimiter is defanged across the complete untrusted mail render before embedding.
The hook bounds the verb at the one load-aware hook budget (`scripts/lib/hook-budget.sh`): the normal tier, shortened to 1s under fleet load, and past the load threshold the run is skipped entirely so a killed mid-ack render cannot lose mail; skipped mail stays pending.
The hook always exits 0.
A miss (timeout 124, identity refusal, crash, or rc 127: jq missing, so the session id was unreadable) records a `mail_notify_self_missed` event row naming the rc and stderr tail, so a delivery gap is diagnosable instead of silent; the other gate skips (no identity, no bus log, no binary) record nothing, because there was nothing to deliver.
A rendering, serialization, write, flush, or process failure before acknowledgement leaves the cursor unchanged, so the next active-turn or SessionStart boundary can repeat the message instead of losing it.
The achievable guarantee is therefore at-least-once display around process failure: a crash may repeat mail, but successful output-before-ack prevents permanent loss.

## Bus-only recipients

Live injection is a bracketed paste into the recipient's input buffer. For a worker that is correct: a BUSY recipient records the paste as a submit-time queue-operation row. It reads the row at its next turn boundary (pinned at `crates/fno-agents/src/mail_inject.rs`). For a session with a human at the keyboard it is a defect. The paste lands mid-sentence in the box the operator is typing into. Two live specimens on 2026-08-14, both inside the operator's own reports of the bug.

The fix is a recipient-level delivery policy, not a heuristic: `delivery_policy: bus-only` on the agents registry row.

- **Who sets it:** the session itself, once: `fno agents register --delivery-policy bus-only` (in the human-attended session). `--delivery-policy off` clears it. A later flagless re-register preserves the stamp. The re-firing SessionStart hook cannot silently revert the recipient to injectable.
- **What senders see:** `queued (durable) for <handle> [DND (bus-only): recipient polls the bus at each turn boundary]` on the name, job, and registered-agent lanes. No recovery warning, no send-time escalation. The queue is designed, not stranded, and it drains through this doc's own turn-boundary push (`fno-agents mail-notify-self`).
- **What never happens:** a prompt-line paste, on any lane. The gate lives inside the three shared injectors (`_mail_inject_claude`, `_mail_inject_codex`, `_mux_pane_send` in `cli/src/fno/agents/dispatch.py`). Name, reply, job, project, raw, dispatch, ask, and annotate lanes inherit it rather than remembering to check.
- **The raw lane:** `--raw` never queues durable, so a raw send to a bus-only recipient refuses non-zero (`refused: ... is DND (delivery-policy bus-only)`). `--check` answers `not-injectable` naming the policy.
- **The naming rule:** bus-only is a DELIVERY-POLICY fact, never a liveness verdict. A bus-only session can be alive and mid-turn. It just belongs on the bus. This is the same distinction that renamed `NOT_INJECTABLE` off "not-live" (see `mail_inject.rs`).

### Timed hold release

When a timed hold ends, one live turn carries a framing line with the message count, local sent-time range, and held duration. Rust renders one sender/id header per original message, oldest first. Each header has a summary capped at 12 words. The body follows on its original lines. The recipient harness's mail-header capability selects mention or plain sender form. After confirmed delivery, the receiver records one `agent_mail_drained` receipt per original message. A missed delivery leaves the source cursors unchanged for the next turn boundary.

## Scope

Bus/handle lane only. Project-inbox markdown delivery honesty and liveness detection (a non-mesh session invisible to the bus) are out of scope.
