# Portals

The operator named the model, 2026-09-26: "No pane is EVER a portal. A portal is a window into any pane. Any pane can get assigned into a portal. It's like a TV and the threads are different channels we are flipping through."

A portal is an index, a placement (which tab and which split in it) and a channel. It is never a pane and never a tab. The screen pane is only what the TV plays now: a live viewer, a parked screen, or nothing. Nothing means the operator closed the portal. The thread is a channel. Tuning swaps the screen and never moves the window.

This does not change substrate semantics. A thread is the persistent lane and still hosts no pane until one is created.

## Who creates and closes

Only an explicit gesture creates or closes a portal. A default never creates one.

| Door | Gesture |
|---|---|
| Any verb, explicit flag | `fno mux thread <key> --portal new` opens the next free index. `--portal N` opens N if N is not open, and tunes if it is. A `--split DIR` halves the caller's own pane; `--from portal N\|worker\|current` names another cell (split right from portal 0 gives the next index, and a 2x2 comes from splitting down from each of those). A spawn carries the same flags and opens the portal right after its receipt names the new index. Human or agent, the rule is the same: the flag is the ask. |
| Any verb, no flag | Tune. The reach focuses the row's open portal. With none open, the door's own portal 0 serves it. A bare `fno agents spawn` is a paneless thread and opens nothing. |
| Sideline | Enter on a paneless live row reaches it. `P` opens the portal picker. The `+` row opens the next free index (shift+HJKL as a split beside the focused pane). |
| Composer | The new-agent popup carries the placement in its launch request. The portal opens server-side on the launch receipt, through the requesting client's own reach. |
| A side effect | Never creates. Retask, `fno mux command` and every other machine call tune an existing portal or open a transient view that is not a portal at all. |

Closing is the operator's gesture only: the row menu's `Close portal` entry (`c`), prefix+x on the focused seat, or `fno mux pane kill <seat>`. A spawn, a reach, a restart, a finished worker or a dead viewer never closes one. Closing a portal never stops the row it showed.

`<key>` is the agent name, or the full `session` id `fno agents whoami` prints. The match is exact, no prefix and no substring. Those tiers belong to `fno mux view`, `fno mux where` and `fno mux pane focus`. Zero matches, or several rows answering the same key, refuse and spawn no worker.

## The transient view

A side effect sometimes needs a screen with no portal behind it. `fno mux command` typing into a paneless row is the case. The view door opens the row's viewer in a pane whose argv carries `FNO_VIEW_TRANSIENT=1`. No `portals` entry is written and nothing is persisted. The restore prune reaps a leftover view instead of tabbing it. The owned view closes once the command's caller finishes, the same Drop contract the owned portal had.

## The parked screen

When a channel ends, the portal stays and says so. A viewer whose process exits leaves the portal on its no-signal screen: `portal N: no signal - <channel> ended`. The screen is a keeper-hosted tail process that takes no input. No interactive shell is minted, so a death can never multiply tabs. The screen carries `FNO_PORTAL_HELD=<channel>` in its own argv, and every portal door reads that provenance, never command presence.

The same mint serves a restore-held slot. The seat comes back parked on its channel until a reach or a focus fills it. `[mux.restore] policy = resume` still fills held portals at startup, because that setting is the operator's ask. A parked portal whose row never returns stays a readable screen naming the row.

An open and a close are both journaled: a portal entering the map writes one `portal_opened` row and leaving it writes one `portal_closed` row carrying the door's cause word, so the journal alone answers which door opened a portal and what took it. A retune is a closed-and-open pair on the same index. The peek mail composer's draft is fno state too. Typed text is persisted per target row and restored when a portal on that row reopens. Only an Esc or a send deletes it.

A fill must prove it is the same session. The portal slot records the row's full session id at capture. A key that resolves under a different id now is a different thread wearing a familiar label. The fill refuses, keeps the seat held, and names both ids.

## Restore brings back exactly what the operator had

The operator's tabs, splits and portals come back placed as before, each held on its last channel until the operator tunes it. Nothing starts a process unasked.

