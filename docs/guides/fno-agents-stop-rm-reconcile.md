# Managing agents — `stop` / `rm` / `reconcile` / `attach`

The four lifecycle write verbs in the `fno agents` subsystem. Use them when you want to:

- stop a running claude background session cleanly (keep the conversation resumable)
- remove an agent from the registry — and from claude's supervisor too
- sweep the fleet to find orphaned or recovered sessions
- drop into a claude agent's interactive TUI

These complement the read verbs (`list`, `logs`) and the create / follow-up verb (`ask`). Together they form the full lifecycle: ask → list → logs → stop → rm.

## `fno agents stop` — pause a claude session

```bash
fno agents stop worker-claude
# stopped: worker-claude (7c5dcf5d)
```

For **claude** agents, this shells out to `claude stop <short_id>` with a 30-second timeout. The supervisor session is paused but the conversation file stays on disk — a subsequent `claude attach <short_id>` (or `fno agents attach worker-claude`) wakes it back up.

For **codex** and **gemini** agents, `stop` is a no-op with an informational stderr message: those providers are synchronous between asks, so there is no persistent process to stop. To interrupt an in-flight `ask`, send SIGINT to the fno process directly.

**Common exit codes:**

| Code | Meaning |
|------|---------|
| 0 | claude stopped cleanly (or codex / gemini no-op confirmed) |
| 1 | claude exited non-zero (e.g., session already stopped — stderr passed through) |
| 2 | agent not found in registry, or invalid name |
| 11 | per-agent flock acquisition timed out (someone else holds the lock on this name) |
| 14 | claude CLI not on PATH |
| 15 | `claude stop` exceeded the 30s timeout |

## `fno agents rm` — remove an agent

```bash
fno agents rm worker-claude
# removed: worker-claude
```

`rm` is the cleanup verb. For **claude** agents the sequence is strict: `claude rm <short_id>` runs first, and the registry row is dropped only after claude reports success. If `claude rm` refuses (e.g., the worktree has uncommitted changes), the registry stays intact so you can address the underlying issue and retry.

To override claude's refusal:

```bash
fno agents rm worker-claude --force
# WARN: claude rm failed but --force given; removing registry only.
# Orphan supervisor: claude rm 7c5dcf5d to clean later.
# removed: worker-claude
```

`--force` drops the registry row regardless of claude's exit code, leaving the supervisor session orphaned. You are responsible for the manual `claude rm 7c5dcf5d` (the stderr WARN spells it out).

The same ordering holds for **codex** and **opencode**, which tear down their own session record before the registry row is dropped:

