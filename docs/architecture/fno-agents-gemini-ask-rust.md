# Rust client handles gemini `ask` + the unconditional flip

The closing follow-up of the codex auto-routing work ([`fno-agents-ask-auto-route.md`](./fno-agents-ask-auto-route.md)). It ports Python's `cli/src/fno/agents/providers/gemini.py` to a Rust client module and then flips `ask` to auto-route **unconditionally** for every provider, removing the provider-conditional special case.

## What this completes

`ask` was the last `fno agents` verb with a provider carve-out. claude shipped client-side first, then codex + a provider-conditional flip (claude/codex routed to Rust, gemini stayed on Python). This node ports gemini and drops the carve-out: `PYTHON_AGENT_VERBS` is now empty, so `AUTO_ROUTE_VERBS == RUST_CLIENT_VERBS` is the whole routing contract.

## The gemini cleavage from codex

gemini's `ask` is a one-shot subprocess like codex, but two things differ:

- **Single JSON blob, not a JSONL stream.** gemini emits one JSON object at EOF: `{"session_id": ..., "response": ..., "stats": ...}`. `parse_response` is a single `serde_json::from_str` over the whole stdout, with schema-drift guards: `session_id` must be string-or-null, `response` must be present and string-or-null (null → `""`, the model-declined case), `stats` must be present (a missing `stats` is drift, not a silent empty reply, a cross-model review finding). Any drift is a parse error (exit 11).
- **Separate stderr drain.** gemini writes structural warnings (Ripgrep/MCP/skill conflicts) to stderr that would corrupt the JSON parse if merged. So `run_gemini` drains stderr on a dedicated thread (codex merges stderr into stdout, LD12) and tees both stdout and stderr to `output.jsonl` under a shared lock, keeping the stdout blob pure for the parse.

argv: `gemini --skip-trust -p <[from: X] prompt> --output-format json [--yolo | --approval-mode default] [--session-id <uuid> | --resume <uuid>]`. cwd is pinned via `Command::current_dir` (gemini sessions are cwd-bound), not a `-C` flag. Create lets gemini auto-generate the session id and captures it from the response; resume passes `--resume <uuid>` from the registry-recorded `gemini_session_id`.

Error taxonomy (mirrors `dispatch.py`'s `_gemini_create_path` / `_gemini_followup_path`):

| Condition | Exit | Variant |
|---|---|---|
| gemini binary missing | 14 | `NotFound` (matches `dispatch_ask`'s `is_provider_available` exit-14) |
| malformed JSON / schema drift / missing session on create | 11 | `Parse` |
| `output.jsonl` tee open fails | 12 | `TeeOpen` |
| gemini exits non-zero (incl. SIGKILL escalation) | reported exit, else 1 | `Invocation` |
| non-ENOENT OSError at spawn | 1 | `OsError` |
| wall-clock timeout (SIGTERM → SIGKILL) | 15 | `Timeout` |
| operator Ctrl-C forwarded to the gemini pgroup | 130 | `Interrupted` |

## The shared subprocess driver

The SIGINT forwarding, process-group kill, grace reap, wall-clock watchdog, output tee, and `--cwd` resolution are identical across codex and gemini, so this node extracts them from `codex_ask.rs` into `crates/fno-agents/src/subprocess_ask.rs`:

- `SigintForwarder` (RAII guard + process-global statics + async-signal-safe handler) — forwards an operator Ctrl-C to the one-shot subprocess's process group so gemini/codex + their sandbox descendants tear down instead of orphaning, and honors a SIG_IGN parent.
- `kill_pgrp`, `wait_with_grace`, `open_tee`, `AskWatchdog` (cancelable: a happy-path completion drops the sender so the kill cascade is skipped).
- `resolve_ask_cwd` — warns at the canonicalize-fallback point, fixing the previously-silent path shared by `maybe_run_codex_ask` and `maybe_run_gemini_ask`.

codex now delegates to this module; its 58-test regression suite proves the extraction is behavior-preserving. The stderr-drain warn-once and drain-thread-panic surfacing hardening apply to both providers.

## The unconditional flip

`cli/src/fno/agents/rust_runtime.py` now has an empty `PYTHON_AGENT_VERBS`, so `make_context` routes `ask` through the same `verb in AUTO_ROUTE_VERBS` branch as every other verb. The provider-conditional special case (`RUST_CLIENT_ASK_PROVIDERS`, `_resolve_ask_provider`, the `elif verb == "ask"` branch, the `_ASK_VALUE_FLAGS` scanner, the `FNO_AGENTS_DEBUG_ROUTING` breadcrumb) is deleted — the Rust client now owns the full create/resume decision for all three providers.

The one case that previously stayed on Python — an `ask` with no resolvable provider (new agent, no `--provider`) — is now surfaced by the Rust client itself in `bin/client.rs::unresolvable_ask_exit`: it reproduces Python's `select_provider` exit-2 error byte-for-byte (`provider is required for new agent <repr(name)>; pass --provider one of: claude, codex, gemini`, or the `unknown provider` variant) rather than falling through to the daemon PTY path (Locked Decision 3). `py_repr` (from `claude_ask.rs`) reproduces Python's `{name!r}` quoting.

`FNO_AGENTS_RUNTIME=python` still forces the Python dispatch for every provider, and an absent installed binary still falls back to Python — the fallback is intact.

## Test surfaces

| Surface | What it pins |
|---|---|
| `crates/fno-agents/tests/gemini_ask_unit.rs` | argv builders, `inject_from_name`, `sandbox_flag`, single-blob `parse_response` + every schema-drift guard, exit-code map |
| `crates/fno-agents/tests/gemini_ask_parity.rs` | cross-language byte parity: Python `providers/gemini.py` vs Rust `gemini_ask` against one shared fake gemini (create/resume/null-response/drift/non-zero/stderr-noise/inject) + the cross-language registry round-trip |
| `crates/fno-agents/tests/gemini_ask_sigint.rs` | Ctrl-C forwards to the gemini pgroup → exit 130; SIG_IGN parent disposition preserved (shared `subprocess_ask` driver) |
| `cli/tests/agents/test_rust_runtime.py` | empty `PYTHON_AGENT_VERBS`, `AUTO_ROUTE_VERBS == RUST_CLIENT_VERBS` identity, ask auto-routes for every provider / no-flag / no-binary fallback / `=python` force |
| `cli/tests/agents/test_ask_e2e_dispatch.py` | CLI-level create + followup dispatch differential parametrized across codex AND gemini |

The fake gemini fixture (single JSON blob + stderr noise) is shape-identical across the Rust parity test and the Python e2e test, so one fake serves both differential paths.

## Carveouts folded in

- SIGINT forwarding: shared `SigintForwarder` gives both providers one clean implementation; gemini needs it for byte-parity (gemini.py forwards SIGINT on KeyboardInterrupt).
- stderr-drain warn / join warn: inherited via the shared driver.
- canonicalize warn: fixed in the shared `resolve_ask_cwd`, applying to both providers.
- CLI-level followup differential + cross-language registry round-trip: added as a parametrized matrix.

## Still deferred

The `EventContext` success-event envelope and the MCP-channel transport optimization remain follow-ups: the Rust ask ports (claude/codex/gemini) all emit events via `emit_event` without the dispatch `EventContext` envelope, matching each other and deferring the envelope alignment to that node.
