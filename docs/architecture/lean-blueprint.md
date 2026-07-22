# Lean Blueprint: Single-Doc Architecture

## What it is

`/blueprint` now mutates the upstream design doc in place rather than creating a separate folder plan (a `00-INDEX.md` + N phase files). When a developer runs `/think` and then `/blueprint <doc-path>`, the same markdown file gains execution metadata - waves, file bindings, kill_criteria - appended as new sections without restating the content that already exists. That single file is the canonical artifact for the feature from design through ship. The old folder-plan output is still supported for plans already in flight, but new plans use the single-doc format.

The motivation was measured redundancy: on the `fno-claim` plan, the folder approach produced 14,000 words across 7 files where 5,200 words in 1 file conveyed the same information. Worker briefs under the old approach consumed ~5,500 tokens; scoped briefs under the new approach consume ~1,000.

## Single-doc lifecycle

The plan document's frontmatter `status` field tracks which pipeline stage the plan is in. Progression is monotonic - backward transitions are rejected by `fno.plan._status.validate_transition`.

```
design → ready → in_progress → reviewing → shipping → shipped
```

Each transition corresponds to a pipeline phase:

| Status | Set by |
|--------|--------|
| `design` | `/think` at doc creation time |
| `ready` | `/blueprint` after appending execution sections |
| `in_progress` | `/do` or `/target` when execution starts |
| `in_review` | `/pr create` on PR creation |
| `done` | the merge, via the write-time projection |

## Section ownership

Each pipeline phase owns specific sections and never writes sections owned by earlier phases. This makes mutations auditable via `git diff`: every new line traces to the phase that wrote it.

The allowlist constant `BLUEPRINT_WRITE_ALLOWLIST` in `fno.plan._ownership` defines what `/blueprint` may write:

```python
BLUEPRINT_WRITE_ALLOWLIST = frozenset({
    "Execution Strategy",
    "File Ownership Map",
    "Patterns to Reuse",
    "kill_criteria",
})
```

Any attempt to write outside this set exits with code 2 and names the offending section. The full section ownership table:

| Section | Owning phase |
|---------|-------------|
| Overview, Architecture, User Stories | `/think` |
| Multi-Perspective Findings, Failure Modes | `/think` |
| Acceptance Criteria, Locked Decisions | `/think` |
| Claude's Discretion, Domain Pitfalls | `/think` |
| Open Questions | `/think` (resolved by `/blueprint` only via frontmatter, not section edit) |
| Execution Strategy | `/blueprint` |
| File Ownership Map | `/blueprint` (brownfield only) |
| Patterns to Reuse | `/blueprint` (brownfield only) |
| kill_criteria | `/blueprint` |
| Implementation Log | `/do` or `/target` |
| Review Verdicts | `/review sigma` |
| Ship Record | `/pr create` and `/ship` |

For the full section-ownership reference including valid/invalid transition examples, atomic-mutation contract, and greenfield/brownfield auto-detect logic, see [skills/blueprint/references/single-doc-spec.md](../../skills/blueprint/references/single-doc-spec.md).

## Brief generation layer

Workers never read the canonical plan directly. The operator dispatches a scoped brief at task-assignment time using:

```bash
fno plan brief <plan-path> --task <task-id> [--format markdown|json]
```

The brief contains: one paragraph of project context from Overview, the task's spec from Execution Strategy, acceptance criteria tagged for this task or untagged (fail-open), Locked Decisions whose `affects_surface` intersects the task's file surface plus all untagged ones, Failure Modes filtered the same way, the task's file list, and any Patterns to Reuse whose source file falls within the task's surface. Output is roughly 500-800 words.

Tag-filter semantics are fail-open: entries without `affects_surface` tags are always included. Tags reduce brief size as they accumulate over time; absent tags never drop content.

Exit codes: 0 success, 1 plan not found, 2 contract violation (missing sections, task-id absent from Execution Strategy), 3 malformed content requiring human inspection.

For operator's detection and dispatch logic, see [skills/do/references/single-doc-shape.md](../../skills/do/references/single-doc-shape.md).

## Migration path

- **PR1 (this PR):** single-doc support added; folder plans still work unchanged. `/blueprint` on a design doc appends execution sections in place. `fno plan brief` is available.
- **PR2 (planned):** `fno plan migrate-folder <path>` converts an existing folder plan to single-doc on opt-in. Folder plans show a deprecation warning at operator read time.
- **PR3 (no date):** folder-plan support removed entirely, conditioned on soak period completing.

## References

- Skill body: [skills/blueprint/SKILL.md](../../skills/blueprint/SKILL.md)
- Section-ownership + mutation spec: [skills/blueprint/references/single-doc-spec.md](../../skills/blueprint/references/single-doc-spec.md)
- Operator detection: [skills/do/references/single-doc-shape.md](../../skills/do/references/single-doc-shape.md)
- Status state machine: `fno.plan._status` (`cli/src/fno/plan/_status.py`)
- Ownership allowlist: `fno.plan._ownership` (`cli/src/fno/plan/_ownership.py`)
