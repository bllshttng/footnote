# Executor Routing Prompt

**Load when:** Step 6.5 of `/think` (after the UI state machine audit, before
generating BDD acceptance criteria). The skill calls into this reference to
detect surface mix, decide whether to prompt or auto-lock, and write the
result to the design doc's `## Locked Decisions` section.

The output of this step is a single Locked Decision entry that `/blueprint` will
later transcribe into the implementation plan's `executor:` frontmatter
(see `skills/blueprint/SKILL.md`, section "Executor Lock Transcription"). The
section anchor is more stable than a step number because step ordering
shifts with skill revisions; the section heading is the contract.

## Why this exists

Today the operator routes tasks through a three-tier resolver
(`task.executor` → `plan.executor` → surface inference); the surface matcher
lives in the in-package module `fno.executor._surface` and is locked by
PR #196's plan. But
the design phase, where the surface decision is actually being made, has no
hook for capturing intent. Plan authors who understand the resolver can set
`executor:` manually; everyone else gets surface inference at runtime, which
is the right default but cannot express "I want the design-aware loop on
this whole plan" up front.

`/think` is the right place to capture that intent because the architecture
section, user stories, and file lists already imply the surface mix. The
work this reference codifies is: read those signals, decide on a routing,
and lock it.

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
filename arm is case-sensitive - frontend folder conventions are reliably
lowercase, and case-insensitive filename matching would silently misroute
backend `api/` directories.

Outputs:

- `frontend-touching` - frontend signals only. Lock to `impeccable`.
- `backend-only` - backend signals only. No lock; runtime resolver picks
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

1. **CLI flag (`cli-flag` provenance).** If `FNO_EXECUTOR_OVERRIDE`
   is set in the environment, write that value to Locked Decisions
   immediately. No detection, no prompt. This is how `/target M
   --executor <value>` plumbs intent down to /think. Acceptable values:
   `do`, `impeccable`, `mixed` (case-insensitive). Garbage values must
   be rejected at /target entry, not silently accepted here.
2. **Target autonomous (`auto-detected` provenance).** If
   `.fno/target-state.md` exists, /think is running inside an
   autonomous target session and cannot block on user input. Run the
   detection rules and lock the result without prompting. Pure-backend
   sessions never lock at all; the absence of a lock is the signal.
3. **Standalone interactive (`user-confirmed` provenance).** No CLI
   flag, no target context. If the detection result is anything other
   than `backend-only` or `unknown`, fire the prompt below and capture
   the user's answer. If the detection is `backend-only` or `unknown`,
   skip the prompt entirely.

## Prompt template (standalone mode only)

```
This design touches {detected_surfaces}. Lock executor routing now?

  do (default)        TDD-disciplined archer. Best for backend, infra,
                      scripts, configs.
  impeccable          frontend-executor + /impeccable craft+critique loop.
                      Best for design-quality-sensitive frontend work.
  mixed               Per-task in the spec phase (some tasks 'do', some
                      'impeccable'). Pick this if the plan has both.

Choice: [user replies]
```

Re-prompt on malformed responses. Map common variations:

- `1`, `do`, `default`, `tdd` → `do`
- `2`, `impeccable`, `frontend`, `design` → `impeccable`
- `3`, `mixed`, `both`, `per-task` → `mixed`
- anything else → re-prompt with the choices restated. Never auto-resolve
  to a silent default; that hides intent.

## Decision capture format

Write a single Locked Decisions entry with one of these provenance suffixes:

```markdown
N. **Executor routing**: plan-level `executor: impeccable` (auto-detected).
   Rationale: this design is frontend-only (settings page, theme toggle, account
   dropdown); /impeccable's banned-pattern detection and critique loop will
   catch design-token mismatches that archer misses.
```

```markdown
N. **Executor routing**: plan-level `executor: do` with per-task overrides
   `executor: impeccable` on tasks touching `**/*.tsx`, `components/**`,
   `routes/**`, `src/styles/**` (auto-detected).
   Rationale: design has a frontend page and a backend migration; impeccable
   runs only on the surface that benefits from it.
```

```markdown
N. **Executor routing**: plan-level `executor: do` (cli-flag).
   Rationale: passed via `/target M --executor do`. Operator overrode the
   surface-inference default.
```

The provenance suffix is one of `(auto-detected)`, `(user-confirmed)`,
`(cli-flag)`. `/blueprint` parses with tolerance for whitespace, casing, and
absent suffixes (per Domain Pitfall #4) but writes the suffix through when
present so the source of the decision survives PR review.

## Mixed-mode per-task overrides

When the result is `mixed`, the entry must explicitly list which file
patterns map to `impeccable` so `/blueprint` can emit per-task overrides. The
patterns echo `fno.executor._surface`'s locked list:

```
**/*.tsx, **/*.jsx, components/**, routes/**, src/styles/**
```

`/blueprint` reads those patterns from the Locked Decisions entry, walks each
phase's file list, and emits `executor: impeccable` blocks on matching
tasks. Plan-level frontmatter remains `executor: do`. Tasks that match
nothing inherit the plan default.

## What this skill does NOT do

- It does not modify `fno.executor._surface`. The runtime
  inference list is locked by PR #196's plan and stays as-is.
- It does not pick `/impeccable` subcommands. The choice is
  `do | impeccable | mixed`; the agent decides which subcommands to run
  inside `impeccable` (today: `craft` + `critique`).
- It does not retro-stamp existing plans. Only plans authored via the
  new `/think → /blueprint` flow get the lock. Older plans rely on surface
  inference at runtime, which already handles them correctly.
