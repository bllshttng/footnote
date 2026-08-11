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
NAME              PROVIDER  STATUS    CHECKED  PID     LAST MESSAGE          CWD
worker-frontend   claude    live      4m       75742   2026-05-20T17:30:12Z  /Users/foo/code/proj
worker-migration  codex     live      18s      75810   2026-05-20T17:15:43Z  /Users/foo/code/proj
worker-design     claude    orphaned  2h       -       2026-05-20T17:00:00Z  /Users/foo/code/proj
```

Columns:

| Column | Meaning |
|---|---|
| NAME | Agent name (the identifier you pass to `ask` / `logs`). |
| PROVIDER | The harness: `claude`, `codex`, `gemini`, `opencode`, or `agy`. |
| STATUS | The transcript verdict: `live`, `orphaned` (the transcript reads done or stalled), or `unknown` (the probe could not answer). Stored registry status is lifecycle metadata and does not decide this column. |
| CHECKED | Relative age since the last reconcile probe (`never` when never probed, `?` when the stored timestamp will not parse). |
| PID | Worker pid for a PTY agent; `-` for a one-shot ask, which has no managed process. |
| LAST MESSAGE | Timestamp of the most recent successful `ask` follow-up, printed raw (RFC3339). |
| CWD | Working directory the agent was created in, printed in full. |

### Filters

```bash
fno agents list --harness claude               # claude agents only
fno agents list --status orphaned              # only stale entries
fno agents list --cwd ~/code/proj              # only agents created in this repo
fno agents list --harness claude --status live --cwd ~/code/proj
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
      "harness": "claude",
      "harness_session_id": "e6f78b98-e594-47ed-ad81-84f8a78b8bb7",
      "short_id": "e6f78b98",
      "session_id": "e6f78b98",
      "address": "e6f78b98",
      "cwd": "/Users/foo/code/proj",
      "created_at": "2026-05-20T17:00:00Z",
      "last_message_at": "2026-05-20T17:30:12Z",
      "status": "live",
      "live_status": null,
      "observed_model": { "kind": "observed", "model": "glm-5.2", "samples": 300 },
      "pid": 75742,
      "last_reconciled_at": "2026-05-20T17:30:00Z",
      "log_path": "/Users/foo/.fno/agents/worker-frontend/output.jsonl",
      "mux": null,
      "crown": null,
      "crown_level": null,
      "crown_scope": null,
      "crown_grantor": null,
      "project_root": "/Users/foo/code/proj"
    }
  ],
  "count": 1,
  "discovered_sessions": [],
  "discovered_count": 0,
  "filters_applied": { "cwd": null, "provider": null, "status": null },
  "schema_version": 2
}
```

The row's key set is pinned by [`schemas/agents-list-row.json`](../../schemas/agents-list-row.json), which both serializers are tested against; edit that file first when adding a key. Every entry carries the same keys regardless of harness, so a consumer never branches on harness to find a field. JSON is the default whenever stdout is a pipe, so `fno agents list | jq .` Just Works without an explicit `--json`.

`harness` is the sole identity axis, and it names the CLI the worker runs under, never the model vendor.
There used to be a `provider` alias beside it carrying the same harness value, so a worker routed to z.ai still listed `provider: claude`; an operator read that as proof the route had silently fallen back and nearly killed three healthy workers over it.
That key is gone. `harness_session_id` is the worker's own session id in its harness's store.

`observed_model` answers the question `provider` looked like it answered: which model the worker is ACTUALLY running.
It is derived from the worker's own transcript at read time, never recorded at spawn, because a spawn records intent and would report the intended model in exactly the case you suspect a fallback.
A worker that quietly fell back to Anthropic reports a `claude-*` id here and disagrees visibly with what you asked for.
The value is a discriminated object, and the five kinds never collapse into one "unknown":

| `kind` | Meaning |
|--------|---------|
| `observed` | The worker has answered. Carries `model` and `samples` (how many records in the read backed it). |
| `no-transcript` | The harness keeps a per-session transcript and none has resolved yet. A worker spawned two seconds ago is legitimately here. |
| `not-file-backed` | The harness keeps no per-session file at all (opencode uses a shared store), so there is nothing to read and never will be. Distinct from `no-transcript`: a permanent absence must not read as a pending one. |
| `no-model-yet` | The transcript exists and carries no answered turn: the shape of a session that came up and never processed one. Not healthy. |
| `unreadable` | The file exists and could not be parsed. Carries `reason`; the row still renders. |

```bash
fno agents list --json | jq -r '.agents[] | "\(.name)\t\(.harness)\t\(.observed_model.model // .observed_model.kind)"'
```

`address` is the one field in the row you can send mail to: the first eight of the session id, the same string `fno mail drain-self` computes for itself.
Every other identifier names something else.
`name` is a spawn label, `short_id` is a transport key that is `null` for most rows, `session_id` is a resume target, and the discovered lane's `LABEL` is a friendly alias.
A reader with no address column copies `name`, and a name-lane durable write queues under a key no drain reads; that is the largest still-growing category of stranded mail on the bus.
`address` is `null` when the row recorded no identity at all, which is reported as absence rather than as a plausible-looking handle.
It is deliberately not promoted to the full session id when two rows share a first eight: ambiguity detection lives in the send resolver, which fails closed and names the candidates, and a second implementation here would be a second answer to one question.

```bash
fno agents list --json | jq -r '.agents[] | select(.address) | "\(.address)\t\(.name)"'
```

`session_id` is the unified, harness-resolving resume target: `short_id` for claude, `harness_session_id` for codex, gemini, and opencode. `short_id` is the transport key and stays claude-only for back-compat (the claude jobId, by construction the leading 8 hex of the session uuid), so for a codex agent you get `short_id: null` but `session_id: "<uuid>"`, and that UUID is exactly what `fno agents resume` (and `codex resume <uuid>`) consume. It is `null` when the id was never captured, and for a claude pane row, which has no transport key for it to resolve from.

`mux` is the hosting ref (`{session, pane_id}`) for a pane-hosted row, `null` otherwise. Such a row holds that ref INSTEAD of a transport key (one live ref per row: mux, worker, or bg), so its `short_id` is `null`. Do not use `session_id == null` to detect a pane row: a codex or opencode pane still resolves one from `harness_session_id`. Test `mux != null`, and address the worker through it:

```bash
fno agents list --json | jq -r '.agents[] | select(.mux != null) | "\(.mux.session):\(.mux.pane_id)"'
```

### Common recipes

```bash
# How many claude agents are live right now?
# `status` carries the transcript verdict; `live_status` is always null on this
# path (see below), so selecting on it would silently always return 0.
fno agents list --harness claude --json | jq '[.agents[] | select(.status == "live")] | length'

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

