# Agent launcher (sideline new-agent popup)

One popup in the mux sideline launches a new harness session through the canonical spawn door and hands off to the native session. This is a launcher, not a chat client. After a verified birth the pane is focused, and the native harness UI owns everything after that. For a bg thread the roster row is the handoff.

## Primary flow

1. Sideline menu, `new agent`: opens the centered popup and arms the harness catalog read.
2. The popup shows harness, project, an optional multiline message, and an `advanced` section (model, effort, permissions, placement).
3. Enter on Launch puts ONE typed request (`ClientMsg::AgentLaunch`, v83) on the wire. The button disables. Duplicate submissions are suppressed at the source and at the server desk.
4. The server validates pre-birth: absolute existing project path, supported substrate, message length. It answers duplicates with the SAME attempt and runs exactly one `fno agents spawn` off the core loop. The message rides `--prompt-file -` stdin, never argv.
5. The client receives `ServerMsg::AgentLaunch` progress: `Starting`, then one terminal state (`Launched` / `Refused` / `Unknown`).
6. A verified pane birth focuses the new pane through the existing `FocusPane` command path. The seed is never sent again on attach.

## What the popup owns, and what it does not

- The harness catalog is the compiled-in `harness_capabilities.toml` the spawn door itself enforces, plus a PATH check per name. No UI-only list.
- Advanced pins are explicit values only. An empty pin reads as "harness default". The popup never displays an invented resolved value.
- Routing, provider capacity, permission gates and seed acceptance stay with canonical spawn. The popup renders refusals verbatim.
- The current mux session is the only draft memory. Esc hides the popup and retains the draft. If the client exits, the draft is lost.

## Draft and outcome lifetime

- One launch request id equals one spawn attempt, enforced twice. The popup disables the button while an attempt it armed is in flight. The server's launch desk replays the in-flight or finished attempt for a duplicate submission instead of spawning again.
- The client remembers the attempt across popup close/reopen. A reopened popup cannot silently spawn a replacement. It shows the pending or resolved attempt.
- An `Unknown` outcome (timeout, unreadable receipt, recovery-required) leaves birth unresolved. Retry stays blocked until the operator acknowledges the outcome. The draft survives throughout.
- An empty message is an intentionally seedless launch. The spawn door decides whether the harness/substrate combination accepts one, and its refusal is the visible explanation. No fabricated seed.

## Boundaries not crossed in v1

- The popup launches pane-hosted sessions only. bg threads and headless workers keep their existing surfaces (roster + row menu / CLI).
- Docked full-screen placement is a later reuse of the same state and render components (`client/agent_launcher.rs`), not a second composer.
- No raw CLI passthrough, crown granting, or permission escalation. No force flags ride the spawn argv. A refusal is the product.

## Tests

- `src/client/tests/agent_launcher_tests.rs`: editor, focus order, draft retention, submit refusals, update correlation, render table.
- `src/server/tests/agent_launcher_tests.rs`: pre-birth validation and desk dedup/replay at the Core.
- `tests/agent_launcher_journey.rs`: real subprocess boundary with a recording fake door. Pins exact argv, verbatim stdin, refusal and unknown journeys, and no duplicate attempt.
