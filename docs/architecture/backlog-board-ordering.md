---
created: 2026-06-03T00:00
status: accepted
---

# Backlog Board Ordering (swimlanes + curated rank + WIP cap)

## Overview

Both backlog boards - `graph.md` (Obsidian Kanban) and `fno backlog view` (the self-contained HTML board) - and the work selector consume one ordering function. The HTML renderer also owns the roadmap and public-backlog card markup. Those projections share its escaping and field policy instead of authoring cards separately.
The board calls `_intake.make_selection_sort_key(entries, swimlane=True)` and selection calls the same function with `swimlane=False`.
The HTML board additionally draws per-project sub-lane dividers and a soft WIP-cap count per column.

Explicit rendering commands load through `fno.graph.read_graph` plus `entries_with_archive`, with working rows winning by ID. Public selection is opt-out (`public: false` excludes), and both public projections must clear one title leak gate before either artifact is replaced.

Both boards are auto-rendered on every backlog mutation inside `locked_mutate_graph`, after `_write_json`.
That placement is load-bearing: **a renderer exception must never abort a backlog mutation**, so every derived read on the render path is defensive.

## The shared order key

`_intake.make_selection_sort_key` returns this logical key for unranked work:

```
[project lane] -> rank band -> live-epic child tier -> in-progress live epic -> live epic priority -> epic created_at -> child priority -> orphan-last -> created_at
```

For a live-epic child the leading rank band is the **epic's own** band. The child's rank band sits between the epic `created_at` term and child priority:

```
[project lane] -> epic rank band -> child tier -> in-progress live epic -> live epic priority -> epic created_at -> child rank band -> child priority -> orphan-last -> created_at
```

The optional project-lane prefix is present only when `swimlane=True`.
Named projects sort alphabetically and `(unscoped)` sorts last, preserving contiguous board swimlanes without teaching global selection an alphabetical project preference.

`_rank_band(entry)` returns `(0, float(rank))` for a finite, non-bool number and `(1, 0.0)` otherwise. Ranked cards precede unranked cards, ascending by rank, and NaN, infinity, booleans, and overflowing integers degrade to unranked so the key remains a total order. A ranked epic floats its whole group. A ranked child reorders only its own position inside that group. It can never pull its epic ahead of another epic or a loose node.

An epic contributes its tier, in-progress signal, priority, and grouping timestamp only while it is a real `type: epic` row that is not completed, done, superseded, or deferred.
Its in-progress signal includes a child with persisted completion/session state or a live lockfile claim, and each consumer binds one claim snapshot into both ordering and column routing.
Terminal field markers (`completed_at`, `superseded_by`, and `deferred_at`) also win over a stale persisted `ready` status.
A child of a missing, malformed, or terminal parent is a loose-node equivalent and sorts on its own priority.

Rank remains scoped per `(column, project)` lane: "web's #1 in Now" is independent of "etl's #1 in Now". A node with a live epic parent ranks within that epic instead. Its peers and anchors are its live-epic siblings across the whole graph. The `--within-epic` flag spells that scope out loud. The flag is refused for a node with no live epic parent, and an anchor outside the epic is rejected. Loose nodes and epic containers keep the lane scope.
Rank never changes a node's column.
`render._kanban_column` remains the sole column authority, while the renderer supplies an effective priority that may promote a child to its live epic's higher priority but never demote a child already above its epic.
`render.make_kanban_column(entries)` binds that projection together with the in-progress-epic and live-claim overlays so renderers, rank lane validation, and WIP counts delegate the same whole-graph context to `_kanban_column`.

## Board order == work order

Board order and work order share the whole decision suffix rather than only the rank term.
The Obsidian board, HTML master board, HTML project boards, public roadmap, `fno backlog next`, `fno backlog ready`, `/megawalk`, and the active-backlog daemon all call `make_selection_sort_key`.
No renderer carries an independent priority/created-at fallback that can drift from the walker.

