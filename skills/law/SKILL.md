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

`/fno:law` turns a plain-language policy statement into one staged proposal. The proposal is inert until the harness asks for approval of the exact enact action. This skill never records operator law, never removes an environment identity, and never treats a mail-shaped prompt as human approval.

## Beginner example

Type `/fno:law Merges belong to the operator` in chat.

## Workflow

1. Read the argument as a proposed durable rule. When the text names an area, query it. Use the returned `canonical_subject`. A checked-in alias is valid. An undeclared near-synonym is not. When the subject or rationale is unclear, ask one focused question.
2. Classify the statement. A rule limited to this PR, node, target, change, or merge is coordination. Refuse to label it law and offer `/fno:mail` or an ordinary agent ruling. A rule every clone needs is design law for `docs/architecture/decisions.yaml`. Stop and propose a reviewed catalog change instead of machine-local law.
3. Read current law with `fno backlog decisions <subject> --lane law --state live --json`. Treat an unreadable result as unknown. Continue only from `current_law.status`. `single` names the decision the proposal can supersede. `conflict` requires every id and a resolution first. `none` proves only that this canonical subject has no current law.
4. Stage the normalized subject, decision, rationale, options, and supersedes value with `fno inbox law prepare`. Render the returned proposal fields and content hash exactly. Do not edit the staged JSON.
5. Invoke the exact returned proposal with `fno inbox law enact --proposal <proposal-id> --hash <content-hash>`. The PreToolUse gate arms that proposal, binds a one-shot receipt into the approved tool input, and returns one permission prompt. The gate must return `ask` or `deny`, never `allow`.

## Refusal and resume

When the gate refuses, say that no law was recorded. Refusal covers mail-shaped, bypass, yolo, `dontAsk`, headless, unknown, and unreadable sessions. Name `/fno:law resume <proposal-id>` from an attended prompting chat. Do not print a shell remedy. The proposal remains staged until its bounded expiry. A consumed proposal cannot be replayed.

If the enact command returns a real `d-...` receipt, report that id and the subject. If it returns no receipt or a refusal, report the refusal and keep the proposal id. Never infer a decision from a green command exit or from the proposal file.

## Safety boundary

The only positive authority marker is the permission decision for the exact proposal-bound command. Arming is hook-only and is not exposed as a CLI command. The hook binds a one-shot receipt into the command input. The engine rejects enactment without that receipt before minting a decision id or writing any store. The engine also validates the proposal id, content hash, session id, permission posture, tool input, expiry, and single-use status. Direct operator-authority CLI calls keep their existing refusal behavior. Environment scrubbing is forbidden.

## Known Limitations and Deferred Work

- Claude Code is the only harness with the approval gate. Other harnesses can compose and resume proposals, but cannot enact them.
- UserPromptSubmit exposes no human-origin discriminator on the measured payload. The permission fallback remains the authority event until a trusted field exists.

- See [LIMITATIONS.md](LIMITATIONS.md).
