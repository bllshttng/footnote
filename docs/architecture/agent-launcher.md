# Agent launcher (the composer: chip row over one input)

One composer launches a new harness session through the canonical spawn door and hands off to the native session. This is a launcher, not a chat client. After a verified birth the pane is focused, and the native harness UI owns everything after that. For a bg thread the roster row is the handoff. The composer has ONE draft, ONE focus cycle, ONE key rule and ONE keybar.

## Primary flow

1. Prefix+i, the sideline menu, or `t` on a board card opens the composer. The catalog read arms at attach (a prefetch), not at first open.
2. The chips carry the pick itself, never a field label. The harness chip reads `claude`. The model chip reads `default` or the pinned row. The Where chip reads `Local`. When a placement is pinned, it reads `Local · <placement>`.
3. Enter in the input puts ONE typed request (`ClientMsg::AgentLaunch`, v83) on the wire. Duplicate submissions are suppressed at the source and at the server desk.
4. The server validates pre-birth: absolute existing project list, supported substrate, message length. It answers duplicates with the SAME attempt and runs exactly one `fno agents spawn` off the core loop. The message rides `--prompt-file -` stdin, never argv.
5. The client receives `ServerMsg::AgentLaunch` progress: `Starting`, then one terminal state (`Launched` / `Refused` / `Unknown`).
6. A verified pane birth focuses the new pane through the existing `FocusPane` command path. The seed is never sent again on attach.

The composer owns every key and every mouse report while it is open. A bare H/J/K/L inside a resize repeat window is text for the draft, never a resize. A wheel or focus report over an open picker is consumed, never forwarded. An unknown CSI (a focus report `ESC [ I`) is dropped whole, so nothing wedges the fold and no byte lands as text.

## The key rule

Tab and Shift-Tab move along the chip cycle. Enter on a chip drops that axis's picker one row under the chip. Enter in a picker picks and closes it. Enter in the input launches. Ctrl-J inserts a newline (a `\n` byte, told apart from Enter's `\r` in the fold). Up/Down move between message lines; Left/Right move the cursor; in the model picker Left/Right cycle the effort. Esc closes a picker, then the composer. The keybar `↵ open/launch · tab next · ^j newline · esc close` is always painted, above the lifecycle line, and a refusal or `starting...` never replaces it.
## The chip row

Top row: Where and Project. The editor (1 to 6 wrapped rows, placeholder `What do you want to work on?`) sits between the rows. Bottom row: `+` and the mode chip on the left; harness, model and effort right-aligned. When the sheet is narrow, the right group wraps to its own row; no chip value is truncated below its full text. A fresh open focuses the input, so typing starts the task at once. While the Project chip is focused or hovered, the line above the chips reads `Working directory` in bold and the full cwd in regular weight. When the sheet cannot admit the path, the path truncates. Chips paint values, never axis names: `Local`, `footnote`, `+`, `auto`, `claude`, `default`, `high`. When the harness's capability row declares no effort axis, the effort chip does not paint. The `+` chip's one-row picker focuses the flags editor in the editor's place; a non-empty flags value shows as one dim line under the input. Chips with a caret carry harness, model, effort, project, mode and placement; the rejected Advanced form stays out.

## What the composer owns, and what it does not

- The harness catalog is the compiled-in `harness_capabilities.toml` the spawn door itself enforces, plus a PATH check per name. No UI-only list.
- Model rows come from one bounded read (30s budget, prefetched at attach). The capability table floors each harness's list, and its first entry leads as the flagship row right under `harness default` with hint `flagship`. Codex tops up from its own models cache. Configured account pins merge over both. Ready rows carry the filled mark, and the list closes with `more…`, which rebuilds the picker over the harness's catalog tail grouped by provider. In the more list every row is enabled: a Ready row picks, a NoKey row shows the hollow mark with the missing env var as the hint, and Enter opens the connect steps (the one `fno config set` line plus the `export`), while an Unreachable row carries the dash and Enter explains the protocol gap; Esc steps back down the ladder (steps -> more -> the main list). A launch on a provider-pinned pick refuses pre-wire naming the env var when its key resolves from neither the env nor `api_key_file`. While the read runs, the picker shows `reading models...`; after a failure it shows the reason as a disabled row. The `<harness> decides` rows launch either way, and a failure re-probes on the next open.
- Advanced pins are explicit values only. An empty pin reads as "harness default". The composer never displays an invented resolved value.
- Routing, provider capacity, permission gates and seed acceptance stay with canonical spawn. The composer renders refusals verbatim.
- The current mux session is the only draft memory. Esc hides the composer and retains the draft. If the client exits, the draft is lost.
## Draft and outcome lifetime

- One launch request id equals one spawn attempt, enforced twice. The composer disables the button while an attempt it armed is in flight. The server's launch desk replays the in-flight or finished attempt for a duplicate submission instead of spawning again.
- The client remembers the attempt across close/reopen. A reopened composer cannot silently spawn a replacement; it shows the pending or resolved attempt.
- An `Unknown` outcome leaves birth unresolved. Esc is the explicit cancel: it resolves back to editing, the draft survives, and retry arms a fresh request id.
- An empty message is an intentionally seedless launch. The spawn door decides whether the harness/substrate combination accepts one, and its refusal is the visible explanation. No fabricated seed.

## Boundaries not crossed

- The composer launches a thread by default, or a pane through the Where chip's placement rows. Headless workers keep the CLI. Both reuse the same state and render components (`client/agent_launcher.rs`), not a second composer.
- No raw CLI passthrough, crown granting, or permission escalation. No force flags ride the spawn argv. A refusal is the product.

## Tests

- `src/client/tests/agent_launcher_tests.rs`: editor units, the chip cycle, draft retention, submit refusals, update correlation, chip labels, chip-row paint, the wrap rule and the cwd line.
- `src/client/tests/backlog_board_tests.rs`: the `t` key. It prefills from a card, refuses a worked card, works in the drill-down, and keeps a held draft.
- `src/server/tests/agent_launcher_tests.rs`: pre-birth validation and desk dedup/replay at the Core.
- `tests/agent_launcher_journey.rs`: real subprocess boundary with a recording fake door. Pins exact argv, verbatim stdin, refusal and unknown journeys, and no duplicate attempt.
- `tests/agent_launcher_client_e2e.rs`: one real-client test per reported defect, plus the chip-row journeys: the sheet paint, the project picker, the model picker, the Where picker, the wrap and the cwd line.
