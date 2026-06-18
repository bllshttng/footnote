# Listing and tailing agents — `fno agents list` / `fno agents logs`

Two read verbs in the `fno agents` subsystem. Use them when you want to:

- see your whole fleet at once (what's running, what's orphaned, what claude is doing right now)
- attach to an agent's output without dropping into a TUI
- script a polling loop that watches for stuck or idle agents

These verbs are pure reads — they never modify the registry. Status mutations belong to `fno agents ask` and the `stop` / `rm` / `reconcile` verbs.

## `fno agents list` — the fleet roster

```bash
fno agents list
```

A typical human-table view:

```
NAME              PROVIDER  STATUS    LIVE         LAST MESSAGE     CWD
worker-frontend   claude    live      Working      17:30:12 (2m)    ~/code/proj
worker-migration  codex     live      -            17:15:43 (16m)   ~/code/proj
worker-design     claude    orphaned  -            17:00:00 (32m)   ~/code/proj
```

Columns:

| Column | Meaning |
|---|---|
| NAME | Agent name (the identifier you pass to `ask` / `logs`). |
| PROVIDER | `claude`, `codex`, or `gemini`. |
| STATUS | fno's view: `live` (registry healthy) or `orphaned` (marked on a failed follow-up). |
| LIVE | claude's supervisor view: `Working`, `Needs input`, `Idle`, or `-` (non-Claude or shellout failed). |
| LAST MESSAGE | Wall-clock time of the most recent successful `ask` follow-up, plus a relative-time suffix. |
| CWD | Working directory the agent was created in (`~` collapses your $HOME). |

### Filters

```bash
fno agents list --provider claude              # claude agents only
fno agents list --status orphaned              # only stale entries
fno agents list --cwd ~/code/proj              # only agents created in this repo
fno agents list --provider claude --status live --cwd ~/code/proj
```

`--cwd` resolves relative paths to absolute before comparing, so `./.` works.

### JSON output

```bash
fno agents list --json
```

Returns a canonical object suitable for scripts:

```json
{
  "agents": [
    {
      "name": "worker-frontend",
      "provider": "claude",
      "short_id": "abc123",
      "session_id": "abc123",
      "cwd": "/Users/foo/code/proj",
      "created_at": "2026-05-20T17:00:00Z",
      "last_message_at": "2026-05-20T17:30:12Z",
      "status": "live",
      "live_status": "Working",
      "log_path": "/Users/foo/.fno/agents/worker-frontend/output.jsonl"
    }
  ],
  "count": 1,
  "filters_applied": { "cwd": null, "provider": null, "status": null },
  "schema_version": 1
}
```

Every entry has the same key set regardless of provider. Codex / gemini entries get `short_id: null` and `live_status: null` because the live-status axis is Claude-specific in this release. JSON is the default whenever stdout is a pipe — `fno agents list | jq .` Just Works without an explicit `--json`.

`session_id` is the unified, provider-resolving resume target: `claude_short_id` for claude, `codex_session_id` for codex, `gemini_session_id` for gemini. `short_id` stays claude-only for back-compat, so for a codex agent you get `short_id: null` but `session_id: "<uuid>"` — that UUID is exactly what `fno agents resume` (and `codex resume <uuid>`) consume. It is `null` only when the id was never captured.

### Common recipes

```bash
# How many claude agents are working right now?
fno agents list --provider claude --json | jq '[.agents[] | select(.live_status == "Working")] | length'

# Names of every orphaned entry (script can stop or rm them):
fno agents list --status orphaned --json | jq -r '.agents[].name'

# Watch your fleet from a separate terminal:
watch -n 2 fno agents list

# Detect a stuck agent (no message in >10 minutes):
fno agents list --json | jq -r '
  .agents[]
  | select(.last_message_at != null)
  | select((now - (.last_message_at | fromdateiso8601)) > 600)
  | .name
'
```

### Failure surfacing

`list` is deliberately best-effort:

- If `claude agents --json` is missing or times out (3-second budget), the call still succeeds with `live_status: null` on every claude entry and a `WARN:` line on stderr. Your orchestrator sees the fleet shape; it just doesn't get the live-status augmentation that run.
- If the registry file is malformed or schema-mismatched, the call exits 1 with the file path + parser error on stderr and an empty stdout. This is a real error — fix the file before continuing.
- If the registry is empty, you get `{"agents": [], "count": 0, ...}` and exit 0. No special-casing needed in your script.

## `fno agents whoami` — this worker's own name

```bash
fno agents whoami
```

Answers the one question `list` cannot: *what is MY registered name* — the handle peers use to address you via `fno mail send <name>`. A worker that lost track of its name after a compaction has a native answer instead of grepping `list` for its own session.

