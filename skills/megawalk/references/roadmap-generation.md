# Roadmap Generation

**Load when:** the user invokes `/megawalk roadmap <vision.md>`. Covers vision-doc validation, optional council phase, roadmap-generator dispatch, campaign + loop state initialization, and the post-generation summary.

## Protocol

1. **Validate input:** Ensure vision doc exists and is non-empty.

2. **Council Phase (`council` modifier only):**

   If `council` modifier is present:
   1. Invoke `/think panel auto standard` with the vision document as the decision context
   2. Wait for the think-tank report
   3. Pass the ranked recommendations from the report to the roadmap-generator agent as additional context:
      - Prioritization guidance from the council consensus
      - Risk areas flagged by the panel
      - Features the council recommended AGAINST (with rationale)
   4. The roadmap-generator uses this to produce a council-informed backlog

   If `council` is not present: skip to step 3 (existing behavior).

   This adds ~5-10 minutes to roadmap generation but produces a backlog informed by multi-perspective analysis rather than a single-pass vision read.

3. **Generate roadmap ID:** `rm-YYYYMMDD-XXXXXX` (date + 6 random hex chars)

4. **Spawn roadmap-generator agent** with:
   - Vision document content
   - Think-tank consensus (if `council`, else omit)
   - Existing done tasks from ledger.json (via `roadmap-tasks.py read --status done`)
   - Domain profiles from settings.yaml

5. **Agent produces tasks** via `roadmap-tasks.py add` (one at a time, atomic writes)

6. **Validate:** Run `validate-roadmap.sh` to check deps, scope, cycles

7. **Initialize campaign state:** Create `.fno/roadmap-state.md` (see [campaign-state.md](campaign-state.md))

8. **Initialize loop state (MANDATORY unless `once`):** Write `.fno/megawalk-state.md` with `status: LOOPING`. Without this file, the stop hook exits approve and the loop dies on first boundary - the file is what keeps the walk-away UX alive across context compactions. If `once` was passed, skip this step (no loop state = no loop enforcement).

   ```yaml
   ---
   roadmap_id: {roadmap_id}
   status: LOOPING
   mode: loop
   current_task_id: null
   tasks_completed_this_session: 0
   consecutive_failures: 0
   avg_task_cost: 50
   total_cost_usd: 0
   budget_cap_usd: {from settings.yaml or null}
   # Auto-merge fields - compute auto_merge_approved at init via is_auto_merge_allowed_for "megawalk"
   auto_merge_approved: {true|false}
   merged_prs: []
   merge_auto_queued: []
   merge_failed: []
   conflicts_resolved: []
   ---
   ```

9. **Display summary:**

   ```
   Roadmap: rm-20260324-a1b2c3
   Vision: path/to/vision.md
   Tasks: 12 generated (0 errors, 1 warning)

   | ID  | Title            | Priority | Domain | Pts | Size |
   |-----|------------------|----------|--------|-----|------|
   | 1   | Setup auth       | high     | code   | 5   | M    |
   | 2   | Build dashboard  | medium   | code   | 8   | M    |
   | 3   | Add billing      | medium   | code   | 13  | L    |
   ...

   Run /megawalk to start executing.
   ```

10. **Begin loop execution:** Load [loop-mode.md](loop-mode.md) and enter the loop. Skip the "Run next" prompt - execution starts immediately. Pass `once` to skip the loop and execute exactly one task instead.
