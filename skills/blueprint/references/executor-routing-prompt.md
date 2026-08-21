# Executor Routing Prompt

When `/blueprint` reaches the Executor Lock Transcription gate on a design doc that touches a frontend or mixed surface, load this reference.
It detects the surface mix, decides whether to prompt or auto-lock, and writes the result to the design doc's `## Locked Decisions` section.

The output of this step is a single Locked Decision entry that `/blueprint`
transcribes into the implementation plan's `executor:` frontmatter (see
`skills/blueprint/SKILL.md`, section "Executor Lock Transcription"). The
section anchor is more stable than a step number. Step ordering shifts with
skill revisions, but the section heading is the contract.

## Why this exists

The operator routes tasks through a three-tier resolver
(`task.executor` → `plan.executor` → surface inference). The surface matcher
lives in the in-package module `fno.executor._surface` and is locked by
PR #196's plan.
But the design phase, where the surface decision is actually made, has no
hook for capturing intent. Plan authors who understand the resolver can set
`executor:` manually. Everyone else gets surface inference at runtime, which
is the right default. It cannot express "I want the design-aware loop on
this whole plan" up front.

`/blueprint` is the right place to capture that intent. The design doc's
architecture section, user stories, and file lists already imply the surface
mix. The work this reference codifies is to read those signals, decide on a
routing, and lock it.

## Detection rules

The helper at `detect-surface.sh` (sibling to this doc) implements the rules
mechanically. Run it from the skill body like this:

```bash
HELPER="${SKILL_DIR}/references/detect-surface.sh"
# DESIGN_TEXT is the user's description plus any prior /think output (the
# stories + architecture sections so far).
SURFACE=$(printf '%s' "$DESIGN_TEXT" | bash "$HELPER")
# SURFACE is one of: frontend-touching | backend-only | mixed | unknown
```

The helper anchors on:

| Family | Vocabulary |
|--------|------------|
| Frontend nouns | UI, page, screen, component, button, form, modal, dropdown, sidebar, layout |
| Frontend frameworks | React, Vue, Svelte, Next.js, Angular, Solid |
| Frontend filenames | `.tsx`, `.jsx`, `components/`, `routes/`, `src/styles/` |
| Backend nouns | API, schema, migration, queue, worker, batch, ETL, ingest |

Matching is word-boundary anchored (`\bform\b` matches "form button" but
not "inform users") and case-insensitive on nouns and frameworks. The
filename arm is case-sensitive. Frontend folder conventions are reliably
lowercase, and case-insensitive filename matching can silently misroute
backend `api/` directories.

Outputs:

- `frontend-touching` - frontend signals only. Lock to `impeccable`.
- `backend-only` - backend signals only. No lock. The runtime resolver picks
  `do` via surface inference.
- `mixed` - both signals fire. Lock plan-level to `do` and surface
  per-task `executor: impeccable` overrides for tasks whose file lists
  match the surface-inference patterns. This mirrors the operator's
  three-tier resolver and keeps cost honest: impeccable runs only where
  it earns its keep.
- `unknown` - neither family matched. Treat like backend-only at the
  call site (no prompt, no lock). The runtime resolver still has the
  surface-inference fallback, so this is a safe default.

## Mode resolution

There are three call modes, in priority order:

1. If `FNO_EXECUTOR_OVERRIDE` is set (the `cli-flag` path), write that value to Locked Decisions immediately.
   No detection, no prompt.
   This is how `/target M --executor <value>` plumbs intent down to `/blueprint`.
   Acceptable values are `do`, `impeccable`, or `mixed` (case-insensitive).
   Garbage values are rejected at `/target` entry, not accepted here.
2. If `.fno/target-state.md` exists (the `auto-detected` path), `/blueprint` runs inside an autonomous target session and cannot block on user input.
   Run the detection rules and lock the result without prompting.
   Pure-backend sessions never lock at all.
   The absence of a lock is the signal.
3. With no CLI flag and no target context (the `user-confirmed` path), `/blueprint` runs standalone.
   If the detection result is anything other than `backend-only` or `unknown`, fire the prompt below and capture the user's answer.
   Otherwise skip the prompt entirely.

## Prompt template (standalone mode only)

```
This design touches {detected_surfaces}. Lock executor routing now?

  tdd (default)       TDD-disciplined archer. Best for backend, infra,
                      scripts, configs.
  impeccable          frontend-executor + /impeccable craft+critique loop.
                      Best for design-quality-sensitive frontend work.
  mixed               Per-task in the spec phase (some tasks 'tdd', some
                      'impeccable'). Pick this if the plan has both.

Choice: [user replies]
```

Re-prompt on malformed responses. Map common variations:

- `1`, `do`, `default`, `tdd` → `tdd`
- `2`, `impeccable`, `frontend`, `design` → `impeccable`
- `3`, `mixed`, `both`, `per-task` → `mixed`
- anything else → re-prompt with the choices restated. Never auto-resolve
  to a silent default. That hides intent.

## Decision capture format

Write a single Locked Decisions entry with one of these provenance suffixes:

```markdown
N. **Executor routing**: plan-level `executor: impeccable` (auto-detected).
   Rationale: this design is frontend-only (settings page, theme toggle, account
   dropdown); /impeccable's banned-pattern detection and critique loop will
   catch design-token mismatches that archer misses.
```

```markdown
N. **Executor routing**: plan-level `executor: tdd` with per-task overrides
   `executor: impeccable` on tasks touching `**/*.tsx`, `components/**`,
   `routes/**`, `src/styles/**` (auto-detected).
   Rationale: design has a frontend page and a backend migration; impeccable
   runs only on the surface that benefits from it.
```

```markdown
N. **Executor routing**: plan-level `executor: tdd` (cli-flag).
   Rationale: passed via `/target M --executor tdd`. Operator overrode the
   surface-inference default.
```

The provenance suffix is one of `(auto-detected)`, `(user-confirmed)`,
`(cli-flag)`.
`/blueprint` parses with tolerance for whitespace, casing, and absent suffixes (per Domain Pitfall #4).
When the suffix is present, `/blueprint` writes it through so the source of the decision survives PR review.

## Mixed-mode per-task overrides

When the result is `mixed`, the entry must explicitly list which file
patterns map to `impeccable` so `/blueprint` can emit per-task overrides. The
patterns echo `fno.executor._surface`'s locked list:

```
**/*.tsx, **/*.jsx, components/**, routes/**, src/styles/**
```

`/blueprint` reads those patterns from the Locked Decisions entry, walks each phase's file list, and emits `executor: impeccable` blocks on matching tasks. Plan-level frontmatter remains `executor: tdd`. Tasks that match nothing inherit the plan default.

## What this skill does NOT do

- It does not modify `fno.executor._surface`. The runtime inference list is locked by PR #196's plan and stays as-is.
- It does not pick `/impeccable` subcommands. The choice is `tdd | impeccable | mixed`. The agent decides which subcommands to run inside `impeccable` (today: `craft` + `critique`).
- It does not retro-stamp existing plans. Only plans authored via the new `/think → /blueprint` flow get the lock. Older plans rely on surface inference at runtime, which already handles them correctly.
