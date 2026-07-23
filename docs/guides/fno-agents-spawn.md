# How-to: create a peer agent with `fno agents spawn`

`fno agents spawn <name> ["<initial message>"] --harness <h>` creates a new peer agent. It is the creation half of the de-overloaded verb pair from the cross-agent message bus epic: **`spawn` creates, `ask` messages**. An `ask` to a name that does not exist errors with exit 16 ("spawn it first") instead of silently creating - see [fno-agents-ask-followup.md](fno-agents-ask-followup.md) for the messaging half.

Use this guide when an orchestrator (script, LLM session, CI job) needs to launch a worker agent and capture a machine-parseable receipt.

## Prerequisites

- `fno` CLI installed; the compiled `fno-agents` binary for the default (Rust) runtime, or `FNO_AGENTS_RUNTIME=python` for the fallback.
- The provider CLI (`claude` / `codex` / `gemini`) on `$PATH`, signed in.

## The three axes: harness, provider, model

A spawn picks three independent things. Confusing them is the mistake this surface exists to prevent.

| Axis | Flag | Short | Values | Means |
|---|---|---|---|---|
| **harness** | `--harness` | `-H` | `claude` \| `codex` \| `gemini` \| `opencode` \| `agy` | The CLI **binary** to launch. |
| **provider** | `--provider` | `-P` | `zai`, or any `model_routing.providers` name | The model **vendor/endpoint** that binary talks to. |
| **model** | `--model` | `-m` | `opus`, `glm-5.2[1m]`, ... | The **model**, at whichever vendor is in play. |

```bash
fno agents spawn worker "review this diff" --harness codex          # binary only
fno agents spawn worker "review this diff" --model opus             # harness-native model
fno agents spawn w bg "review this diff" --provider zai --model glm-5.2   # routed
```

`--harness` defaults to the invoking harness, then `claude`. `--provider` + `--model` together name a **route** and are the decomposed spelling of `--route <vendor>,<model>`; passing both spellings exits 2, and `--provider` without `--model` exits 2 (a vendor is not a model).

A **harness name on the provider axis is refused by name**: `--provider claude` exits 2 with `claude is a harness, not a provider; use --harness claude`. This used to be how the harness was selected, so the refusal names the fix rather than launching the wrong thing.

Two shorts moved to make room. `-H` takes a harness value; it used to mean headless. `-p` now means headless, mirroring the harnesses' own one-shot short (`claude -p`), which is why the provider axis takes the capital `-P`. A one-shot is `--headless`, `-p`, `--once`, `-o`, or `--substrate headless`.

## Routing a worker to another vendor

`--provider <vendor> --model <m>` (or the single-string `--route <vendor>,<m>`) points a claude worker at a different model endpoint. The vendor must be a known `model_routing.providers` record with a resolvable key; an unknown, non-anthropic-compatible, or keyless vendor is refused before anything spawns, so the node stays dispatchable.

```bash
fno agents spawn glm-worker "review this diff" --substrate bg --provider zai --model glm-5.2
```

Routing is claude-only and reaches the `bg` and `headless` substrates only. The route is applied by writing a `0600` claude `--settings` file and passing `--settings <path>`: a `claude --bg` session's serving process is forked by the claude daemon, which drops per-spawn `ANTHROPIC_*` env before the first model request, and a settings file is read by the session process itself so it survives that fork. `pane` is not a routed lane and is refused rather than silently running the primary model.

## Persistent claude peer (plain spawn)

```bash
fno agents spawn frontend-worker "/target no-merge" --harness claude
```

This shells `claude --bg --name frontend-worker <message>` (the subscription lane - never `-p`/`--bare`, which would move billing to the API-credit pool and strip hooks). The peer persists; follow up later with `fno agents ask frontend-worker "..."`.

stdout is exactly one compact JSON receipt line:

```json
{"name": "frontend-worker", "short_id": "7c5dcf5d", "provider": "claude", "status": "live"}
```

Pipe it: `fno agents spawn w1 "task" -H claude | jq -r .short_id`.

## Persistent codex/gemini peer (daemon PTY)

Plain `spawn` for codex/gemini creates a PTY-backed hosted worker under the `fno-agents` daemon (lazy-started). The receipt is the daemon's JSON payload, also carrying `.short_id`. The Python fallback runtime cannot host PTY workers and exits 13 with guidance.

## Place a pane in a mux workspace

Pane-hosted agents can join an existing mux workspace and tile beside its focused pane:

```bash
fno agents spawn reviewer "review the current diff" \
  -H codex -s reviews -x right
```

`-s` / `--squad` selects a workspace by the same visible name shown in the mux sideline. `-x` / `--split` accepts `left`, `right`, `up`, or `down`; omit it to create a new tab in that squad. Omitting both options preserves the cwd-routed new-tab behavior. Placement does not change the worker's cwd, and the options are rejected for `bg` and `headless`, which have no mux geometry.

