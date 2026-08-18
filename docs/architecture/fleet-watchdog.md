# Fleet watchdog

`fno agents watchdog` runs outside every session and decides, per fleet row, one of four things: wake it, reroute it, reap it, or leave it. A leg on the `pr_watch` tick can do the same on a cadence behind `config.recovery.watchdog`. The classifier lives in `cli/src/fno/agents/watchdog.py`. It is pure over injected inputs, so tests need no live fleet.

## Why the transcript is the truth source

Both stores lied, in both directions, on 2026-08-15. Eight roster rows claimed `working` while their transcripts had not moved in 30 or more minutes. The fno registry called a working session exited and a removed one live. `claude agents --json` inverted on a capped lane, calling the capped row working and the productive one stopped. The transcript, keyed by session id, was right every time.

The transcript directory name is the launch cwd, so it is never derivable from a repo or worktree name. Resolution goes through `fno.provenance.resolver`, which is content-aware across every project dir. A missing transcript renders as a fact (`ghost`, or an unknown age), never as fresh.

## The decision table

Order is precedence. The top row wins.

| Verdict | Condition | Basis it prints |
|---------|-----------|-----------------|
| `ghost` | state is `working`, `busy`, or `blocked` and no transcript resolves for the row's recorded id | `no transcript for <id>` |
| `reap` | the node is done, or the `node:<id>` claim is held live by a different session | `node <id> done` / `claim held by <other>` |
| `stale` | a wake-state row whose last transcript event is older than the 1d ceiling | `<state> <n>d old, past the 1d wake ceiling, needs a human` |
| `reroute` | state `blocked` and the transcript tail carries a 429 whose reset window has not opened | `429 resets <utc>, <n>m out` |
| `wake` | state `blocked` or `stopped`, a parseable last event under the ceiling, a tail that positively owes its next move, and no live 429 window | `<state> <n>m silent, last 429 window passed` |
| `leave` | everything else, including every healthy injectable row | `reachable, last turn <n>m ago` |

`stale` is the needs-human bucket. It is checked before the 429 window math on purpose. The reset stamp carries no date, so on a tail older than the ceiling its time-of-day reading is garbage. That reading must not poison reroute. A session stopped for two months has a dead node, a stale branch, and a context describing a repository that has moved. Waking it is not recovery. `stale` never auto-acts at any apply level.

Every wake condition is positive evidence. The last event parses. The age sits under the ceiling. And the shipped classifier (`session_truth.classify_tail`) reads the tail as `stalled`: silent while still owing its next move. "No 429 in tail" is an absence and never a wake reason, and a tail with no parseable evidence reads leave, never an action lane.

Reset stamps ride the provider error text in Singapore time, UTC+8. `02:48:21 SGT` is `18:48:21Z`. Two sessions launched at 18:45 and 18:46 took a 429 three minutes before their window opened. When a stamp fails to parse, the window is unknown and the row classifies `leave`, never `wake`. Bouncing a session off a closed window was proved twice by hand and costs a real turn each time.

`reap` is the one verdict that must satisfy all three signals. The basis says the process is real. The last event says what it is doing now. The node says whether its old task finished. A session whose state reads `done` finished a turn and is resumable. A done node proves the old task ended. It proves nothing about re-tasking: an operator mail can hand a worker new work after its PR merges. The 2026-08-15 bulk reap that trusted `done` as terminal killed live sessions. If a done-node row's transcript has gone quiet past the idle threshold and its last event was not a tool call, it reaps. A row executing a tool never reaps.

## The two traps

These were measured by hand on 2026-08-15. Both are pinned by tests in `cli/tests/test_agents_watchdog.py`.

1. Node identity joins on recorded identity only, never a name regex. The worktree manifest comes first. The execution ledger is the fallback. Its rows are machine-written and map each claude session to its node. That covers a worker that ran in the canonical checkout and has no manifest of its own. Eight auto-named workers read as nobody-on-this-node under a name join and were nearly double-dispatched. The operator fleet's `t-` shorthand strips the node's dash, so no regex can find the slug boundary. Worker names are a convention, not a guarantee.
2. A wake is confirmed by content in the recipient transcript, never by a state field. The scratch `wake.sh` printed `working -> working` for both a message that landed and one that did not. The watchdog calls `fno agents resume`, which verifies the state move and holds a single-writer claim. It then separately requires the wake message to appear in the transcript after the pre-wake marker. This mirrors `confirm_content_after` in `crates/fno-agents/src/mail_inject.rs`.

