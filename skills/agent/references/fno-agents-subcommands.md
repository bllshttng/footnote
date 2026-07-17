# `fno agents` subcommands

The classification companion to [`docs/harness-command-matrix.md`](../../../docs/harness-command-matrix.md). That page answers *which provider supports a subcommand*; this page answers *which subcommands a human drives, which are machine-internal, and which `/agent` routes vs leaves raw*. It is a map, not an encyclopedia: it exists so `/agent` documents its routing boundary and a reader can tell "not a `/agent` subcommand" apart from "missing feature".

("Subcommand" because the set is mixed: `spawn`/`stop`/`register` are verbs, but `whoami`/`top`/`logs` are not - they are all subcommands of `fno agents`.)

## Two layers, two `--help`

`fno agents <subcommand>` dispatches across two layers:

- **Python typer group** (`fno agents --help`) - the human-facing help. Supports `hidden=True` to keep a machine subcommand out of the listing while it still runs.
- **Rust client** (`fno-agents --help`) - emits JSON of every routable subcommand (a machine list, correctly exhaustive). Most machine subcommands are Rust-only and never reach the typer help, so they do not clutter the human `--help` at all.

So "declutter the human `--help`" only touches the handful of *Python-registered* machine subcommands. Everything Rust-only is already invisible to `fno agents --help`.

## Classification

### Human-facing (a person drives these)

`/agent` routes the dispatch/observe subset (see the `/agent` router in `SKILL.md`); the rest stay raw `fno agents <subcommand>` and are documented, not hidden.

| Subcommand | Routed by `/agent`? | What it does |
|------|:---:|--------------|
| `spawn` | yes (default) | Launch a worker (substrate `pane`/`bg`/`headless`). |
| `ask` | yes (`ask`/bare) | One-shot prompt, verbatim, no `/target` wrap. |
| `stop` | yes | Terminate a worker (confirm). |
| `watch` / `list` / `logs` | yes | Observe. |
| `whoami` | yes | This session's registered mesh identity. |
| `register` | raw (via `/fno-me`) | Join THIS hand-started session to the roster under its canonical `<harness>-<shortid>` handle, so peers can `fno mail send` to it. The write-side counterpart of `whoami`. |
| `status` | raw | Daemon liveness + per-agent state. |
| `rm` / `restart` / `resume` / `attach` | raw | Registry / session admin. |
| `top` | raw | Live agent-activity view. |

### Machine-internal (the loop and hooks call these; a human rarely does)

Out of scope for the `/agent` router by design - naming one is not a gap. Rust-only unless flagged **(typer)**.

| Subcommand | Caller |
|------|--------|
| `loop` / `loop-check` / `finalize` | the target/megawalk loop (stop decision, ledger/stamp writer). |
| `reconcile` / `reap` | post-merge close-out, agent-view GC. |
| `kill-check` | kill-criteria evaluation. |
| `verify-evidence` / `digest` / `report` / `wait` / `trace` / `ping` | evidence audit, catch-up fold, progress report, blocking wait, tracing, liveness. |
| `spawn-guard` **(typer, hidden)** | atomic node-claim probe used by the spawn path. |
| `drive-authority` **(typer, hidden)** | reports whether an operator holds a drive window. |
| `gate` **(typer, hidden)** | retired at G4; kept for back-compat, hidden from help. |

The three **(typer, hidden)** subcommands are the only machine subcommands that used to show in `fno agents --help`; they carry `hidden=True` now. The rest are Rust-only and were never listed.

### Exploratory - channel pub/sub (keep the code, mark experimental)

| Subcommand | State |
|------|-------|
| `register-channel` / `unregister-channel` / `push-channel` / `subscribe` | The intended pub/sub avenue for the MCP-route comms channel (Discord and/or agent-to-agent, each session subscribed to a channel). Zero shell call sites as of this writing and untested end-to-end - deliberate infra, **not dead code**. Rust-only, so already absent from `fno agents --help`. Do not cut. |

A subcommand with zero shell call sites is *not-yet-wired*, not dead: cutting requires proving no shell call site **and** no internal dispatch **and** no forward intent. The channel subcommands fail that test (forward intent is explicit), so they stay.

## The a2a reply contract (x-605c)

Agent-to-agent mail is a protocol, not a transport the caller reasons about. It has exactly two halves:

- **Inbound is self-addressed.** A message lands as `<fno_mail from="H" harness="..." model="...">body`. `H` is the sender's canonical handle (`<harness>-<short8>`), the ONE string its `mail drain-self` cursor also reads.
- **Reply by handle.** Run `fno mail send H "..."`. Never inspect `harness`/`model` to pick a transport; the CLI resolves `H`. The outbound envelope auto-stamps the *invoking* session's own handle + real model (from its transcript store), so the reply is itself self-addressed.

**Resolution ladder** (`discover.resolve_or_suggest`, one scan serving match + suggestions):

1. **fno-agents registry** - a named worker (`x-d899-us8-build`) also answers to its `<provider>-<short8>` handle.
2. **claude daemon roster** (`~/.claude/daemon/roster.json`) - a `claude --bg` worker leaves no pid-sidecar, so the roster is the only source that surfaces it. Presence is enough; the `mail-inject` connect is the authoritative liveness gate.
3. **disk sessions** - live `~/.claude/sessions/<pid>.json` sidecars, else the transcript store.
4. **codex rollouts** - hand-started codex sessions from `~/.codex/sessions`.
5. **durable bus floor** - a handle that resolves but whose live inject misses gets a durable envelope addressed to its canonical handle; the recipient's SessionStart `drain-self` picks it up. The universal floor: no live rung is ever required for a message to eventually land.

A handle-resolved send is session-addressed: live-inject first (claude over `control.sock`, codex over the app-server daemon), durable floor to the canonical handle on a miss. Project anycast is only ever explicit via `--to-project`.

## Why `hide` over an `internal` namespace

Hiding a typer command (`hidden=True`) is one line per subcommand and zero call-site churn - the subcommand still dispatches, it just leaves the `--help` listing. Moving subcommands under an `fno agents internal <subcommand>` namespace would rewrite every hook/script/crate/test call site for no user-visible gain. Live-internal subcommands (`loop-check` and friends the loop depends on) stay reachable; they are simply not advertised.
