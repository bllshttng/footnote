# Section Headers — canonical shape for `/blueprint`-generated plans

`/blueprint`-generated plans use a fixed set of top-level `##` headers. Two things depend on this:

1. **Humans** browsing a plan in Obsidian or GitHub can navigate by section.
2. **Backlog wikilinks** (`plan_path: internal/.../2026-04-29-feature-name.md#wave-3-component-2-profile-sweep-nh-first`) resolve to a specific shippable unit by hashing into the slug-form of the header. Without a stable header, the link silently breaks.

For new single-doc plans, `## Execution Strategy` YAML is the executable authority and other headers are optional navigation.
`scripts/validate-plan.sh` retains heading checks only for legacy plans without the semantic single-doc shape.

## Canonical headers

In emit order, inside the single plan doc's `## Execution Strategy` waves:

| Header | Required | Purpose |
|--------|----------|---------|
| `## Execution Strategy` | yes | Machine-readable YAML wave manifest for `/execute waves` |
| `## Wave N: <name>` (one per wave) | optional | Human or legacy wikilink target for a shippable unit |
| `## Phase Dependencies` | optional | Human-readable phase DAG when YAML alone is insufficient |
| `## User Stories Summary` | optional | BDD acceptance criteria roll-up by epic |
| `## Technical Architecture Overview` | optional | High-level tables, components, decisions |
| `## Success Metrics` | optional | Target/measurement table |
| `## Goal Alignment` | when `project.goals` present | Task → goal mapping |
| `## Critical Path Trace` | optional | User journey + status markers (✅🔨⚠️❌🔗) |
| `## Scope Classification` | optional | `feature` \| `scaffolding` \| `poc` |
| `## File Ownership Map` | optional | Per-task file ownership for parallel-wave conflict detection |
| `## Out of Scope` | optional | Explicit non-goals |

Quick plans skip waves entirely and carry `kill_criteria` in frontmatter (never a `## Kill Criteria` heading — the heading form is invisible to the stamp/validate parser) — they do not produce wave headers.

## Obsidian slug rules

The wikilink fragment after `#` is the header rendered through Obsidian's slug pipeline. The footnote ecosystem agrees on this version of the rules:

1. **Lowercase** the entire header text.
2. **Spaces → `-`** (single dash per space).
3. **Strip most punctuation**: `:`, `()`, `[]`, `,`, `.`, `/`, `?`, `!`, `'`, `"`, backticks.
4. **Collapse consecutive `-`** into a single `-`.
5. **Preserve numbers and single `-`** verbatim (runs of `-` are still collapsed by rule 4; this rule only says the dash character is not stripped like other punctuation).
6. **Strip leading and trailing `-`** from the result.

Underscores are not touched (they're rare in headers and harmless when they survive).

## Worked examples

| Header | Slug |
|--------|------|
| `## Wave 3: Component 2 profile sweep (NH-first)` | `wave-3-component-2-profile-sweep-nh-first` |
| `## Wave 1: Foundation` | `wave-1-foundation` |
| `## File Ownership Map` | `file-ownership-map` |
| `## Out of Scope` | `out-of-scope` |
| `## User Stories Summary` | `user-stories-summary` |
| `## Critical Path Trace` | `critical-path-trace` |

To verify by hand:

```bash
python3 -c "
import re, sys
header = sys.argv[1]
s = header.lower()
s = re.sub(r'^##\s*', '', s)            # drop the '## ' prefix
s = re.sub(r'[^a-z0-9\s_-]', '', s)     # strip punctuation (preserves _)
s = re.sub(r'\s+', '-', s)              # spaces -> dashes
s = re.sub(r'-+', '-', s)               # collapse dashes
s = s.strip('-')                        # trim leading/trailing dashes
print(s)
" "## Wave 3: Component 2 profile sweep (NH-first)"
# -> wave-3-component-2-profile-sweep-nh-first
```

## Backlog usage

A backlog node can target a specific wave inside a multi-wave plan by appending the slug fragment:

```bash
fno backlog intake \
  --plan-path "internal/etl/plans/2026-04-29-florida-ahca-etl.md#wave-1-schema-migrations" \
  --title "FL AHCA v1: Wave 1 schema migrations"
```

The graph stores the full path verbatim, including the `#fragment`.
Fragment-targeted plans are a legacy shape and must retain the matching header while they remain in flight; new decomposition writes separate child plan files instead.

When splitting a previously-monolithic plan into per-wave nodes, point each sibling node at the same plan_path file with different fragments. `additional_prs` is no longer needed for that case — each wave becomes its own node with its own PR, and the plan file itself is the shared source of truth.

## Where this lives in the toolchain

| File | Role |
|------|------|
| [`../scripts/mutate_doc.py`](../scripts/mutate_doc.py) | Appends the authoritative `## Execution Strategy` YAML to the single plan doc |
| [`../scripts/validate-plan.sh`](../scripts/validate-plan.sh) | Routes semantic plans to `fno plan validate --execution` and preserves legacy heading checks |
| This file | Reference doc — slug rules, examples, backlog usage |

Adding a new machine-consumed field belongs in the Execution Strategy schema and semantic validator.
Add a heading only when it improves navigation or an explicit legacy fragment still consumes it.
