# Mail socket drain (claude last hop)

Mail delivered as an injected user turn impersonates the operator. The recipient's chat box cannot tell an agent from the human. Every instrument that counts user messages has the same blindness. This doc records the claude-only fix and the harness-independent half that shipped with it.

## The harness-independent half: readers bucket `<fno_mail>` turns

Every fno mail on every harness carries the `<fno_mail>` envelope in the body. So footnote's own transcript readers can separate agent traffic from operator traffic. No transport change is needed:

- `fno cost` buckets a `<fno_mail>` user turn as `mail_messages`, not `user_messages`. User-message statistics stop counting fleet chatter as the human.
- Autobrief extractors (`_claude_pairs`, `_codex_pairs`, `_opencode_pairs`) skip `<fno_mail>` turns. Peer mail is never ingested as conversation.
- `fno agents peek` labels those turns `role=peer` across the claude, codex and opencode readers.
- `session_truth` classifies the last non-peer record. A trailing peer mail turn no longer clears an operator question.

This covers codex, opencode and agy on the day it lands. Third-party report generators do not know the envelope exists. So this fixes only instruments footnote controls.

## The claude half: the drain posts to the session's own socket

Claude Code binds one Unix socket per session (`cc-socks/<pid>.sock`, newline-delimited JSON). A `user` frame written there is minted as a distinct `cross-session-message` kind, not a user turn. A foreign peer writing to that socket is held for operator approval. The hold expires after five minutes.

One exception exists, verified live. A process inside the session's own process tree is delivered, not held. Authentication uses the session's exported `CLAUDE_CODE_MESSAGING_TOKEN`. fno already runs hooks inside every session. A hook is own-child.

So the last hop belongs to the recipient. `fno agents mail drain-self` posts each drained message to `CLAUDE_CODE_MESSAGING_SOCKET`. The post is authenticated with `CLAUDE_CODE_MESSAGING_TOKEN`. The `<fno_mail>` body rides verbatim, keeping the reply address the socket frame lacks. A post failure falls back to the stdout render. The cursor still advances only after output, so a crash repeats rather than drops.

## Spawns take inbound: `crossSessionInbound: "accept"`

The shared spawn settings writer stamps `crossSessionInbound: "accept"` into every claude spawn settings file. The Python writer is `_write_settings_env_file`. The Rust writer is `write_scrub_settings`. A clean spawn with no model vars to floor still floats one.

An explicit `refuse` in project or user settings is never overridden. The stamp is skipped and no pointless file is written. The refuse check reads `<cwd>/.claude/settings.local.json`, then `<cwd>/.claude/settings.json`, then `~/.claude/settings.json`.

## What deliberately did not change

- `--raw` stays on the keystroke lane. A slash command arriving over the socket is inert. Firing verbs a model cannot self-invoke is the entire reason `--raw` exists. Keystrokes drive. Sockets inform.
- Senders never post to a foreign recipient's socket. A recipient without `accept` silently holds a foreign write and lets it expire. A "sent" receipt must never claim delivery on that evidence. The sender keeps the inject lane. The socket lane is the recipient's own drain.
- Live inject remains the primary claude send lane. Flipping it to bus plus drain needs a wake story for idle recipients first. Nothing fires `UserPromptSubmit` or `SessionStart` while a session idles.
- `send_to_session` reads the messaging token only from its explicit argument. The ambient env var carries the caller's own session token. A foreign sender passing its own token authenticates as nobody.
