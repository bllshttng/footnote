# `fno agents ask`: auto-routing to the Rust client

> **Superseded by the unconditional flip.** This document describes the *provider-conditional* state shipped earlier (claude/codex routed to Rust, gemini stayed on Python via the `RUST_CLIENT_ASK_PROVIDERS` / `_resolve_ask_provider` special case). That special case is now **removed**: gemini is ported and `ask` auto-routes for every provider. `PYTHON_AGENT_VERBS` is empty and `AUTO_ROUTE_VERBS == RUST_CLIENT_VERBS` is the whole contract. See [`fno-agents-gemini-ask-rust.md`](./fno-agents-gemini-ask-rust.md) for the current state. The sections below are retained as the design history of the intermediate provider-conditional step.

Builds on the claude port ([`fno-agents-claude-ask-rust.md`](./fno-agents-claude-ask-rust.md)) which shipped the **capability** for claude.

## What this is

`ask` was the last `fno agents` verb still owned by Python by default. Every other verb (`list`, `stop`, `rm`, `reconcile`, ...) auto-routes to the compiled `fno-agents` client. This node makes `ask` auto-route as well, for the providers whose client-side port is live: **claude** (parent) and **codex** (Wave B of this node). **gemini** continues to use the Python dispatch until its port lands (deferred follow-up).

## Why ask is provider-split

`ask` has two backend shapes depending on the target provider:

- **claude** is a `claude --bg` shellout (`ClaudeProvider::as_pty()` is `None`). It is self-supervised: own bg daemon, a rendezvous AF_UNIX socket, and state under `~/.claude/jobs/<short-id>/`. The Rust client replicates this path directly in `claude_ask.rs`, bypassing the fno daemon. Byte-parity proven by `claude_ask_parity.rs`.
- **codex** is a one-shot `codex exec --json ...` subprocess: not a daemon-PTY agent. The Rust client replicates this path directly in `codex_ask.rs` (new in this node), bypassing the fno daemon. Byte-parity proven by `codex_ask_parity.rs`.
- **gemini** has no Rust client port yet; the Python dispatch handles it.

The fno daemon's `handle_ask` returns the whole rendered `TerminalGrid` screen of a resident PTY worker — a different execution model and output channel. Screen-diffing it CANNOT reach byte-parity with the Python providers' parsed reply text. The daemon `handle_ask` exists for resident-agent interaction (`drive`/`attach`), not the `ask` verb. So both claude and codex (and gemini, when its port lands) handle `ask` CLIENT-SIDE.

## The provider-conditional flip

`ask` deliberately **stays** in `PYTHON_AGENT_VERBS` (`cli/src/fno/agents/rust_runtime.py`). Removing it would route gemini `ask` to the wrong backend. Instead, a new branch in `make_context` handles `ask` as a special case:

```python
RUST_CLIENT_ASK_PROVIDERS = frozenset({"claude", "codex"})

# Inside make_context, after the AUTO_ROUTE_VERBS short-circuit:
elif mode == "auto" and verb == "ask":
    provider = _resolve_ask_provider(args)
    if provider in RUST_CLIENT_ASK_PROVIDERS:
        binary = resolve_installed_binary()
        if binary is not None:
            route_to_rust(list(args), binary=binary)  # execs
    # else: gemini, unresolvable, or no installed binary -> Python dispatch
```

`_resolve_ask_provider(args)` resolves the target provider in two passes:

1. An explicit `--provider <p>` or `--provider=<p>` wins (the create path always carries this).
2. Otherwise, the first non-flag positional after the verb is the agent name; look it up in `agents_registry_path()` (top-level `agents` list, per the cross-language schema in [`cross-language-schema-parity.md`](./cross-language-schema-parity.md)) and return its `provider`.

The scanner skips past the value tokens of every value-carrying flag on `cmd_ask` (`--cwd`, `--timeout`, `--from-name`, plus `--provider` special-cased) so flag values do not get misread as the agent name. Trap: `ask --from-name sender registered-agent hi` — without value-flag skip, "sender" would look like the name; the scanner now correctly captures "registered-agent".

Returns `None` on any failure (missing registry, perm denied, corrupt JSON, agent absent, no positional name). The caller treats `None` as "let Python handle it" — Python emits the canonical actionable error. Deliberately tolerant; the Rust binary does its own registry read once routed.

### Why ask stays in `PYTHON_AGENT_VERBS`

The contract test `AUTO_ROUTE_VERBS == RUST_CLIENT_VERBS - PYTHON_AGENT_VERBS` is the routing-drift tripwire. If a future change removes `ask` from `PYTHON_AGENT_VERBS` before gemini is ported, gemini `ask` would silently route to the binary, which has no gemini-ask path. Keeping `ask` in `PYTHON_AGENT_VERBS` and handling routing via the explicit provider-conditional branch keeps the tripwire honest. The membership of `RUST_CLIENT_ASK_PROVIDERS` is itself pinned by a contract test that flags drift if gemini is added without porting `providers/gemini.py`.

## Codex `ask` client-side port

