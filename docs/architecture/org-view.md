# Org view

Press `V` to cycle Agents, Backlog when enabled, Org, then Agents. Org shows each Lead's owned nodes and the current and former sessions behind them. Workers without an owned node remain under Unowned. Recently completed nodes stay in the team's departure list for 24 hours.

Press `Tab` to switch Tree, Table and Graph. Press `s` to switch current, former and all sessions. These choices persist across client restarts. Press `F` to toggle full screen. Use `hjkl` or arrows to move; `h` and `l` collapse and expand Tree rows. Graph arrows pan the canvas. Press `/` to filter Lead, node and worker text, `?` for keys, and `esc` to return to Agents.

Org reads the Rust court fold and the shared backlog model off the UI loop, with one gather in flight and a 60-second cadence. The court subprocess has a 30-second budget. Team membership uses the court's canonical ownership map. Session joins prefer the full harness session identifier; an eight-character identifier joins only when unique. Exited registry rows and sessions without a live registry row appear as former. The footer shows read age and names failed reads. A failed refresh retains the last successful tree and its graph inputs; a first failure shows its reason.

Context, run time and unread counts come from the server's stamped agent rows. Missing readings stay unobserved. Rendering does not probe workers or refresh measurement stamps. Graph layout is cached by gathered generation, width and active filters; repainting a frame does not run the layout again.
