# Source field migration (`adopt` -> `intake`)

This doc covers the `adopt` -> `intake` rename completion and the
one-shot migration script users run after the upgrade.

## Background

An earlier change removed the user-facing `fno backlog adopt` verb and replaced
every in-tree caller with `intake`. It explicitly deferred two
follow-ups: the private internals (`_adopt.py` and its helper
symbols) and the `source: "adopt"` field literal stored on every
node intaked from a plan. The 2026-05-02 plan closes both.

## What changed

### Module + symbol rename

`cli/src/fno/graph/_adopt.py` is now `_intake.py`. Every helper
symbol followed: `_prepare_adopt -> _prepare_intake`,
`_build_adopt_node -> _build_intake_node`,
`_collect_adopt_paths -> _collect_intake_paths`. The TypedDict union
family went `_AdoptResult/Already/Ready/Claim ->
_IntakeResult/Already/Ready/Claim`. `git mv` preserved history so
`git log --follow cli/src/fno/graph/_intake.py` walks the full
backstory.

The `_do_adopt_multi` and `_collect_adopt_paths_typer` helpers in
`cli/src/fno/graph/cli.py` were renamed to `_do_intake_multi`
and `_collect_intake_paths_typer` for the same consistency reason.

### Writer flip

`_build_intake_node` (formerly `_build_adopt_node`) now emits
`"source": "intake"` for every newly intaked node. Old graph.json
rows with `"source": "adopt"` continue to read correctly because
every reader compares against `INTAKE_SOURCE_VALUES`.

### Back-compat constant

`fno.graph._intake.INTAKE_SOURCE_VALUES` exports a
`frozenset[Literal["intake", "adopt"]]` containing both spellings.
Any future code that filters or sorts by `source` should use
`entry.get("source") in INTAKE_SOURCE_VALUES` rather than comparing
against the literal `"intake"`. The constant is permanent: backups
and synced volumes may carry the legacy spelling indefinitely.

### Output strings

User-visible output from `fno backlog intake` switched verbs. The
single-path success line now reads
`intake ab-XXXXXXXX -> backlog: "title"` instead of
`adopted ab-XXXXXXXX into backlog`. Multi-path output, the tallies
dict (`{"intaked": ..., "already": ...}`), the dry-run preview, and
the error messages all match the new vocabulary.

## Migration: rewriting old graph.json files

`cli/scripts/migrate_source_field.py` rewrites every node carrying
`source: "adopt"` to `source: "intake"`. Run it once per machine
after pulling the rename:

```bash
# Preview what would change (recommended first step):
uv run python cli/scripts/migrate_source_field.py ~/.fno/graph.json --dry-run

# Apply:
uv run python cli/scripts/migrate_source_field.py ~/.fno/graph.json
```

The script:

- Acquires the same `/tmp/fno-graph.lock` flock that
  `fno backlog intake` uses, so a concurrent intake in another
  terminal cannot race the read-modify-write window.
- Atomic-renames a `.tmp` file over the target so a crash mid-write
  cannot leave a half-written graph.
- Is idempotent: a second run on an already-migrated graph reports
  zero changes and exits 0.
- Touches only the `source` field. Every other field on every node
  is preserved verbatim.

It is NOT wired into `postinstall` or any startup hook. The user
opts in explicitly. Old graph.json files on backup volumes that
never get migrated will continue to read correctly forever via
`INTAKE_SOURCE_VALUES`.

## What did NOT change

- The `source` field name itself stays. Only the value vocabulary
  shifts.
- The `roadmap-tasks.py` shim retains its argument vocabulary; only
  internal symbols moved.
- The `status` enum, the lifecycle phrase
  (`intake -> triage -> ready/next -> done`), and every other graph
  schema field are unchanged.

## Related

- 2026-04-27 plan: user-facing CLI rename and alias removal
- 2026-05-02 plan (this rename): private internals + schema migration
- `INTAKE_SOURCE_VALUES` definition:
  `cli/src/fno/graph/_intake.py`
- Migration script: `cli/scripts/migrate_source_field.py`
- Migration tests: `cli/tests/integration/test_migrate_source_field.py`
