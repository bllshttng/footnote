# Fleet watchdog

`fno agents watchdog` runs outside every session and decides, per fleet row, one of four things: wake it, reroute it, reap it, or leave it. A leg on the `pr_watch` tick can do the same on a cadence behind `config.recovery.watchdog`. The classifier lives in `cli/src/fno/agents/watchdog.py`. It is pure over injected inputs, so tests need no live fleet.

## Why the transcript is the truth source

Both stores lied, in both directions, on 2026-08-15. Eight roster rows claimed `working` while their transcripts had not moved in 30 or more minutes. The fno registry called a working session exited and a removed one live. `claude agents --json` inverted on a capped lane, calling the capped row working and the productive one stopped. The transcript, keyed by session id, was right every time.

The transcript directory name is the launch cwd, so it is never derivable from a repo or worktree name. Resolution goes through `fno.provenance.resolver`, which is content-aware across every project dir. A missing transcript renders as a fact (`ghost`, or an unknown age), never as fresh.

## The decision table

Order is precedence. The top row wins.

| Verdict | Condition | Basis it prints |
|---------|-----------|-----------------|
| `ghost` | state is `working` or `blocked` and no transcript resolves for the row's recorded id | `no transcript for <id>` |
| `reap` | the node is done, or the `node:<id>` claim is held live by a different session | `node <id> done` / `claim held by <other>` |
| `reroute` | state `blocked` and the transcript tail carries a 429 whose reset window has not opened | `429 resets <utc>, <n>m out` |
| `wake` | state `blocked` or `stopped`, a transcript exists, and no live 429 window | `<state> <n>m, last 429 window passed` |
| `leave` | everything else, including every healthy injectable row | `reachable, last turn <n>m ago` |

Reset stamps ride the provider error text in Singapore time, UTC+8. `02:48:21 SGT` is `18:48:21Z`. Two sessions launched at 18:45 and 18:46 took a 429 three minutes before their window opened. When a stamp fails to parse, the window is unknown and the row classifies `leave`, never `wake`. Bouncing a session off a closed window was proved twice by hand and costs a real turn each time.

`reap` reads the deliverable, never the session's own state. A session whose state reads `done` finished a turn and is resumable. On 2026-08-15 a bulk reap that trusted `done` as terminal killed live sessions in another project. The reap conditions are the graph artifacts: the node is done, or another live session owns the node claim.

## The two traps

These were measured by hand on 2026-08-15. Both are pinned by tests in `cli/tests/test_agents_watchdog.py`.

1. Node identity joins on the recorded claim holder and worktree manifest, never on a name regex. Eight auto-named workers read as nobody-on-this-node under a name join and were nearly double-dispatched. Worker names are a convention, not a guarantee.
2. A wake is confirmed by content in the recipient transcript, never by a state field. The scratch `wake.sh` printed `working -> working` for both a message that landed and one that did not. The watchdog calls `fno agents resume` (x-c136), which verifies the state move and holds a single-writer claim. It then separately requires the wake message to appear in the transcript after the pre-wake marker. This mirrors `confirm_content_after` in `crates/fno-agents/src/mail_inject.rs`.

## Lanes

`fno agents watchdog` is a dry run by default and prints every row with its verdict and basis. `--apply` executes the wake lane only, because a wake is the one action that cannot destroy work. `--apply=all` adds reap and reroute, which both stop a session. A ghost never auto-acts at any level: the remedy is a respawn under a new id, and that is the operator's call.

Actions delegate. The watchdog owns the decision, never the mechanism.

| Verdict | Action |
|---------|--------|
| `wake` | `fno agents resume <id>`, then content confirmation in the transcript |
| `reroute` | `fno.recovery._redispatch`: stop first, then respawn in the same worktree. Skip the stop and the old session wakes into a duplicate when the window opens |
| `reap` | refuse when the worktree has uncommitted changes or unpushed commits, naming the count. Then `fno agents stop` and `fno agents rm`, never forced: `claude rm`'s own refusal on a dirty worktree is a safety feature to lean on |
| `ghost` | report only |

A bus-only row (`delivery_policy == "bus-only"`, x-e21e) stays bus-only. It is still eligible for `wake`, because a wake is an attach and a neutral resume, not a paste of a mail body. The wake message for such a row is the bare resume word.

## Cadence

`config.recovery.watchdog` rides the pr_watch tick: `off` (default), `report` (one `watchdog_verdict` event per non-leave row), `wake` (also apply the wake lane). No tick value reaps or reroutes. A destructive action stays behind an operator running `--apply=all` by hand. Every sweep, tick or manual, writes `~/.fno/watchdog-sweep.json` as freshness evidence. Its row lives in `docs/state-root-inventory.md`.

## What this does not replace

`fno.recovery` keeps its own job: provider failover on swap-class deaths and close-surfacing for finished-but-lingering sessions. The watchdog adds the transcript-truth decisions recovery never had: wake on a passed 429 window, reap on settled deliverables, and the ghost flag. `claude_agents_rows` (`--all`) is the one enumeration both read, so stopped rows are never invisible to either.