- `live_status` is always `null` on the served path. The daemon does not duplicate the harness supervisor view, so read `status` for liveness. The key is retained so the row shape stays stable for existing consumers.
- If the registry file is malformed or schema-mismatched, the served path currently returns an empty roster and exit 0 rather than an error, so an empty result is not proof of an empty fleet. Distinguish the two by reading the registry file directly. This is a known gap, filed for a fix that has to decide what a version-skewed daemon should do.
- If the registry is genuinely empty, you get `{"agents": [], "count": 0, ...}` and exit 0. No special-casing needed in your script.

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

If the registry is unreadable but `FNO_AGENT_SELF` is set, the name still comes back (with a `WARN:` line) — the env answer never depends on the registry. This verb reports your *mesh* identity; the top-level `fno whoami` reports operating context (fleet, walker, session, harness) and, when you are a mesh worker, now echoes your name on one extra `agent:` line as a pointer here.

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

**Looking for the LIVE column** — it was removed. `CHECKED` (probe age) and `PID` replaced it, and `STATUS` now carries the transcript verdict that LIVE used to approximate.

**A row shows `harness: null` or `mux: null` for a worker you know is pane-hosted** — you are on a build that predates the row projection carrying those keys. Confirm against the registry file itself before concluding a worker is unbound; that mistake has produced a wrong diagnosis before.

**`fno agents list` is slow** — a reconcile probe runs per row. If you're polling in a tight loop, that probe is the bottleneck, not the registry read.

**`--follow` printed everything at once then exited** — you're on an older fno build. The streaming path is in current builds; the buffered behavior was a bug in an early cut.

## Related

- [fno-agents-ask-followup.md](fno-agents-ask-followup.md) — the follow-up flow for sending messages.
- [../architecture/fno-agents-list-logs.md](../architecture/fno-agents-list-logs.md) — internal architecture for these verbs.
- [../architecture/fno-agents-registry-and-dispatch.md](../architecture/fno-agents-registry-and-dispatch.md) — the registry storage substrate both verbs read.
