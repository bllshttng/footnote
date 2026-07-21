# Triage: the two-source backlog picker

Triage is where deferred work gets sorted and promoted. As of the
`fno backlog capture` feature it draws from **two sources** in one
picker:

1. **Graph nodes** (`ab-XXXXXXXX`) in `graph.json`: real backlog items that
   carry a plan or at least an idea-stage intent. Managed by `fno backlog`
   (intake / idea / ready / next / done / defer).
2. **Inbox items** (`fu-XXXXXX`) in the capture-tier inbox file: small
   follow-ups too minor for even an idea node. Managed by `fno backlog capture`.

The tier ladder, cheapest to most ceremony:

```
inbox item (fu-, markdown, NOT in graph)
  -> idea node     (ab-, graph node, no plan_path, status: idea)
    -> full node   (ab-, graph node, plan_path set, ready/next)
```

Promotion only ever moves **up** the ladder.

## The picker

`fno backlog triage context` emits a single JSON payload for the reasoning
layer. It now carries both sources:

- `candidates` / `ideas`: graph nodes (`id` is `ab-...`).
- `inbox_items`: unchecked inbox items, each tagged `"id_type": "fu"`.
- `inbox_count`: number of open inbox items.

Each item is labelled by its id type so the reasoning step can route a chosen
item to the right verb: graph nodes go through the normal triage
`propose` / `validate` / `apply` loop; a selected inbox item routes to the
promotion flow below.

## Promotion flow (fu- to ab-)

```bash
fno backlog capture promote
# -> creates a graph node, strikes the inbox checkbox:
#    - [x] — title (p1) ->
```

Promotion is **idempotent**: re-running `promote` on an already-promoted item
reports the existing node id and creates no duplicate. The node is created
before the checkbox is struck, so a failed strike surfaces loudly (a node with
an un-struck line) rather than silently diverging. Unknown or dismissed ids are
rejected with a non-zero exit.

To drop an item without promoting it:

```bash
fno backlog capture dismiss --reason "superseded by ab-..."
# -> - [-] — title (p1) (dismissed: superseded by ab-...)
```

Struck lines (promoted `[x]` and dismissed `[-]`) are **never deleted**, so the
trail survives for autocorrect provenance.

## Bloat control

The inbox is append-cheap, so it grows. Two guards:

- **Soft ceiling.** `fno backlog capture list` prints a non-fatal warning once the
  open-item count crosses **100**. The warning is advisory; nothing blocks.
- **Archive.** `fno backlog capture archive` sweeps struck items (with their
  `source:` / `why:` / `where:` sub-lines) into a sibling `inbox-archive.md`,
  leaving open items in place. Run it whenever the file feels heavy, typically
  during a weekly triage pass.

## Where the inbox lives

The inbox file path resolves via `paths.inbox_path()`. With no configuration
it lands at a project-local `.fno/backlog/inbox.md`. Point it elsewhere with
`config.paths.inbox_path`, or let it follow `config.post_merge.parking_lot_path`
when that is set. On a maintainer setup with an Obsidian vault enabled it
resolves to a backlog file under the vault instead, so sibling worktrees share
one file and writes are flock-serialized on that target.

## Provider behavior note

The deferrals-capture pass that feeds the inbox is advisory. Since the
control-plane collapse it runs and logs but does not
block session completion - completion authority is the external reads plus
budget, not a gate boolean. It still runs by size profile: profile S sets
`no_deferrals_capture: true` (the pass is skipped); M and L run it.
