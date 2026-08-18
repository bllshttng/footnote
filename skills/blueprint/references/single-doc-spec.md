# Single-Doc Blueprint Spec

Reference for the `/blueprint` mutation behavior introduced in PR1 (2026-05-18, `ab-69f7ee8f`). The architectural overview lives at [docs/architecture/lean-blueprint.md](../../../docs/architecture/lean-blueprint.md).

## Design doc shape

The frontmatter is the one locked part of the plan.
When a doc carries no frontmatter, blueprint first-fills it.
The Claims-Ingestion gate then refuses to adopt a plan whose node never bound.
The locked fields, with the design-stage placeholders an author fills:

```markdown
---
status: design
created: <YYYY-MM-DD>
type: blueprint
node: <node id>
claims: <node id>
sources: [<artifacts actually read>]
consolidation:
  outcome: <absorb | append | proceed_alone>
  proceed_alone_against: []   # or absorbed: / appended_to:, each entry an id with its reason
---

# <Title>

## User Stories

**US1:** As a <role>, I can <capability>, so that <outcome>.

## Acceptance Criteria
```

The body is a contract for the implementer, not a rigid template.
`## User Stories` seeds the task skeleton.
When that section is absent, blueprint emits a single default task.
The BDD acceptance criteria carry the test contract the implementer must satisfy.
Every other section (problem, design rationale, open questions) is optional context, authored as the plan needs it.

## Status progression

```
design → ready → in_progress → reviewing → shipping → shipped
```

Enforced by `fno.plan._status.validate_transition(old, new)`. Backward transitions raise `StatusTransitionError`. The Pydantic model uses `mode="before"` validators to coerce unquoted YAML booleans (Python `True`/`False`) that bash writers may emit - see `feedback_literal_string_rejects_yaml_bool`.

**Valid transition examples:**

| From | To | Result |
|------|----|--------|
| `design` | `ready` | OK |
| `ready` | `in_progress` | OK |
| `in_progress` | `in_review` | OK |
| `ready` | `design` | StatusTransitionError |
| `in_review` | `in_progress` | StatusTransitionError |

**Invalid input examples:**

| Scenario | Exit code | Stderr |
|----------|-----------|--------|
| `/blueprint` on `status: ready` without `rewrite` | 1 | "doc already in `ready` status; pass `rewrite` to regenerate execution sections." |
| `/blueprint` writes to section outside allowlist | 2 | "section ownership violation: '<section>' is not in BLUEPRINT_WRITE_ALLOWLIST" |

## Section ownership allowlist

`BLUEPRINT_WRITE_ALLOWLIST` in `fno.plan._ownership`:

```python
BLUEPRINT_WRITE_ALLOWLIST = frozenset({
    "Execution Strategy",
    "File Ownership Map",
    "Patterns to Reuse",
    "kill_criteria",
    "consolidation",
})
```

`assert_blueprint_can_write(section_name)` raises `OwnershipViolation` on any name outside the set. This is checked before each section write, not only at the end. Future skill additions must define their own allowlist constants; `BLUEPRINT_WRITE_ALLOWLIST` is never widened.

## Atomic mutation contract

Writes use tempfile + `os.replace` within the same directory as the plan file. The sequence:

1. Write all new content to `<plan-path>.tmp.<pid>`
2. Call `os.replace(tmp_path, plan_path)` - atomic on POSIX for same-filesystem writes
3. On any failure before step 2, the original plan file is unchanged

Cross-volume rename is non-atomic on macOS APFS. Keeping the tempfile in the same directory as the plan (always within `internal/fno/plans/`) avoids this. The plan doc is either fully updated or unchanged; no partial writes reach disk.

Concurrent writers acquire `<plan-path>.lock` via `flock` or `filelock` before mutating. The brief generator is read-only and takes no lock; it reads the file in a single `open(...).read()` call, which is a point-in-time snapshot at POSIX semantics for typical plan sizes.

## Greenfield/brownfield auto-detect

1. Parse the `## Architecture` section, extract path-shaped mentions (slash + extension, or absolute path starting with `/`). Simple heuristics - not a full path parser.
2. Probe each extracted path for existence on disk.
3. If fewer than 50% exist: greenfield mode (skip File Ownership Map and Patterns to Reuse).
4. If 50% or more exist: brownfield mode (include both sections).

The threshold is per-plan, not per-project. A plan touching a new subsystem in an existing codebase may auto-detect as greenfield even though the project itself is brownfield.

## Modifier set

Positional canonical form. Flag aliases (`--quick`, `--greenfield`, etc.) are accepted. Modifiers compose freely.

| Modifier | Effect |
|----------|--------|
| `quick` | Skip Execution Strategy; stamp status and kill_criteria only (single-task plans) |
| `greenfield` | Force greenfield mode regardless of file-existence probe |
| `brownfield` | Force brownfield mode regardless of file-existence probe |
| `rewrite` | Allow re-running on `status: ready` docs; replaces /blueprint-owned sections in place, never touches /think or downstream sections |
| `verbose` | Inline content instead of cross-references; use for portable standalone docs |
| `no-adopt` | Skip `fno backlog intake` after mutation |
| `no-collision-check` | Skip `fno backlog collisions check` before mutation |

Example composing multiple modifiers:

```bash
/blueprint quick greenfield rewrite internal/fno/plans/2026-05-18-foo.md
```

## Blueprint owns the whole plan

Blueprint takes any input and produces the plan.
A feature description, a node id, or a cited-findings doc all work.
The skill resolves a save path via `fno plan path`, seeds the body, and mutates.
A prior `/think` run is optional research, never a prerequisite.
`/target` chains blueprint into do and ship whether or not think ran.
