# Plan Mode Front Door (Mode 1, Claude Code only)

Read this only when a Claude Code native Plan-Mode plan was just approved (a fresh sidecar exists) AND the run is attended. This whole step is a no-op on CLIs without the capture hook, in any unattended / headless run, and whenever no fresh sidecar exists - so `/target` behaves exactly as today there (US4). Deeper backfill-adapter contract: [plan-mode-backfill.md](plan-mode-backfill.md).

After init (which session-start-wipes a stale sidecar) and before preflight, check for the approved plan.

**Attended-only.** SKIP this entire step in any unattended / headless run - megawalk-spawned workers (e.g. the bundled `archer` agent), `--unattended`, `config.unattended.enabled`, or any context with no interactive human. The front door requires a human confirm (`[y/N]`), so a headless run must NOT detect, backfill, or consume a sidecar (Open Question 2: headless never consumes). Those runs already carry their own plan/backlog node, so there is nothing to detect anyway.

Run detection (pass the user's explicit argument, if any, so precedence is decided):

```bash
DET="$(bash "${SKILL_DIR}/scripts/detect-pending-plan.sh" detect --arg "${ORIGINAL_ARG:-}")"
RESULT="$(printf '%s\n' "$DET" | sed -n 's/^result=//p')"
```

Branch on `$RESULT`:

- `none` / `expired` / `malformed` -> proceed with normal `/target` behavior. For
  `superseded_by_arg`/`malformed`/`expired` the sidecar was logged, not fatal.
- `superseded_by_arg` -> the user gave an explicit argument AND a fresh sidecar
  exists. The explicit argument WINS (US3). Print exactly once, then proceed with
  the argument (the sidecar stays `pending`, re-offerable):
  > "a pending approved plan also exists; ignored in favor of your argument. Run bare /target to use it."
- `pending` -> run the **backfill adapter**, then confirm:
  1. Extract the native body: `bash "${SKILL_DIR}/scripts/detect-pending-plan.sh" body "$STAGE/native.md"`.
  2. Skeleton: `bash "${SKILL_DIR}/scripts/backfill-plan.sh" skeleton "$STAGE/native.md" "$STAGE/enriched.md"` (stage under `.fno/`, e.g. `.fno/.pending-plan.enriched.md`). Read its `has_failure_modes` / `has_acceptance_criteria` report.
  3. **Synthesize (LLM step, the one new piece of reasoning):** if a section is reported absent, append it to the enriched doc from the native plan's intent - a `## Failure Modes` section with the four bold sub-labels `**Boundaries**` / `**Errors**` / `**Invariants**` / `**Concurrency**`, and a `## Acceptance Criteria` section with all 5 BDD types (`#### AC1-HP:` / `AC1-ERR:` / `AC1-UI:` / `AC1-EDGE:` / `AC1-FR:`). A section reported present is REUSED, never duplicated (AC2-EDGE). Preserve the native body verbatim - ADD sections only (AC2-FR).
  4. Validate + bounded retry: `bash "${SKILL_DIR}/scripts/backfill-plan.sh" check-sections "$STAGE/enriched.md"`. If it lists `missing:` items, re-synthesize ONLY those sections and re-check. **Max 2 attempts.** On persistent failure, print the partial doc path and STOP - do NOT enter the autonomous loop on a half-built plan (AC1-ERR / AC2-ERR).
  5. Blueprint: invoke `/blueprint "$STAGE/enriched.md"` (Skill) to append Execution Strategy + File Ownership Map + kill_criteria and set `status: ready`. A `/blueprint` failure surfaces and stops (AC1-ERR).
  6. Show the diff + confirm: `bash "${SKILL_DIR}/scripts/backfill-plan.sh" render-diff "$STAGE/native.md" "$STAGE/enriched.md"`, then ask the user **"Execute autonomously? [y/N]"** (no auto-proceeding default - AC1-UI).
     - **y** -> `bash "${SKILL_DIR}/scripts/detect-pending-plan.sh" consume --holder "target-session:$SESSION_ID"`. If consume exits 3 (already consumed / claimed by a racing run), STOP - another `/target` owns this plan (Concurrency). On success, move/keep the enriched doc as the canonical plan, set it as `plan_path`, and continue into the normal do -> sigma-review -> ship loop.
     - **N** -> leave the sidecar `pending` (do NOT consume). Print "Kept. Run bare `/target` again to use it." and stop (AC1-FR).

Consume ONLY after confirm-yes (Invariant: a declined confirm leaves the plan re-offerable; one authoritative plan per run - explicit argument XOR sidecar).
