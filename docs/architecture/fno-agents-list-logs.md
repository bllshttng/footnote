# fno agents — list + logs read surface

Two read verbs sit on top of the registry substrate: `fno agents list` (registry roster + live status) and `fno agents logs <name>` (per-agent transcript tail). Both are pure-read — they never mutate the registry. This doc covers the design; the how-to lives in the [list/logs user guide](../guides/fno-agents-list-logs.md).

Parent: [fno-agents-registry-and-dispatch.md](fno-agents-registry-and-dispatch.md). Sibling: [fno-agents-followup.md](fno-agents-followup.md).

## Audience: LLM orchestrator first

The primary caller is an LLM orchestrator. `list --json` returns the fleet shape, `logs --json` returns recent assistant output as JSON-Lines (codex/gemini only — Claude is raw passthrough for now). Human callers get a table for `list` and raw passthrough for `logs` when stdout is a TTY. JSON is the load-bearing contract; the human format is a convenience adapter on top.

## Module layout

```
cli/src/fno/agents/
├── cli.py                  ← Typer wiring; adds the `logs` verb
├── format.py               ← pure JSON + table renderers
├── read.py                 ← list_agents + read_logs entry points
└── providers/
    └── claude.py           ← claude_agents_json + logs shellouts
```

`read.py` and `format.py` are pure-read modules: they never mutate the registry, never flip `status` based on inferred live state, never emit events. Status mutations belong to the dedicated write verbs (`stop`, `rm`, `reconcile`).

## Three axes, two views

The registry's view (`status`, mutated by the follow-up and reconcile paths) and Claude's supervisor view (`live_status`, ephemeral) are deliberately kept on separate axes:

| Axis | Source | Values | Owner |
|---|---|---|---|
| `status` | `~/.fno/agents/registry.json` | `live`, `orphaned` | fno |
| `live_status` | shell-out to `claude agents --json` | `Working`, `Needs input`, `Idle`, `Done`, `null` | claude supervisor |

Those four values are the OUTPUT vocabulary, not the wire vocabulary.
Which key and which spelling a claude build emits has changed twice, so the provider reads both `id`/`short_id` and `state`/`status` and maps every observed spelling (`working`, `busy`, `blocked`, `idle`, `done`) onto the table above.
`state` outranks `status` when a row carries both: current builds emit them in different vocabularies with disagreeing values, and `state` is the field the binary's own header count renders.
A spelling not in the input map passes through unchanged with a drift warning, which is what keeps the NEXT rename loud instead of silent.

Both fields appear on every JSON entry. `live_status` is `null` for non-Claude entries and for Claude entries when the shellout fails or omits the entry. Conflating them would lose information — an `orphaned` registry entry whose `live_status` is `null` because claude reports it doesn't exist is a different story than a `live` entry whose `live_status` is `Idle` because the supervisor is between jobs.

## Live-status augmentation flow

```
list_agents(filters, json_out, tty)
   │
   ├─ load_registry()                      ← read-only, no flock
   ├─ apply filters (cwd / provider / status)
   ├─ if any claude entry survives filtering:
   │    └─ providers.claude.claude_agents_json()
   │         ├─ subprocess.run(timeout=3.0, capture)
   │         ├─ on every failure mode → ({}, [warning])
   │         └─ on success → {short_id: {live_status, ...}}
   ├─ serialize_entry(entry, live_status, observed_model)  ← canonical row dict
   └─ render_json | render_table           ← format selection
```

Failure modes that the augmentation step catches internally and surfaces as warnings (never as a non-zero exit):

- `FileNotFoundError` — `claude` binary missing from PATH
- `subprocess.TimeoutExpired` — exceeded the 3-second per-call budget
- non-zero claude exit
- `json.JSONDecodeError`
- structural drift in the response shape (no short id under any accepted key)
- a TOTAL drop of the AGENT rows (zero parsed, counting only rows whose `kind` is not `interactive`) adds one summary warning naming the count, because an empty map otherwise reads exactly like "no agents running". The operator's own interactive sessions appear in the same array and carry no id by design, so they are excluded: counting them would cry drift at a machine that simply has no agents running.
- live-status sentinel drift (a resolved value outside `{Working, Needs input, Idle, Done}` triggers a forensic warning but passes through unchanged)

`read.py` does NOT add a broad `except Exception` around the call — programmer errors should crash visibly. The contract is: `claude_agents_json` returns `({}, warnings)` on every documented failure; anything else escaping is a bug.

## Logs branch by provider

