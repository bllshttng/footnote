---
name: growth-launch
description: Growth-studio launch orchestrator. Drafts a four-role campaign bundle against verified product truth, runs the factual/brand/accessibility evaluators, and holds at a founder approval gate. Refuses outright when growth-studio is not activated. Dispatches no external effect.
pack: growth-studio
---

# growth-launch

Orchestrate one growth campaign through the four growth-studio role subagents,
evaluate every draft, and hold the bundle at a founder approval gate.
This skill drafts and reviews; it never publishes.
The mechanical guards that make that true are the subagents' bounded tool lists
(no Bash, no Task, no WebSearch, no WebFetch, no MCP tool) and the unchanged
approval authority; this prose is documentation, not a control.

The flow is fixed-round, never convergent: exactly one draft round and at most
one revision round.
There is no "until green" condition anywhere, so a persistently failing draft
costs a bounded two dispatches and then stops.

## 1. Activation gate

Run `fno plugins ls`.
If growth-studio is absent or not active, print exactly this line and stop with
a non-zero exit, creating no campaign directory and falling back to nothing:

    fno plugins activate plugins/growth-studio/plugin.yaml

A failing or non-zero `fno plugins ls` is a refusal, never an assume-active.
Print the underlying error and stop.
This gate is the whole reason the faucet is gated: bundling made the skill
present unconditionally at session start, activation is what makes it permitted.

## 2. Objective intake

Take the free-text objective and a campaign slug from the invocation.
Create `.fno/campaigns/<slug>/`.
If that directory already exists, refuse and name it rather than interleaving
into a prior campaign.

Resolve the two artifacts the roles read, in this order:

- `product-truth`: the path the project configured under `[context.artifacts]`
  in `.fno/config.toml`. Read it with `fno roles context --json` and take the
  `product-truth` entry's `provenance`. If none is configured, stop and tell the
  founder to configure it (resolution would block with MISSING_CONTEXT).
- `brand-voice`: the pack-supplied `plugins/growth-studio/assets/brand-voice.md`.

## 3. One draft round

Dispatch all four role subagents concurrently via the Task tool:

- `@fno:growth-marketer` producing `campaign-plan.md`
- `@fno:growth-comms` producing `press-draft.md`
- `@fno:growth-designer` producing `rendered-mock.md` (with alt text and a
  contrast note)
- `@fno:growth-social` producing `social-post.md` and `social-calendar.md`

Hand each subagent the objective, the campaign directory, and the two asset
paths.
One round.
A subagent that returns `FAILED` or `BLOCKED` is recorded as such beside its
draft and the campaign continues to a partial bundle; it does not abort the
other three.

## 4. One evaluation round

Run the three evaluator scripts over each returned draft, from the pack
directory so the pack-relative paths resolve:

- `bash evaluators/factual-check.sh <draft> <product-truth-path>`
- `bash evaluators/brand-check.sh <draft>`
- `bash evaluators/accessibility-check.sh <draft>` (design role only)

Write each verdict as a JSON evidence file beside its draft, for example
`campaign-plan.factual.json` and `campaign-plan.brand.json`, recording
`{"evaluator": ..., "passed": true|false, "detail": "..."}`.
Accessibility review runs only against the design role's rendered mock.

## 5. At most one revision round

Re-dispatch only the subagents whose draft failed an evaluator, with the
verdict attached to the brief.
Exactly one retry.
A second failure is recorded as a failed draft and excluded from the
approvable set; it is never retried.
This is the bounded answer to an unbounded review loop: two constants, no
convergence.

## 6. Founder approval gate

Print the bundle inventory, the per-draft evidence status, and one sentence
naming exactly what approval would authorize and what it would not.
The terminal state is `approved-draft-bundle`.
Do not dispatch any effect.
The skill holds no tool that could: the subagents cannot publish, and this
orchestrator only composes their drafts and the evaluator verdicts.

## Exit states

- Refusal, pack not active: the activate line, non-zero exit, no campaign dir.
- Partial: one or more subagents returned FAILED or BLOCKED; the inventory
  prints per-role status and the campaign continues. Exit zero with a partial
  banner, because a three-of-four bundle is a real deliverable.
- Evidence failure after the single revision round: the draft is listed with its
  failing verdict and excluded from the approvable set. Exit zero; the founder
  decides.
- Success: the bundle inventory, per-draft evidence, and the approval prompt.
  Terminal state `approved-draft-bundle`.
