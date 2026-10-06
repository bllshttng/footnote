# Org view

Press `V` to cycle Agents, Org, then Agents. With Backlog enabled, the cycle also includes Backlog before Org. Org shows each Lead's owned nodes and the current and former sessions behind them. Workers without an owned node remain under Unowned. Recently completed nodes stay in the team's departure list for 24 hours.

Press `Tab` to switch Tree, Table and Graph. Press `s` to switch current, former and all sessions. These choices persist across client restarts. Press `F` to toggle full screen. Use `hjkl` or arrows to move. Use `h` and `l` to collapse and expand Tree rows. Graph arrows pan the canvas. Press `/` to filter Lead, node and worker text, `?` for keys, and `esc` to return to Agents.

Org reads the Rust org fold and the shared backlog model off the UI loop, with one gather in flight and a 60-second cadence. The org subprocess has a 30-second budget. Team membership uses the org's canonical ownership map. Session joins prefer the full harness session identifier. A unique eight-character identifier can also join a row. Exited registry rows and sessions without a live registry row appear as former. The footer shows read age and names failed reads. A failed refresh retains the last successful tree and its graph inputs. A first failure shows its reason.

Context, run time and unread counts come from the server's stamped agent rows. Missing readings stay unobserved. Rendering does not probe workers or refresh measurement stamps. Graph layout is cached by gathered generation, width and active filters. Repainting a frame does not run the layout again.

Press `Enter` on a Lead or session to focus, attach or resume through the sideline's action path. A former session without a registry row reports `no registry row`. Press `Enter` on a node to inspect its existing node detail. Session keys `space`, `x` and `P` use the sideline's peek, stop and portal actions. Stop keeps the existing confirmation policy.

Press `d` on a session to inspect its context, activity, model, queue and last message. Detail opens immediately from the served row. A roster read then supplies model observations, requested settings and the full served last message. This read runs off the UI loop with a 30-second bound. Failed reads show their reason. Results for an earlier selection or closed view are discarded.
