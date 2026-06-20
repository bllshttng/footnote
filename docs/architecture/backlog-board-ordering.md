---
created: 2026-06-03T00:00
status: accepted
---

# Backlog Board Ordering (swimlanes + curated rank + WIP cap)

## Overview

Both backlog boards - `graph.md` (Obsidian Kanban) and `fno backlog view` (the
self-contained HTML board) - render from one shared ordering engine so they can
never drift. A column's cards are ordered by a single lane key that clusters by
project (swimlanes), floats hand-curated cards to the front (rank), and falls
back to today's priority order. The HTML board additionally draws per-project
sub-lane dividers and a soft WIP-cap count per column.

Both boards are auto-rendered on every backlog mutation, inside
`locked_mutate_graph` (after `_write_json`). That placement is the load-bearing
constraint for the whole feature: **a renderer exception must never abort a
backlog mutation**, so every new read on the render path is defensive.

## The shared lane key

`render._lane_sort_key(entry)` is the single sort key both renderers use for
non-Done columns:

```
(_lane_order_key(project), _rank_band(entry), _graph_sort_key(entry))
```

- `_lane_order_key(project)` -> `(project == UNSCOPED_LABEL, project)`: named
  projects sort alphabetically, the `(unscoped)` lane sorts last.
- `_rank_band(entry)` -> `(0, float(rank))` when `rank` is a finite, non-bool
  number, else `(1, 0.0)`. Ranked cards (band 0, ascending rank) precede
  unranked cards (band 1). `math.isfinite` excludes NaN/inf so the key stays a
  total order (a NaN would compare False both ways and silently mis-order).
- `_graph_sort_key(entry)` -> `(priority_rank, created_at)`: the pre-existing
  fallback, unchanged.

`_project_key` and `UNSCOPED_LABEL` were hoisted from `render_html.py` into
`render.py` (the shared module) so both renderers import one definition.

Rank is therefore scoped per `(column, project)` lane: "web's #1 in Now" is
independent of "etl's #1 in Now". Rank never changes a node's column -
`render._kanban_column` remains the sole column authority; rank only orders
within a column.

## Board order == work order

Board order and work order are one ordering (x-d1fe). The lane key above orders
the **board** (the Obsidian Kanban + the HTML board), and *selection* - what
`fno backlog next` returns, and therefore what `/megawalk`, the active-backlog
daemon, and a `/target <id>` walk pick up - shares the **same** `_rank_band`
term. So "top of the board" means "worked next."

Selection uses `make_selection_sort_key` (`cli/src/fno/graph/_intake.py`):

```
rank band  ->  epics-first  ->  priority (pN)  ->  created_at
```

