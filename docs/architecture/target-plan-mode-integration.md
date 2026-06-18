# Target × native Plan Mode integration (Mode 1 front door)

Connects Claude Code's native **Plan Mode** to the `/target`
pipeline. After you research-and-approve a plan in Plan Mode (Shift+Tab →
`ExitPlanMode` → approve), the next bare `/target` detects that approved plan,
backfills the structure target's gates require, shows you what it added, and on
your confirm executes it through the normal do → review → ship loop.

Guiding principle: **the native plan supplies the intent; footnote supplies the
rigor.** Target's completion gates are never relaxed; the missing structure is
backfilled.

This is **Phase 1 (Mode 1)** only. Mode 2 (target wrapping its own
`/think`+`/blueprint` plan in the native approval card) and the `claude --bg`
execution host are a separate Phase 2 design.

## Flow

```
You: Shift+Tab -> Plan Mode -> ExitPlanMode(plan) -> APPROVE
        |
        v  [hook] hooks/capture-plan-mode.sh  (PostToolUse matcher: ExitPlanMode)
   .fno/.pending-plan.md   (frontmatter: captured_at, session_id, slug,
                                  source: claude-plan-mode, status: pending
                                  + the approved plan body, VERBATIM)
        |
        v  You: > /target            (bare, no argument)
   init  -> session-start wipe of a stale sidecar (init-target-state.sh)
   detect-> detect-pending-plan.sh detect  ->  result=pending
        |
        v  [backfill]  skills/target/scripts/backfill-plan.sh + the skill body
   body -> skeleton -> (LLM synthesizes ## Failure Modes + 5 BDD ACs) ->
   check-sections (<=2 retries) -> /blueprint -> render-diff
        |
        v  "Execute autonomously? [y/N]"
   y -> detect-pending-plan.sh consume  (sidecar -> consumed) -> do/review/ship
   N -> sidecar stays pending (re-offerable)
```

## Components

| Component | File | Role |
|---|---|---|
| Capture hook | `hooks/capture-plan-mode.sh` | PostToolUse(`ExitPlanMode`) → writes the sidecar. Registered in `hooks/hooks.json` (plugin manifest), NOT `.claude/settings.json`. |
| Session-start wipe | `hooks/helpers/init-target-state.sh` | Removes a stale sidecar on fresh init (session-mismatch or past-TTL), mirroring `.target-cancelled`. |
| Backfill scaffolding | `skills/target/scripts/backfill-plan.sh` | `skeleton` / `check-sections` / `render-diff` — the deterministic parts. |
| Detection + consume | `skills/target/scripts/detect-pending-plan.sh` | `detect` / `body` / `consume` — precedence, body extraction, atomic consume. |
| Orchestration | `skills/target/SKILL.md` §3f-pm | The LLM-facing flow, including the one new reasoning step (synthesizing Failure Modes + ACs). |
| Contract | `skills/target/references/plan-mode-backfill.md` | Sidecar schema + backfill contract. |

## The sidecar `.fno/.pending-plan.md`

Inline-frontmatter only (the stdlib reader cannot parse block lists), all
scalars. `status` is the one mutable field: `pending` → `consumed`, flipped
ONLY after confirm-yes. Body is the native plan, byte-for-byte. Last-writer-wins
(a fresh approval overwrites a prior pending sidecar).

## The chicken-and-egg, and why synthesis precedes /blueprint

