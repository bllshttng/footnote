# fno agents — lifecycle (stop / rm / reconcile / attach)

The lifecycle write verbs round out the `fno agents` surface alongside create (`ask`), follow-up (`ask` again), and read (`list`, `logs`): `stop`, `rm`, `reconcile`, and `attach`.

Parent: [fno-agents-registry-and-dispatch.md](fno-agents-registry-and-dispatch.md). Sibling: [fno-agents-codex-provider.md](fno-agents-codex-provider.md). User guide: [../guides/fno-agents-stop-rm-reconcile.md](../guides/fno-agents-stop-rm-reconcile.md).

## Surface

```bash
# Stop a running claude background session (keeps the conversation resumable):
fno agents stop worker-claude

# Remove an agent from the registry (and from claude's supervisor):
fno agents rm worker-claude
fno agents rm worker-claude --force        # override claude's refusal (e.g. uncommitted worktree)

# Sweep the registry, flip statuses against provider reality:
fno agents reconcile
fno agents reconcile --json | jq .         # machine-readable

# Drop into a claude agent's interactive TUI:
fno agents attach worker-claude
```

Provider matrix:

| Verb | claude | codex | gemini |
|------|--------|-------|--------|
| stop | `claude stop <id>` shellout (30s timeout) | no-op + info on stderr (synchronous between asks) | no-op + info on stderr |
| rm | `claude rm <id>` then registry-row removal (atomicity invariant); `--force` overrides | registry-only; on-disk session files stay (operator cleans manually) | registry-only |
| reconcile | `claude logs <id> --tail 1` exit-code (10s timeout) | presence in `~/.codex/session_index.jsonl` | tri-state reachability probe (see [fno-agents-gemini-commands.md](fno-agents-gemini-commands.md)) |
| attach | `claude attach <id>` with inherited stdio | exit 13 (interactive attach not supported) | exit 13 (interactive attach not supported) |

## Architecture

```
cli.py
   ├─ cmd_stop(name)         → dispatch.stop_agent(name)
   ├─ cmd_rm(name, --force)  → dispatch.rm_agent(name, force=False)
   ├─ cmd_reconcile(--json)  → dispatch.reconcile_agents()
   └─ cmd_attach(name)       → dispatch.attach_agent(name)

dispatch.py
   ├─ stop_agent     ─ per-agent flock → claude_stop → events.agent_stopped
   ├─ rm_agent       ─ per-agent flock → claude_rm   → update_registry → events.agent_removed
   ├─ reconcile_agents ─ no flock; per-entry update_registry; one-shot capability checks (claude-on-PATH, codex-session-index)
   └─ attach_agent   ─ no flock; claude_attach with inherited stdio

providers/claude.py
   ├─ claude_stop(short_id, timeout=30)         → (exit_code, stderr)
   ├─ claude_rm(short_id, timeout=30)           → (exit_code, stderr)
   ├─ claude_attach(short_id)                   → exit_code  (no capture, no timeout)
   └─ claude_logs_reachable(short_id, timeout=10) → bool

providers/codex.py
   ├─ default_session_index_path()              → ~/.codex/session_index.jsonl
   ├─ session_index_exists(path=None)           → bool
   └─ load_known_session_ids(path=None)         → set[str]   (UUID regex extraction)
```

## Design rules

