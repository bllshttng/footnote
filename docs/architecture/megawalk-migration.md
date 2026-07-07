---
note: "Shared reference. Linked from removal redirect messages in megawalk SKILL and from CLAUDE.md."
---

# Megawalk Migration (2026-04-20)

Four subcommands were removed as part of the megawalk surface cleanup. This page is the single source of truth for the before/after so users who hit a "command was removed" message can see the whole shape in one place.

## Removed: `/megawalk continue`

**Replacement:** bare `/megawalk`

Bare invocation is resume-aware - it reads `megawalk-state.md` and picks the next ready task whether or not a prior session was interrupted. `continue` was a synonym for "resume"; the distinction added cognitive load without functional value.

```bash
# Before
/megawalk continue
# After
/megawalk            # bare: enters the loop (resume-aware)
```

## Removed: `/megawalk next`

**Replacement:** bare `/megawalk`

`next` was a no-op alias for bare invocation - both entered the loop and picked the top ready task. Keeping two spellings for the same behavior added surface area without functional value. All modifiers that worked after `next` (e.g. `/megawalk next parallel`, `/megawalk next auto-merge`) now work directly on bare.

```bash
# Before
/megawalk next
/megawalk next parallel
/megawalk next auto-merge
# After
/megawalk
/megawalk parallel
/megawalk auto-merge
```

## Removed: `/megawalk adopt --batch <dir>`

**Replacement:** multi-path `adopt` or shell glob

`--batch` was built to accommodate one Opus-generated plan format that `/blueprint` never actually produces (the `tot-buildout` shape with `00-MASTER.md` and letter-suffixed phase files). The multi-path `adopt` form plus shell globbing covers the same cases without a special flag.

```bash
# Before
/megawalk adopt --batch plans/folder/
# After
/megawalk adopt plans/folder/*.md       # shell glob
/megawalk adopt plans/a.md plans/b.md   # explicit multi-path
```

`roadmap-tasks.py intake --batch` exits 1 with the same redirect so scripts and muscle-memory calls fail loudly instead of silently. (The `adopt` Typer alias is gone entirely; calling `roadmap-tasks.py adopt` exits 2 with "No such command".)

## Removed: top-level `/megawalk vision.md`

**Replacement:** `/megawalk roadmap <vision.md>`

Top-level positionals are now either verbs (`roadmap`, `adopt`, `status`, `defer`, `cancel`, `retro`) or graph IDs (`ab-xxxxxxxx`). Arbitrary paths at the root were inconsistent with that grammar and made the Commands section harder to scan.

```bash
# Before
/megawalk vision.md
# After
/megawalk roadmap vision.md
```

## Added: `/megawalk ab-xxxxxxxx`

Any graph node ID is now a valid argument - it resolves to the node's `plan_path` via `~/.fno/graph.json` and runs just that node, instead of picking the top-ranked ready task. Same form works in `/target`.

```bash
/megawalk ab-1234abcd           # run this specific node
/target M ab-1234abcd             # same node, specific size
```

Unknown IDs soft-fail: the resolver echoes the arg back with a stderr warning, and the skill's existing "plan path does not exist" error fires for obvious typos. Set `RESOLVE_STRICT=1` in the environment to opt into a hard fail instead.

## Added: quick-plan artifact sidecar

Quick plans (single `.md` files under `plans/`) now write `HANDOFF.md`, `COMPLETION.md`, `.completed/`, and `scratchpad-archive/` to `{plan_path}.artifacts/` instead of the parent `plans/` directory. Folder plans are unchanged (artifacts live inside the folder, which already namespaces them).

Before, two back-to-back quick plans on `plans/a.md` then `plans/b.md` would silently overwrite each other's artifacts in the shared `plans/` root. After the sidecar change, each plan keeps its own artifact folder alongside the plan file.
