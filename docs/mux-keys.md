# Mux keybindings: the sideline surfaces

The three sideline-adjacent actions sit on three different keys, and their names do not always match what they do. This table is the map. The in-app `prefix+?` modal is the live authority. It renders the same table the scanner dispatches from.

| Key | Action id | Event | What opens |
|---|---|---|---|
| `prefix+f` | `find` | OpenNav | The navigator: a global goto picker over every squad, tab, pane, agent and work-queue card. Text-filter by label, pane id, node id, title-slug or workspace. |
| `prefix+w` | `selector` | OpenSelector | The sideline row selector: cursor over the sideline's own rows. |
| `prefix+b` | `toggle-sideline` | TogglePanel | Sideline panel visibility. |

## The global chord: Ctrl+Opt+Left

`Ctrl+Opt+Left` opens the sideline row selector with no prefix, from anywhere. It lands on the focused pane's row, not the top.

Why this chord: plain `Ctrl+arrow` is macOS Mission Control, so it never reaches the terminal. `Ctrl+Opt` is free at the OS level. In xterm encoding it is `ESC[1;7D`. That is the next rung on the modifier ladder the prefix arrows already parse: `1;5` Ctrl resize, `1;2` Shift move. See `esc_chord` in `crates/fno/src/keys.rs`.

The cost, accepted: the sequence is consumed by the mux and never forwarded to the pane. A program inside a pane that binds `Ctrl+Opt+arrow` loses it. `Opt+arrow` alone (word motion) is untouched. That is modifier 3, not 7.

A bare `Esc` is the first byte of the chord, so the mux holds it briefly to see what follows. If nothing follows within 40ms, the Esc forwards to the pane exactly as typed. Esc-to-cancel in vim or fzf never waits on the next keystroke. The chord itself arrives as one read, so the hold never costs a real chord.

Emulators: iTerm2, Ghostty, kitty and WezTerm emit the xterm CSI form. Verify yours before relying on it: run `cat -v`, press `Ctrl+Opt+Left`, expect `^[[1;7D`. If your emulator sends something else, bind what it sends.

## Inside the navigator

The overlay owns every keystroke, so bare arrows are free there.

- `Right` or `Enter`: goto the selected row. A thread row attaches its session, a pane row focuses the pane. A row that cannot resolve says so and keeps the overlay open.
- `Left` or `Esc`: back to the pane you came from.
- `Up`/`Down` or `Ctrl-p`/`Ctrl-n`: move the cursor.
- `Tab`/`Shift-Tab`: cycle the state chip.
- Typing: filter by what the row IS. A hit invisible in the label shows the matched token as a suffix.

## Rebinding the prefix form

`w` is left-hand and out of reach. Move the selector off it with `mux.keys` (config only, no code):

```toml
[mux.keys]
selector = ";"
```

The action id is `selector`, not `toggle-sideline`. `;` is a free single-byte chord, right pinky against the default left-hand `Ctrl-b` prefix. A collision or unknown action id is refused and reported, never silently ignored.