The lower-level equivalent avoids option punctuation entirely: `fno mux pane run squad reviews split right echo ready`. It also accepts `-s` / `--squad` and `-x` / `--split`. A missing squad or a split that would make a pane too small fails without leaving a child process or layout mutation behind.

## Reasoning effort (`--effort`)

`--effort` tunes reasoning without changing the selected model. Omit it to keep
the provider default.

```bash
fno agents spawn planner "analyze the migration" --harness claude --effort high
fno agents spawn verifier "check the invariants" --harness codex --effort medium
```

The accepted user-facing vocabulary is `minimal`, `low`, `medium`, `high`,
`xhigh`, and `max`, but each provider supports only its native subset:

| Provider | Supported effort values | Surface |
|---|---|---|
| Claude | `low`, `medium`, `high`, `xhigh`, `max` | pane, bg, headless |
| Codex | `minimal`, `low`, `medium`, `high` | pane, headless |
| OpenCode | superset, subject to the selected model | pane; persists the model variant |
| Gemini / agy | none | rejected before spawn |

Unsupported pairs fail with exit 2 before a pane or worker is created. For
OpenCode, the selected effort becomes the persisted variant for that explicit
`provider/model` in `~/.local/state/opencode/model.json`, matching OpenCode's
own variant toggle behavior.

## Ephemeral one-shot (`--once` / `-o`, codex and gemini)

```bash
fno agents spawn tmp1 "summarize the failing tests" -H codex --once
```

Create, exchange, tear down: stdout is the model's reply verbatim (the deliverable); the teardown receipt rides stderr (`once: tmp1 (codex/<session-id>) torn down`); no registry row survives. This is the explicit home of what `ask`-on-a-new-name used to do implicitly for codex/gemini.

- `--once` with claude is refused (exit 2): claude peers are persistent bg threads; use plain spawn.
- If teardown fails after a successful exchange, the peer is NOT silently leaked: a loud stderr warning names it and points at `fno agents rm <name>`, and the exit stays 0 (the exchange succeeded).
- An empty initial message defaults to a `"hello"` probe on the once paths.

## Canonical-root cwd (`--fresh` / `--here`)

By default a spawned worker inherits the caller's working directory. When the caller sits inside a git worktree that leaks the worktree into the worker, and a `/target`-class worker started there fights other sessions over the shared `.fno/` session state (manifest, claims, STATE), because a worktree's `.fno/` is symlinked back to canonical. `--fresh` resolves the worker's cwd to the **canonical main checkout** (the parent of `git rev-parse --git-common-dir`) regardless of where the caller runs:

```bash
fno agents spawn w1 "/target ab-1234abcd" -H claude --fresh
```

Precedence is `--cwd` > `--fresh` > caller cwd. An explicit `--cwd` always wins; `--here` (alias `--in-place`) opts back out of `--fresh` and keeps the caller cwd. `--fresh` is a no-op when the caller is already at canonical, and falls back to the caller cwd (the safe side) when resolution is ambiguous: a bare or `--separate-git-dir` checkout, or no `git` on `$PATH`. A real redirect prints one stderr line so it is never silent.

`--fresh` is opt-in at this layer, so plain interactive `ask` / `host` / `spawn` keep the caller cwd. The policy that turns it on for autonomous single-repo target-class work lives one layer up: `/target bg` (via `dispatch-node.sh`) defaults a node with no recorded cwd to `--fresh`, and a megawalk worker launched from a worktree is rooted at canonical. Cross-project dispatch and non-target verbs are exempt.

## Errors you will see

| Condition | Exit | Message shape |
|---|---|---|
| Name already registered | 2 | `agent '<name>' already exists; use 'fno agents rm <name>' first or pick another name` |
| No `--harness` for a new name | 2 | harness required |
| Unknown provider | 2 | `unknown provider '<p>'; supported: claude, codex, gemini` |
| `--once` with claude | 2 | persistent bg threads; use plain spawn |
| Unsupported provider/effort pair | 2 | names the provider's supported effort values |
| Plain codex/gemini spawn on the Python fallback | 13 | requires the fno-agents daemon (Rust runtime); use `--once` |
| Registry unreadable | 12 | `registry read failed: ...` |

Both runtimes produce byte-identical receipts and error messages for the shared surfaces; parity is pinned by `cli/tests/agents/test_ask_e2e_dispatch.py` and `crates/fno-agents/tests/spawn_routing.rs`.

## Rollout note

The deployed `fno` and `~/.cargo/bin/fno-agents` are snapshots: until `fno update` and the cargo bins refresh run after this epic group merges, the old binaries still carry the create-on-ask behavior and reject `spawn -H claude`. `fno doctor` detects the skew.
