<!-- style-exception: mechanical verb rename preserves pre-existing prose -->
# Explicit Capability Escalation

Context pressure is a compaction trigger, never a succession trigger. Continue in this session across blueprint/do and wave boundaries. A fresh session is reserved for capability escalation after an external operator or supervising king selects a stronger destination.

The current worker may signal that it is stuck. It may not select its own replacement or infer a destination from ambient harness, model, or account state. The external caller invokes `bash "${SKILL_DIR}/scripts/handoff.sh" --harness <harness> --model <model> [--account <id> | --dispatch-account <id>]` and obeys the decision line.

**Claim-wait BLOCKED:** If `fno do target init` (or `init-target-state.sh`) output contains `RESULT: BLOCKED`, the session MUST stop immediately. Relay the block contract as your final output (`REASON: ...` / `UNBLOCKS_AFTER: ...`). Do NOT run any pipeline phases without a live claim.

**Transaction proof**

The helper must prove the selected child can work before parent ownership moves. A registered pane or prompt-ready pane is insufficient. The child must answer the random capability challenge with the exact digest, and `fno agents truth` must report the configured observed model. After claim release, delegation commits only when the child holds `node:<id>` and its new target manifest names the child's harness session.

Any failure before parent mutation stops the uncommitted child and leaves parent state untouched. Any later failure restores the parent claim and manifest. If restoration or reacquisition fails, the parent stops on the explicit exit-12 outcome.

**Decision-line handling**

| Exit | Decision line prefix | Action |
|------|---------------------|--------|
| 0 | `delegated <node> ...` | Print `result: do-phase delegated to <child> (<session>)`. Then **stop immediately** - do NOT continue pipeline phases, do NOT run `claude stop`. The parent's close is sanctioned; the stop hook allows exit because the manifest was archived. |
| 10 | `parked <node> reason="..."` | Continue in-session. The parent retains or has restored its ownership. |
| 12 | `handoff-restore-failed <node> ...` | Emit `<help reason="handoff-restore-failed" evidence="<reason>">` and stop work. Never continue silently without a manifest. |
| 12 | `handoff-claim-lost <node> ...` | Emit `<help reason="handoff-claim-lost" evidence="<reason>">` and stop work. The claim may be held by another worker; do NOT continue in-session on this node. |

## Cross-project is retired (migration shim)

The `scope: cross-project` parallel-worktree pipeline has been removed. A session works only in its OWN project; foreign work is spawned into its project via `fno agents spawn --cwd <root>` (spawn-into-project). A multi-repo feature is now a set of single-project backlog nodes linked by `blocked_by`, each shipping its own PR in its own repo.

**Check target-state.md BEFORE any execution.** If `cross_project: true` (a legacy `cross-project` subcommand, or a plan with `scope: cross-project`):

1. **WARN** the user: "scope: cross-project is deprecated and the parallel pipeline was removed. Model multi-repo work as one backlog node per project (linked by blocked_by); each ships its own PR. Use `fno backlog decompose` to split a legacy plan."
2. **Do NOT** invoke any cross-project pipeline (removed) and **do NOT** `cd` into other repos to write code.
3. **Route to spawn-into-project:** continue THIS session in its own project only. Foreign waves are handled by `/execute` (auto-spawn when the foreign node is unblocked; defer + carveout when it is blocked); cross-project dependents are dispatched on merge by `fno backlog advance`.

`cross_project: true` no longer forks the pipeline; it only triggers this deprecation warning + the spawn-into-project routing above. The manifest field and the plan-graduation timing in `fno-agents finalize` are retained so an already-stamped legacy plan still parses and graduates correctly.