The project prefix is an explicit display exception.
Default `fno backlog next` is project-scoped, so its order matches that project's board lane.
`fno backlog next --all` intentionally omits the prefix and compares work globally, while the master board remains grouped into visible project swimlanes.

Consequences:

- **`rank` changes the order, not what dispatches.** `fno backlog rank <id> --top` floats a card to the top of its swimlane on the board. A walker or daemon that drains an active mission scope then picks it first. Rank alone never dispatches the node: a top-ranked node outside every active mission scope stays undispatched. An explicit rank on a loose node overrides the epics-first heuristic. A ranked loose node beats an in-progress epic's children. A rank on a live-epic child is parent-scoped. It reorders the child inside its epic group only. The epic's own position decides the group's turn. Inside the group, the drain takes the children in the curated order.
- **Priority still leads unranked work.** Among unranked nodes, the shared suffix keeps the epics-first and priority terms. Live-epic priority and child priority follow, then orphan-last and creation-time. `fno backlog reprioritize <id> p0` remains the way to promote an unranked node. Reprioritizing a live epic can promote its lower-priority children into the same board column without rewriting those children.
- **Rank is per-`(column, project)` lane.** Selection is project-scoped by default (`fno backlog next [--project P]`), so rank orders within the project's ready set and matches the board's swimlane rank.
  It never reorders across projects, and `fno backlog update` does not clear `rank`.
  A moved node keeps its rank in the new lane's ranked band; run `fno backlog rank <id> --clear` to rejoin the unranked flow.

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

- `--top` / `--bottom`: below / above the scope's ranked band. When the scope has no ranked cards yet, the band starts at `0.0`.
- `--before <anchor>` / `--after <anchor>`: float midpoint next to a **ranked** anchor in the same scope. The anchor must already be ranked, because the band model puts all ranked cards ahead of all unranked. Position it relative to other ranked cards. Seed the first with `--top`.
- `--clear`: `rank = null`.

When the node has a live epic parent, the scope is that epic. Peers and anchors are the epic's children across the whole graph. The `--within-epic` flag is the explicit spelling. Otherwise the scope is the `(column, project)` lane. The verb resolves the target id through `_find_node`, which fuzzy-resolves partial ids. It compares on the **resolved** id for both peer-exclusion and the self-anchor guard. Rejections all print to stderr and exit non-zero. They cover a cross-scope anchor (both scopes named), an unranked anchor (actionable hint), a self-anchor, a non-existent node, and a wrong flag count. An explicit `--within-epic` without a live epic parent is also refused. The mutator raises *before* the locked write, so no partial rank is ever persisted.

## HTML board: the canonical dashboard

`render_html` no longer emits a kanban of cards. One dashboard renderer now authors every backlog surface. Ordering there is the client-side filter's job, not a column bucket's. The sub-lane divider and the per-column WIP-cap `<summary>` went with the card projection they belonged to.

`graph.md` headings stay bare (`## Now`, no count). That keeps the Obsidian Kanban plugin's per-column collapse state across re-renders. The md board labels each card `· <project>` instead. The plugin is column-only, so per-card labels and clustered order are the swimlane ceiling there.

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

1. Rank is per-`(column, project)` lane, not per-column. A live-epic child's rank is scoped within its epic. It orders the child inside the group only. Loose nodes and epic containers keep the lane scope.
2. WIP count/cap is HTML-board-only; md headings stay clean.
3. `fno backlog rank` is the ranking surface (no fzf drag-reorder).
4. `rank` is a nullable float, ordered ahead of the unranked shared-order suffix within a lane.
5. Rank never changes a node's column.
6. Done is untouched (history, capped at 10, sorted by `completed_at`).
7. A live epic can promote a child's effective board priority but can never demote a higher-priority child.
8. Completed, done, superseded, deferred, missing, and non-epic parents confer no ordering or column priority.

See the design doc (in the maintainers' vault) for the full
spec, acceptance criteria, and discretion notes.
