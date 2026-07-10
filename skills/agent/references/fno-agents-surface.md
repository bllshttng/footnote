# `fno agents` surface map

The classification companion to [`docs/provider-command-matrix.md`](../../../docs/provider-command-matrix.md). That page answers *which provider supports a verb*; this page answers *which verbs a human drives, which are machine-internal, and which `/agent` routes vs leaves raw*. It is a map, not an encyclopedia: it exists so `/agent` documents its routing boundary and a reader can tell "not a `/agent` verb" apart from "missing feature".

## Two layers, two `--help`

`fno agents <verb>` dispatches across two layers:

- **Python typer group** (`fno agents --help`) - the human-facing help. Supports `hidden=True` to keep a machine verb out of the listing while it still runs.
- **Rust client** (`fno-agents --help`) - emits JSON of every routable verb (a machine list, correctly exhaustive). Most machine verbs are Rust-only and never reach the typer help, so they do not clutter the human surface at all.

So "declutter the human `--help`" only touches the handful of *Python-registered* machine verbs. Everything Rust-only is already invisible to `fno agents --help`.

## Classification

### Human-facing (a person drives these)

`/agent` routes the dispatch/observe subset (see the verb router in `SKILL.md`); the rest stay raw `fno agents <verb>` and are documented, not hidden.

| Verb | Routed by `/agent`? | What it does |
|------|:---:|--------------|
| `spawn` | yes (default) | Launch a worker (substrate `pane`/`bg`/`headless`). |
| `ask` | yes (`ask`/bare) | One-shot prompt, verbatim, no `/target` wrap. |
| `stop` | yes | Terminate a worker (confirm). |
| `watch` / `list` / `logs` | yes | Observe. |
| `whoami` | yes | This session's registered mesh identity. |
| `status` | raw | Daemon liveness + per-agent state. |
| `rm` / `restart` / `resume` / `attach` | raw | Registry / session admin. |
| `top` | raw | Live agent-activity view. |
| `chat` | yes | Costed live A↔B relay (always-confirm). |

### Machine-internal (the loop and hooks call these; a human rarely does)

Out of scope for the `/agent` router by design - naming one is not a gap. Rust-only unless flagged **(typer)**.

| Verb | Caller |
|------|--------|
| `loop` / `loop-check` / `finalize` | the target/megawalk loop (stop decision, ledger/stamp writer). |
| `reconcile` / `reap` | post-merge close-out, agent-view GC. |
| `kill-check` | kill-criteria evaluation. |
| `verify-evidence` / `digest` / `report` / `wait` / `trace` / `ping` | evidence audit, catch-up fold, progress report, blocking wait, tracing, liveness. |
| `spawn-guard` **(typer, hidden)** | atomic node-claim probe used by the spawn path. |
| `drive-authority` **(typer, hidden)** | reports whether an operator holds a drive window. |
| `gate` **(typer, hidden)** | retired at G4; kept for back-compat, hidden from help. |

The three **(typer, hidden)** verbs are the only machine verbs that used to show in `fno agents --help`; they carry `hidden=True` now. The rest are Rust-only and were never listed.

### Exploratory - channel pub/sub (keep the code, mark experimental)

| Verb | State |
|------|-------|
| `register-channel` / `unregister-channel` / `push-channel` / `subscribe` | The intended pub/sub avenue for the MCP-route comms channel (Discord and/or agent-to-agent, each session subscribed to a channel). Zero shell call sites as of this writing and untested end-to-end - deliberate infra, **not dead code**. Rust-only, so already absent from `fno agents --help`. Do not cut. |

A verb with zero shell call sites is *not-yet-wired*, not dead: cutting requires proving no shell call site **and** no internal dispatch **and** no forward intent. The channel verbs fail that test (forward intent is explicit), so they stay.

## Why `hide` over an `internal` namespace

Hiding a typer command (`hidden=True`) is one line per command and zero call-site churn - the verb still dispatches, it just leaves the `--help` listing. Moving verbs under an `fno agents internal <verb>` namespace would rewrite every hook/script/crate/test call site for no user-visible gain. Live-internal verbs (`loop-check` and friends the loop depends on) stay reachable; they are simply not advertised.