The `rank band` is `_rank_band` from `cli/src/fno/graph/_constants.py` - the
ONE helper both the board lane key (`render._lane_sort_key`) and the selection
key import, so the two can never drift (this section's old "board lies about
work order" gap is closed). It returns `(0, rank)` for a finite rank and
`(1, 0.0)` otherwise, so ranked nodes (band 0, ascending rank) precede all
unranked nodes (band 1). Consequences:

- **`rank` changes what runs next.** `fno backlog rank <id> --top` floats a card
  to the top of its swimlane on the board AND makes the walker / daemon pick it
  first. An explicit rank overrides the epics-first heuristic (a ranked loose
  node beats an in-progress epic's children - Locked Decision 1).
- **Priority is still the lever for unranked work.** Among unranked nodes
  (no `rank` set), selection is byte-for-byte the prior epics-first -> priority
  -> created_at order, so `fno backlog reprioritize <id> p0` remains the way to
  promote a node you have not explicitly ranked. Epic *children* outrank loose
  nodes (Locked Decision 7) only within the unranked band.
- **Rank is per-`(column, project)` lane.** Selection is project-scoped
  (`fno backlog next [--project P]`), so rank orders within the project's ready
  set, matching the board's per-`(column, project)` swimlane rank; it never
  reorders across projects. `fno backlog update` does NOT clear `rank`, so a
  moved node keeps its rank in the new lane's ranked band; run
  `fno backlog rank <id> --clear` to rejoin the unranked flow.

## The rank model

`Entry.rank: Optional[float] = None` (nullable). Float, not int, so
`--before`/`--after` insert at a midpoint between two neighbors and never
renumber siblings. `null` = unranked (rejoins the priority fallback).

`rank` is in `store.CANONICAL_FIELD_ORDER` and `_apply_graph_defaults`
sets it to `None`, so canonicalize backfills `rank: null` on every node on
the next mutation - self-healing, like the status-forward migration. Without
the `CANONICAL_FIELD_ORDER` entry, canonicalize would drop the field.

### `fno backlog rank <id>`

Mirrors `reprioritize`; writes through `locked_mutate_graph`. Exactly one of:

- `--top` / `--bottom`: below / above the lane's ranked band (`0.0` if the lane
  has no ranked cards yet).
- `--before <anchor>` / `--after <anchor>`: float midpoint next to a **ranked**
  anchor in the same lane. The anchor must already be ranked (the band model
  puts all ranked cards ahead of all unranked, so you position relative to other
  ranked cards; seed the first with `--top`).
- `--clear`: `rank = null`.

The verb resolves the target id through `_find_node` (which fuzzy-resolves
partial ids like) and compares on the **resolved** id for both
peer-exclusion and the self-anchor guard. Rejections - cross-lane anchor
(names both lanes), unranked anchor (actionable hint), self-anchor, non-existent
node, wrong flag count - all print to stderr and exit non-zero, and the mutator
raises *before* the locked write so no partial rank is ever persisted.

## HTML board: sub-lanes + WIP cap

`render_html._board_html` gained two optional behaviors, used only by the master
board (per-project sections render unchanged):

- **Sub-lanes** (`sublanes=True`): a lightweight `<div class="lane">` divider
  before each project's run of cards, emitted only in multi-project columns
  (a single-project column stays clean). Cards are pre-sorted by the lane key,
  so the divider-on-project-change yields contiguous, labeled runs.
- **WIP cap** (`caps`): each column `<summary>` shows `<count> / <cap>` with an
  `.over` class when the count exceeds the cap; uncapped columns show the plain
  count.

`graph.md` headings stay bare (`## Now`, no count) so the Obsidian Kanban plugin
keeps per-column collapse state across re-renders; the md board labels each card
`· <project>` (the plugin is column-only, so per-card labels + clustered order
are the swimlane ceiling there).

## Defensive config read

`render_html._load_wip_caps()` reads `config.kanban.wip_caps` directly from the
**global** settings file (`_global_settings_path()`), the same rationale as
`_load_obsidian_vault` (graph.html is a global artifact; reading via the
project-local-first loader would let a project's settings shadow the global
config on auto-render). It is fully defensive because it runs inside
`locked_mutate_graph`:

- block absent -> defaults `{now: 20, next: 50}` (others uncapped)
- block present -> only its entries; a non-int / negative / zero / bool / string
  cap is dropped (that column renders uncapped)
- any read/parse error -> `{}` (all uncapped), never raised

This is a deliberate fail-safe-and-silent design (a soft WIP cap is advisory,
not an enforcement gate). A consequence is that a mis-typed cap silently does
nothing with no user feedback; surfacing that (e.g. via `fno config doctor`) is
tracked as a follow-up, not built here.

## Locked decisions

1. Rank is per-`(column, project)` lane, not per-column.
2. WIP count/cap is HTML-board-only; md headings stay clean.
3. `fno backlog rank` is the ranking surface (no fzf drag-reorder).
4. `rank` is a nullable float, ordered ahead of the `(priority, created_at)`
   fallback within a lane.
5. Rank never changes a node's column.
6. Done is untouched (history, capped at 10, sorted by `completed_at`).

See the design doc (in the maintainers' vault) for the full
spec, acceptance criteria, and discretion notes.
