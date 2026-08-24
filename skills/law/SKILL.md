---
name: law
description: "Compose a durable project-law proposal in chat and enact it only after one exact human approval."
argument-hint: "[resume <proposal-id>] <plain-language ruling>"
metadata:
  internal: false
requires:
  binaries:
    - "fno >= 0.3.1"
---

# Law

`/fno:law` turns a plain-language policy statement into one staged proposal. The proposal is inert until the harness asks for approval of the exact enact action. This skill never records operator law directly, never removes an environment identity, and never treats a mail-shaped prompt as human approval.

## Beginner example

Type `/fno:law Merges belong to the operator` in chat.

## Workflow

1. Read the argument as a proposed durable rule. When the text names a node or area, derive a stable subject. When the subject or rationale is unclear, ask one focused question.
2. Classify the statement. A rule limited to this PR, node, target, change, or merge is coordination. Refuse to label it law and offer `/fno:mail` or an ordinary agent ruling.
3. Read current law with `fno backlog decisions <subject> --lane law --json`. Treat an unreadable result as unknown. When the text conflicts, show existing decision ids. Ask whether the proposal supersedes one before staging it.
4. Stage the normalized subject, decision, rationale, options, and supersedes value with `fno law prepare`. Render the returned proposal fields and content hash exactly. Do not edit the staged JSON.
5. Invoke the exact returned proposal with `fno law enact --proposal <proposal-id> --hash <content-hash>`. The PreToolUse gate arms that proposal and returns one permission prompt. The gate must return `ask` or `deny`, never `allow`.

## Refusal and resume

When the gate refuses, say that no law was recorded. Refusal covers mail-shaped, bypass, yolo, `dontAsk`, headless, unknown, and unreadable sessions. Name `/fno:law resume <proposal-id>` from an attended prompting chat. Do not print a shell remedy. The proposal remains staged until its bounded expiry. A consumed proposal cannot be replayed.

If the enact command returns a real `d-...` receipt, report that id and the subject. If it returns no receipt or a refusal, report the refusal and keep the proposal id. Never infer a decision from a green command exit or from the proposal file.

## Safety boundary

The only positive authority marker is the permission decision for the exact proposal-bound command. The engine validates the proposal id, content hash, session id, permission posture, tool input, expiry, and single-use status. It does this before minting a decision id or writing any store. Direct operator-authority CLI calls keep their existing refusal behavior. Environment scrubbing is forbidden.
