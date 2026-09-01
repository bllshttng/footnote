# Thread lanes

A thread is fno's own persistent session lane: fno can put a worker on a live session by id, end the client, and re-attach later. Whether a harness can take that lane is not a property of the harness. It is a property of what fno has built for it. This page is the rule the code follows, the measured lane table, and the gate that keeps the table honest.

## The selector

Threadability is derived, never named. The selector is one field in the packaged capability contract, `cli/src/fno/agents/harness_capabilities.toml`: `resume_strategy.forms`. Both runtimes resolve the lane from it with the same three branches. Neither resolver contains a harness name, so a new row lands in its lane with no code edit.

- `interactive_attach` declares a form: the lane is `attach`.
- Otherwise `interactive_resume` declares a form: the lane is `keeper`.
- Otherwise: `none`, and no lane can be built.

The resolvers are `thread_lane` in `cli/src/fno/agents/harness_map.py` and `HarnessContract::thread_lane` in `crates/fno-agents/src/harness_capabilities.rs`. They are kept identical the way loop participation is pinned across the two runtimes: one contract, two readers, no room to disagree.

## The two lanes

`attach` is the harness owning the session store. Its `interactive_attach` form declares the argv a client uses to re-enter a live session that survives out of process. fno renders that argv, joins the session, and the harness keeps it alive.

`keeper` is fno owning the PTY. The harness declares `interactive_attach` as `unsupported` while `interactive_resume` declares a working argv: the transcript persists, but no live process does. A thread on such a harness needs fno to hold the terminal for the session's whole life. For pi that lane is hosted: a pane-less keeper process (`fno-agents-worker --keeper`, `crates/fno-agents/src/pane_keeper.rs`) holds the pty master, mail reaches the thread through the keeper's Input frames, and stop/rm deliver the Kill frame down the row's own socket. What is NOT yet proven is the DRIVING half: pi's TUI accepts pasted text in a raw pty but does not submit it on Enter there, while the same bytes submit in tmux (x-61bc run of record, pi 0.84.2/0.84.3). Until that gap closes, a keeper-hosted pi is a live thread no one can drive by mail.

One invariant governs the daemon's role, because the epic prose once said otherwise: the daemon does not HOST keepers, it DISCOVERS and REBINDS them. The keeper is a separate process whose parent is launchd, never the daemon; `handle_spawn` in `crates/fno-agents/src/daemon.rs` still states that the daemon hosts no agent PTYs, and the daemon-start sweep only walks existing keeper sockets and re-binds survivors to their registry rows.

A `keeper` harness with no built lane still gets an honest refusal naming what is missing, never a verdict that the harness cannot thread.

## Where each harness sits

| Harness | Lane | `thread` row | Why |
|---|---|---|---|
| claude | `attach` | true | Attach form is a subcommand; the session store is the harness's own, and the unattended restart journey passed. |
| codex | `attach` | true | Attach goes through the app-server daemon, which keeps the session alive out of process; the journey passed. |
| agy | `keeper` | false | Resume is a session flag whose session binding strategy is `unsupported`, so identity cannot be preserved across a restart; the keeper lane needs fno to hold the PTY, which is built, but the pi-grade journey cannot pass on agy until binding works. |
| opencode | `keeper` | false | The serve lane is launch-only, and its session binding is `store-lookup` with `required = false`, which races the store write; the keeper PTY itself is built and reusable. |
| pi | `keeper` | false | The keeper PTY is built and the row is one journey away from true. `cli/tests/agents/test_thread_keeper_journey.py` (opt-in `FNO_PI_LIVE=1`) runs the seven-step restart journey: both supervisors killed with SIGKILL, the recorded child pid surviving under the same keeper, cwd/session id/session store unchanged. Steps 1-6 pass; the journey is RED on the conversation step - pi's TUI does not submit pasted text on Enter outside tmux (x-61bc run of record) - so the gate stays shut and the row stays false. |
| gemini | n/a | false | Refused earlier, at `command_surface`: the harness is deprecated in favor of agy and never reaches the thread gate. Its resume form would otherwise read `keeper`. |

## The gate rule

A `thread` row flips to true only in the same commit as a passing unattended restart journey. The journey is a dispatched worker that resumes into a fresh session, completes its task, and stops on its own; for a keeper lane it is the restart journey above, asserting a named pid outlived a named death. Flip the row early and the honest refusal becomes a spawn that accepts and then fails at launch. Until that commit lands, a false row records an fno backlog item. It never records a harness verdict.