| Harness | What teardown removes |
|---------|-----------------------|
| claude | `claude rm <short_id>` (session record + worktree, via claude's own delegation contract) |
| codex | the session's entry in `~/.codex/session_index.jsonl` |
| opencode | the session, via `opencode session delete <ses_...>` |
| gemini | nothing; registry-only (no teardown arm for a deprecated provider) |

Teardown is record-only.
Transcripts always stay on disk: codex rollout files under `~/.codex/sessions/<date>/<id>` and opencode session JSON are never touched, so `rm` cleans up discovery without destroying history.
Removing an agent also does not stop a running session; use `fno agents stop` for that.

A harness record that is already gone counts as success, so re-running `rm` after a manual cleanup will not wedge.

Worktrees are not removed here for non-claude harnesses.
Nothing on the registry row distinguishes an isolated worktree from the canonical checkout, so `rm` will not guess; reap them with `fno worktree cleanup --merged --apply`.

**Common exit codes:**

| Code | Meaning |
|------|---------|
| 0 | row removed (teardown succeeded, was already gone, or `--force` override) |
| 1 | teardown failed and `--force` was not passed (registry unchanged) |
| 2 | agent not found, invalid name, or a harness with no rm arm |
| 11 | flock timeout |
| 12 | registry write failed, or the stored session id is malformed (stderr names the underlying error) |
| 14 | the harness CLI is not on PATH |
| 15 | teardown exceeded the 30s timeout |

## `fno agents reconcile` — sync registry with provider reality

```bash
fno agents reconcile
worker-claude (claude/7c5dcf5d): live → orphaned
worker-codex (codex/019eabcd...): orphaned → live
worker-gemini (gemini/...): live (no change)
3 entries scanned: 1 orphaned, 1 recovered
```

`reconcile` walks the registry and probes each entry against its provider's reality:

- **claude**: `claude logs <short_id> --tail 1` exit code decides reachability (10-second timeout).
- **codex**: presence of the session_id in `~/.codex/session_index.jsonl` decides reachability.
- **gemini**: a tri-state reachability probe against the session's chat file decides reachability.

Status flips are bidirectional: an agent can go `live → orphaned` if the supervisor lost the session, and `orphaned → live` if the supervisor restarted and re-created it. Reconcile never deletes — that's `rm`'s job.

For automation, pass `--json`:

```bash
fno agents reconcile --json | jq .
```

```json
{
  "scanned": 3,
  "orphaned": [{"name": "worker-claude", "provider": "claude", "id": "7c5dcf5d"}],
  "recovered": [{"name": "worker-codex", "provider": "codex", "id": "019eabcd-..."}],
  "skipped": [],
  "errors": []
}
```

**When does reconcile produce an `errors` entry instead of flipping a status?**

| Scenario | `errors` entry reason | Status change |
|----------|----------------------|---------------|
| `~/.codex/session_index.jsonl` does not exist (fresh codex install) | `codex-session-index-missing` | None — codex statuses preserved |
| `claude` CLI is not on PATH | `claude-cli-not-on-path` | None — claude statuses preserved |
| Registry row has neither `claude_short_id` nor `codex_session_id` | `missing-claude-short-id` / `missing-codex-session-id` | None — row left for manual triage |
| Row's provider is not in (`claude`, `codex`, `gemini`) | `unknown-provider-<name>` | None |

Reconcile refuses to mass-flip statuses on insufficient evidence: missing tooling is reported as an error, never silently inferred as "all agents are orphaned".

## `fno agents attach` — drop into a claude TUI

```bash
fno agents attach worker-claude
# (claude TUI takes over the terminal until you detach)
```

For **claude** agents, `attach` shells out to `claude attach <short_id>` inheriting stdin / stdout / stderr — the claude TUI takes over the terminal. fno's exit code mirrors claude's on detach.

For **codex** and **gemini** agents, `attach` refuses with exit 13:

```bash
fno agents attach worker-codex
# codex agents are one-shot; no persistent session to attach to.
# Use 'fno agents logs worker-codex --follow' for live output.
# Cross-provider attach is planned for the fno-owned supervisor.
```

The fno-owned supervisor will land cross-provider attach in a future story. Until then, tail the logs:

```bash
fno agents logs worker-codex --follow
```

**Common exit codes:**

| Code | Meaning |
|------|---------|
| _claude's exit_ | (claude attach inherits stdio; fno mirrors the TUI's exit on detach) |
| 1 | OSError invoking claude (e.g., PermissionError) |
| 2 | agent not found |
| 13 | codex / gemini attach refused |
| 14 | claude not on PATH |

## When to use what

| Scenario | Verb |
|----------|------|
| "I want to pause this agent but keep the conversation" | `stop` |
| "I want to delete this agent entirely" | `rm` |
| "I want to know which agents are still alive" | `reconcile --json` |
| "I want to interactively talk to this agent right now" | `attach` |
| "I want to read what this agent already produced" | `logs` |
| "I want to send a new message to this agent" | `ask` |

## Forensics: events.jsonl

Every lifecycle verb writes a structured event to `~/.fno/events.jsonl`:

- `agent_stopped` — `{name, provider, claude_exit?, short_id?, timed_out?, lock_timeout?}`
- `agent_removed` — `{name, provider, claude_exit?, force, registry_changed, short_id?, lock_timeout?, error?}`
- `reconcile_done` — `{scanned, orphaned, recovered, skipped, errors}`
- `agent_attached` — `{name, provider, short_id?, claude_exit?, error?}`
- `agent_attach_refused` — `{name, provider, reason}` (codex / gemini path)

Use these to drive a daemon that watches for stuck or idle agents:

```bash
tail -f ~/.fno/events.jsonl | jq 'select(.kind == "agent_stopped" or .kind == "reconcile_done")'
```

## Concurrency notes

- `stop`, `rm`, and `ask` all serialize on the same per-agent flock for a given name. Two concurrent operations on the same agent will execute sequentially (one will block with a `(waiting for ask lock...)` stderr line if it waits >1s).
- `reconcile` does NOT take the per-agent flock. It is read-mostly and per-entry atomic via `update_registry`'s internal flock. Stale orphan flags self-correct on the next sweep.
- `attach` does NOT take the per-agent flock. It is interactive and may hold the terminal indefinitely; locking it would deadlock every other operation on the agent. claude's own supervisor handles concurrent attach safety natively.

## See also

- [fno-agents-ask-followup.md](fno-agents-ask-followup.md) — the create / follow-up verb
- [fno-agents-list-logs.md](fno-agents-list-logs.md) — the read verbs
- [`docs/architecture/fno-agents-lifecycle.md`](../architecture/fno-agents-lifecycle.md) — full architecture
