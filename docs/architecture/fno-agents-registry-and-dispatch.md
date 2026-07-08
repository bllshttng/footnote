# fno agents — registry and dispatch substrate

Storage and dispatch primitives for the `fno agents` subsystem. The registry remembers which named agent belongs to which provider; the dispatch layer routes `fno agents ask` to the right provider adapter under a per-agent lock. Everything else in the subsystem (follow-up, list/logs, lifecycle verbs, the codex and gemini providers, the MCP channel) builds on this layer.

## Why this exists

`fno agents` lets one CLI invocation address a named, persistent agent session across Claude, Codex, or Gemini, with the registry remembering which provider owns which name. The subsystem is cross-CLI: it dispatches *to* any of the three providers, and fno itself can run under any of them.

## Surface

```
fno agents ask <name> <message> [--provider P] [--cwd PATH] [--timeout SECS]
fno agents list
fno agents ping
```

## Modules

| Module | Role |
|--------|------|
| `cli/src/fno/agents/registry.py` | JSON-backed `AgentEntry` storage with atomic-rename + per-agent flock-path |
| `cli/src/fno/agents/dispatch.py` | `KNOWN_PROVIDERS`, `is_provider_available`, `select_provider`, `dispatch_ask` orchestrator |
| `cli/src/fno/agents/events.py` | Best-effort JSONL telemetry to `~/.fno/events.jsonl` |
| `cli/src/fno/agents/providers/__init__.py` | Single source of truth for `KNOWN_PROVIDERS` |
| `cli/src/fno/agents/providers/base.py` | `ProviderResult` dataclass |
| `cli/src/fno/agents/cli.py` | Typer subapp wiring |
| `cli/src/fno/agents/lock.py` | `hold_agent_lock(name, registry_path, timeout, on_wait)` context manager wrapping the per-agent flock |
| `cli/src/fno/agents/providers/claude.py` | `--bg` create + messaging-socket follow-up |
| `cli/src/fno/agents/providers/codex.py` | `exec` create + `exec resume` follow-up |
| `cli/src/fno/agents/providers/gemini.py` | `-p --session-id` create + `-p --resume` follow-up |

## Storage shape

The registry on disk is one JSON file at `state_dir() / "agents" / "registry.json"`:

```json
{
  "schema_version": 1,
  "agents": [
    {
      "name": "my-agent",
      "provider": "claude",
      "cwd": "/home/user/project",
      "log_path": "/tmp/my-agent.log",
      "claude_short_id": "7c5dcf5d",
      "codex_session_id": null,
      "gemini_session_id": null,
      "created_at": "2026-05-19T22:00:00Z"
    }
  ]
}
```

`config.paths.agents_registry_path` in `config.toml` overrides the default location for users with non-standard `state_dir` setups.

### Three optional session-id fields

`AgentEntry` carries one optional session-id field per provider (`claude_short_id`, `codex_session_id`, `gemini_session_id`). Exactly one is set per entry, dictated by the `provider` field. This is a flat shape rather than a discriminated union — the simpler shape JSON-serializes cleanly and the provider field already communicates which session-id namespace applies. (The type-design analyzer flagged the union as deferred work; the flat shape has held across every provider added since.)

### Schema-version guard

`load_registry` raises `RegistryVersionError` when:

1. The on-disk `schema_version` differs from the in-process constant.
2. A row carries a `provider` outside `KNOWN_PROVIDERS` — catches typos like `"calude"` that would otherwise round-trip silently and confuse downstream dispatch.
3. A row has unknown or missing fields — catches a future caller adding fields without bumping `schema_version`. The version guard is the single point of failure; if it misses, the registry should fail loud, not corrupt the in-memory entry.

All three diagnostics use the same exception class so callers can handle "alien shape" uniformly.

## Atomic write

`write_registry` serializes the payload BEFORE opening the temp file:

1. `text = _json_dumps(payload, ...)` — raises here on serialization errors (a mid-payload exception cannot corrupt the existing file).
2. `tmp = target.with_suffix(target.suffix + ".tmp")` — sibling temp file.
3. `tmp.write_text(text)` — full write to temp.
4. `os.replace(tmp, target)` — atomic POSIX rename.
5. On `OSError` anywhere in steps 3-4, `tmp.unlink(missing_ok=True)` cleans up the orphan before re-raising.

`_json_dumps` is exposed as a module-level attribute so tests can monkeypatch it to simulate disk failures (the bare-name call resolves through `globals()` at call time, so `monkeypatch.setattr(reg_module, "_json_dumps", ...)` works without ceremony).

## Per-agent flock

`_agent_lock_path(name, registry_path)` returns a deterministic path under `<registry-dir>/locks/<name>.lock`. The storage layer does NOT acquire the flock itself — callers in the dispatch layer hold the lock around a `load_registry` → mutate → `write_registry` cycle to serialize per-agent updates. Storage stays composable; production code gets serialization at the dispatch boundary where it matters.