`/blueprint` hard-refuses a doc without `## Failure Modes` (literal
`grep -q '^## Failure Modes$'`) and requires `status` ∈ {design, ready}. A
native plan has neither. So the adapter must synthesize `## Failure Modes` (the
four sub-labels Boundaries/Errors/Invariants/Concurrency) and the 5 BDD AC types
(HP/ERR/UI/EDGE/FR) **before** calling `/blueprint`. Those are `/think`'s
artifacts: native Plan Mode replaces `/think`'s interactive *exploration*, and
the adapter runs `/think`'s *artifact generation* against the approved plan. The
synthesis is LLM-powered (it reads the plan's intent); `check-sections` is the
deterministic gate that bounds retries to 2 and names exactly what is missing so
a retry re-synthesizes only the rejected section. A native plan that already
contains a section is reused, never duplicated.

## Concurrency: the consume CAS

`consume` flips `pending → consumed` under two locks:

1. A local atomic `mkdir` lock (`<sidecar>.consume.lock`) — race-safe even when
   `fno` is absent. Stale locks (>30s, holder died mid-flip) are stolen.
2. An `fno claim` (`pending-plan:<slug>`) for cross-session/host coordination,
   **released once the flip lands** so a later same-slug re-approval is not
   falsely blocked by the 30m TTL.

The flip is a compare-and-set: re-read `status`; abort (exit 3) if no longer
`pending`. Two racing `/target` runs therefore collapse to a single execution.

## Detection path (Open Question 1, source-confirmed)

Approve and keep-planning route to **different hook events**. A kept-planning /
rejected `ExitPlanMode` fires `PermissionDenied` (the can-use-tool path);
`PostToolUse` fires only after a successful tool `call()`, i.e. after approval.
So a `PostToolUse(ExitPlanMode)` fire **already means the plan was approved** —
the event type is the discriminator. The `Output` (`tool_response`) carries no
approval field: there is no `approved` / `decision` / `isError`, so the original
guard on those was vacuous (testing fields that read as undefined and never
match). The hook captures on every fire and skips only on the one genuine
pending signal, the teammate path's `awaitingLeaderApproval == true` (plan
submitted to a team lead, not yet approved). The plan body is read from disk
first (`tool_response.filePath` → `tool_input.planFilePath` → inline `plan`),
because the V2 tool's inline `plan` is frequently `null` with the body saved to a
file — so reading only the inline field silently missed real approved plans. The
`/target` confirm step remains the human backstop.

Source provenance (open-sourced Claude Code tree, ~2.1.143-156): the approve/
reject fork lives in `toolExecution.ts` (rejected can-use-tool `:1001` →
`PermissionDenied` `:1081`, vs `PostToolUse` `:1483`); the `Output` schema (no
approval field; `filePath`, `awaitingLeaderApproval`) in
`ExitPlanModeV2Tool.ts:110-142, 304-312`. A live installed-build capture
confirmed the approved payload shape (`{plan:null, isAgent:false, filePath,
hasTaskTool:true}`) and that keep-planning emits no `PostToolUse` event. The
deferred follow-up on attended rejection is therefore resolved (no live attended
rejection was ever required — that payload does not exist on this channel by
construction).

## Graceful degradation (Claude-Code-only, no-op elsewhere)

`EnterPlanMode`/`ExitPlanMode` are Claude Code tools. On Gemini/Codex the
matcher never fires, no sidecar is written, and `/target` behaves exactly as
today. The feature lives entirely in the optional hook + skill-relative scripts,
so the portable driver-skill contract (CI-enforced self-containment) is intact.
See `docs/SKILL-COMPAT-MATRIX.md` (the `target (plan-mode front door)` row).

**Headless / megawalk runs skip the front door entirely** (it requires a human
confirm). The bundled `archer` agent carries an explicit attended-only guard, so
a headless worker never detects, backfills, or consumes a sidecar
(Open Question 2: headless never consumes).

## Failure handling

- Hook write failure → logged to `.fno/hook-events.jsonl`
  (`plan_mode_capture_failed`), never fatal (exit 0).
- Empty/whitespace plan → no sidecar. Malformed sidecar → logged, treated absent.
- A torn sidecar yielding an empty body → `detect-pending-plan body` exits 2 (no
  silent empty plan into backfill).
- Backfill failure after 2 retries, or a `/blueprint` failure → surface the
  partial doc path and STOP; never enter the autonomous loop half-built. The
  sidecar stays `pending`, so a declined or failed run is re-offerable.

## Tests

`tests/hooks/test_capture_plan_mode.sh`, `tests/hooks/test_pending_plan_wipe.sh`,
`tests/target/test_backfill_plan.sh`, `tests/target/test_detect_pending_plan.sh`,
`tests/target/test_plan_mode_e2e.sh` (wired into `.github/workflows/cli-ci.yml`).
