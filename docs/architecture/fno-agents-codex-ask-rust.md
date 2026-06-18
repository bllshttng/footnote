# Rust client handles codex `ask` (byte-parity reply extraction)

`ask` was the last `fno agents` verb without a client-side port. As of the auto-routing follow-up to the claude port, the Rust `fno-agents` client handles codex `ask` directly via a one-shot `codex exec --json` subprocess, byte-parity against Python's `cli/src/fno/agents/providers/codex.py`, bypassing the fno daemon entirely.

This document explains the architecture, the provider-conditional routing that drives it, and the deferred follow-ups.

## Why client-side, not daemon

The fno daemon PTY-supervises codex and gemini for resident-agent interaction (`drive`, `attach`, long-lived `spawn`). Its `handle_ask` returns the whole rendered `TerminalGrid` screen of that resident worker. That is fundamentally different from what the `ask` verb wants:

- Python `providers/codex.py` invokes `codex exec --json` as a one-shot subprocess and parses the JSONL event stream (`thread.started` → session_id, `item.completed`/`agent_message` → reply text, `turn.completed` → end-of-turn).
- The daemon's rendered-screen output cannot reach byte-parity with the parsed-JSON reply Python returns.

The prior cutover that routed codex `ask` to the daemon (Task 4.1, commit c54c6d78) was reverted in sigma-review for exactly this reason. The correct shape is the same one claude already uses: a Rust-client module that ports the Python provider one-to-one, with the daemon untouched.

## Architecture

The Rust binary's entry point sequence (`crates/fno-agents/src/bin/client.rs::run`) is:

1. Parse the agents-namespace arg list.
2. Try `maybe_run_claude_ask(home, &params, &name)` — returns `Some(exit_code)` if the target resolves to a claude agent, else `None`.
3. Try `maybe_run_codex_ask(home, &params, &name)` — same contract for codex (new in this PR).
4. Fall through to `build_request` and the daemon RPC for all other verbs.

`maybe_run_codex_ask` resolves the target's provider from `--provider` (create) or the registry row keyed by name (followup). When the provider is `codex`, it dispatches via `dispatch_codex_ask` and prints the result; otherwise it returns `None` and the daemon path handles it.

### Module shape (`crates/fno-agents/src/codex_ask.rs`)

| Layer | Responsibility |
|---|---|
| Pure core | `inject_from_name`, `build_argv_create`, `build_argv_resume`, `sandbox_flag(_resume)`, `parse_jsonl_line`, `CodexAskError` enum + `exit_code()` map |
| Subprocess driver (`run_codex`) | Spawn into its own process group, drain stdout (JSONL), drain stderr side-thread, tee every raw line to `output.jsonl`, run a cancelable watchdog (SIGTERM→SIGKILL on timeout), reap with grace |
| Entry fns | `codex_create` (argv via `build_argv_create`, subprocess cwd unset, `-C` carries it) and `codex_resume` (argv via `build_argv_resume`, subprocess cwd pinned to the registry-recorded cwd, no `-C`) |
| Dispatch orchestrator (`dispatch_codex_ask`) | Validate inputs, acquire the per-agent flock, decide create-vs-resume by registry lookup, stamp the registry, emit `agent_ask_done` / `agent_followup_done`/`failed` events, build the `AskOutcome` |
| Client hook (`maybe_run_codex_ask`) | Provider resolution, mismatch guard, dispatch, print stdout/stderr, return exit code |

### The byte-parity contract

The five JSONL literals pinned in the constants block (`thread.started`, `turn.completed`, `item.completed`, `agent_message`, `error`) are the parity contract with codex 0.130.0 captured by `scripts/smoke/capture-codex-jsonl.sh`. The argv shapes match Python exactly (verified by `crates/fno-agents/tests/codex_ask_parity.rs`):

- Create: `codex exec --json -C <cwd> --skip-git-repo-check [--sandbox workspace-write | --dangerously-bypass-approvals-and-sandbox] <[from: X]\n\n prompt>`
- Resume: `codex exec resume <session_uuid> --json --skip-git-repo-check [--dangerously-bypass-approvals-and-sandbox] <[from: X]\n\n prompt>` (subprocess cwd = registry cwd; no `-C`)

