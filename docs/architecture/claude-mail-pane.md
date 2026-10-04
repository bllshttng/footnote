# Open delivered mail in Claude Code

When you use Claude Code without an FNO mux pane, the Footnote plugin makes delivered mail headers tappable.

Use Claude Code 2.1.287 or later and enable mods to use this feature.

## Use the header

When a delivered header shows `@sender · fmail-id · summary`, press `@sender` to open that session's recent turns in a dock pane.

Press the fmail id to open its chat and mark the selected message.

The sender button finds the message in the local mail thread index, then uses its sender session id.

It never uses the visible sender label to choose a session.

The pane refreshes while it is shown. Close the pane to stop reads.

If your terminal is narrow, Claude Code can wait to place a pane until there is enough width.

After you open it, Claude Code can place the pane inline.

## Fallbacks

If Claude Code is older than 2.1.287 or mods are disabled, the header stays plain text and existing hooks keep working.

If `FNO_PANE` identifies a pane in the FNO mux, the mux keeps its own header navigation. `FNO_SERVER` by itself does not identify a pane.

If the message, sender session, or transcript is unavailable, the pane reports that state.

The pane never guesses a session from a display name.

Other harnesses keep the existing plain header and copyable `fno agents peek <session-id>` hint.