The lock-path computation rejects path separators and `..` segments in the agent name to prevent traversal at the lock-file layer (the registry's `name` field is user-controlled).

## Provider selection

`select_provider(name, requested_provider)` is the chokepoint that prevents the most common misuse: re-using an agent name with the wrong provider on a follow-up.

| Agent exists? | `requested_provider` | Outcome |
|---------------|---------------------|---------|
| Yes | None | Returns recorded provider |
| Yes | Same as recorded | Returns it |
| Yes | Different from recorded | `ProviderMismatchError` |
| No | Given | Returns it (will be persisted on first dispatch) |
| No | None | `ValueError` — need a provider for a new agent |

`ProviderMismatchError` carries the agent name, recorded provider, and requested provider in its message so the CLI can surface a useful error without the caller doing detective work.

## Events

`emit(kind, **data)` appends one well-formed JSON line per call to `state_dir() / "events.jsonl"`. Telemetry is best-effort — `OSError` (disk full, permission denied, unwritable parent) is logged to stderr and swallowed so a failed log write cannot break a successful dispatch.

The project-level `fno.events` module is intentionally NOT reused for this. That module carries schema validation, mkdir-mutex locking, and provenance bindings tied to target sessions, none of which apply to cross-CLI agent dispatch events. Keeping the agents emitter minimal keeps the substrate decoupled.

Event schema:

```jsonl
{"ts":"2026-05-19T22:00:00Z","kind":"agent_ask_started","name":"foo","provider":"claude","message_sha256":"..."}
{"ts":"2026-05-19T22:00:01Z","kind":"agent_ask_done","name":"foo","provider":"claude","duration_ms":1234}
{"ts":"2026-05-19T22:00:00Z","kind":"agent_ask_failed","name":"foo","provider":"claude","stage":"subprocess","error":"..."}
```

Concurrent `emit()` calls from sibling processes interleave at JSONL line boundaries — a single line is well under `PIPE_BUF` (4096 bytes on macOS and Linux), so POSIX `O_APPEND` writes are atomic at that granularity.

## Wiring into the CLI

`agents` is registered in `LAZY_SUBCOMMANDS` in `cli/src/fno/cli.py`:

```python
"agents": ("fno.agents.cli:agents_app", "Cross-CLI agent dispatch (claude / codex / gemini)."),
```

The lazy loader defers the import until the user actually runs `fno agents ...`, keeping `fno --help` cold-start under the 160ms budget the lazy-imports refactor established.

## Create dispatch (claude)

> **Group 1 (cross-agent bus epic):** creation moved off `ask`.
> `fno agents ask` now messages EXISTING agents only (unknown name -> exit 16,
> "spawn it first"); the create machinery below is reached via
> `fno agents spawn --provider claude` (`dispatch_spawn` in `dispatch.py`),
> and the codex/gemini one-shot create lineage via `spawn --once`.

The `claude` provider's create path is the reference dispatch flow every other provider mirrors.

| Module | Role |
|--------|------|
| `cli/src/fno/agents/lock.py` | `hold_agent_lock` — wraps the per-agent flock with timeout + progress callback + a `detach()` escape hatch for the registry-write-failure path |
| `cli/src/fno/agents/providers/claude.py` | `bg_create(name, message, cwd, timeout) -> ProviderResult`, `parse_short_id(stdout)`, `ProviderParseError`, `ProviderSubprocessError`, argv-overflow stdin routing |

`dispatch.py` exposes `dispatch_ask(...)` + `DispatchAskError`; `cli.py` wires `cmd_ask` to call it.

### Surface

```
fno agents ask <name> <message> --provider claude [--cwd PATH] [--timeout SECS]
```

On success the command prints the 8-hex supervisor short-id on stdout and exits 0. The assistant reply is asynchronous — `claude --bg` spawns the supervisor and returns immediately; reply retrieval is `claude logs <short-id>` (or `fno agents logs <name>`).

### Dispatch flow

```
cmd_ask
  ↓
dispatch_ask
  ├─ validate name (length ≤128, not path-traversal, not 8-hex shape)
  ├─ validate message (non-empty after strip)
  ├─ acquire per-agent flock (hold_agent_lock, timeout=30s)
  │     INSIDE the lock:
  │       ├─ load_registry()                  ← single read for existing-name + provider selection
  │       ├─ reject if name already exists    (exit 2)
  │       ├─ reject if provider missing on new agent (exit 2)
  │       ├─ select_provider(name, provider)  (exit 2 on mismatch / unknown)
  │       ├─ is_provider_available(chosen)    (exit 14 if CLI not on PATH)
  │       ├─ emit agent_ask_started
  │       ├─ providers.claude.bg_create(...) (subprocess + parse short-id)
  │       │     ├─ argv ≤200KB → message via argv
  │       │     └─ argv >200KB → message via subprocess.run(input=msg)
  │       ├─ update_registry(append new AgentEntry)
  │       │     └─ on OSError: detach lock + emit agent_ask_failed + raise (exit 12)
  │       └─ emit agent_ask_done
  ├─ on AgentLockTimeout: emit agent_ask_failed + raise (exit 11)
  └─ return short_id
```

`is_provider_available` runs inside the lock rather than as a pre-flock fast-fail. A pre-lock probe left a TOCTOU window where a concurrent dispatch could complete between the probe and the lock acquisition, and a bare `except Exception` on a pre-lock `load_registry` hid registry-read failures. Pushing both inside the lock costs one cheap `shutil.which` call per dispatch while eliminating both races.

### Exit codes

| Code | Failure | Source |
|------|---------|--------|
| 0    | Success | stdout = `<short_id>\n`, registry entry written, events emitted |
| 1    | Subprocess non-zero OR unparseable stdout | provider.bg_create raises `ProviderSubprocessError` / `ProviderParseError` |
| 2    | Validation: empty/whitespace message; name too long; name matches `^[0-9a-f]{8}$`; name already in registry; missing `--provider` for new agent; unknown provider |
| 11   | Per-agent flock timeout (30s default) | `hold_agent_lock` raises `AgentLockTimeout` |
| 12   | Registry read OR write failed | wrapped `OSError` / `ValueError` from `load_registry` / `update_registry` |
| 14   | Provider CLI not on PATH | `shutil.which(chosen)` returned None |

### Argv-overflow safety

macOS argv limit is ~256KB; Linux is ~128KB. The implementation routes messages above 200KB (conservative threshold under both) via `subprocess.run(input=msg)` instead of as an argv token. The exact stdin convention for real `claude` is version-dependent; the unit tests use a fake-claude script that reads stdin unconditionally, so the substrate is verified portably. A real-claude smoke marker test catches CLI version drift.

### Lock release semantics

`hold_agent_lock` releases the per-agent flock in a `finally` branch by default. The yielded handle exposes `detach()` for one specific case: a post-subprocess registry-write failure. When `update_registry` raises `OSError` after `claude --bg` already created a supervisor session, the registry doesn't know about the orphan. The dispatcher calls `lock_handle.detach()` and re-raises so the next caller sees a stuck lock — a "manual cleanup needed" signal pointing at `claude rm <short_id>` (printed verbatim in the error stderr).

POSIX flocks release on file-descriptor close, so the detached file handle is stashed in a module-global list to keep it alive until process exit. Contract: this is CLI-process-lifetime only — the dispatcher raises `DispatchAskError(12)` immediately after detach and the typer CLI propagates the exit code to the shell, so the process exits within milliseconds. A long-lived host (test harness, future daemon) reusing the module must clear `_detached_handles` between operations or switch to a sentinel-file approach.

### Concurrency contract

Two parallel `ask <same-name>` calls serialize via the per-agent flock. The second to acquire the lock sees the first's registry write inside the locked `load_registry` call and exits 2 ("already exists") without invoking its own subprocess. Verified end-to-end in `tests/agents/test_dispatch_ask.py::test_dispatch_ask_two_processes_same_name` via `multiprocessing.fork`.

Two parallel `ask <different-names>` calls run their subprocesses concurrently (different per-agent flocks) and serialize only the final registry write via the registry-wide flock inside `update_registry`.

### Events

The create path emits four kinds with consistent payload shapes:

```jsonl
{"name":"foo","provider":"claude","ts":"...","kind":"agent_ask_started"}
{"stage":"dispatch","name":"foo","provider":"claude","short_id":"7c5dcf5d","duration_ms":1234,"ts":"...","kind":"agent_ask_done"}
{"stage":"subprocess","name":"foo","provider":"claude","returncode":1,"ts":"...","kind":"agent_ask_failed"}
{"stage":"parse","name":"foo","provider":"claude","short_id_raw":"Session created: foo","ts":"...","kind":"agent_ask_failed"}
{"stage":"registry-write","name":"foo","provider":"claude","short_id":"7c5dcf5d","ts":"...","kind":"agent_ask_failed"}
{"stage":"registry-read","name":"foo","ts":"...","kind":"agent_ask_failed"}
{"stage":"lock-timeout","name":"foo","ts":"...","kind":"agent_ask_failed"}
```

Every `agent_ask_started` is followed by exactly one `agent_ask_done` OR exactly one `agent_ask_failed` (modulo Ctrl-C, where the failed event is best-effort because `events.emit` is best-effort by design).

## Related capabilities

- [fno-agents-followup.md](fno-agents-followup.md) — claude follow-up over the messaging socket.
- [fno-agents-list-logs.md](fno-agents-list-logs.md) — the `list` and `logs` read verbs.
- [fno-agents-codex-provider.md](fno-agents-codex-provider.md) — the codex provider.
- [fno-agents-gemini-commands.md](fno-agents-gemini-commands.md) — the gemini provider.
- [fno-agents-lifecycle.md](fno-agents-lifecycle.md) — `stop` / `rm` / `reconcile` / `attach`.
- [fno-agents-mcp-channel.md](fno-agents-mcp-channel.md) — the sanctioned MCP channel send path.
