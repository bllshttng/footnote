# Section Headers — canonical shape for `/blueprint`-generated plans

`/blueprint`-generated plans use a fixed set of top-level `##` headers. Two things depend on this:

1. **Humans** browsing a plan in Obsidian or GitHub can navigate by section.
2. **Backlog wikilinks** (`plan_path: internal/.../2026-04-29-feature-name.md#wave-3-component-2-profile-sweep-nh-first`) resolve to a specific shippable unit by hashing into the slug-form of the header. Without a stable header, the link silently breaks.

`scripts/validate-plan.sh` enforces the wave-header subset via `validate_wave_section_headers()`. Other headers are convention, not validated, but emitted into the single plan doc by the mutation script ([`../scripts/mutate_doc.py`](../scripts/mutate_doc.py)) for consistency.

## Canonical headers

In emit order, inside the single plan doc's `## Execution Strategy` waves:

| Header | Required | Purpose |
|--------|----------|---------|
| `## Execution Strategy` | yes | Machine-readable YAML wave manifest for `/do waves` |
| `## Wave N: <name>` (one per wave) | yes when waves declared in YAML | Human + wikilink target for each shippable unit |
| `## Phase Dependencies` | yes | Visual + tabular phase DAG |
| `## User Stories Summary` | yes | BDD acceptance criteria roll-up by epic |
| `## Technical Architecture Overview` | yes | High-level tables, components, decisions |
| `## Success Metrics` | optional | Target/measurement table |
| `## Goal Alignment` | when `project.goals` present | Task → goal mapping |
| `## Critical Path Trace` | yes | User journey + status markers (✅🔨⚠️❌🔗) |
| `## Scope Classification` | yes | `feature` \| `scaffolding` \| `poc` |
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

The graph stores the full path verbatim, including the `#fragment`. `validate-plan.sh` guarantees the fragment resolves before the node is adopted, so callers don't see silently-broken wikilinks weeks later.

When splitting a previously-monolithic plan into per-wave nodes, point each sibling node at the same plan_path file with different fragments. `additional_prs` is no longer needed for that case — each wave becomes its own node with its own PR, and the plan file itself is the shared source of truth.

## Where this lives in the toolchain

| File | Role |
|------|------|
| [`../scripts/mutate_doc.py`](../scripts/mutate_doc.py) | Appends `## Execution Strategy` (and, per wave, `## Wave N: <name>`) to the single plan doc |
| [`../scripts/validate-plan.sh`](../scripts/validate-plan.sh) | `validate_wave_section_headers()` asserts YAML-vs-headers parity |
| This file | Reference doc — slug rules, examples, backlog usage |

Adding a new canonical header? Update the table in the [Canonical headers](#canonical-headers) section above and (if it should be enforced) add a check to `validate-plan.sh`. Adding to the emitter alone is not enough — the convention has to be queryable from outside `/blueprint`.