```
read_logs(name, tail, follow, json_out, stdout, stderr)
   │
   ├─ load_registry(); find by name (exit 13 if not found)
   ├─ provider == "claude":
   │    ├─ short_id missing on entry → exit 1 (data drift, not name-miss)
   │    └─ providers.claude.logs(short_id, tail, follow, stdout, stderr)
   │         ├─ follow=False → subprocess.run capture, tail slice in-process
   │         └─ follow=True  → subprocess.Popen line-buffered passthrough,
   │                            SIGINT forwarded to child on KeyboardInterrupt
   └─ provider in {"codex", "gemini"}:
        ├─ log_path does not exist → "provider not yet shipped", exit 13
        ├─ read jsonl_tail (slice last N lines if --tail set)
        └─ if --follow → _follow_jsonl 500ms poll loop with
                          rotation/truncation detection
```

### Streaming vs capture

`subprocess.run(capture_output=True)` blocks until the child exits, so it's wrong for `--follow`. The implementation switches to `subprocess.Popen` with `bufsize=1` and iterates `proc.stdout.readline` so the operator sees lines as claude emits them. `KeyboardInterrupt` (Ctrl-C) is intercepted, propagated to the child via `proc.send_signal(SIGINT)`, and the function returns 0 with no traceback on stderr.

### `_follow_jsonl` rotation handling

The codex/gemini follow loop stats the file each iteration and exits the loop with a structured stderr note when either:

- the file disappears (`stat` returns ENOENT) — atomic-rename rotation followed by old unlink
- the inode changes — atomic-rename rotation where the new file is being written
- size shrinks below the read offset — truncate-in-place rotation

Without these checks, the loop would hang silently on every common log-rotation strategy. The operator gets a real diagnostic; an external supervisor can re-invoke `logs --follow` to attach to the new file.

## JSON output contract

```json
{
  "agents": [
    {
      "name": "worker-frontend",
      "harness": "claude",
      "short_id": "abc123",
      "cwd": "/Users/foo/code/proj",
      "created_at": "2026-05-20T17:00:00Z",
      "last_message_at": "2026-05-20T17:30:12Z",
      "status": "live",
      "live_status": "Working",
      "observed_model": { "kind": "observed", "model": "glm-5.2", "samples": 300 },
      "log_path": "/Users/foo/.fno/agents/worker-frontend/output.jsonl"
    }
  ],
  "count": 1,
  "filters_applied": { "cwd": null, "provider": null, "status": null },
  "schema_version": 1
}
```

The schema version is owned by `format.py::JSON_SCHEMA_VERSION` and is intentionally distinct from the registry's `SCHEMA_VERSION`. The CLI output and the storage substrate evolve on independent cadences.

All entries have the same key set regardless of harness. Consumers that grep for keys can rely on `short_id == null` for non-Claude entries rather than checking for key presence.

`harness` names the CLI; `observed_model` names the model the worker is actually answering as, derived per row from its own transcript by `session_truth.observed_model`. The two list emitters reach that one resolver rather than each reading transcripts: Python calls it directly at `read.py`, and the Rust daemon gets it back from the `fno agents truth --json` probe it already runs once per row for `status`. There is deliberately no `provider` key: it was an alias carrying the harness value, and it read as evidence that a routed worker had fallen back to Anthropic.

## Format selection

| `--json` | TTY | Output |
|---|---|---|
| true  | true  | JSON |
| true  | false | JSON |
| false | true  | human table |
| false | false | JSON |

The JSON-when-non-TTY default aligns with the orchestrator-first audience: a piped or subprocess-captured invocation gets parseable output without an explicit flag.

## Drift defenses

The CLI's `AgentStatusFilter` Typer enum exposes the family-1 read verdicts `live`, `orphaned`, and `unknown`.
It intentionally does not mirror `registry.KNOWN_STATUSES`: stored lifecycle metadata and rendered transcript liveness answer different questions.

The `KNOWN_LIVE_STATUSES` allowlist in `providers/claude.py` catches the orthogonal drift case — claude shipping a new sentinel like `"Reflecting"`. A rename it has already made, such as `"Working"` to `"working"`, is not drift: `_LIVE_STATUS_INPUT` maps every observed spelling onto the output vocabulary, and only an UNMAPPED value warns. The value still passes through (we don't fail closed on an unknown enum from an external CLI), but a forensic warning lands on stderr so operators see the change rather than getting silently-stale table values.

## Known gaps

- `logs --json` for Claude entries would require parsing claude's log format; the verb currently emits a `WARN:`-prefixed gap message and continues with raw passthrough.
- codex / gemini `logs` paths require tee'd JSONL files written by those providers' create paths; where absent, the verb emits a precise "provider not yet shipped" stderr and exits 13.

## Test surface

`list` + `logs` coverage lives in `cli/tests/agents/`: `test_format.py` (serialize_entry / render_json / render_table), `test_read.py` (list_agents filters, fallback paths, pure-read invariant), `test_providers_claude_read.py` (claude_agents_json failure modes and logs() streaming + SIGINT), `test_cli_list_logs.py` (CLI plumbing, exit codes, the `--json` Claude branch), and `test_follow_signal.py` (a real subprocess `python -m fno.cli` invocation with SIGINT delivery). The acceptance-criterion-to-test mapping lives in the design doc.