A stored index is never reshuffled: portal 3 is held at 3. A duplicate stored slot holds once at its own index. The second seat closes, with a notice. No index above the stored maximum ever appears.

The one-time stand-in prune runs after the slots bind. The shapes an older server minted close instead of coming back. They are: a `portalN` shell, an orphaned held screen, a leftover transient view, or a paneless row's shell stand-in. A pane whose child runs a child of its own is never a candidate. The operator typed `vim` into that shell. This server's own held worker placeholders are resume doors, kept on purpose.

## Rows are not per-portal

`proto::AgentRow::pane_id` is a POINTER to whichever pane hosts that agent. `None` means a watch-only row. The relation is a pointer, never a pairing, so one row moving between portals stays ONE row. A design that mints a row per portal re-creates the duplicate-row problem the mux operator UX epic exists to remove.

The sideline renders `◫N` for the portal showing a row. The server DERIVES that index at projection time from the open portal seats. Nothing is stored per row, so the index never goes stale.

Pane ids allocate from zero, so pane 0 is a valid seat. Every portal lookup matches on the `Option` and compares seats for EQUALITY. A truthiness test there is the defect that once made six live workers invisible, and it hides on every other pane id.

## A channel is a key a row answered

A claude viewer can switch sessions inside its own TUI, and fno is never told. The server reads the seat's OSC title once a second. A title that names one free row moves the channel, the attach mapping and the pane name to that row. A title that names no single free row drops the attach claim and touches nothing else. The channel only ever holds a key a row answered, so free title text never becomes one. A title naming a row another portal shows never steals it.

## Wire

`PanePlacement.portal: Option<u8>` and `PanePlacement.portal_new: bool` arrived in proto v64. `AgentRow.portal` arrived the same generation. `PanePlacement.view` and `PanePlacement.from` arrived in v93. All are additive and `#[serde(default)]`, so the compatibility floor did not move. A v63 client still attaches.

`portal` names an index. `portal_new` asks for the next free one and names none, because the caller must not choose it. Two clients computing "next free" from the rows they last rendered pick the same number, and the second reach repoints the first one's new portal. The server handles reaches one at a time, so it allocates. An explicit index wins over `portal_new`.

`view` asks for a screen that is never a portal. `from` names the cell a split halves: `portal N`, a worker name, or `current`. A split that names no cell defaults to the caller's own pane (FNO_PANE). A caller with no pane names `--from` or is refused with the flag named. A calling pane resolves `current` before the reach. The server resolves `from` to the anchor pane before any geometry runs. If both `at` and `from` name a pane, `at` wins.

`PanePlacement.thread_pane: bool` stays for one generation as a deprecated alias meaning portal 0. Code reads it only through `PanePlacement::portal_target()`. That folds the two fields into one value at the server's decode edge, so nothing past that point sees two fields that overlap. Drop the bool once `MIN_COMPAT_PROTO` passes 64.

Changing the bool in place is a change to an existing shape. The versioning rule in `proto.rs` says such a change must move the floor too, which refuses every client older than the build. Adding a field does not.

## Cost

A portal is a VIEWER, not an agent. The thread session runs whether or not a pane shows it. So a second portal costs one process and one PTY, not another agent's share. Process count is not the bound.

Measured 2026-09-02 with `fno doctor footprint`: fleet CPU 2.077 cores at 17.3 percent of capacity, descendant CPU 1.286 cores across 154 processes, verdict within.

Every pane drains and renders its PTY, so portals cost redraw work. That plus the screen mechanic is the real bound. This is why there is no numeric cap. When a measurement asks for a cap, add one.

## A conversion leaves no portal

Converting a pane session to a thread (`fno agents resume <name> --substrate thread`) is a substrate change, not a view change. Afterward the mux server hosts nothing for that session. The conversion opens no portal and leaves none behind. To look at the converted thread, tune one with `fno mux thread <name>`. A portal never changes the substrate. The reseat verb moves the worker into the named portal's screen while the server keeps hosting the process.
