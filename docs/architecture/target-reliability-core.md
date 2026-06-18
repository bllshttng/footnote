# Target Reliability Core

This document covers two improvements to target that catch the most common
non-code failure modes before they burn iterations: the
**target-preflight** skill and **phase handoff artifacts**.

## The problem

A meaningful share of target BLOCKED states are not code bugs. They are:

- **Environmental:** dirty working tree, expired auth, missing dependencies,
  stale codemap.
- **Cross-phase context drift:** each phase reconstructs context from the
  full session, drifts, and sometimes loses critical info from the prior
  phase (a deferred story, an edge case noted, a file intentionally left
  alone).

Both fail the same way: target spends 15-20 minutes on a pipeline that
was always going to fail, and only discovers the problem at ship time.

The reliability core attacks both surfaces.

## Part 1: target-preflight

### What it is

A small, fast, read-only audit that target invokes before phase 1 of any
run. It executes a sequence of named checks, each producing one of
`pass | fail | warn | unknown` with a one-line message.

The preflight checks live under `skills/target/scripts/preflight/`. The
orchestrator is `skills/target/scripts/preflight/run-checks.sh`. Each check
is an independent shell script under `skills/target/scripts/preflight/checks/`.

### Contract

- Output: human-scannable lines with glyph (✓ ✗ ⚠ ?) per check, plus a
  JSON summary on the last line.
- Exit code: `0` if all checks pass or warn; non-zero if any check fails.
- Read-only: the skill never modifies the workspace.
- Each check exits `0` regardless of result. Failure is encoded in
  stdout, not exit code, so a single buggy check cannot kill the runner.
- Each check has a runtime budget under 2 seconds.

The full check catalog is at
[skills/target/references/preflight-checks.md](../../skills/target/references/preflight-checks.md).

### Canonical checks

| Check | Detects |
|---|---|
| `working-tree-clean` | uncommitted changes |
| `branch-state` | detached HEAD, dangerous branches (main, master, prod) |
| `deps-installed` | missing `node_modules` or `.venv` relative to lockfile mtime |
| `test-suite-green` | broken state at HEAD before changes (opt-in) |
| `codemap-fresh` | stale `.fno/codemap.md` (older than 24h) |
| `auth-valid` | unauthenticated `gh` CLI |
| `disk-space` | low free space in `$HOME` |

`test-suite-green` is opt-in via `PREFLIGHT_RUN_TESTS=1` because of its
60-second budget. The other checks all complete well under 2 seconds.

### Wiring into target

`skills/target/SKILL.md` invokes
`run-checks.sh` immediately after `target-state.md` is written. On
failure, the wrapper:

1. Records the failed checks to
   `.fno/artifacts/preflight-{session_id}.md`.
2. Touches `.fno/.target-cancelled` (the typed-blocker cancel
   sentinel - the stop hook owns BLOCKED writes; the LLM does not).
3. Emits `<promise>MISSION BLOCKED: preflight failure ...</promise>`
   and exits.

The cancel-sentinel pattern matters because the typed-blocker invariant
(shipped 2026-04-28) made `status: BLOCKED` hook-written-only.
The LLM cannot author BLOCKED directly; the stop hook reverts any forged
attempt. Touching `.target-cancelled` is the supported way to request a
BLOCKED transition.

### Override

`--skip-preflight` bypasses the check chain for the rare case where the
operator knows about a specific dirty file and wants to start anyway.
The bypass decision is recorded in state for forensic review.

## Part 2: Phase handoff artifacts

### What they are

Every target phase (`think`, `plan`, `do`, `clean`, `review`, `validate`,
`ship`, `external`, `docs`) writes a small structured artifact at
`.fno/artifacts/handoff/{phase}-{session_id}.md`. The next phase
reads its predecessor's artifact at start.

### Path namespacing

There are now two artifact families under `.fno/artifacts/`:

- **Gate-attestation artifacts** at `.fno/artifacts/{phase}-{session_id}.md`.
  Top-level. Already shipping from earlier work. The stop hook reads these
  to verify factor 2 of the three-factor gate check.
- **Handoff artifacts** at `.fno/artifacts/handoff/{phase}-{session_id}.md`.
  Subdirectory. Introduced by this work. Carry per-phase structured
  context for the next phase.

The subdirectory exists to prevent filename collision with the
gate-attestation contract. Both families coexist after a real target run.

### Format

Each handoff artifact is YAML frontmatter + markdown body:

```yaml
---
phase: do
session_id: f31156b3
timestamp: 2026-04-27T14:32:00Z
status: complete
stories_completed: [1, 2, 3]
stories_deferred:
  - id: 4
    reason: "needs supabase migration first"
    blocking: false
files_changed:
  - src/auth/login.ts
  - src/auth/login.test.ts
notes_for_next_phase: |
  Story 4 deferred until migration lands. Clean phase should skip
  auth/migration files. Review can ignore the empty migration directory.
---

# Phase do summary

[Optional human-readable narrative, under 200 words.]
```

The wrapper format is universal so a generic reader always works. The
structured fields vary per phase; per-phase schemas are documented in
[skills/target/references/phase-artifacts.md](../../skills/target/references/phase-artifacts.md).

### Helpers

`scripts/lib/phase-handoff.sh` is a sourceable shell library:

- `ph_write <phase> <session_id> <yaml_payload>` writes atomically
  (tmp file + rename + EXIT trap to clean tmp on early exit), enforces
  a 500-token soft cap with truncation marker, and refuses to overwrite
  an existing artifact for the same phase + session.
- `ph_read <phase> <session_id>` validates frontmatter markers exist,
  emits the frontmatter as JSON to stdout, and returns non-zero on
  malformed input.
- `ph_read_latest <phase>` reads from the most recent session_id (used
  by cross-session resume scenarios). Documented as a best-effort
  helper; callers should pass a session_id explicitly when correctness
  matters.
- `ph_list <session_id>` enumerates all phase artifacts for a session.

All helpers print one-line confirmations to stderr so the shell's
stdout stays clean for completion signals.

### Why this isn't another gate

Handoff artifacts are **additive context**, not gates. A phase that
fails to write its artifact still allows the next phase to run; the next
phase logs `no prior handoff` and proceeds with reduced context. This
is by design - the goal is reduced drift, not new failure modes.

The completion-gate machinery (three-factor verification, the stop hook
`verify_provenance` check, skip-flag drift detection) is unchanged. The
core target state machine (`status` enum, completion gates) is also
unchanged.

## What this enables

Both improvements set up the **autocorrect** spec, which consumes
categorized BLOCKED reasons from the typed-blocker spec plus the
structured per-phase context from handoff artifacts to build the
monthly review packet that drives skill-level improvements over time.

Preflight is also a structural input to the **target-postmortem** spec, which uses preflight outcomes to classify the dominant failure modes.

## Boundaries

- Preflight never modifies the workspace. Every check is read-only.
- Handoff artifacts are read-only after their phase ends; subsequent
  phases must not rewrite them. `ph_write` enforces this by refusing
  to overwrite an existing artifact for the same phase + session pair.
- Two target runs in different worktrees use different session IDs, so
  artifact files never collide even if they touch the same project.
- Preflight check itself errors are graceful: a buggy check reports
  `unknown` rather than failing preflight as a whole. The orchestrator
  surfaces the first stderr line so the bug is debuggable instead of
  invisible.
