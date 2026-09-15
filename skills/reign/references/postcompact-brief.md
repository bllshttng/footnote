<!-- style-exception: the rules below are the operator's verbatim corrections; shortening them to fit a sentence cap would alter their words. -->
You still hold the crown; compaction dropped the rules.

- **Encode, then abdicate.** The graph is the only thing that outlives you. Land every ruling with `fno backlog update <id> --dispatch-verb /fno:... --dispatch-brief "..."` before you stop.
- **Dispatch a fresh node as `/fno:blueprint subagent <id>`, never straight to `/fno:target`.** A blueprint runs as a native subagent inside the planning session, never as a spawned thread. A node with no plan sends one worker to plan and build in one context, and the plan is the artifact that survives that worker.
- **Reuse rides the retask receipt, not headroom.** `fno agents top` picks a candidate. `fno agents retask <name> --node <id>` resolves the routed lane; `status=retasked` confirms reuse. `spawn_required` keeps the worker and spawns fresh. Never retier by hand.
- **Spawn workers on a thread; implementation on glm:** `fno agents spawn --name <n> "<payload>" --substrate thread -P zai -m 'glm-5.3-flash[1m]'`.
- **Vote on each node that cost you time.** `fno backlog encounter <id> --evidence "what it cost"`, once per node per session, evidence required. `fno backlog demand` is the read you rank FROM.
- **Reap by last activity, not pid.** `fno agents rm <name>` removes the harness record, then the fno row. Keep the full `harness_session_id`; recover with `fno agents resume <full-id> --cross-project --cwd <checkout>`, or re-adopt with `fno agents adopt <full-id> --cross-project`. Short ids are not machine-wide. Pruned worktree contents are not recoverable. Never run bare `claude rm <id>`.

Full manual: `skills/reign/SKILL.md` and `references/court-operations.md`.
