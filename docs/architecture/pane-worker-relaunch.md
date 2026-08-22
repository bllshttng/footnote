# Pane worker relaunch

A worker pane (codex, agy, a pane-hosted claude) survives a mux restart as an idle row in the agent panel. Selecting that row resumes the session through the harness's own resume form. Nothing respawns silently at start. This is the record and the mechanism, and the reason respawn is refused.

## The two resume mechanisms

`claude attach <jobId>` reconnects a viewer to a session that is still running. A claude bg session is owned by claude's daemon, and the daemon kept it alive. `codex resume <session_id>` restarts a session from a rollout persisted on disk. Different mechanisms, the same user-visible result, and only the first needs a living process. "Codex has no daemon" is true and irrelevant.

Each harness owns its own session truth. fno's job is to remember WHICH session and ask the harness to bring it back, never to host ptys for harnesses that lack a daemon. An fno-owned detached pty host was proposed and withdrawn: it is a much larger build, and resume does not need it.

## The record is one field

`~/.fno/squads.json` stored a member as `{"attach_id": "<8hex claude jobId>"}`. A codex worker has no claude jobId. It was unrepresentable rather than unrestored, and `valid_attach_id` dropped anything that was not 8 hex digits at load.

A member now carries `worker`: the registry name of the worker pane. That one field is the join key. The registry row (`~/.fno/agents/registry.json`) already holds `harness`, `harness_session_id`, `cwd`, and `name`. No other member fields exist and none are needed. Duplicating argv, model, or account onto the member creates a second copy that drifts, and the drifted copy is the one that runs.

Capture sits on the server, in the `PaneRun` handler behind `fno mux pane run --worker <name>`. Every pane producer crosses that one operation. The Python spawn lane (`dispatch_spawn_pane`) passes the flag, and so does the dispatch porcelain the TUI's work-queue cards shell. A guard on one caller of N is decorative.

### Why three of four squads held zero members

Membership meant "a claude agent row was attached here", not "a worker pane lives here". Every write to `attached` followed an `attach_argv` spawn, so a pane created by `pane run` never became a `StoredMember`. Squads full of codex and agy workers persisted with `members: []` correctly, by the rules the code held. Capture was not broken. It measured a different thing than the operator expected. The `--worker` funnel is the fix.

## Restore never spawns a worker member

A worker pane's pty was a child of the previous mux server pid and died with it. After a restart such a member is always dead. No liveness probe runs, because probing an already-known answer is the receipt-can-lie shape. No process starts either.

The member stays as-is and the row renders idle in the agent panel. Restore also prints one notice naming how many idle worker rows it created. That makes "nothing resumed" distinguishable from "the counter never ran".

Restore also prunes a worker member whose registry row is gone (an `fno agents rm`, a GC pass): a name that can never resume is dead weight, not a card, and each restart names the prune count in a notice. A name that still exists stays, exited or not.

Claude attach members restore exactly as before: `claude attach` in the recorded cwd, because the daemon still owns those sessions.

## The resume gesture

Selecting an idle row sends `Command::ResumeAgent { name }`. The server joins the name to its registry row, then spawns the harness's own form in a pane at the row's recorded cwd. When the recorded directory is gone, the pane lands in the sender's squad cwd and a notice names both paths:

| Harness | Resume argv |
|---------|-------------|
| claude (pane-hosted, or dead bg row) | `claude --resume <harness_session_id>` |
| codex | `codex resume <harness_session_id>` |
| anything else | no Resume offered |

Only a DEAD row offers Resume. A live row has a process writing its session state, and resuming under it would open a second writer on the same rollout. A live claude bg row with a jobId attaches instead: the daemon owns the session, and that gesture already exists. A harness with no resume form here (agy has none verified) offers no Resume button at all. A button that fails is worse than an honest dead row. The session id is always a positional argument, never a shell string. `worker` names are validated to the registry slug charset at load (`valid_worker_name`) before any resume can key on them.

The resumed pane is placed in the squad that holds the worker's recorded membership, falling back to the squad owning its cwd. The two tokens the server builds are pinned against `harness_capabilities.toml` by a test that reads the toml, so the Rust mirror cannot drift from the file that owns it. The pane is titled from the registry row and recorded as a worker member again, so it survives the NEXT restart too.

## Files

- `crates/fno/src/squad_store.rs` - `StoredMember.worker`, `valid_worker_name`, the load gate
- `crates/fno/src/server.rs` - capture in `run_pane` (`record_worker_member`), the idle branch in `restore_squads`, the `ResumeAgent` handler, `row_resumable`
- `crates/fno/src/mux_cli.rs` - `pane run --worker` parsing and help
- `crates/fno/src/agents_view.rs` - `RegistryAgent.harness`
- `crates/fno/src/proto.rs` - `Command::ResumeAgent`, `AgentRow.resumable` (v49)
- `cli/src/fno/agents/mux_spawn.py` - passes `--worker <name>` on the pane run argv
