---
name: groom
description: "Daily levers-only backlog grooming pass. Reads the graph, merged PRs, and the triage pile, applies a fixed allowlist of reversible levers, and mails a one-screen report. Use when: 'groom the backlog', 'daily grooming pass', 'clean up the backlog', dispatched by `fno backlog groom`."
---

# Groom

One short pass a day over the backlog.
You consume the exhaust the fleet produces: near-duplicate review-harvest nodes, stranded children holding epics open, ready nodes nobody will ever pick up.

Your entire output is graph mutations plus one mailed report.
You do not write code, you do not open PRs, and you do not plan features.

## Step 0: read today's proposals

The dispatcher already ran the mechanical pass (archive, reconcile, `maintain --apply`, relatedness build) under today's claim, before you were spawned.
Those legs applied only their deterministic work; `maintain`'s dedup and stale legs stay proposal-only regardless of `--apply`, and those proposals are your input.

Start every pass by re-deriving them from the live graph:

```bash
fno backlog maintain
```

Read-only, and fresh by construction - there is no intermediate file to go stale, which is why this pass re-derives rather than reading one.
What comes back is today's judgment-required set: near-duplicate families to consider superseding, stale ideas to consider deferring.
Work it with the levers below; anything it proposes that you cannot decide from the evidence goes to the pile as a question.

## The levers (allowlist - nothing else)

You may use ONLY these verbs.
Each is reversible or additive, which is what makes an unattended daily pass safe.

| Lever | Use it for |
|-------|-----------|
| `fno backlog supersede <new-id> --replaces <old-id> --reason "..."` | A node genuinely replaced by another (dedup families, retitles) |
| `fno backlog defer <id> --reason "..."` | Work that should stop being selected, with the why recorded |
| `fno backlog undefer <id>` | A deferred node whose blocker is demonstrably gone |
| `fno backlog update <id> --priority <p0..p3>` | A priority the evidence contradicts |
| `fno backlog rank <id> --top` | Float a card that should run next within its lane |
| `fno backlog idea "..."` | File genuinely NEW follow-up work you noticed |
| `fno backlog intake <plan-path>` | Promote a demonstrably blueprint-complete plan into a tracked node |
| `fno backlog update <id> --blocked-by <ids>` | Encode the order a track must run in (see Auto-convene) |

The table is the whole contract: if an action you want is not on it, it is not yours to take - file the question instead.

**Never** edit `~/.fno/graph.json`, its Kanban sibling, or any state file directly - not with Edit, Write, `jq -i`, or `sed -i`.
Every mutation goes through a lever above so it lands with a receipt.
A `PreToolUse` hook blocks direct edits as a backstop, but the rule is yours to keep, not the hook's to enforce.

## What you read

- Today's mechanical pass: the dispatcher's per-leg outcomes, and `fno backlog maintain` (read-only) for the proposals it deliberately left to you (Step 0).
- The graph: `fno backlog find`, `fno backlog get <id>`, and the triage pile (`deferred` nodes with their `deferred_reason`).
- Recently merged PRs, to catch nodes whose work landed but never closed.
- Starvation receipts and any guard exclusions from the selection path.

## The decision rule

Act only where the pattern is unambiguous.
A dedup family with an identical title stem and one merged member is unambiguous.
A p1 that has sat untouched for a month is a question, not a call.

**Anything you cannot decide from the evidence goes to the triage pile as a one-line question, never a guess.**
The pile IS the `deferred` status - a node is in it because it carries a `deferred_reason`, not because it exists.
So park an undecidable node with the question as its reason:

```bash
fno backlog defer <id> --reason "question: <the one line you need answered>"
```

Use `fno backlog idea` only for genuinely new work you noticed in passing; an idea-status node is NOT in the pile and will not surface in the triage view.
The pile is the pressure-release valve that lets grooming stay unattended: an honest question costs a line, a wrong supersede costs real work.

**Auto-convene:** when a track has several `ready` nodes but no encoded order (no `blocked_by` edges expressing the sequence a human clearly intended), you MAY encode that order yourself with `fno backlog update <id> --blocked-by <ids>`, one edge per node whose predecessor is unambiguous.
Its entire output is `blocked_by` edges plus a line each in the report.
Do this at most once per run, and only for a track where the ready depth is real and the intended order is evident from the plan - an order you would have to guess is a question for the pile, not an edge.

## The report

Finish by mailing one screen - if it does not fit on a screen, you are reporting too much.

```bash
fno mail send --to-project fno --kind fyi "groom <YYYY-MM-DD>" --body-file <report>
```

The report carries, in this order:

0. **Mechanical** - one leading line itemizing every leg of the dispatcher's pass by name with its outcome, e.g. `Mechanical: archive ok, reconcile ok, maintain ok, relatedness failed: 1: ...`. Your seed brief carries these verbatim; report them as given. Name all four legs every time; an aggregate count alone hides which one broke. Anything other than `ok` (`failed:` or `partial:`) also belongs under **Anomalies** - this line is the only signal an operator gets that a leg has quietly stopped working, and a nightly job that degrades unnoticed is what this pipeline was built to prevent.
1. **Mutations** - every lever you pulled, one line each, with its receipt (node id + what changed).
2. **Pile** - what is in the triage pile now, and what you added to it today.
3. **Anomalies** - starvation receipts, guard exclusions, anything that looks wrong but was not yours to fix.
4. **Net mint rate** - nodes opened minus nodes closed today, so the trend is visible without asking.

A day with no defensible action is a valid day: report "no action", exit 0, mutate nothing.
Still send the report, and still lead it with the Mechanical line - a quiet night and a night the pass never ran look identical otherwise.

## Boundaries

- **Empty delta day** - nothing changed since the last pass: report "no action", exit 0.
- **Dies mid-run** - every lever is an atomic CLI verb, so a partial run leaves only completed mutations, each with its receipt. Report what completed; do not attempt to unwind.
- **Second run same day** - `fno backlog groom` dedups on a daily claim, so this should not happen. If you were invoked anyway, check the day's report was already sent before acting.
