# Portals

A portal is the dedicated pane a thread is shown through, indexed from 0.

This does not change substrate semantics. A thread is the persistent lane and still hosts no pane until one is created. A portal is what one is.

## Why the name

The operator named it, 2026-09-02: "i think we should call viewports: portal since it's a portal to view multiple harness threads."

A viewport is a passive window onto something. A portal is the thing you go through to reach a live harness thread. That is what this is.

This project already forbids confusing five axes: harness, provider, model, effort and account. It keeps pane, thread and headless each meaning one exact thing. `viewport` reads as a sixth near-synonym for pane. `portal` names the relation rather than the geometry, so it does not compete.

## What lifted

The server hosted exactly ONE thread viewport. `crates/fno/src/server.rs` declared `thread_pane: Option<(String, u64, TabId)>`. It repointed that one slot at whichever thread had focus, so two threads never sat side by side. The field is now `portals: BTreeMap<u8, Portal>`.

`BTreeMap` rather than `HashMap` is a determinism property, not a preference. Iteration is index-ordered, so the sideline's portal column never reshuffles between frames.

The tiling primitive already existed. The tab menu offers Join Left, Join Right, Join Up and Join Down. Panes already tile inside one tab. Only the cap of one had to go.

## Addressing

| Door | Gesture |
|---|---|
| CLI, spawn | `fno agents spawn --substrate thread --portal N` (one call; `--tab`/`--split` honored on a fresh open), or `fno mux thread <name> --portal N`. Omitted is portal 0. |
| Sideline, portal 0 | Enter (or a click) on a paneless live row. |
| Sideline, a new portal | `P` opens the next free index. |
| Layout | The existing Join actions tile open portals. |

The one-call spawn form, its geometry rules, and its refusals are documented in [fno-agents-spawn.md](../guides/fno-agents-spawn.md).

A bare digit is deliberately NOT the sideline gesture for an index. `0`..`9` on the peek overlay is the answerable-prompt path. So `P` pairs with `p` (the placement picker) the way `X` pairs with `x`.

## Rows are not per-portal

`proto::AgentRow::pane_id` is a POINTER to whichever pane hosts that agent. `None` means a watch-only row. The relation is a pointer, never a pairing, so one row moving between portals stays ONE row. A design that mints a row per portal re-creates the duplicate-row problem the mux operator UX epic exists to remove.

The sideline renders `◫N` for the portal showing a row. The server DERIVES that index at projection time. It matches the row's pane against the open portal seats. Nothing is stored per row, so the index never goes stale.

Pane ids allocate from zero, so pane 0 is a valid seat. Every portal lookup matches on the `Option` and compares seats for EQUALITY. A truthiness test there is the defect that once made six live workers invisible, and it hides on every other pane id.

## Three rules that are easy to get wrong

**Portals persist as slots and restore held.** A pane binds a session to geometry. A thread binds a session to a row. Both facts still hold. What is persisted is not the thread. It is the slot `(index, row_key)` in its tab's stored tree, written by `SlotCapture::name_leaf` beside every other slot. The binding stays `Shell`, because at restore the seat IS a shell until the first reach fills it. A tab holding only portals is a tab like any other: it is captured whole, name included.

Restore holds the seat idle. A named shell takes the slot the tree kept, the `Portal` entry goes back in the map, and the pane says what it waits for. Held idle is the only safe shape, not a compromise. For opencode and agy `interactive_attach` is unsupported, so a portal onto them can only be a RESUME, and a resume starts a real agent process. Eager restore of three portals spawns three agents unasked. The operator's focus spends the process.

Two gestures fill a held seat. Focusing the seat pane runs the row's reach, whose repoint respawns the viewer in that seat. Reaching the row with no explicit index goes home to the held seat that names the row. A seat held at index 1 fills at 1, instead of stranding while a fresh viewer mints at 0. An explicit `--portal N` still means N and never hijacks a held seat. A held portal whose row never returns stays a readable shell naming the row, which is the honest placeholder.

**A closing portal vanishes, except the last one.** The idle-shell stand-in swap in `close_pane` exists for one reason. A viewer whose child died must not delete the only window onto the fleet. With another portal open that premise is false, so the swap fires only for the last one. Without that condition, closing four portals leaves four idle stand-in shells, each holding a tab open.

Liveness is counted from `panes`, not from `portals.len()`. An entry left stale-naming a closed pane is deliberate. The reach reads its remembered tab id, so a replacement viewer lands back where the operator had it. That is what the single slot always did. Counting entries instead lets a dead row disarm the swap for a real portal.

**The `>=1-pane` invariant needs nothing added.** Its only statement lives in `sweep_dead_sideline` and compiles to `panes.len() <= 1`, a whole-session pane count. Portals are panes, so N portals move away from that floor rather than toward it. A portal-specific invariant is a second, weaker rule competing with a guard that already holds.

## Wire

`PanePlacement.portal: Option<u8>`, `PanePlacement.portal_new: bool` and `AgentRow.portal: Option<u8>` arrived in proto v64. All three are additive and `#[serde(default)]`, so the compatibility floor did not move. A v63 client still attaches.

`portal` names an index. `portal_new` asks for the next free one and names none, because the caller must not choose it. Two clients computing "next free" from the rows they last rendered pick the same number, and the second reach repoints the first one's new portal. The server handles reaches one at a time, so it allocates. An explicit index wins over `portal_new`.

The allocator reads liveness, not presence, the same read `close_pane` uses. An entry whose pane closed elsewhere holds no portal, so its index is free. The reach's stale-slot path then reads that leftover entry for its remembered tab, which lands the new viewer where the old one was.

`PanePlacement.thread_pane: bool` stays for one generation as a deprecated alias meaning portal 0. Code reads it only through `PanePlacement::portal_target()`. That folds the two fields into one value at the server's decode edge, so nothing past that point sees two fields that overlap. When `MIN_COMPAT_PROTO` passes 64, drop the bool.

Changing the bool in place is a change to an existing shape. The versioning rule in `proto.rs` says such a change must move the floor too, which refuses every client older than the build. Adding a field does not.

## Cost

A portal is a VIEWER, not an agent. The thread session runs whether or not a pane shows it. So a second portal costs one process and one PTY, not another agent's share. Process count is not the bound.

Measured 2026-09-02 with `fno doctor footprint`: fleet CPU 2.077 cores at 17.3 percent of capacity, descendant CPU 1.286 cores across 154 processes, verdict within.

Every pane drains and renders its PTY, so portals cost redraw work. That plus the seat mechanic is the real bound. This is why there is no numeric cap. When a measurement asks for a cap, add one.