1. **rm scope on codex is registry-only.** Codex's per-cwd session files stay on disk after `fno agents rm`. The operator cleans them manually if desired.
2. **reconcile is mark-only.** Detect orphans and stamp `status=orphaned`. Never delete. The operator decides removal via explicit `rm`.
3. **reconcile is bidirectional.** `live → orphaned` AND `orphaned → live`. Handles transient supervisor failures gracefully.
4. **stop for codex / gemini is a trivial no-op + informational message.** Codex / gemini asks are synchronous; the kill switch is SIGINT to the fno ask process. `stop` between asks is a no-op.
5. **claude rm refusal propagates by default.** A non-forceful claude refusal leaves the registry unchanged with claude's stderr verbatim. `--force` overrides: it removes the registry row even on claude failure, with a stderr WARN about the orphan supervisor session.
6. **stop / rm serialize on the per-agent flock.** The same flock as `ask`, so the three verbs cannot race against each other for a given name.
7. **reconcile does NOT take the per-agent flock.** Read-mostly + per-entry atomic update via `update_registry`'s existing flock. Concurrent reconcile + ask is safe; stale orphan flags self-correct on the next sweep.
8. **attach does NOT take the per-agent flock.** Attach is interactive and may hold the terminal indefinitely; a flock would block all other ops on the same agent. claude's own supervisor handles concurrent attach safety natively; codex / gemini exit 13 before any state mutation.
9. **claude shellout timeouts: 30s for stop / rm, 10s for `claude logs --tail 1` during reconcile.** The operator can retry.
10. **Revival of an exited claude row is probe-first, same-uuid.** `claude --resume <uuid>` continues the *same* session uuid, so an exited claude bg row stays revivable. `resume <name>` is the smart human verb: it probes reality (`locate_session` + a 250ms socket connect, never the registry `status` field) and, on a live supervisor, attaches exactly as before; on a dead one, execs `claude --resume <uuid>` in the recorded cwd (registry-read-only — the row stays exited, revivable detached later). `attach <name>` stays strict live-only, but a dead claude row with a recorded uuid now refuses with the two revival commands instead of dead-ending. `spawn <name> --resume <uuid> --substrate bg` revives the row **in place** (fresh short_id, same uuid, status live) when the colliding row is exited and the uuid is its own; every other same-name case (live row, uuid mismatch, no `--resume`) stays fail-closed. Probe-first is load-bearing: `--resume` against a live supervisor would put two writers in one transcript, so liveness is always checked before the resume lane fires.

## Failure modes addressed

Three classes of silent failure are pinned by tests:

1. **Mass-orphan storm when the claude CLI is missing.** Without the guard, every claude agent would flip to `orphaned` on a host where `claude` was uninstalled. `reconcile_agents` runs a one-shot `is_provider_available("claude")` check; when claude is not on PATH, claude entries land in `errors` with `reason=claude-cli-not-on-path` and statuses stay untouched. This mirrors the codex-session-index-missing pattern.

2. **`rm_agent` confirmation could lie when the registry write failed.** The stdout `removed: <name>` print happens **after** `update_registry` succeeds. Otherwise a registry-write `OSError` would raise after the operator had already been told the removal succeeded.

3. **`rm_agent` lock-timeout was silent in events.jsonl.** A symmetric `agent_removed` emit with `lock_timeout=true` lets forensic audits distinguish "rm refused at the flock layer" from "operator never ran rm".

## Test surface

Lifecycle coverage lives in `cli/tests/agents/`: `test_dispatch_lifecycle.py` (stop / rm / reconcile / attach across happy-path, error, and edge cases, with event-stream assertions), `test_cli_lifecycle.py` (Typer CliRunner tests for the four verbs, `--force`, `--json`, exit-code propagation), `test_cli_yolo_e2e.py` (`--yolo` passthrough end-to-end), `test_codex_flock_parallel.py` (parallel codex asks serialize via the per-agent flock), and `test_codex_fatal_error_dispatch.py` (`CodexInvocationError` / `NoSessionIdError` → `DispatchAskError` with the right exit code; events.jsonl + registry-empty atomicity).

## Not covered by these verbs

- **Attach for codex / gemini.** Both providers are non-interactive; the natural surface is "tail logs while messaging" — closer to `tmux attach` than `claude attach`.
- **Auto-delete in reconcile.** reconcile is mark-only; removal stays manual via `fno agents rm`.
- **`fno agents ask --stream`.** Per-event progress to stderr is a separate capability.

## Operator workflow examples

**Daily cleanup loop (LLM orchestrator):**

```bash
fno agents reconcile --json | jq '.orphaned[] | "\(.name) \(.provider)"'
fno agents rm <name>   # for any the operator decides to clean
```

**Recovering an orphaned claude session:**

```bash
fno agents reconcile        # a claude supervisor restart can flip orphaned → live
fno agents attach worker    # drop into the TUI to check state
```

**Force-removing an agent with uncommitted worktree changes (the operator knows the state):**

```bash
fno agents rm worker-claude --force
# stderr: WARN: claude rm failed but --force given; removing registry only.
# Orphan supervisor: claude rm 7c5dcf5d to clean later.
```

## Phase 6 Wave 5 — Rust daemon lifecycle verbs

