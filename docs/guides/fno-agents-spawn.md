# How-to: create a peer agent with `fno agents spawn`

`fno agents spawn <name> ["<initial message>"] --provider <p>` creates a new peer agent. It is the creation half of the de-overloaded verb pair from the cross-agent message bus epic: **`spawn` creates, `ask` messages**. An `ask` to a name that does not exist errors with exit 16 ("spawn it first") instead of silently creating - see [fno-agents-ask-followup.md](fno-agents-ask-followup.md) for the messaging half.

Use this guide when an orchestrator (script, LLM session, CI job) needs to launch a worker agent and capture a machine-parseable receipt.

## Prerequisites

- `fno` CLI installed; the compiled `fno-agents` binary for the default (Rust) runtime, or `FNO_AGENTS_RUNTIME=python` for the fallback.
- The provider CLI (`claude` / `codex` / `gemini`) on `$PATH`, signed in.

## Persistent claude peer (plain spawn)

```bash
fno agents spawn frontend-worker "/target no-merge" --provider claude
```

This shells `claude --bg --name frontend-worker <message>` (the subscription lane - never `-p`/`--bare`, which would move billing to the API-credit pool and strip hooks). The peer persists; follow up later with `fno agents ask frontend-worker "..."`.

stdout is exactly one compact JSON receipt line:

```json
{"name": "frontend-worker", "short_id": "7c5dcf5d", "provider": "claude", "status": "live"}
```

Pipe it: `fno agents spawn w1 "task" -p claude | jq -r .short_id`.

## Persistent codex/gemini peer (daemon PTY)

Plain `spawn` for codex/gemini creates a PTY-backed hosted worker under the `fno-agents` daemon (lazy-started). The receipt is the daemon's JSON payload, also carrying `.short_id`. The Python fallback runtime cannot host PTY workers and exits 13 with guidance.

## Ephemeral one-shot (`--once` / `-o`, codex and gemini)

```bash
fno agents spawn tmp1 "summarize the failing tests" -p codex --once
```

Create, exchange, tear down: stdout is the model's reply verbatim (the deliverable); the teardown receipt rides stderr (`once: tmp1 (codex/<session-id>) torn down`); no registry row survives. This is the explicit home of what `ask`-on-a-new-name used to do implicitly for codex/gemini.

- `--once` with claude is refused (exit 2): claude peers are persistent bg threads; use plain spawn.
- If teardown fails after a successful exchange, the peer is NOT silently leaked: a loud stderr warning names it and points at `fno agents rm <name>`, and the exit stays 0 (the exchange succeeded).
- An empty initial message defaults to a `"hello"` probe on the once paths.

## Canonical-root cwd (`--fresh` / `--here`)

By default a spawned worker inherits the caller's working directory. When the caller sits inside a git worktree that leaks the worktree into the worker, and a `/target`-class worker started there fights other sessions over the shared `.fno/` session state (manifest, claims, STATE), because a worktree's `.fno/` is symlinked back to canonical. `--fresh` resolves the worker's cwd to the **canonical main checkout** (the parent of `git rev-parse --git-common-dir`) regardless of where the caller runs:

```bash
fno agents spawn w1 "/target ab-1234abcd" -p claude --fresh
```

Precedence is `--cwd` > `--fresh` > caller cwd. An explicit `--cwd` always wins; `--here` (alias `--in-place`) opts back out of `--fresh` and keeps the caller cwd. `--fresh` is a no-op when the caller is already at canonical, and falls back to the caller cwd (the safe side) when resolution is ambiguous: a bare or `--separate-git-dir` checkout, or no `git` on `$PATH`. A real redirect prints one stderr line so it is never silent.

`--fresh` is opt-in at this layer, so plain interactive `ask` / `host` / `spawn` keep the caller cwd. The policy that turns it on for autonomous single-repo target-class work lives one layer up: `/target bg` (via `dispatch-node.sh`) defaults a node with no recorded cwd to `--fresh`, and a megawalk worker launched from a worktree is rooted at canonical. Cross-project dispatch and non-target verbs are exempt.

## Errors you will see

| Condition | Exit | Message shape |
|---|---|---|
| Name already registered | 2 | `agent '<name>' already exists; use 'fno agents rm <name>' first or pick another name` |
| No `--provider` for a new name | 2 | provider required |
| Unknown provider | 2 | `unknown provider '<p>'; supported: claude, codex, gemini` |
| `--once` with claude | 2 | persistent bg threads; use plain spawn |
| Plain codex/gemini spawn on the Python fallback | 13 | requires the fno-agents daemon (Rust runtime); use `--once` |
| Registry unreadable | 12 | `registry read failed: ...` |

Both runtimes produce byte-identical receipts and error messages for the shared surfaces; parity is pinned by `cli/tests/agents/test_ask_e2e_dispatch.py` and `crates/fno-agents/tests/spawn_routing.rs`.

## Rollout note

The deployed `fno` and `~/.cargo/bin/fno-agents` are snapshots: until `fno update` and the cargo bins refresh run after this epic group merges, the old binaries still carry the create-on-ask behavior and reject `spawn -p claude`. `fno doctor` detects the skew.