`crates/fno-agents/src/codex_ask.rs` mirrors `claude_ask.rs`'s shape (pure-function core + dispatch orchestrator + `maybe_run_codex_ask` client hook). Covers both shapes:

- **Create** (`codex_create`): `codex exec --json -C <cwd> --skip-git-repo-check [--dangerously-bypass-approvals-and-sandbox if --yolo else --sandbox workspace-write] <[from: X] prompt>`. Parse the JSONL event stream:
  - `thread.started` → capture `thread_id` as session id
  - `item.completed` with `item.type == "agent_message"` → capture `text` as reply
  - `item.completed` with `item.type == "error"` → soft-error reply
  - `turn.completed` → end-of-turn marker
- **Resume** (`codex_resume`): `codex exec resume <session_id> --json ...`. Subprocess `cwd` is pinned to the registry-recorded cwd (codex sessions are cwd-bound). No `--sandbox` arg.

Faithful to the Python error taxonomy in `providers/codex.py`:

| Condition | Exit code | Mapping |
|---|---|---|
| codex binary missing | 14 | `NotFound` → provider unavailable |
| no `thread.started` on create | 11 | `NoSessionId` |
| `output.jsonl` tee open fails | 12 | `TeeOpen` |
| codex exits non-zero with reply | reported exit | `Invocation` |
| OSError / SIGKILL escalation | 1 | `Invocation` |
| Timeout (SIGTERM -> SIGKILL) | 15 | `Timeout` |

The fake-codex fixture used by `codex_ask_dispatch.rs` (B2) and `codex_ask_parity.rs` (B3) is shape-identical to the script used by Python's tests, so a single fake serves both differential paths.

### Stdout shape (codex create vs claude create)

Codex deliberately returns the model reply on `create`, not just the session id — codex's first call captures real model output and the user already paid for it. Python's `dispatch_ask` codes this as `DispatchAskResult(kind="followup", short_id=session_id, reply=last_msg)` and `cmd_ask` prints `result.reply or ""` (no newline). The Rust client returns `AskOutcome::ok_reply(result.last_msg)` and `maybe_run_codex_ask` writes that to stdout the same way.

Claude `--bg` create returns only the short_id (the supervisor session is not yet engaged), so `cmd_ask` prints `<short_id>\n` and the Rust client matches. The two providers' create-stdout shapes differ for principled reasons; both are byte-parity within their provider.

## Observability: `FNO_AGENTS_DEBUG_ROUTING`

`_resolve_ask_provider` is silent on every failure path by design (every fallthrough returns `None` and lets Python's mature dispatch emit the canonical error). On a registry with restrictive perms or corruption, the user sees "agent not found" from Python with no breadcrumb to the root cause. Set `FNO_AGENTS_DEBUG_ROUTING=1` to surface a one-line stderr note naming the reason:

```
fno agents: ask route fell through to Python (reason: registry read failed (/Users/.../registry.json): PermissionError(13, 'Permission denied'))
```

Reasons covered: `paths import failed`, `agents_registry_path() raised`, `registry missing at <path>`, `registry read failed`, `registry JSON parse failed`, `registry top-level is not an object`, `registry 'agents' key missing or not a list`, `agent <name> absent from registry`. Silent without the env var so normal CLI runs (every fresh install hits "registry missing") stay quiet.

## Test surfaces

| Surface | What it pins |
|---|---|
| `cli/tests/agents/test_rust_runtime.py` (contract + parametric) | `_resolve_ask_provider` correctness, routing-decision branches, `RUST_CLIENT_ASK_PROVIDERS` set, value-carrying flag handling, `FNO_AGENTS_DEBUG_ROUTING` observability |
| `cli/tests/agents/test_ask_e2e_dispatch.py` | CLI-level dispatch differential: Python `dispatch_ask` vs Rust binary subprocess against one shared fake codex; happy path + non-zero exit propagation |
| `crates/fno-agents/tests/codex_ask_unit.rs` (B1) | Pure-function core: argv build, JSONL parse, error enum, exit-code map |
| `crates/fno-agents/tests/codex_ask_dispatch.rs` (B2) | Subprocess driver + dispatch orchestrator against the fake codex |
| `crates/fno-agents/tests/codex_ask_parity.rs` (B3) | Cross-language byte parity: Python `providers/codex.py` vs Rust `codex_ask` against the SAME fake codex |
| `crates/fno-agents/tests/claude_ask_parity.rs` (parent) | Cross-language byte parity for claude |

## Scope boundary

This node ships:

- Codex client-side `ask` port (Wave B): `codex_ask.rs` + tests.
- Provider-conditional auto-route flip (Wave A): claude+codex auto, gemini stays Python.
- `_resolve_ask_provider` + value-flag handling.
- `FNO_AGENTS_DEBUG_ROUTING` observability.

Deferred to follow-up nodes:

- Gemini `ask` Rust client port + unconditional flip.
- MCP-channel transport optimization.
- `EventContext` success-event envelope.
- Polish carveouts.
- Six post-flip findings tracked as follow-up carveouts.