The sections above describe the original Python (US4) lifecycle for one-shot agents. Phase 6 Wave 5 implements the same verbs in the Rust supervisor daemon (`crates/fno-agents/src/daemon.rs`), where agents are persistent PTY-backed sessions owned by per-agent worker processes (Outcome B). The verbs are JSON-RPC handlers on `supervisor.sock`; the `fno-agents` client maps the daemon's structured `ErrorCode` to process exit codes (`bin/client.rs::exit_code_for`). The driver-awareness is new: a verb's behavior depends on whether an operator is currently driving the agent (an interactive/step/paranoid `drive` session, tracked in the daemon's in-memory `DriveTable`).

### Exit-code conventions (client-side)

| ErrorCode | Exit | Used by |
|-----------|------|---------|
| `AgentNotFound` / `InvalidStatus` / `ChannelUnknown` | 13 | not-found, wrong-status, daemon-down `status` |
| `SpawnFailed` | 14 | pre-launch spawn failure |
| `LockTimeout` | 15 | flock contention |
| `Busy` | 18 | driver-active refusals (stop/rm), capacity caps |
| `InvalidParams` / `MalformedFrame` / `UnknownMethod` | 2 | bad invocation |
| `Internal` | 1 | catch-all |

### stop (US6.7)

`handle_stop` refuses with **exit 18** (`Busy`) while a controlling driver is active, unless `--force`. A watcher never blocks — only the single controlling driver does. With `--force`, the daemon force-closes the driver (the drive session loop emits `drive_detached{reason:"stop_force"}` and clears its `DriveTable` slot + `state.json` window in `cleanup`), waits up to 2s for the slot to clear (emitting `drive_force_close_timeout` if it does not), then stops the agent.

Worker shutdown escalates: graceful `worker.shutdown` RPC → poll up to a 5s grace → `SIGTERM` the worker → another grace → `SIGKILL`. Killing the worker closes the PTY master, which `SIGHUP`s the child (Outcome B), so the whole PTY tree comes down. The registry flip to `Exited` is only reported after the worker is **confirmed** down, and a failed registry write surfaces as `Internal` (never a false "stopped: true"). Claude agents have no worker: `stop` shells out to `claude stop <short_id>`.

### rm (US6.8)

`handle_rm` refuses with **exit 18 UNCONDITIONALLY** while a controlling driver is active — even with `--force`. Unlike `stop`, `--force` does NOT evict the driver here; the operator must detach → stop → wait `exited` → rm. A live agent without a driver still needs `--force` (also exit 18 without it). Force-removing a live agent stops its worker first (refusing if the worker can't be confirmed down, so a live PTY is never orphaned). Orphaned entries are removed with no subprocess action and emit `agent_removed{was_orphaned: true}`.

### reconcile (US6.9)

`handle_reconcile` probes each entry via the `Provider` trait's `reachability` (250ms per call, `provider::for_name` resolving the impl) under a **5s sweep budget**; entries beyond the budget defer to the next tick (`reconcile_deferred{remaining_count}`). Entries are probed least-recently-reconciled first (`last_reconciled_at` ASC, `None` first) so a budget-exhausted sweep stays fair across a large registry. A live worker pid short-circuits the probe (authoritative liveness, no 250ms cost).

Transitions are status-aware and tri-state (the pure, unit-tested `plan_reconcile`):

- `Ok(true)` (reachable): recover an `Orphaned` entry → `Live`; leave others unchanged.
- `Ok(false)` (unreachable): flip a live-ish entry (`Live`/`Ready`/`Idle`/`Busy`/`Spawning`) → `Orphaned`. `Restarting`/`Failed` are excluded (the restart supervisor owns them); terminal `Exited`/`PermanentDead` are left alone.
- `Err` (inconclusive): preserve status, emit `agent_inconsistent{name, reason}`. Never orphan on a probe timeout.

All changes land in one batched `update_registry`; the sweep emits `reconcile_done{updated, orphans, recovered}`.

### status (US6.10)

`handle_status` returns the locked `status-v1.json` shape (Wave 7 codifies the schema + CI parity):

```json
{
  "schema_version": 1,
  "daemon":   {"state": "serving", "pid": 0, "uptime_secs": 0, "version": "..."},
  "agents":   {"total": 0, "by_status": {"live": 0, "...": 0}},
  "drives":   {"active": 0},
  "restarts": {"queue_depth": 0, "consecutive_failures_max_seen": 0},
  "channels": {"registered": 0}
}
```

`drives.active` counts controlling drivers across all agents (read from the in-memory `DriveTable`, so the handler is async). The `fno-agents status` client probes an already-running daemon via `call_if_running` and exits **13** without lazy-starting one when the daemon is down — `status` describes a daemon, it does not boot one.

### list (US6.6)

`handle_list` filters to the current `project_root` by default (`--all` shows every project) and each row now carries `last_message_at` alongside name/provider/status/project_root.
