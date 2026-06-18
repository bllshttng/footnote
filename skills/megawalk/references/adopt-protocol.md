# Adopt Protocol

**Load when:** the user invokes `/megawalk adopt <plan_path...>`. Adopt is pure graph mutation - it adds nodes to `graph.json` and returns. It never runs `/target` and never starts a loop.

## What adopt does

Takes a plan authored via `/blueprint` (whose planning cost is tracked in `ledger.json`) and adds it to the graph as a feature node. Two modes:

- **Backlog mode (no `--roadmap-id`):** The plan lands on the general graph with `roadmap_id=null`. Any `/target <plan_path>` invocation can pick it up directly. Use this for brainstorming output where you want the spec tracked but haven't committed it to a specific roadmap.
- **Roadmap mode (`--roadmap-id <id>`):** The plan is tagged to the roadmap so `/megawalk --roadmap-id <id>` includes it. Use this when the spec clearly belongs to an in-flight roadmap.

The ledger entry is unchanged. Adopt only creates a NEW node in `graph.json`, linked by `plan_path`. There is no dual row.

## Steps

1. **Resolve roadmap_id.** If `--roadmap-id` is supplied, use it. Otherwise, optionally read `.fno/roadmap-state.md` for an active roadmap; if none, fall through to backlog mode (no arg).
2. **Call the script:**
   ```bash
   fno backlog intake "$PLAN_PATH" \
     [--roadmap-id "$ROADMAP_ID"] \
     --priority p2
   ```
3. **Echo result** (e.g. `adopted ab-a1b2c3d4 into rm-20260416-abc123: "Ledger docs sweep"` or `adopted ab-a1b2c3d4 into backlog: "Ledger docs sweep"`) and suggest bare `/megawalk` (for roadmap mode) or `/target <plan_path>` (for backlog mode) to execute.

Optional flags forwarded to the script: `--title`, `--priority`, `--deps`, `--points`, `--force-new-roadmap`, `--dry-run`.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Adopted (or already-adopted idempotency no-op) |
| 1 | Invalid CLI input (missing paths, empty `--from`, bad `--deps` ID) - caught pre-flight, graph unchanged |
| 2 | Unknown `roadmap_id` (only when `--roadmap-id` is supplied without `--force-new-roadmap`) |
| 4 | Multi-path invocation where every path was missing |

## Idempotency

If a graph node already has the same `plan_path` AND `roadmap_id` (including the case where both are null), the command prints `already adopted: ab-xxx` and exits 0 without duplicating. Adopting the same plan once to backlog and once to a roadmap produces two separate nodes, which is intentional - the backlog and roadmap entries represent different commitments.

## Folder-of-peers adoption

For fork-style plan folders (multiple numbered plan files that each represent a separate feature), use the multi-path form with a shell glob:

```bash
/megawalk adopt plans/2026-04-19-tot-buildout/*.md
# Multi-adopting 10 plans:
#   adopted ab-a1b2c3d4: "Repo scaffold"
#   adopted ab-b2c3d4e5: "Verification hooks"
#   ...
# 0 already adopted, 10 newly adopted.
```

This replaces the old `--batch` flag; the multi-path form plus shell globbing covers the same shape without a dedicated subcommand. If any of the adopted files has `depends_on:` frontmatter, the edges are wired automatically (see "Dependency resolution" below).

## Dry run

```bash
/megawalk adopt --dry-run plans/2026-04-19-tot-buildout/*.md
# Multi-adopt preview (dry-run, no changes):
#   would adopt: "Repo scaffold"  (plan: plans/.../01-repo-scaffold.md)
#   ...
# 10 plans would be adopted. Run without --dry-run to apply.
```

Print the adoption plan without mutating the graph. Works with single and multi-path invocations. Useful for verifying a folder before committing its contents to the backlog.

## Dependency resolution (`depends_on` in frontmatter)

If the plan file (or folder's `00-INDEX.md`) has a `depends_on:` entry in its frontmatter, adopt resolves each reference to a graph node ID and wires up the `blocked_by` edges automatically. Supported reference forms:

```yaml
---
# Block list - one reference per line:
depends_on:
  - ../2026-04-19-kill-criteria   # sibling plan folder slug (resolved
                                    # via graph.plan_path match)
  - ab-d359579e                   # or an explicit graph node ID
  - 02a                           # or a bare sequence token - resolves
                                    # to the 02a-*.md sibling in the same
                                    # folder (batch-local only)
  - 3                             # numeric shorthand normalizes: `3`
                                    # matches 03-*.md, `01` matches 1
---
```

```yaml
---
# Inline list - equivalent to block form:
depends_on: [ab-d359579e, 02a, 3]
---

# Empty list:
depends_on: []
---

# Scalar - a single reference, coerced to a one-element list:
depends_on: ab-d359579e
---
```

Sequence-token resolution is **batch-local**: `depends_on: 02a` from a plan in folder `plans/foo/` matches only `plans/foo/02a-*.md`, never a `02a-*.md` in `plans/bar/`. This avoids silent cross-project collisions for a convention every folder shares.

Unresolvable references emit a stderr warning and are skipped - the plan still adopts cleanly, just without that edge. Combine with `--deps ab1,ab2` on the command line to add extra edges beyond what frontmatter declares.

## Safeguard: multi-path refuses `/blueprint full` folders

If one of the resolved paths is a `/blueprint full` folder (has `execution_mode:`, `waves:`, or a `## Execution Strategy` in its `00-INDEX.md`), adopt treats it as a single-feature plan with phases (one PR outcome) rather than a batch of separate features, and refuses to fan it out. Drop the offending path or pass `--force-batch` to override.

## Multi-path adoption

A single `adopt` invocation can take many plan paths at once. Four ways to pass them:

```bash
# Form A - positional list (bash-expanded)
/megawalk adopt plans/a plans/b plans/c

# Form B - comma-separated single arg
/megawalk adopt "plans/a,plans/b,plans/c"

# Form C - from a file or stdin (one path per line; # comments skipped)
/megawalk adopt --from paths.txt
cat paths.txt | /megawalk adopt --from -
/megawalk adopt --from - <<EOF
plans/a
plans/b
EOF
```

Rules across forms:

- Each resolved path becomes its own graph node. A single path (legacy invocation) preserves the original `adopted ab-XXX into backlog: "Title"` output format; two or more paths switch to the aggregate `Multi-adopting N plans:` block.
- Shared flags (`--roadmap-id`, `--priority`, `--points`, `--deps`, `--title`) apply uniformly to every adopted node.
- Missing paths emit `warning: not found, skipped: PATH` and the rest of the batch proceeds. When every path is missing, exit code is 4.
- Exit 0 covers both "all newly adopted" and "all idempotent no-op." CLI input errors (missing paths from `--from`, bad `--deps` IDs) are caught pre-flight and fail fast with exit 1 before any graph mutation, so there is no partial-write failure mode to report.

## Companion commands

- `/blueprint` auto-adopts by default (opt out with `no-adopt`), so you rarely need to call `/megawalk adopt` directly — it is mostly for manually promoting older plans or adopting plans created outside the normal workflow.
- `/triage` proposes an optimal ordering across all pending nodes (backlog + roadmap). Run it after bulk adopt operations to see inferred dependencies and priority adjustments. `/triage` mutations flow through the same `locked_mutate_graph()` as adopt, so `graph.md` re-renders automatically after each applied proposal.
