# Self-Handoff at Pipeline Boundaries (ab-534bcc55)

Read this at a pipeline boundary (blueprint->do, or a wave boundary during do/review) when deciding whether to hand the rest of the run to a fresh-context successor. Never invoke mid-wave or mid-task.

Session succession hands the rest of the pipeline to a fresh-context worker via `bash "${SKILL_DIR}/scripts/handoff.sh"`. The helper performs all state mutations atomically; the LLM invokes it and obeys the decision line.

**Claim-wait BLOCKED:** If `fno target init` (or `init-target-state.sh`) output contains `RESULT: BLOCKED`, the session MUST stop immediately. Relay the block contract as your final output (`REASON: ...` / `UNBLOCKS_AFTER: ...`). Do NOT run any pipeline phases without a live claim.

**blueprint->do boundary (structural)**

- **Unattended** (`attended: false` in target-state.md): run `bash "${SKILL_DIR}/scripts/handoff.sh" --boundary blueprint-do` automatically.
- **Attended** (`attended: true`): ask exactly `Plan ready - dispatch fresh worker for the build? [Y/n]` (one line, no preamble). `y` or Enter -> run the helper. `n` -> park (continue in-session, no claim churn, no spawn). If the question cannot be answered (timeout or no interactive surface) -> park conservatively and continue in-session.

**Wave/phase boundaries during do + review (pressure)**

Run `bash "${SKILL_DIR}/scripts/handoff.sh" --boundary wave` at each wave boundary. The helper parks on no-pressure (probe reads < `config.target.handoff.used_pct_trigger`, default 50); always invoke it and obey the decision line. Never invoke mid-wave or mid-task.

**Decision-line handling**

| Exit | Decision line prefix | Action |
|------|---------------------|--------|
| 0 | `delegated <node> ...` | Print `result: do-phase delegated to <child> (<session>)`. Then **stop immediately** - do NOT continue pipeline phases, do NOT run `claude stop`. The parent's close is sanctioned; the stop hook allows exit because the manifest was archived. |
| 10 | `parked <node> reason="..."` | Continue in-session exactly as if no handoff was attempted. If the reason contains `chain-exhausted`, emit `<help reason="handoff-chain-exhausted" evidence="<reason>">` first, then continue. |
| 12 | `handoff-restore-failed <node> ...` | Emit `<help reason="handoff-restore-failed" evidence="<reason>">` and stop work. Never continue silently without a manifest. |
| 12 | `handoff-claim-lost <node> ...` | Emit `<help reason="handoff-claim-lost" evidence="<reason>">` and stop work. The claim may be held by another worker; do NOT continue in-session on this node. |

## Cross-project is retired (migration shim)

The `scope: cross-project` parallel-worktree pipeline has been removed. A session works only in its OWN project; foreign work is spawned into its project via `fno agents spawn --cwd <root>` (spawn-into-project). A multi-repo feature is now a set of single-project backlog nodes linked by `blocked_by`, each shipping its own PR in its own repo.

**Check target-state.md BEFORE any execution.** If `cross_project: true` (a legacy `cross-project` subcommand, or a plan with `scope: cross-project`):

1. **WARN** the user: "scope: cross-project is deprecated and the parallel pipeline was removed. Model multi-repo work as one backlog node per project (linked by blocked_by); each ships its own PR. Use `fno backlog decompose` to split a legacy plan."
2. **Do NOT** invoke any cross-project pipeline (removed) and **do NOT** `cd` into other repos to write code.
3. **Route to spawn-into-project:** continue THIS session in its own project only. Foreign waves are handled by `/do` (auto-spawn when the foreign node is unblocked; defer + carveout when it is blocked); cross-project dependents are dispatched on merge by `fno backlog advance`.

`cross_project: true` no longer forks the pipeline; it only triggers this deprecation warning + the spawn-into-project routing above. The manifest field and the plan-graduation timing in `fno-agents finalize` are retained so an already-stamped legacy plan still parses and graduates correctly.
