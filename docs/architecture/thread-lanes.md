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

The `attach` lane has two destinations. The lane alone cannot pick between them. Both claude and codex answer `attach`, because both declare an `interactive_attach` form. Their sessions are owned by different processes. The discriminator is `pre_exec` on that same form, and it needs no new field. `pre_exec` names the process that keeps a session alive with no client attached.

| Harness | `interactive_attach` | Who owns the live session |
|---|---|---|
| codex | `pre_exec = ["codex","app-server","daemon","start"]`, then `codex resume {session_id} --remote unix://` | a shared harness-owned server, started outside the spawn |
| claude | no `pre_exec`, `claude attach {short_id}` | the detached client process itself |

A non-empty `pre_exec` means the daemon ensures the harness's own server and delegates to it. An empty `pre_exec` means the session lives in the spawned client. The daemon does not host that client. A thread spawn for such a harness is refused there, with a pointer at the client-side lane. `handle_spawn` in `crates/fno-agents/src/daemon.rs` routes on `thread_lane` and then `attach_needs_server`, never on a harness name.

That refusing arm is the reason the split is written down. A route that tested the lane alone sends a claude thread spawn into codex's app-server, because both read `attach`. No claude thread spawn reaches the daemon today. The arm guards the next attach-lane harness rather than fixing a live misroute.

`keeper` is fno owning the PTY. The harness declares `interactive_attach` as `unsupported` while `interactive_resume` declares a working argv: the transcript persists, but no live process does. A thread on such a harness needs fno to hold the terminal for the session's whole life. For pi that lane is hosted and driven: a pane-less keeper process (`fno-agents-worker --keeper`, `crates/fno-agents/src/pane_keeper.rs`) holds the pty master, mail reaches the thread through the keeper's Input frames (the payload as a bracketed paste, then the submit CR as its own write), and stop/rm deliver the Kill frame down the row's own socket. The seven-step restart journey passed end to end on pi 0.84.2: with the mux server and `fno-agents-daemon` both SIGKILLed, the keeper-hosted child kept its pid, cwd and session id, the daemon-start sweep re-bound the same registry row on restart, the session store gained no file, and a second prompt through mail was answered from the first turn's memory (`cli/tests/agents/test_thread_keeper_journey.py`, opt-in `FNO_PI_LIVE=1`).

One invariant governs the daemon's role, because the epic prose once said otherwise: the daemon does not HOST keepers, it DISCOVERS and REBINDS them. The keeper is a separate process whose parent is launchd, never the daemon; `handle_spawn` in `crates/fno-agents/src/daemon.rs` still states that the daemon hosts no agent PTYs, and the daemon-start sweep only walks existing keeper sockets and re-binds survivors to their registry rows.

A `keeper` harness with no built lane still gets an honest refusal naming what is missing, never a verdict that the harness cannot thread.

## Where each harness sits

| Harness | Lane | `thread` row | Why |
|---|---|---|---|
| claude | `attach` | true | Attach form is a subcommand; the session store is the harness's own, and the unattended restart journey passed. |
| codex | `attach` | true | Attach goes through the app-server daemon, which keeps the session alive out of process; the journey passed. |
| agy | `keeper` | false | Resume is a session flag whose session binding strategy is `unsupported`, so identity cannot be preserved across a restart; the keeper lane needs fno to hold the PTY, which is built, but the pi-grade journey cannot pass on agy until binding works. |
| opencode | `keeper` | false | The serve lane is launch-only, and its session binding is `store-lookup` with `required = false`, which races the store write; the keeper PTY itself is built and reusable. |
| pi | `keeper` | true | The keeper PTY is built and the seven-step restart journey passed: both supervisors SIGKILLed, the recorded child pid surviving under the same keeper, cwd/session id/session store unchanged, the second mail prompt answered (`cli/tests/agents/test_thread_keeper_journey.py`, opt-in `FNO_PI_LIVE=1`). The bit backs the thread lane a one-shot dispatch resolves onto; the autonomous `/target` template still refuses at the loop gate until pi's loop extension ships, and the spawn arm is still unbuilt, so `fno agents spawn --substrate thread` refuses pi until that arm ships with its own journey. |
| gemini | n/a | false | Refused earlier, at `command_surface`: the harness is deprecated in favor of agy and never reaches the thread gate. Its resume form would otherwise read `keeper`. |

## The gate rule

A `thread` row flips to true only in the same commit as a passing unattended restart journey. The journey is a dispatched worker that resumes into a fresh session, completes its task, and stops on its own; for a keeper lane it is the restart journey above, asserting a named pid outlived a named death. Flip the row early and the honest refusal becomes a spawn that accepts and then fails at launch. Until that commit lands, a false row records an fno backlog item. It never records a harness verdict.