## Lanes

`fno agents watchdog` is a dry run by default and prints every row with its verdict and basis. `--apply` executes the wake lane only, because a wake is the one action that cannot destroy work. `--apply-all` adds reap and reroute, which both stop a session. A ghost never auto-acts at any level: the remedy is a respawn under a new id, and that is the operator's call.

Actions delegate. The watchdog owns the decision, never the mechanism.

| Verdict | Action |
|---------|--------|
| `wake` | `fno agents resume <id>`, then content confirmation in the transcript |
| `reroute` | `fno.recovery._default_failover`: rotate the provider, stop first, then respawn in the same worktree. A bare redispatch would respawn onto the same capped account, so with no alternate armed the lane refuses and names the outcome rather than looping the fleet on the dead account |
| `reap` | refuse when the worktree has uncommitted changes or unpushed commits, naming the count. Then `fno agents stop` and `fno agents rm`, never forced: `claude rm`'s own refusal on a dirty worktree is a safety feature to lean on |
| `ghost` | report only |

A bus-only row stays bus-only. Every row is eligible for `wake`, because a wake is an attach and a neutral resume, not a paste of a mail body. The wake message is always the bare resume word, never a mail payload.

## Cadence

`config.recovery.watchdog` rides the pr_watch tick: `off` (default), `report` (one `watchdog_verdict` event per non-leave row), `wake` (also apply the wake lane). No tick value reaps or reroutes. A destructive action stays behind an operator running `--apply-all` by hand. Every sweep, tick or manual, writes `~/.fno/watchdog-sweep.json` as freshness evidence. Its row lives in `docs/state-root-inventory.md`.

A sweep that reads zero rows refuses, and the refusal writes nothing. A binary update once made the roster read 0 rows against an intact 19-row registry file. A zero-row sweep writes `counts={}` with a fresh mtime, and that reads as a healthy quiet fleet. An empty fleet and a broken instrument must never produce the same output. The refusal starves the sweep file, so staleness reads loud within two ticks and never certifies a fleet that was not read.

The most common way to read zero rows is the enumeration budget, not a broken binary. `claude agents --json --all` is a fleet-wide live-status probe, and it grows with the fleet: 3.4s on 43 rows against the 3.0s interactive default that `claude_agents_rows` ships. That default timed out on an operator's own `fno-agents resume` calls for a whole night before anyone read the fallback line as a defect. The sweep buys its own 30s budget (`ROSTER_TIMEOUT_S`) because no human waits on a tick, and it warns once it spends half of that. A fixed budget against a growing fleet is a bug that gets worse with success. The approach to the line has to speak before the line is crossed.

## Push, not pull

A verdict the king has to remember to fetch goes unread. When `config.recovery.watchdog_mail_to` names a handle (or `--mail <handle>` is passed), the sweep mails a one-screen digest of the non-leave verdicts with their basis strings. The digest is gated on change. The signature of the non-leave set rides in the sweep file, and an unchanged signature sends nothing. A row stuck for a day reads once, not on every tick. A `project:<slug>` recipient addresses the project mailbox instead of one agent.

## Two row sources, one truth

The sweep enumerates from `claude agents --json --all` and joins registry identity where it exists, so it counts MORE rows than `fno agents list`. On 2026-08-17 the sweep saw 38 rows where the roster returned 23, and most named wake rows were absent from the roster. That gap is expected, not a bug. Claude's supervisor view holds every claude bg session on the machine. That includes hand-started `claude --bg` threads, restored rows, and sessions other tools launched. The registry holds only fno-spawned workers and goes stale in both directions. It called a working session exited and a removed one live on 2026-08-15. The watchdog reads the superset on purpose. A non-fno session burning a gigabyte is still a fleet fact. Both stores are hints over the transcript.

## What this does not replace

`fno.recovery` keeps its own job: provider failover on swap-class deaths and close-surfacing for finished-but-lingering sessions. The watchdog adds the transcript-truth decisions recovery never had: wake on a passed 429 window, reap on settled deliverables, and the ghost flag. `claude_agents_rows` (`--all`) is the one enumeration both read, so stopped rows are never invisible to either. A future port into the Rust daemon's `gc_sweep_impl` is filed as a dedicated follow-up node. It is blocked by the daemon lazy-start leak, because a second cadence can triple the verdicts and the mail.
