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

`keeper` is fno owning the PTY. The harness declares `interactive_attach` as `unsupported` while `interactive_resume` declares a working argv: the transcript persists, but no live process does. A thread on such a harness needs fno to hold the terminal for the session's whole life. That lane is not built. A `keeper` answer is therefore an honest refusal naming what is missing, never a verdict that the harness cannot thread.

## Where each harness sits

| Harness | Lane | `thread` row | Why |
|---|---|---|---|
| claude | `attach` | true | Attach form is a subcommand; the session store is the harness's own, and the unattended restart journey passed. |
| codex | `attach` | true | Attach goes through the app-server daemon, which keeps the session alive out of process; the journey passed. |
| agy | `keeper` | false | Resume is a session flag; the live process dies with its terminal, so fno would need to hold the PTY. |
| opencode | `keeper` | false | The serve lane is launch-only; nothing live persists to re-attach to. |
| pi | `keeper` | false | Resume is a session flag; no keeper PTY exists yet. |
| gemini | n/a | false | Refused earlier, at `command_surface`: the harness is deprecated in favor of agy and never reaches the thread gate. Its resume form would otherwise read `keeper`. |

## The gate rule

A `thread` row flips to true only in the same commit as a passing unattended restart journey. The journey is a dispatched worker that resumes into a fresh session, completes its task, and stops on its own. Flip the row early and the honest refusal becomes a spawn that accepts and then fails at launch. Until that commit lands, a false row records an fno backlog item. It never records a harness verdict.
