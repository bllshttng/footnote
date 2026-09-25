# Agent launcher (the composer: one form, two paints)

One composer launches a new harness session through the canonical spawn door and hands off to the native session. This is a launcher, not a chat client. After a verified birth the pane is focused, and the native harness UI owns everything after that. For a bg thread the roster row is the handoff. The composer has ONE draft, ONE focus order, ONE key rule and ONE hint row, with two paints: the bottom form in full-screen sideline, the centered sheet everywhere else.

## Primary flow

1. Prefix+i, the sideline menu, or `t` on a board card opens the composer. The catalog read arms at attach (a prefetch), not at first open.
2. The chips carry the pick itself, never a field label: `claude-opus-5-5 · high`, `footnote`, `thread`. When nothing can be shown, the chip falls back to its label word (`agent`).
3. Enter on Launch puts ONE typed request (`ClientMsg::AgentLaunch`, v83) on the wire. The button disables. Duplicate submissions are suppressed at the source and at the server desk.
4. The server validates pre-birth: absolute existing project list, supported substrate, message length. It answers duplicates with the SAME attempt and runs exactly one `fno agents spawn` off the core loop. The message rides `--prompt-file -` stdin, never argv.
5. The client receives `ServerMsg::AgentLaunch` progress: `Starting`, then one terminal state (`Launched` / `Refused` / `Unknown`).
6. A verified pane birth focuses the new pane through the existing `FocusPane` command path. The seed is never sent again on attach.

The composer owns every key and every mouse report while it is open: a bare H/J/K/L inside a resize repeat window is text for the draft, never a resize. A wheel or focus report over an open list is consumed, never forwarded. An unknown CSI (a focus report `ESC [ I`) is dropped whole, so nothing wedges the fold and no byte lands as text.

## The key rule

Tab and Shift-Tab move between fields. Enter on a field opens its list. Enter in a list picks. Enter in the message launches. Ctrl-J inserts a newline (a `\n` byte, told apart from Enter's `\r` in the fold). Up and Down move inside a list or between message lines. Esc closes a list, then the composer. The hint row `tab next · ↵ open/launch · ↑↓ choose · esc close` is always painted, above the lifecycle line, and a refusal or `starting...` never replaces it.

## Two paints, one width rule

`form_mode` is a function of the state the paint can check. When the full-screen sideline is on AND one chip row of chosen values fits untruncated in the width it has, the bottom form paints. Everywhere else - the regular 28-column sidebar, the menu, prefix+i with the panel hidden - the composer paints as the centered sheet (width `min(cols - 8, 96)`, shared `Chrome`, title `new agent`). The sheet is an overlay: opening it sends no Resize, so no pane changes width. The in-sidebar 28-column dock reservation is gone. The bottom form keeps its reservation only in full-screen mode.

## The agent chip and its list

The agent chip folds harness, model and effort into one value chip. With a model pinned it reads `claude-opus-5-5 · high`. With none, `claude default`. Its list is Conductor-style: a header per installed native harness, then a `<harness> default` row, then that harness's routing rows, each showing the row name with the row's route as its hint (`zai/glm-5.3-flash[1m]`) and a check glyph on the current pick. Unavailable rows stay greyed with their verdict. Picking a row pins the model and lets the door resolve the row's harness, route and account. Left and Right in the list cycle the effort through the current harness's capability-table list (default first). Typing filters in place. The query shows in the list's title, never as a row. The model free-text row is gone: every launchable model comes from a routing row.

## What the composer owns, and what it does not

- The harness catalog is the compiled-in `harness_capabilities.toml` the spawn door itself enforces, plus a PATH check per name. No UI-only list.
- Model rows come from one bounded read of `fno config route inventory --json` (30s budget, prefetched at attach). While the read runs, the list shows `reading models...`. After a failure it shows the reason as a disabled row. The `<harness> default` rows launch either way, and a failure re-probes on the next open.
- Advanced pins are explicit values only. An empty pin reads as "harness default". The composer never displays an invented resolved value.
- Routing, provider capacity, permission gates and seed acceptance stay with canonical spawn. The composer renders refusals verbatim.
- The current mux session is the only draft memory. Esc hides the composer and retains the draft. If the client exits, the draft is lost.

## Draft and outcome lifetime

- One launch request id equals one spawn attempt, enforced twice. The composer disables the button while an attempt it armed is in flight. The server's launch desk replays the in-flight or finished attempt for a duplicate submission instead of spawning again.
- The client remembers the attempt across close/reopen. A reopened composer cannot silently spawn a replacement. It shows the pending or resolved attempt.
- An `Unknown` outcome leaves birth unresolved. The `cancel` chip (Enter on it returns to editing with the draft intact) resolves it. Retry arms a fresh request id. The draft survives throughout.
- An empty message is an intentionally seedless launch. The spawn door decides whether the harness/substrate combination accepts one, and its refusal is the visible explanation. No fabricated seed.

## Boundaries not crossed

- The composer launches a thread by default, or a pane through the placement chip. Headless workers keep the CLI. Both reuse the same state and render components (`client/agent_launcher.rs`), not a second composer.
- No raw CLI passthrough, crown granting, or permission escalation. No force flags ride the spawn argv. A refusal is the product.

## Tests

- `src/client/tests/agent_launcher_tests.rs`: editor, focus order, draft retention, submit refusals, update correlation, render table, width rule, agent list, hint row. Board prefill: binds message, project and node. A held draft survives. A launched attempt makes way. A message edit drops a stale node binding.
- `src/client/tests/backlog_board_tests.rs`: the `t` key. It prefills from a card, refuses a worked card, works in the drill-down, and keeps a held draft.
- `src/client/tests/agent_launcher_tests.rs`: the width rule, the agent list, the key rule and the sheet paint beside the editor, focus and retention units already pinned there.
- `src/server/tests/agent_launcher_tests.rs`: pre-birth validation and desk dedup/replay at the Core.
- `tests/agent_launcher_journey.rs`: real subprocess boundary with a recording fake door. Pins exact argv, verbatim stdin, refusal and unknown journeys, and no duplicate attempt.
- `tests/agent_launcher_client_e2e.rs`: one real-client test per reported defect. Each was red at the branchpoint and is green on the branch.
