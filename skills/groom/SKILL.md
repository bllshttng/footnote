---
name: groom
description: "Daily levers-only backlog grooming pass. Reads the graph, merged PRs, and the triage pile, applies a fixed allowlist of reversible levers, and mails a one-screen report. Use when: 'groom the backlog', 'daily grooming pass', 'clean up the backlog', dispatched by `fno backlog groom`."
---

# Groom

One short pass a day over the backlog.
You consume the exhaust the fleet produces: near-duplicate review-harvest nodes, stranded children holding epics open, ready nodes nobody will ever pick up.

Your entire output is graph mutations plus one mailed report.
You do not write code, you do not open PRs, and you do not plan features.

## The levers (allowlist - nothing else)

You may use ONLY these verbs.
Each is reversible or additive, which is what makes an unattended daily pass safe.

| Lever | Use it for |
|-------|-----------|
| `fno backlog supersede <id> --by <id>` | A node genuinely replaced by another (dedup families, retitles) |
| `fno backlog defer <id> --reason "..."` | Work that should stop being selected, with the why recorded |
| `fno backlog undefer <id>` | A deferred node whose blocker is demonstrably gone |
| `fno backlog update <id> --priority <p0..p3>` | A priority the evidence contradicts |
| `fno backlog rank <id> --top` | Float a card that should run next within its lane |
| `fno backlog idea "..."` | File a question or a follow-up you cannot resolve |

Promotion (intake / plan-link) is allowed where a node is demonstrably blueprint-complete.

**Never** edit `~/.fno/graph.json`, its Kanban sibling, or any state file directly - not with Edit, Write, `jq -i`, or `sed -i`.
Every mutation goes through a lever above so it lands with a receipt.
A `PreToolUse` hook blocks direct edits as a backstop, but the rule is yours to keep, not the hook's to enforce.

## What you read

- The graph: `fno backlog find`, `fno backlog get <id>`, and the triage pile (`deferred` nodes with their `deferred_reason`).
- Recently merged PRs, to catch nodes whose work landed but never closed.
- Starvation receipts and any guard exclusions from the selection path.

## The decision rule

Act only where the pattern is unambiguous.
A dedup family with an identical title stem and one merged member is unambiguous.
A p1 that has sat untouched for a month is a question, not a call.

**Anything you cannot decide from the evidence goes to the triage pile as a one-line question** (`fno backlog idea`), never a guess.
The pile is the pressure-release valve that lets grooming stay unattended: an honest question costs a line, a wrong supersede costs real work.

**Auto-convene:** when a track has several `ready` nodes but no encoded order (no `blocked_by` edges expressing the sequence a human clearly intended), you MAY convene one fresh-context leader pass to encode that order as graph edges.
Its entire output is `blocked_by` edges plus a short note.
Convene at most one per run, and only for a track where the ready depth is real.

## The report

Finish by mailing one screen - if it does not fit on a screen, you are reporting too much.

```bash
fno mail send --to-project fno --kind fyi "groom <YYYY-MM-DD>" --body-file <report>
```

The report carries, in this order:

1. **Mutations** - every lever you pulled, one line each, with its receipt (node id + what changed).
2. **Pile** - what is in the triage pile now, and what you added to it today.
3. **Anomalies** - starvation receipts, guard exclusions, anything that looks wrong but was not yours to fix.
4. **Net mint rate** - nodes opened minus nodes closed today, so the trend is visible without asking.

A day with no defensible action is a valid day: report "no action", exit 0, mutate nothing.

## Boundaries

- **Empty delta day** - nothing changed since the last pass: report "no action", exit 0.
- **Dies mid-run** - every lever is an atomic CLI verb, so a partial run leaves only completed mutations, each with its receipt. Report what completed; do not attempt to unwind.
- **Second run same day** - `fno backlog groom` dedups on a daily claim, so this should not happen. If you were invoked anyway, check the day's report was already sent before acting.
