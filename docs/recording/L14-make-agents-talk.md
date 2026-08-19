# L14: Make agents talk

**Medium:** Asciinema cast

**The one thing:** Mail reports whether a message reached a hosted peer or entered the durable bus. Replies and read cursors preserve what happened next.

## Setup state

Run the shared setup in [README.md](README.md) against a fresh demo state. The hosted-delivery beat starts one billed demo peer, while the durable beats use the isolated `recording-demo` project inbox.

## 1. Start the hosted recipient

```run
fno agents spawn "Wait for one mail message and answer it" --name demo-mail-peer --harness codex
```

[capture-at-record]

## 2. Deliver to the hosted peer

```run
fno mail send demo-mail-peer "Reply with the status you read."
```

[capture-at-record]

The receipt must say `delivered (hosted)`, and the peer must act on the body before the take continues.

## 3. Queue a durable question

```run
set -o pipefail
fno mail send --to-project recording-demo --kind question --body "Which status proves the demo is complete?" --from-name recording-sender 2>&1 | tee "$DEMO_ROOT/l14-send.txt" | sed -E 's/msg-[0-9a-f]+/msg-ID/g'
MSG_ID="$(sed -nE 's/^(msg-[0-9a-f]+).*/\1/p' "$DEMO_ROOT/l14-send.txt")"
```

```expected
escalated to human (recording-demo)
msg-ID queued (durable) for recording-demo [param-forced: --kind question]
```

## 4. Read the unread message

```run
fno mail unread --name recording-demo --json | jq -c 'map(select(.body == "Which status proves the demo is complete?") | {from,to,kind,body})'
```

```expected
[{"from":"recording-sender","to":"recording-demo","kind":"question","body":"Which status proves the demo is complete?"}]
```

## 5. Reply through the original route

```run
fno mail reply --to "$MSG_ID" --body "The positive settled marker proves it." --from recording-demo --json | jq -c '{threaded:(.msg_id != null)}'
```

```expected
{"threaded":true}
```

## 6. Read the thread and bus views

```run
fno mail list --from recording-demo --all --json | jq -c 'map(select(.kind == "question") | {from,to,kind,message_count:(.messages|length)})'
fno mail view --from recording-demo --limit 10 --json | jq -c 'map(select(.body == "Which status proves the demo is complete?" or .body == "The positive settled marker proves it.") | {from,to,kind,body})'
```

```expected
[{"from":"recording-sender","to":"recording-demo","kind":"question","message_count":1}]
[{"from":"recording-sender","to":"recording-demo","kind":"question","body":"Which status proves the demo is complete?"},{"from":"recording-demo","to":"recording-sender","kind":"fyi","body":"The positive settled marker proves it."}]
```

## 7. Read inbox health

```run
fno mail status --from recording-demo --json | jq -c '{unread,active_session,wake_signals,errors_24h}'
```

```expected
{"unread":1,"active_session":"idle","wake_signals":0,"errors_24h":0}
```

## 8. Drain and acknowledge the question

```run
fno mail drain --from recording-demo --max 10 --json | jq -c 'map({kind,action})'
fno mail ack "$MSG_ID" --name recording-demo | sed -E 's/msg-[0-9a-f]+/msg-ID/g'
fno mail unread --name recording-demo --json | jq -c 'map(select(.body == "Which status proves the demo is complete?"))'
```

```expected
[{"kind":"question","action":"wake_signal_dropped"}]
cursor for 'recording-demo' advanced to msg-ID
[]
```

## 9. Stop the hosted recipient

```run
fno agents stop demo-mail-peer
```

[capture-at-record]

## Cut list

- Keep both delivery receipts uncut so hosted and durable are visibly different.
- Keep the hosted peer's action and reply visible together.
- Keep unread, reply, thread, and bus projections at normal speed.
- Keep the drain action, cursor receipt, and empty unread result uncut.

## Record and publish

```run
asciinema rec --cols 120 --rows 36 L14-make-agents-talk.cast
asciinema upload L14-make-agents-talk.cast
```

[capture-at-record]