The from_name injection is plain concatenation: `[from: <from_name>]\n\n<prompt>`. No escaping (codex consumes plain text). Validation of name/message/from_name is shared with claude via the now-public `claude_ask::validate_inputs` (one canonical pre-flight gate for all providers, mirroring Python's `dispatch.py::_validate_inputs` + `_validate_from_name`).

### Error / exit-code taxonomy

| Variant | Exit | When |
|---|---|---|
| `NotFound` | 14 | `codex` binary not found on PATH (mirrors Python's `dispatch.py` mapping of `CodexInvocationError(127)` → "provider unavailable") |
| `NoSessionId { types_seen }` | 11 | Create finished without a `thread.started` event; `types_seen` carries forensics |
| `TeeOpen` | 12 | Cannot open `output.jsonl` |
| `Timeout` | 15 | Wall-clock exceeded |
| `Invocation { exit_code }` | `exit_code` or 1 | Subprocess exited non-zero with no captured reply |
| `SigkillEscalated { partial_exit_code }` | `partial_exit_code` or 1 | Watchdog escalated to SIGKILL; partial reply is always treated as failure (silent-failure-hunter row 4) |
| `OsError` | 1 | `Command::spawn` IO failure |

Both `dispatch_create` and `dispatch_resume` propagate `e.exit_code()` directly; the mapping lives on the error type so no path can leak the wrong code.

### Watchdog cancellation

The watchdog runs on a side thread that does `recv_timeout(d)` on a channel the main thread holds. On read-loop exit the main thread drops its sender; the watchdog's `recv_timeout` returns `Disconnected` and the kill cascade is skipped. Both stages (SIGTERM and SIGKILL escalation) are cancelable. This mirrors codex.py's `finally: for t in timers: t.cancel()` and prevents the "watchdog SIGTERMs a pid-reused process minutes later" hazard the earlier `AtomicBool`-only design carried.

## Provider-conditional routing (the flip)

`ask` could not simply leave `PYTHON_AGENT_VERBS`: gemini still has no Rust port, and an unconditional route would send gemini asks to the (wrong-shape) daemon. Instead, a new conditional branch in `cli/src/fno/agents/rust_runtime.py::make_context` routes `ask` to the Rust client *only* when the resolved provider is in `RUST_CLIENT_ASK_PROVIDERS = {"claude", "codex"}`:

```python
elif mode == "auto" and verb == "ask":
    provider = _resolve_ask_provider(args)
    if provider in RUST_CLIENT_ASK_PROVIDERS:
        binary = resolve_installed_binary()
        if binary is not None:
            route_to_rust(list(args), binary=binary)  # execs
```

`_resolve_ask_provider` scans args for `--provider` (or `--provider=`), else looks up the first positional non-flag token in `~/.fno/agents/registry.json`'s top-level `agents` list. Value-carrying flags (`--cwd`, `--timeout`, `--from-name`, `--message`) consume their value token so they don't get mistaken for the agent name. Any failure (no provider, missing registry, corrupt JSON, unresolvable name) returns `None`, and the Python dispatch handles it with its mature actionable error.

`ask` deliberately stays in `PYTHON_AGENT_VERBS` so the `AUTO_ROUTE_VERBS = RUST_CLIENT_VERBS - PYTHON_AGENT_VERBS` identity contract test remains the routing-drift tripwire. The conditional routing lives on a separate code path; the `RUST_CLIENT_ASK_PROVIDERS` membership is its own contract test that flips red the moment gemini is added without porting `providers/gemini.py`.

## End-to-end differential

`cli/tests/agents/test_ask_e2e_dispatch.py` drives the same fake codex through Python's `dispatch_ask` (in-process) and the Rust `fno-agents` subprocess, then compares stdout + exit code. The harness branches on `py_result.kind` so codex creates (Python: `kind="followup"`, returns reply) and claude creates (Python: `kind="create"`, returns `<short_id>\n`) are both verified at the user-visible CLI boundary. The library-level byte-parity tests in `crates/fno-agents/tests/codex_ask_parity.rs` cover the same surface against the real Python source for AC1-AC9 of the parent.

## Scope boundary (deferred to follow-ups)

This PR delivers the codex client-side ask + the provider-conditional flip for claude+codex. Three follow-up nodes capture the deferred work:

| Node | Scope |
|---|---|
| gemini ask port | Port `providers/gemini.py` to a Rust client module (single JSON-blob parse + schema-drift guards), wire `maybe_run_gemini_ask`, add `"gemini"` to `RUST_CLIENT_ASK_PROVIDERS` |
| MCP-channel transport | MCP-channel transport US6 (`ask_followup_via_mcp`, `mcp_channel_reachable`, demote-to-socket) — transport optimization over the now-correct one-shot path |
| EventContext + polish | EventContext envelope + reconcile/list polish |

Five additional carveouts from sigma-review capture lower-priority hardening: stderr-drain warn, canonicalize fallback warn, stderr-handle join warn, CLI-level followup parity differential, cross-language registry round-trip test.

## Related

- `docs/architecture/fno-agents-claude-ask-rust.md` — the parent's claude port; this doc's sibling.
