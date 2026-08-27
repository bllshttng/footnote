# Mail envelope filtering (fno_mail turns are not the operator)

Mail delivered as an injected user turn impersonates the operator. The recipient's chat box cannot tell an agent from the human. Every instrument that counts user messages has the same blindness. This doc records the portable fix: footnote's own transcript readers bucket `<fno_mail>` turns.

## Why the envelope, not a transport

Every fno mail on every harness carries the `<fno_mail>` envelope in the body. The envelope is advisory, inside the body, and invisible to anything that does not parse it. So the readers footnote controls can separate agent traffic from operator traffic today, with no transport change and no per-harness adapter.

## What filters

- `fno cost` buckets a `<fno_mail>` user turn as `mail_messages`, not `user_messages`. User-message statistics stop counting fleet chatter as the human. The JSON `messages` object gains a `mail` key.
- Autobrief extractors (`_claude_pairs`, `_codex_pairs`, `_opencode_pairs`) skip `<fno_mail>` turns. Peer mail is never ingested as conversation.
- `fno agents peek` labels those turns `role=peer` across the claude, codex and opencode readers.
- `session_truth` classifies the last non-peer record. A trailing peer mail turn no longer clears an operator question.

This covers codex, opencode and agy on the day it lands. Third-party report generators do not know the envelope exists. So this fixes only instruments footnote controls.

## Scope boundary

A per-harness socket lane (claude's messaging socket) can upgrade one driver from "marked in the body" to "a different message kind the harness itself frames". That work is deliberately NOT here. It serves one driver and needs its own verification. Landing it here gives one harness a first-class inbound kind. The others keep a transport that cannot tell an agent from the operator.
