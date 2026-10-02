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

Tab and Shift-Tab move along the chip cycle. Enter on a chip drops that axis's picker one row under the chip. Enter in a picker picks and closes it. Enter in the input launches. Ctrl-J inserts a newline (a `\n` byte, told apart from Enter's `\r` in the fold). When the terminal spells it (`CSI 13;2u`), Shift+Enter launches with force. Ctrl-U or Cmd+Backspace deletes everything left of the cursor on the current row. The fold maps both spellings (`0x15` and `ESC DEL`) to one key, and every fno text input honors it. Up/Down move between message lines. Left/Right move the cursor. In the model picker Left/Right cycle the effort. `?` on an empty input opens the composer help sheet, which lists the whole grammar. Esc closes it. Ctrl-O toggles a refusal's raw text under its one-sentence head. Esc closes a picker, then the composer. The keybar `↵ open/launch · tab next · ^j newline · esc close` is always painted, above the lifecycle line, and a refusal or `starting...` never replaces it.
## The chip row

Top row: Where and Directory. The editor (1 to 6 wrapped rows) sits between the rows, and the pills row sits between the editor and the bottom row. On an empty input a dim placeholder names the grammar: `prompt · -- flags · @ node · ! shell · ? help`. It vanishes at the first keystroke. Bottom row: the mode chip sits on the left. Harness, model and effort right-align. When the sheet is narrow, the right group wraps to its own row, and no chip value is truncated below its full text. A fresh open focuses the input, so typing starts the task at once. While the Directory chip is focused or hovered, the line above the chips reads `Working directory` in bold and the full cwd in regular weight. When the sheet cannot admit the path, the path truncates. Chips paint values, never axis names: `Local`, `footnote`, `main`, `auto`, `claude`, `default`, `high`. When the harness's capability row declares no effort axis, the effort chip does not paint. Chips with a caret carry harness, model, effort, project, mode and placement. The rejected Advanced form stays out.

## Typed flags are pills

Any flag beyond the chips is typed, never a form. Typing `--` at a word start in the message opens the flags picker. Its rows come from the installed harness's own `--help`, read at runtime and cached per binary version (`--version` output is the version). Each row carries the flag's one-line description. When the read fails or the binary is absent, the static capability-table capture stays as the offline fallback. A `fno spawn options` section sits above the harness rows, labelled apart: `--force`, `--name`, `--account` (substrate and effort live on their chips). Picking a flag removes the typed text and adds a pill. A flag that takes a value takes the next word you type as that value (Space or Enter finalizes it). Space after an unlisted `--word` commits it verbatim as a pill and it passes through to the harness untouched. `--model`, `--effort` and `--harness` pin their chip instead of storing a pill. Pills paint as one row between the input and the bottom row, each with a clickable `x`. Backspace at an empty message start removes the last pill. At submit every pill joins `extra_flags` in order, one argv element per flag and one per value. The wire door's own validation runs on them, so a chip-owned flag such as `--cwd` refuses exactly as before.

## Shell lines, force, and how refusals read

A `!` on an empty input turns the composer into a one-line shell prompt. The line is the user's own typed command. Its pane run rides the same human admission exemption the user's own attach carries (`PaneRun.human`, v99). The runaway brake warns instead of refusing, and the fleet census still gates. A `!` line never needs force.

A refusal shows as one sentence naming the limit and the way out, for example `process limit: machine runaway brake on`. The raw gate text is never painted mid-word. Ctrl-O swaps the raw text in, and the journal row (`composer_shell_refused`, `composer_launch_forced`) always carries it.

Force is a deliberate user gesture. Shift+enter arms it, and so does the `--force` row in the picker's fno section. The armed state shows as a `--force` pill. The request carries `force` once (v100), and the pill clears when a launch actually rides it; a refusal keeps it for the retry. The server journals `composer_launch_forced` with the user as the actor. It runs the door with `--force` and admits the door child under the human exemption. Cap, RAM floor and the CPU ceiling stand down. The provider cap and the blueprint axis stay enforced. The armed state clears on submit.

