# Agent launcher (sideline new-agent dock)

One dock in the mux sideline launches a new harness session through the canonical spawn door and hands off to the native session. This is a launcher, not a chat client. After a verified birth the pane is focused, and the native harness UI owns everything after that. For a bg thread the roster row is the handoff.

## Primary flow

1. Sideline menu, `new agent`: opens the dock at the bottom of the sideline column and arms the harness catalog read.
2. The dock shows harness, project, a dynamic-height message editor, and an `advanced` section (model, effort, permissions, placement).
3. Enter on Launch puts ONE typed request (`ClientMsg::AgentLaunch`, v83) on the wire. The button disables. Duplicate submissions are suppressed at the source and at the server desk.
4. The server validates pre-birth: absolute existing project path, supported substrate, message length. It answers duplicates with the SAME attempt and runs exactly one `fno agents spawn` off the core loop. The message rides `--prompt-file -` stdin, never argv.
5. The client receives `ServerMsg::AgentLaunch` progress: `Starting`, then one terminal state (`Launched` / `Refused` / `Unknown`).
6. A verified pane birth focuses the new pane through the existing `FocusPane` command path. The seed is never sent again on attach.

## Launching a backlog node

`t` on a board card or in the drill-down opens the dock prefilled with `/fno:target <id>` and the node's project. The launch is bound to the node with `--node <id>`. The door's dispatch guard judges the node, and the worker joins its roster row and card. The operator picks harness, model and effort. Nothing spawns before the Launch press. A card already being worked refuses before the dock opens. A kept draft is never overwritten.

## Dock geometry

- The dock pins to the bottom of the sideline column, above the bottom chrome row. It reserves rows the way the court block does. While it is open the passive court block yields: an active editor outranks glance chrome.
- The message editor is dynamic. It shows the typed lines up to a cap near half the panel. Deleting lines shrinks it. On a tiny panel the editor floors at one line and the painter clips. The dock never hides while open.
- The dock is live UI, not chrome: plain cells, no DIM, no frame. The footer line carries the launch lifecycle (starting, refusal reason, seed note).
- While the dock is open, keys stay with the composer. Presses outside the dock are unconsumed: the roster above and the panes keep their normal mouse behavior.

## What the dock owns, and what it does not

- The harness catalog is the compiled-in `harness_capabilities.toml` the spawn door itself enforces, plus a PATH check per name. No UI-only list.
- Advanced pins are explicit values only. An empty pin reads as "harness default". The dock never displays an invented resolved value.
- Routing, provider capacity, permission gates and seed acceptance stay with canonical spawn. The dock renders refusals verbatim.
- The current mux session is the only draft memory. Esc hides the dock and retains the draft. If the client exits, the draft is lost.

## Draft and outcome lifetime

- One launch request id equals one spawn attempt, enforced twice. The dock disables the button while an attempt it armed is in flight. The server's launch desk replays the in-flight or finished attempt for a duplicate submission instead of spawning again.
- The client remembers the attempt across dock close/reopen. A reopened dock cannot silently spawn a replacement. It shows the pending or resolved attempt.
- An `Unknown` outcome (timeout, unreadable receipt, recovery-required) leaves birth unresolved. Retry stays blocked until the operator acknowledges the outcome. The draft survives throughout.
- An empty message is an intentionally seedless launch. The spawn door decides whether the harness/substrate combination accepts one, and its refusal is the visible explanation. No fabricated seed.

## Boundaries not crossed

- The dock launches a thread by default, or a pane through the placement chip. Headless workers keep the CLI. Both reuse the same state and render components (`client/agent_launcher.rs`), not a second composer.
- No raw CLI passthrough, crown granting, or permission escalation. No force flags ride the spawn argv. A refusal is the product.

## Tests

- `src/client/tests/agent_launcher_tests.rs`: editor, focus order, draft retention, submit refusals, update correlation, render table, dock geometry. Board prefill: binds message, project and node. A held draft survives. A launched attempt makes way. A message edit drops a stale node binding.
- `src/client/tests/backlog_board_tests.rs`: the `t` key. It prefills from a card, refuses a worked card, works in the drill-down, and keeps a held draft.
- `src/server/tests/agent_launcher_tests.rs`: pre-birth validation and desk dedup/replay at the Core.
- `tests/agent_launcher_journey.rs`: real subprocess boundary with a recording fake door. Pins exact argv, verbatim stdin, refusal and unknown journeys, and no duplicate attempt.
