---
name: harness-verbs
description: Run this before you run or mail a harness-native command. Renders your harness's native verb roster (verb, risk class, when to use) through fno-agents verbs, and teaches the two raw-mail rules. Works on every harness footnote drives.
---

# Know your own harness before you run or mail a native command

footnote drives several coding CLIs (claude, codex, opencode, agy, pi, cursor-agent, grok). Each has its own native slash verbs. The measured roster, each verb's risk class, and a one-line when-to-use live in the capability table; `fno-agents verbs` renders your row of it.

## Run this first

```bash
fno-agents verbs
```

Bare form detects your own harness from the session env. On a refusal, pass the name: `fno-agents verbs claude`. `--json` for machines.

Read the roster before you reach for a native verb you have not run in this harness. The risk column is the mail guard's own vocabulary: `safe`, `context-destroying` (clears or swaps the session's context), `session-ending` (exits or logs out).

## The two raw-mail rules

`fno agents mail send --raw` runs a command on another session, as user-shaped text. Two rules:

1. `--raw` is commands only, never a message. A message goes wrapped (the plain send wraps it; the sender stays visible).
2. A `session-ending` or `context-destroying` verb is refused by the mail guard unless the send names it: `--ack-verb-risk <verb>`. The refusal names the risk class and the remedy. An acked send still carries the verb's full effect. The ack is an acknowledgment, not a safety net.

## Pitfalls

Harness-specific failure modes (symptom, cause, fix) are tabled per harness in `docs/HARNESSES.md`. Read your harness's table before debugging anything harness-specific.

## Honesty

The table says which rows are vendor-measured and which are name-derived, right beside the data. Never claim a verb does what the table does not say.
