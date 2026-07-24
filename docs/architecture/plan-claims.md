# Plan Claims to Existing Idea Nodes

## Problem

When `/think` produces a design doc, it often auto-queues idea-state
placeholders on the backlog graph for follow-on specs. When the next
`/blueprint` invocation writes the implementation plan, the standard
`fno backlog intake` flow appends a fresh node and leaves the original
idea node dangling. Over time the kanban accumulates orphan placeholders
that require manual `fno backlog supersede` cleanup. Every spec compounds
the debt.

## Solution

A plan can declare it implements an existing idea-state node with a
`claims:` frontmatter field. When intake sees the claim it updates the
existing node in place rather than appending a duplicate.

Three layers, each more authoritative than the last:

1. **Plan frontmatter `claims: ab-XXXXXXXX`** - declarative, preferred.
   Read by `_resolve_claim` in `cli/src/fno/graph/_intake.py`.
   Malformed values (typos, missing nodes) are silently ignored on the
   frontmatter path so an unrelated typo never blocks intake.

2. **CLI flag `fno backlog intake plan.md --claims ab-XXX`** - runtime
   override. Beats frontmatter when both are present. Strict: malformed
   values exit non-zero. Use this for repair operations against past
   dangling nodes.

3. **Filing-time dedup net** - `_warn_similar_nodes` runs on every
   node-birth path (idea/add, single intake, multi-intake) and emits a
   stderr `dedup:` receipt naming up to three existing nodes that resemble
   the new one. It scores token-Jaccard via `relatedness.similar_nodes`
   across all live states (a shipped `done` node is the answer to a
   duplicate filing), at a 0.30 floor. For an idea-state top candidate the
   receipt suggests `--claims` so the author can re-file and consolidate.
   Warn-only - it never blocks a filing.

## Authoring

`/blueprint` writes `claims:` automatically when its argument matches the
`^ab-[0-9a-f]{8}$` shape. The classifier lives in the "Plan Claims
Ingestion" section of `skills/blueprint/SKILL.md` and shells out to
`scripts/lib/parse-claims-arg.sh` to resolve title and details from the
graph. After the plan file is written, `/blueprint` greps for a literal
`claims: ab-XXX` line and refuses to adopt when the line is missing.

```bash
# Author a plan that auto-claims the idea node
/blueprint ab-XXX

# Or, if /blueprint missed the claim somehow, repair at intake time
fno backlog intake path/to/plan.md --claims ab-XXX
```

The resulting plan frontmatter (folder mode lands in 00-INDEX.md;
quick mode in the single .md file) carries:

```yaml
---
created: 2026-05-05T04:35
claims: ab-XXX
executor: do
kill_criteria: ...
---
```

## Refusal paths

`fno backlog intake` refuses (exit non-zero) in three cases so silent
failures cannot land:

| Condition | Message hint |
|-----------|--------------|
| `--claims ab-XXX` names a node in non-idea state | "node ab-XXX is in state 'done'; refuse to claim a non-idea node" |
| Plan path was already adopted as a different node | "plan_path already adopted as ab-Y, but --claims names ab-X. Remove or supersede ab-Y before claiming" |
| Malformed `--claims` value | "invalid claims value: 'not-an-id' (expected ab-XXXXXXXX format)" |

The repair-path message names the exact `fno backlog supersede` command
that unblocks the operator, so recovery is one shell line away.

## Mutator semantics

When a claim resolves to an idea-state node, intake's `claim_mutator`
preserves the original `id`, `created_at`, and `details` while updating:

- `plan_path` to the new authoritative pointer
- `title` to the plan's derived title
- `blocked_by` deduplicated union of existing edges plus any new
  `depends_on` from plan frontmatter
- `priority` only when the plan supplies a non-default value (skips the
  implicit `p2` to avoid downgrading a p1 idea)
- `points` when the plan supplies an estimate
- `claimed_at` reset to None so `recompute_statuses` can flip
  `idea -> ready` on the next read

The original idea's textual `details` (the design body) survives; the
plan path is the new source of truth for execution.

## Tests

- `cli/tests/unit/test_intake_claims.py` - 20 unit tests covering all
  three layers, both the function-call path (`_intake_impl`) and the
  CliRunner end-to-end path against the real Typer command.
- `tests/blueprint/test_claims_arg.sh` - 21 bash assertions covering the
  parser script and the SKILL doc surface; runs against a fixture graph
  in a temp HOME so CI doesn't depend on the user's live `~/.fno`.

## See also

- `skills/blueprint/SKILL.md` - "Plan Claims Ingestion" section
- `cli/src/fno/graph/_intake.py` - `_resolve_claim`, `_warn_similar_nodes`
- `scripts/lib/parse-claims-arg.sh` - bash classifier
- `docs/architecture/graph-collision-detection.md` - sister mechanism
  for plan-vs-plan file overlap detection
