# Plan Mode front door: sidecar schema + backfill contract

Claude-Code-only enhancement (Phase 1, Mode 1). After you approve a plan in
native Plan Mode, a PostToolUse hook captures it to a sidecar; the next bare
`/target` detects it, backfills the structure target's gates require, shows you
what was added, and on your confirm runs the normal do -> review -> ship loop.

On any CLI without the capture hook, or with no pending sidecar, this whole
path is a no-op and `/target` behaves exactly as it does today.

## The sidecar: `.fno/.pending-plan.md`

Written by `hooks/capture-plan-mode.sh`. Inline-frontmatter only (the stdlib
frontmatter reader does not parse block-list indentation), all scalars:

```
---
captured_at: 2026-06-02T21:30:00Z     # ISO-8601 UTC; drives the TTL staleness check
session_id: <claude session id>       # from the PostToolUse hook input .session_id
slug: <kebab slug>                    # derived from the plan's first heading / line
source: claude-plan-mode              # provenance discriminator (constant)
status: pending                       # pending -> consumed (set only after confirm-yes)
---

<the approved plan text, VERBATIM>
```

Body is the native plan preserved byte-for-byte. `status` is the one mutable
field: it flips to `consumed` only after the user answers `y` at the confirm
step, so a declined confirm leaves the sidecar re-offerable (AC1-FR).

## Write contract (capture hook)

- Fires on `PostToolUse` matcher `ExitPlanMode` (Claude Code only).
- PostToolUse fires only after the tool's `call()` succeeds, i.e. after the user
  approved; a kept-planning / rejected exit fires `PermissionDenied` (a
  different event the matcher never sees). So a fire here ALREADY means approval
  — the event type is the discriminator, not a field (source-confirmed,
  `ab-588650c7`; see `docs/architecture/target-plan-mode-integration.md`).
- Captures on every fire, SKIPPING only on the one real "not approved yet"
  signal: `tool_response.awaitingLeaderApproval == true` (teammate plan
  submitted to a team lead). The `Output` has no `approved` / `decision` /
  `isError` field — those were never real, so any guard on them was vacuous.
- Reads the plan body from disk first (`tool_response.filePath` →
  `tool_input.planFilePath` → inline `plan`); the V2 tool's inline `plan` is
  frequently `null` with the body saved to a file.
- Empty / whitespace-only plan (no file body, no inline body) -> no sidecar.
- Last-writer-wins: a fresh approval overwrites any prior pending sidecar.
- Never fatal: any error is logged to `.fno/hook-events.jsonl`
  (`plan_mode_capture_failed` / `plan_mode_capture_skipped`) and the hook exits 0.

## Staleness (init-target-state.sh, session-start)

A fresh `/target` init wipes a stale sidecar (mirrors `.target-cancelled`):
wiped when its `session_id` differs from the current Claude session, OR its
`captured_at` is older than `PENDING_PLAN_TTL_SECONDS` (default 14400 = 4h).
A same-session, in-TTL sidecar survives so `/target` can detect it.

## Detection + precedence (skills/target/scripts/detect-pending-plan.sh)

- `detect` — emit the fresh pending sidecar's slug + age, or nothing. A
  malformed sidecar (corrupt frontmatter, missing required fields, wrong
  `source`/`status`) is logged and treated as absent, never fatal.
- Precedence: an explicit `/target "arg"` always wins. When a fresh sidecar
  also exists, target runs the argument and prints a one-line "pending plan
  ignored; run bare /target to use it" note (AC3-HP). Explicit argument XOR
  sidecar -- never both consumed at once.
- `consume` — flips `status: consumed` atomically, only after confirm-yes,
  guarded by an `fno claim` node lock so two racing `/target` runs collapse to
  one execution.

## Backfill (skills/target/scripts/backfill-plan.sh + the /target skill body)

Deterministic scaffolding lives in the script; the one genuinely new piece of
reasoning (synthesizing `## Failure Modes` + `## Acceptance Criteria` from the
native plan's intent) is LLM-powered and orchestrated by the skill body.

- `skeleton <native-plan> <out-doc>` — wraps the native plan in a design-doc
  skeleton + inline frontmatter, body preserved VERBATIM (AC2-FR). If the
  native plan already contains a `## Failure Modes` and/or `## Acceptance
  Criteria` section, it is carried through and reused, never duplicated
  (AC2-EDGE).
- The skill body then synthesizes any missing `## Failure Modes`
  (Boundaries/Errors/Invariants/Concurrency sub-labels) + the 5 BDD AC types
  (AC-HP / AC-ERR / AC-UI / AC-EDGE / AC-FR) into the doc.
- `check-sections <doc>` — validates the gate-required structure is present and
  prints exactly what is missing, so a retry re-synthesizes ONLY the rejected
  section. Bounded to 2 attempts (AC2-ERR); on persistent failure the partial
  doc path is surfaced and target does NOT enter the autonomous loop (AC1-ERR).
- The skill body then invokes `/blueprint <doc>` to append Execution Strategy,
  File Ownership Map, kill_criteria and set `status: ready` (AC2-HP).
- `render-diff <native-plan> <enriched-doc>` — prints the ADDED sections
  distinctly from the original plan body for the confirm step (AC1-UI).

`/blueprint` hard-refuses a doc without `## Failure Modes` (literal
`grep -q '^## Failure Modes$'`), which is why synthesis MUST precede it.
