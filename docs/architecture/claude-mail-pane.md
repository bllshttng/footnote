# Open delivered mail in Claude Code

The Footnote plugin makes delivered mail headers tappable when you use Claude Code without a pane in the FNO mux. This feature requires Claude Code 2.1.287 or later with mods enabled.

## Use the header

When a delivered header shows `@sender · fmail-id · summary`, press `@sender` to open that session's recent turns in a dock pane. Press the fmail id to open its chat and mark that message.

The sender button resolves the message through the local mail thread index and uses the sender's session id. It does not use the visible sender label to choose a session.

The pane refreshes while it is shown. Close it to stop reads. A narrow terminal may delay an unrequested pane until there is enough width; Claude Code can place it inline after you open it.

## Fallbacks

If Claude Code is older than 2.1.287 or mods are disabled, the plugin keeps the header as plain text and its existing hooks continue to work.

If `FNO_PANE` identifies a pane in the FNO mux, the mux keeps its own header navigation. `FNO_SERVER` by itself does not identify a pane.

If the message, sender session, or transcript cannot be read, the pane shows an unavailable or stale status. It does not guess from a display name.

Other harnesses keep the existing plain header and copyable `fno agents peek <session-id>` hint.
