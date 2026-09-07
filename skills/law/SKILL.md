---
name: law
description: "Record a durable project law from chat in one step."
argument-hint: "<plain-language ruling>"
metadata:
  internal: false
  requires:
    binaries:
      - "fno >= 0.3.1"
---

# Law

`/fno:law` records a durable project law in one step. The operator types the ruling. A `d-` id comes back in the same turn. There is no staged proposal, no hash, no approval receipt, and no resume path.

A recording made from chat carries `authority_source: chat_attested`. It reads in the law lane. It is never recorded as `operator`. So a reader can always tell a chat recording from a person at a terminal. `fno inbox law set` is the same destination for an operator already at a terminal. It is the same one call.

## Beginner example

Type `/fno:law Merges belong to the operator` in chat. A `d-` id comes back.

## Workflow

1. Read the argument as a proposed durable rule. When the text names an area, query it and use the returned `canonical_subject`. A subject matches exactly. A near-synonym does not. When the subject or the rationale is genuinely unclear, ask one focused question. Do not ask for permission you already have. The operator typing the line IS the invocation.
2. Classify the statement. A rule limited to this PR, node, target, change, or merge is coordination. Refuse to label it law. Offer `/fno:mail` or an ordinary agent ruling instead. `fno inbox law set` refuses those markers itself, so a slip is caught rather than recorded.
3. Read current law with `fno inbox decisions <subject> --lane law --state live --json`. Treat an unreadable result as unknown. Continue only from `current_law.status`. `single` names the decision this ruling can supersede. `conflict` requires every id and a resolution first. `none` proves only that this canonical subject has no current law.
4. Record it:

```bash
fno inbox law set "<subject>" "<decision>" --rationale "<why>" [--supersedes d-xxxxxxxx]
```

The command prints the `d-` id on stdout. Report that id and the subject.

## What this store does and does not reach

This store is machine-local. A stranger who clones the repository must obey some rules. Those rules do not reach that person from here. Land them in the code, a doc, or a gate, in a PR. When the operator wants the rule recalled by subject, record it here as well.

Recording law from chat works. Retracting it does not. On a law-lane row, `retract_decision` requires `authority_source` to be exactly `operator`. So a chat-recorded law needs an attended terminal to withdraw. Supersession follows the same line. A chat recording can supersede another `chat_attested` row. It cannot supersede an `operator` row. Every live-state reader then stops seeing the operator's law.

## Refusals

`fno inbox law set` refuses, records nothing, and exits **3** in five cases:

- the statement is coordination rather than durable law,
- the rationale is missing,
- `--supersedes` is not a `d-` decision id, or names no recoverable decision,
- `--graduation` is not a valid kind,
- no harness session resolves and no terminal is attached, so nothing marks a decider.

Exit **1** means something else entirely. It means the row IS recorded and the recall index write failed. Run `fno backlog decide-reindex`. Never re-run the command.

Report the refusal verbatim. Say that no law was recorded. Never infer a recording from a green exit. Read the printed `d-` id.

## Safety boundary

Invocation is the authority. The door is the law LANE, not the `operator` value. Any process descended from a harness session records as `chat_attested`. That includes an agent's own Bash call. A mail-injected slash command is a special case of it. It is the case that is impossible to detect. An `operator` row stays refused from any session. That trade is deliberate. [LIMITATIONS.md](LIMITATIONS.md) states it with the measurement behind it. What the skill still guarantees is honest attribution. A chat recording says `chat_attested` and never claims the operator lane. Environment scrubbing stays forbidden.

## Known Limitations and Deferred Work

- See [LIMITATIONS.md](LIMITATIONS.md).