## What the composer owns, and what it does not

- The harness catalog is the compiled-in `harness_capabilities.toml` the spawn door itself enforces, plus a PATH check per name. No UI-only list.
- Model rows come from one bounded read (30s budget, prefetched at attach). The capability table floors each harness's list. Its first entry leads as the flagship row under `harness default`, hint `flagship`. Codex tops up from its own models cache. Configured account pins merge over both. Ready rows carry the filled mark. The list closes with `more…`. It rebuilds the picker over the catalog tail, grouped by provider. Every more row is enabled. A Ready row picks. A NoKey row shows the hollow mark and names the missing env var. Enter opens its connect steps: the `fno config set` line plus the `export`. An Unreachable row carries the dash. Enter explains the protocol gap. Esc steps back: steps -> more -> the main list. A launch checks the pinned row's key: env var first, then `api_key_file`. Nothing resolving means a pre-wire refusal naming the env var. While the read runs, the picker shows `reading models...`. After a failure it shows the reason as a disabled row. The `<harness> decides` rows launch either way. A failure re-probes on the next open.
- Advanced pins are explicit values only. An empty pin reads as "harness default". The composer never displays an invented resolved value.
- Routing, provider capacity, permission gates and seed acceptance stay with canonical spawn. The composer renders refusals verbatim.
- The current mux session is the only draft memory. Esc hides the composer and retains the draft. If the client exits, the draft is lost.
## Draft and outcome lifetime

- One launch request id equals one spawn attempt, enforced twice. The composer disables the button while an attempt it armed is in flight. The server's launch desk replays the in-flight or finished attempt for a duplicate submission instead of spawning again.
- The client remembers the attempt across close/reopen. A reopened composer cannot silently spawn a replacement; it shows the pending or resolved attempt.
- An `Unknown` outcome leaves birth unresolved. Esc is the explicit cancel. It resolves back to editing, the draft survives, and retry arms a fresh request id. A timed-out launch no longer stops at "may have been born". A follow-up registry read names the worker born during the window, or reports a clean read with no birth. An unreadable registry says the read failed and stays unresolved. An admission refusal at the child spawn surfaces as a refusal with its reason, never as a fake timeout.
- The editor cursor paints with the theme's cursor surface (`Role::BodyCursor`), the role the popups' input fields already use. It is no longer a default-styled bar.
- An empty message is an intentionally seedless launch. The spawn door decides whether the harness/substrate combination accepts one, and its refusal is the visible explanation. No fabricated seed.

## Boundaries not crossed

- The composer launches a thread by default, or a pane through the Where chip's placement rows. Headless workers keep the CLI. Both reuse the same state and render components (`client/agent_launcher.rs`), not a second composer.
- No raw CLI passthrough, crown granting, or permission escalation. Force exists only as the explicit user gesture above. The request field, not a free argv token, carries it. The override is journaled with the user as the actor. The axes `--force` can never excuse (provider cap, blueprint) stay enforced. Without the gesture, a refusal is still the product.

## Tests

- `src/client/tests/agent_launcher_tests.rs`: editor units, the chip cycle, draft retention, submit refusals, update correlation, chip labels and chip-row paint. Also kill-left, the refusal sentence and its Ctrl+O raw view, and the `?` help sheet.
- `src/client/tests/backlog_board_tests.rs`: the `t` key. It prefills from a card, refuses a worked card, works in the drill-down, and keeps a held draft.
- `src/server/tests/agent_launcher_tests.rs`: pre-birth validation and desk dedup/replay at the Core.
- `tests/agent_launcher_journey.rs`: real subprocess boundary with a recording fake door. Pins exact argv, verbatim stdin, refusal and unknown journeys, and no duplicate attempt.
- `tests/agent_launcher_client_e2e.rs`: one real-client test per reported defect, plus the chip-row journeys: the sheet paint, the project picker, the model picker, the Where picker, the wrap and the cwd line.
