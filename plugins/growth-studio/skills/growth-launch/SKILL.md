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
If growth-studio is absent or not active, print the activate command for this
pack's `plugin.yaml` and stop with a non-zero exit, creating no campaign
directory and falling back to nothing. The path is the pack's source manifest:
in this (dogfood) checkout that is `plugins/growth-studio/plugin.yaml`; in a
consuming project it is wherever that project installed the pack:

    fno plugins activate plugins/growth-studio/plugin.yaml

A failing or non-zero `fno plugins ls` is a refusal, never an assume-active.
Print the underlying error and stop.
This gate is the whole reason the faucet is gated: bundling made the skill
present unconditionally at session start, activation is what makes it permitted.

## 2. Objective intake

Take the free-text objective and a campaign slug from the invocation.
Reject the slug if it is empty or contains a path separator, a traversal
component (`..`), or a NUL: it becomes a filesystem path under
`.fno/campaigns/`. Create `.fno/campaigns/<slug>/`.
If that directory already exists, refuse and name it rather than interleaving
into a prior campaign.

Derive the activated pack's root from its receipt so paths resolve whether the
pack is the in-tree dogfood copy or an installed pack in another project:

```bash
PACK_ROOT="$(dirname "$(fno plugins inspect growth-studio --json \
  | python3 -c 'import json,sys;print(json.load(sys.stdin)["manifest_path"])')")"
```

The roles read three artifacts, all resolved through the project's context
catalog (`[context.artifacts]` in `.fno/config.toml`):

- `product-truth`: project-supplied product facts (sensitivity `public`).
- `brand-voice`: the pack-supplied voice contract. Voice is a property of the
  pack, so the project configures this entry to point at the pack's
  `$PACK_ROOT/assets/brand-voice.md` (sensitivity `internal`).
- `brand-identity`: project-supplied brand identity (the brand name, founder,
  and positioning a draft speaks from). A second consumer points this entry at
  its own identity file; the pack's `$PACK_ROOT/assets/brand-identity.md` is
  the Footnote dogfood default (sensitivity `internal`).

Build the catalog and confirm all three are present and readable:

```bash
fno roles context --json > .fno/campaigns/<slug>/catalog.json
```

If any of `product-truth`, `brand-voice`, or `brand-identity` is absent from
the catalog or `readable: false`, stop and name the missing artifact and its
config key (resolution would block with MISSING_CONTEXT or UNREADABLE_CONTEXT).

## 3. Resolution gate

Before dispatching any subagent, resolve each role for this work order so the
capability and context gates are enforced at launch, not bypassed. Stamp one
shared revision - the plugin role layer's snapshot revision - on the catalog,
the facts, and the resolve call so they agree (a mismatch blocks with
MIXED_REVISION):

```bash
REV="$(fno roles show marketing --json \
  | python3 -c 'import json,sys;d=json.load(sys.stdin);print(d[-1]["raw_definition"]["snapshot_revision"])')"
fno roles context --json --snapshot "$REV" > .fno/campaigns/<slug>/catalog.json
```

For each role, write a capability-facts JSON covering that role's
`required_capabilities` (marketing: `growth.draft`, `growth.research`;
communications: `growth.draft`, `growth.statement`; design: `growth.render`,
`growth.accessibility`; social: `growth.draft`, `growth.schedule`). The grant
is the founder's: running `/fno:growth-launch` authorizes this campaign's work
order, and each fact RECORDS that authorization (`source_id` names the founder's
launch, `available: true`); it does not assert a capability from the manifest.
If the work order is not authorized, refuse before dispatch. Each fact carries
every required field, scoped to this work order:

```json
[{"capability": "growth.draft", "available": true,
  "source_id": "founder-launch/<slug>", "snapshot_revision": "<REV>",
  "work_order_scope": {"node_id": "<slug>", "attempt_id": "<attempt>", "role_id": "<role>"}}]
```

Then resolve, passing the shared revision explicitly:

```bash
fno roles resolve <role> --work-order <slug> --attempt <attempt> --snapshot "$REV" \
  --capabilities .fno/campaigns/<slug>/<role>-capabilities.json \
  --context .fno/campaigns/<slug>/catalog.json --json
```

A role that returns `RoleResolutionBlocked` (`MISSING_CAPABILITY`,
`MISSING_CONTEXT`, or `UNREADABLE_CONTEXT`) is recorded as blocked and NOT
dispatched; the campaign continues with the roles that resolved. This is the
only place the gates can be enforced at launch: bundling made the subagents
present, activation permitted the roles, and this step proves the work order is
authorized and the context is configured before any draft is written.

## 4. One draft round

Dispatch the roles that resolved, concurrently via the Task tool:

- `@fno:growth-marketer` producing `campaign-plan.md`
- `@fno:growth-comms` producing `press-draft.md`
- `@fno:growth-designer` producing `rendered-mock.md` (with alt text and a
  contrast note)
- `@fno:growth-social` producing `social-post.md` and `social-calendar.md`

Hand each subagent only the asset paths its role resolved: design gets
brand-voice and brand-identity; marketing, communications, and social get
product-truth, brand-voice, and brand-identity.
One round.
A subagent that returns `FAILED` or `BLOCKED` is recorded as such beside its
draft and the campaign continues to a partial bundle; it does not abort the
other three.

## 5. One evaluation round

Run the three evaluator scripts over each returned draft, using the derived
pack root so the pack-relative script and asset paths resolve:

- `bash "$PACK_ROOT/evaluators/factual-check.sh" <draft> <product-truth-path>`
- `bash "$PACK_ROOT/evaluators/brand-check.sh" <draft>`
- `bash "$PACK_ROOT/evaluators/accessibility-check.sh" <draft>` (design role only)

Write each verdict as a JSON evidence file beside its draft, for example
`campaign-plan.factual.json` and `campaign-plan.brand.json`, recording
`{"evaluator": ..., "passed": true|false, "detail": "..."}`.
Accessibility review runs only against the design role's rendered mock.

## 6. At most one revision round

Re-dispatch only the subagents whose draft failed an evaluator, with the
verdict attached to the brief.
Exactly one retry.
A second failure is recorded as a failed draft and excluded from the
approvable set; it is never retried.
This is the bounded answer to an unbounded review loop: two constants, no
convergence.

## 7. Founder approval gate

Print the bundle inventory, the per-draft evidence status, and one sentence
naming exactly what approval would authorize and what it would not.
The run ends in an `approval-requested` state; it records no approval and
dispatches no effect. The founder's explicit approval is recorded out of band
and only then does the bundle become `approved-draft-bundle`.
The skill holds no tool that could dispatch: the subagents cannot publish, and
this orchestrator only composes their drafts and the evaluator verdicts.

## Exit states

- Refusal, pack not active: the activate line, non-zero exit, no campaign dir.
- Partial: one or more subagents returned FAILED or BLOCKED; the inventory
  prints per-role status and the campaign continues. Exit zero with a partial
  banner, because a three-of-four bundle is a real deliverable.
- Evidence failure after the single revision round: the draft is listed with its
  failing verdict and excluded from the approvable set. Exit zero; the founder
  decides.
- Success: the bundle inventory, per-draft evidence, and the approval prompt.
  Terminal state `approval-requested` (becomes `approved-draft-bundle` only on
  the founder's out-of-band approval).