It resolves identity from the `FNO_AGENT_SELF` environment variable the spawn path injects into every worker, falling back to a registry row whose recorded session id matches `CLAUDE_CODE_SESSION_ID` when the env is absent. The resolved name is then enriched, best-effort, from the registry row (provider, session id, short id, status, claude's live status) and from the local target session (the held backlog node, when one is bound).

```
name:        worker-frontend
provider:    claude
session:     abc12345
short_id:    abc12345
status:      live
node:        node:abc12345
```

Like `list`, it is a pure read (it never mutates the registry, writes state, or emits an event) and emits JSON when stdout is not a TTY or `--json` is passed. Exit codes:

| Exit | Meaning |
|---|---|
| 0 | A name was resolved (from the env, or the session fallback). |
| 3 | Not a registered mesh agent — a human shell or top-level session with no injected identity. The JSON shape carries `registered: false`. |

If the registry is unreadable but `FNO_AGENT_SELF` is set, the name still comes back (with a `WARN:` line) — the env answer never depends on the registry. This verb reports your *mesh* identity; the top-level `fno whoami` reports operating context (fleet, walker, session, provider) and, when you are a mesh worker, now echoes your name on one extra `agent:` line as a pointer here.

## `fno agents logs <name>` — tail an agent's output

```bash
fno agents logs worker-frontend
```

Default behavior is to show the last 100 lines of the agent's log output. For claude agents, this delegates to `claude logs <short_id>` and passes the raw output through verbatim (so any formatting claude applies is preserved).

### Flags

```bash
fno agents logs worker-frontend --tail 500       # last 500 lines
fno agents logs worker-frontend --tail 0         # nothing (zero requested)
fno agents logs worker-frontend --follow         # stream as new lines arrive
fno agents logs worker-frontend -f -n 50         # short-flag form
```

`--tail` and `--follow` interact differently per provider:

- **Codex/gemini agents:** `--follow` re-emits the last N lines (from `--tail`, default 100) before entering the 500ms poll loop. Pass `--tail 0 --follow` to start from the live tip.
- **Claude agents:** `--tail` is ignored when `--follow` is set — we cannot retroactively buffer lines that the upstream `claude logs --follow` is already streaming live. If you need backfill on a claude agent, run `fno agents logs <name> --tail N` without `--follow`, then re-run with `--follow` once you've caught up.

### `--follow` and Ctrl-C

`--follow` streams output line-by-line in real time. Press Ctrl-C to exit cleanly — the polling loop traps SIGINT, forwards it to the underlying claude subprocess if necessary, and exits 0 with no traceback. Your scrollback stays clean.

For codex/gemini follow mode, the poll loop stats the log file every 500ms. If the file is rotated (atomic-rename) or truncated, the loop exits cleanly with a structured stderr note rather than hanging silently.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | logs delivered (or `--tail 0` requested no output) |
| 1 | registry could not be loaded, OR the entry exists but has no `claude_short_id` (data drift; suggests re-dispatching with `fno agents ask`) |
| 2 | usage error (e.g. `--tail -5`) |
| 13 | agent not found by name, OR codex/gemini entry whose log file does not exist yet |
| _other_ | claude's exit code propagates verbatim when its own invocation fails |

A negative `--tail` value is rejected at parse time:

```
$ fno agents logs worker-X --tail -3
--tail must be >= 0 (got -3)
```

### Today's limitations

- `fno agents logs --json` for **Claude entries** is a future concern (we'd have to parse claude's log format). The verb emits a `WARN:` that JSON output for Claude logs is not implemented and continues with raw passthrough.
- `fno agents logs` for **codex / gemini entries** returns exit 13 when the tee'd JSONL log file does not exist yet.

## Troubleshooting

**`agent not found: <name>`** — the name isn't in your registry. Run `fno agents list` to see what's actually there. Names are case-sensitive.

**LIVE column always shows `-` for claude entries** — the `claude agents --json` shellout failed. Check the stderr `WARN:` for the specific reason (likely: `claude` binary not on PATH, or timeout). The list itself is still correct — only the live-status augmentation is missing.

**`fno agents list` is slow** — each invocation shells out to `claude agents --json` (capped at 3 seconds). If you're polling in a tight loop, the bottleneck is claude's response time, not fno.

**`--follow` printed everything at once then exited** — you're on an older fno build. The streaming path is in current builds; the buffered behavior was a bug in an early cut.

## Related

- [fno-agents-ask-followup.md](fno-agents-ask-followup.md) — the follow-up flow for sending messages.
- [../architecture/fno-agents-list-logs.md](../architecture/fno-agents-list-logs.md) — internal architecture for these verbs.
- [../architecture/fno-agents-registry-and-dispatch.md](../architecture/fno-agents-registry-and-dispatch.md) — the registry storage substrate both verbs read.
