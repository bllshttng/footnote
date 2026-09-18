//! The portal reach: the `fno mux thread` control verb (`portal_ctl`) and the
//! AttachAgent portal arm it drives (`reach_portal`), extracted from the
//! parent under the file-budget gate - the code the change touched moves with
//! it.

use super::*;

/// Does `portal_key` name the same ROW the reach resolved?
///
/// Not the same KEY: the TUI door keys a portal by the attach id while
/// `fno agents attach` keys it by the registry name, and both doors advertise
/// the same pane. Attach ids are unique per bg session, so a portal keyed by
/// this row's attach id IS this row whatever door the reach came from - and the
/// reverse (keyed by name, reached again by attach id) is the same row too,
/// which is why the name comparison runs both directions.
///
/// One function, three callers: the same-row focus arm for the REQUESTED
/// portal, the held-seat default-reach arm, and the one-row-one-viewer check
/// across every OTHER portal. Copies of this comparison drifting apart is
/// how a duplicate viewer gets minted.
pub(super) fn row_matches_portal_key(row: &RegistryAgent, key: &str, portal_key: &str) -> bool {
    portal_key == key || row.attach_id.as_deref() == Some(portal_key) || portal_key == row.name
}

/// The live-paneless-row match, shared by every door: the reach's
/// resolution, the control door's ambiguity check, and the held-seat
/// focus fill must agree on who answers a key.
pub(super) fn row_answers_key(a: &RegistryAgent, key: &str) -> bool {
    a.mux.is_none() && !a.exited && (a.attach_id.as_deref() == Some(key) || a.name == key)
}

/// The session a claude viewer's OSC title names: the title minus one
/// leading status glyph token ("✳", "◐", a braille spinner frame).
pub(super) fn title_session_name(title: &str) -> &str {
    let t = title.trim();
    match t.split_once(' ') {
        Some((glyph, rest)) if !glyph.chars().any(char::is_alphanumeric) => rest.trim(),
        _ => t,
    }
}

/// Remember every restored portal slot's seat (index, row, pane,
/// tab id) until the tab ids are final.
pub(super) fn collect_portal_slot_seats(
    kept_slots: &[&crate::proto::LayoutSlot],
    slot_pane: &HashMap<&str, u64>,
    tid: TabId,
    into: &mut Vec<(u8, String, u64, TabId)>,
) {
    for slot in kept_slots {
        if let (Some(p), Some(portal)) = (slot_pane.get(slot.name.as_str()), slot.portal.as_ref()) {
            into.push((portal.index, portal.row.clone(), *p, tid));
        }
    }
}

/// One restore receipt for both held kinds. A zero count is not
/// silent when the other is non-zero, so "no portal came back" and "the
/// counter never ran" stay distinguishable.
pub(super) fn notify_held_receipt(core: &mut Core, workers_total: usize, portals_total: usize) {
    let portals_note = if portals_total > 0 {
        format!(" and {portals_total} portal(s)")
    } else {
        String::new()
    };
    core.notice_all(format!(
        "restore: held {workers_total} worker pane(s){portals_note}; focus one to resume it"
    ));
}

/// Re-arm every held portal seat after restore: the entry goes back
/// in the map, the seat pane gets its name and its held message, and the
/// reach or a focus fills it on first demand. A held portal is NOT a squad
/// member - the slot in its tab is the whole record. Returns the count of
/// seats re-armed, for the restore receipt.
pub(super) fn rearm_held_portal_seats(
    core: &mut Core,
    seats: Vec<(u8, String, u64, TabId)>,
) -> usize {
    let mut held = 0;
    for (index, row, seat, tid) in seats {
        let index = if core.portals.contains_key(&index) {
            match core.next_free_portal() {
                Some(free) => {
                    core.notice_all(format!(
                        "restore: portal {index} was taken; held {row} at portal {free} instead"
                    ));
                    free
                }
                None => {
                    core.notice_all(format!(
                        "restore: all portal indices live; portal slot for {row} skipped"
                    ));
                    continue;
                }
            }
        } else {
            index
        };
        core.portals.insert(
            index,
            Portal {
                row_key: row.clone(),
                seat,
                tab: tid,
            },
        );
        if let Some(entry) = core.panes.get_mut(&seat) {
            entry.name = Some(format!("portal{index}"));
        }
        core.write_restore_message(
            seat,
            &format!("portal {index} ({row}, held across restart) - reach the row, or focus this pane, to resume"),
        );
        held += 1;
    }
    held
}

/// A focused portal seat that is still the held shell fills in
/// place: the reach's repoint respawns the row's viewer in THIS seat.
/// `None` falls through to a plain focus, so a seat whose row resolves to
/// zero or several live paneless rows stays a readable placeholder.
pub(super) fn fill_held_portal_seat(
    core: &mut Core,
    client_id: u64,
    view: (u64, TabId),
    vp: Rect,
    pid: u64,
) -> Option<Flow> {
    let idx = core.portal_of(Some(pid))?;
    fill_held_portal_at(core, client_id, view, vp, idx)
}

/// The fill by portal INDEX: the one body behind the focus door
/// ([`fill_held_portal_seat`]) and the restore verb, so the two doors cannot
/// disagree about what fills a held seat. The seat's own leaf wins
/// (`portal_explicit = false`); the reach repoints the stand-in in place.
pub(super) fn fill_held_portal_at(
    core: &mut Core,
    client_id: u64,
    view: (u64, TabId),
    vp: Rect,
    idx: u8,
) -> Option<Flow> {
    let stand_in = core.portals.get(&idx).is_some_and(|portal| {
        core.panes
            .get(&portal.seat)
            .is_some_and(|entry| entry.cmd.is_none())
    });
    if !stand_in {
        return None;
    }
    let row_key = core.portals.get(&idx)?.row_key.clone();
    let mut hits = core.agents.iter().filter(|a| row_answers_key(a, &row_key));
    if let (Some(_), None) = (hits.next(), hits.next()) {
        return Some(core.reach_portal(
            client_id,
            view,
            vp,
            idx,
            &row_key,
            &PanePlacement::default(),
            false,
        ));
    }
    None
}

/// What the restore verb would do with portal `idx` right now. One
/// classifier behind both callers - `workspace_restore_start` collects the
/// claude plans to resolve off-loop, `workspace_restore_apply` turns each
/// verdict into the report row - so the two halves cannot disagree about
/// who fills, who plans, and who refuses. `None`: no portal at `idx`.
pub(super) enum PortalRestoreClass {
    /// The seat pane left the session; the entry is stale.
    SeatGone,
    /// The seat already runs a viewer.
    Focused,
    /// No live paneless row answers the key.
    NoRow,
    /// Two or more rows answer the key.
    Ambiguous,
    /// One claude Drive row: its attach re-entry plan must be staged before
    /// a fill. Carries the row NAME the resolver keys on.
    NeedsClaudePlan(String),
    /// One row whose argv builds inline (codex Drive, Follow, Locate).
    FillDirect(RegistryAgent),
}

pub(super) fn classify_portal_restore(core: &Core, idx: u8) -> Option<PortalRestoreClass> {
    let portal = core.portals.get(&idx)?;
    let seat_in_tree = core.session.find_pane(portal.seat).is_some();
    let seat_viewer = seat_in_tree
        && core
            .panes
            .get(&portal.seat)
            .is_some_and(|entry| entry.cmd.is_some());
    if seat_viewer {
        return Some(PortalRestoreClass::Focused);
    }
    if !seat_in_tree || !core.panes.contains_key(&portal.seat) {
        return Some(PortalRestoreClass::SeatGone);
    }
    let mut hits = core
        .agents
        .iter()
        .filter(|a| row_answers_key(a, &portal.row_key));
    match (hits.next(), hits.next()) {
        (None, _) => Some(PortalRestoreClass::NoRow),
        (Some(_), Some(_)) => Some(PortalRestoreClass::Ambiguous),
        (Some(row), None) => {
            let claude_drive = row.harness.as_deref() == Some("claude")
                && matches!(
                    agents_view::thread_reach(row.harness.as_deref(), row.attach_id.as_deref()),
                    Reach::Drive
                );
            if claude_drive {
                Some(PortalRestoreClass::NeedsClaudePlan(row.name.clone()))
            } else {
                Some(PortalRestoreClass::FillDirect(row.clone()))
            }
        }
    }
}

/// The notice a filled Locate-tier portal carries: the seat shows
/// where the thread lives, not the thread, and the row says so instead of
/// reading as a Drive fill that shows nothing.
pub(super) fn locate_tier_notice(row: &RegistryAgent) -> Option<String> {
    let locate = matches!(
        agents_view::thread_reach(row.harness.as_deref(), row.attach_id.as_deref()),
        Reach::Locate
    );
    locate.then(|| {
        format!(
            "{} reaches Locate only - the portal shows where the thread lives, not the thread",
            row.harness.as_deref().unwrap_or("this harness")
        )
    })
}

/// The passive client id a restore-verb fill rides: the verb has no
/// focused client, so the seat's own tab rect and this id stand in.
pub(super) const RESTORE_CLIENT: u64 = u64::MAX;

/// The row names of held portals whose fill needs the off-loop
/// claude re-entry plan: one Drive row answering the key, seat still held.
pub(super) fn portals_needing_claude_plan(core: &Core) -> Vec<String> {
    core.portals
        .keys()
        .copied()
        .filter_map(|idx| match classify_portal_restore(core, idx) {
            Some(PortalRestoreClass::NeedsClaudePlan(name)) => Some(name),
            _ => None,
        })
        .collect()
}

/// One report row per stored portal, classified by the same door
/// the focus fill uses, appended after the member rows. A held claude Drive
/// seat fills from its staged attach verdict; a fill that did not land
/// reports refused, never silent. The one-row / ambiguous / no-row texts
/// are the reach's own refusal vocabulary.
pub(super) fn portal_restore_rows(
    core: &mut Core,
    dry_run: bool,
    plans: &mut HashMap<String, Result<ReentryVerdict, String>>,
) -> Vec<RestoreRow> {
    let portal_indices: Vec<u8> = core.portals.keys().copied().collect();
    let mut rows = Vec::with_capacity(portal_indices.len());
    for idx in portal_indices {
        let (row_key, seat, tab_id) = match core.portals.get(&idx) {
            Some(p) => (p.row_key.clone(), p.seat, p.tab),
            None => continue,
        };
        let mk = |outcome: &str,
                  pane: Option<u64>,
                  tab: Option<u64>,
                  reason: Option<String>,
                  notice: Option<String>| RestoreRow {
            member: row_key.clone(),
            harness: None,
            squad: 0,
            portal: Some(idx),
            outcome: outcome.into(),
            pane,
            tab,
            reason,
            notice,
        };
        match classify_portal_restore(core, idx) {
            Some(PortalRestoreClass::SeatGone) => {
                rows.push(mk(
                    "refused",
                    None,
                    None,
                    Some("portal seat is gone".into()),
                    None,
                ));
            }
            Some(PortalRestoreClass::Focused) => {
                rows.push(mk("focused", Some(seat), Some(tab_id), None, None));
            }
            Some(PortalRestoreClass::NoRow) => {
                rows.push(mk(
                    "refused",
                    None,
                    None,
                    Some(format!("no live row answers {row_key}")),
                    None,
                ));
            }
            Some(PortalRestoreClass::Ambiguous) => {
                rows.push(mk(
                    "refused",
                    None,
                    None,
                    Some(format!("{row_key} is ambiguous - reach it by its pane")),
                    None,
                ));
            }
            Some(
                cls @ (PortalRestoreClass::NeedsClaudePlan(_) | PortalRestoreClass::FillDirect(_)),
            ) => {
                if dry_run {
                    rows.push(mk("planned", None, None, None, None));
                    continue;
                }
                let locate_notice = match &cls {
                    PortalRestoreClass::FillDirect(row) => locate_tier_notice(row),
                    _ => None,
                };
                if let PortalRestoreClass::NeedsClaudePlan(name) = &cls {
                    match plans.remove(&format!("portal:{name}")) {
                        Some(Ok(verdict)) => {
                            core.reentry_verdict = Some(verdict);
                        }
                        Some(Err(reason)) => {
                            rows.push(mk("refused", None, None, Some(reason), None));
                            continue;
                        }
                        None => {
                            rows.push(mk(
                                "refused",
                                None,
                                None,
                                Some(
                                    "claude re-entry plan unresolved; resume it from the agent panel"
                                        .into(),
                                ),
                                None,
                            ));
                            continue;
                        }
                    }
                }
                // Fill through the focus door's one body; the seat pane's
                // own size keeps the replacement viewer at the geometry it
                // held. The reach repoints in place, so the entry's own
                // seat and tab are the report's values.
                let (srows, scols) = core
                    .panes
                    .get(&seat)
                    .map(|e| e.vt.size())
                    .unwrap_or((crate::vt::DEFAULT_ROWS, crate::vt::DEFAULT_COLS));
                let vp = tree::Rect {
                    x: 0,
                    y: 0,
                    rows: srows,
                    cols: scols,
                };
                let _ = fill_held_portal_at(core, RESTORE_CLIENT, (0, tab_id), vp, idx);
                core.reentry_verdict = None;
                let filled = core
                    .portals
                    .get(&idx)
                    .is_some_and(|p| core.panes.get(&p.seat).is_some_and(|e| e.cmd.is_some()));
                if filled {
                    let (pseat, ptab) = core
                        .portals
                        .get(&idx)
                        .map(|p| (Some(p.seat), Some(p.tab)))
                        .unwrap_or((None, None));
                    rows.push(mk("resumed", pseat, ptab, None, locate_notice));
                } else {
                    rows.push(mk(
                        "refused",
                        None,
                        None,
                        Some("portal fill did not land; the seat stays held".into()),
                        None,
                    ));
                }
            }
            None => continue,
        }
    }
    rows
}

/// One control-door reach parked while the row's re-entry plan resolves
/// off-loop: the observer stays registered, the harvest receiver and the
/// held reply wait here, and the ReentryPlanReady replay finishes the reach
/// through `finish_pending_thread_reply`.
pub(super) struct PendingThreadReply {
    pub(super) client: u64,
    name: String,
    portal: u8,
    rx: mpsc::Receiver<ServerMsg>,
    reply: ControlReply,
}

/// The join behind the control door's reply. Row-aware, not key-aware, and
/// index-aware: a focus on a portal keyed by the attach id (the TUI door)
/// reached through the registry name (this door) is a landing, not a
/// refusal, and landing in a DIFFERENT index would be a refusal reported as
/// success. A focus of the portal that already shows the row counts as
/// landed too: the caller's index stayed empty because the door focused the
/// existing viewer instead.
fn portal_reply(landed: bool, landing: Option<String>, name: &str, portal: u8) -> ServerMsg {
    match (landed, landing) {
        (true, Some(text)) => ServerMsg::Notice { text },
        (true, None) => ServerMsg::Notice {
            text: format!("thread pane -> {name} (portal {portal})"),
        },
        (false, Some(text)) => ServerMsg::Err {
            code: err_code::BAD_REQUEST,
            msg: text,
        },
        // The fallback arm: the reach itself never reported. The
        // old "no such agent: NAME" here read as a resolver-style refusal
        // and cost a measurement pass; this text can only mean the harvest
        // came back empty.
        (false, None) => ServerMsg::Err {
            code: err_code::BAD_REQUEST,
            msg: format!("portal reach produced no verdict for {name}"),
        },
    }
}

impl Core {
    /// Reach `key` (an attach id for a claude row, a registry name
    /// for every other harness) through portal `portal`. The tier is
    /// capability-computed (`agents_view::thread_reach`): Drive runs the
    /// account-wrapped attach argv, Follow tails the transcript with
    /// `fno agents peek --follow`, Locate renders the self-teaching screen.
    ///
    /// Resolution order is per INDEX, and every arm touches only its
    /// own portal: no portal at `portal` opens one through the ordinary
    /// placement path; a portal at `portal` on another row repoints it in
    /// place (the open-here mechanic: spawn-first, `tree::replace_leaf`,
    /// reap-last - the geometry never moves); a portal at `portal` on this row
    /// focuses it; a recorded pane the tree no longer knows reads as absent.
    /// NEVER persists a squad member; an open portal is persisted as its
    /// tab's slot and restored held: a pane
    /// binds a session to geometry, a thread binds a session to a row.
    ///
    /// The geometry decision lives HERE, after the slot lookup that
    /// knows whether index N is occupied - not at the decode edge, which
    /// cannot see occupancy. `here` is refused in both cases (a portal mints
    /// its own seat pane; open-here repoints the sender's focused pane).
    /// Everything else the caller named is IGNORED, visibly, when the portal
    /// already has a live seat (a portal owns its geometry;
    /// remembered tab steers the replacement), and HONORED on a fresh open,
    /// where there is no geometry to own yet.
    ///
    /// A portal is persisted as its tab's slot and restored held: a
    /// stand-in seat is a HELD portal until a reach or a focus fills it, so
    /// the one-row-one-viewer check treats a live viewer as the row's home
    /// and a default (no explicit index) reach retargets to a held seat that
    /// names the row. An explicit index is never hijacked.
    pub(super) fn reach_portal(
        &mut self,
        client_id: u64,
        view: (u64, TabId),
        vp: Rect,
        mut portal_idx: u8,
        key: &str,
        placement: &PanePlacement,
        portal_explicit: bool,
    ) -> Flow {
        // open-here is never a portal, in either case below. Refused
        // before any lookup, exactly as the decode edge refused it before
        // this decision moved in here.
        if placement.here {
            self.notice(client_id, "a portal takes no split, target, or anchor");
            return Flow::Continue;
        }
        // Geometry a fresh open could honor. Computed once, before the slot
        // lookup: every live-seat arm ignores it (with a notice) and only
        // the fresh-open arm consumes it.
        let caller_geometry = placement.split.is_some()
            || placement.at.is_some()
            || placement.tab.is_some()
            || !matches!(placement.target, PaneTarget::CurrentRoute);
        // Resolve exactly one live paneless row for the key. Names are not
        // unique; a name that matches two rows must refuse, never pick.
        let mut hits = self.agents.iter().filter(|a| row_answers_key(a, key));
        let row = match (hits.next(), hits.next()) {
            (Some(a), None) => a.clone(),
            (Some(_), Some(_)) => {
                self.notice(
                    client_id,
                    "more than one row goes by that name - reach it by its pane",
                );
                return Flow::Continue;
            }
            _ => {
                // The bare "no such agent" this arm used to emit
                // sent a reader hunting a lifecycle resolver that is not in
                // this chain. Name the door and the row it looked for.
                self.notice(
                    client_id,
                    format!("portal reach: no live row answers {key}"),
                );
                return Flow::Continue;
            }
        };
        // A DEFAULT reach goes home to the held seat that remembers
        // the row. Restore put a shell in the seat and the entry back in the
        // map; without this retarget the reach would open a fresh viewer at
        // the requested index and strand the held seat. Only a stand-in
        // (no argv provenance) qualifies - a live viewer of the row already
        // returned through the one-row-one-viewer arm above. An explicit
        // index is never hijacked: `--portal 0` means portal 0.
        if !portal_explicit {
            if let Some(held_idx) = self
                .portals
                .iter()
                .find(|(idx, portal)| {
                    **idx != portal_idx
                        && row_matches_portal_key(&row, key, &portal.row_key)
                        && self.session.find_pane(portal.seat).is_some()
                        && self
                            .panes
                            .get(&portal.seat)
                            .is_some_and(|entry| entry.cmd.is_none())
                })
                .map(|(idx, _)| *idx)
            {
                self.notice(
                    client_id,
                    format!("portal {held_idx}: resuming {}", row.name),
                );
                portal_idx = held_idx;
            }
        }
        // ONE ROW, ONE VIEWER. A reach for a row that ANOTHER portal
        // already shows focuses that portal rather than minting a second
        // viewer for it. The single slot enforced this by construction: there
        // was nowhere else for the row to be, so the same-row arm below caught
        // every case. With several portals the same-row arm sees only the
        // REQUESTED index, and everything past it opens fresh.
        //
        // A duplicate is not cosmetic. `attached` holds ONE pane per attach id,
        // so the second viewer's insert overwrites the first and leaves a live
        // pane no row points at - the duplicate-viewer problem this epic exists
        // to remove, re-created one layer down.
        if let Some(other_idx) = self.live_viewer_portal(&row, key, portal_idx) {
            let (other_seat, other_tab) = {
                let portal = &self.portals[&other_idx];
                (portal.seat, portal.tab)
            };
            match self.session.find_pane(other_seat) {
                Some((sid, _)) => {
                    // This focus ignores caller geometry; saying so
                    // beats a silent drop.
                    if caller_geometry {
                        self.notice(client_id, "a portal takes no split, target, or anchor");
                    }
                    self.set_view(client_id, sid, other_tab);
                    if let Some(tab) = self.viewed_tab_mut((sid, other_tab)) {
                        tab.focus = other_seat;
                    }
                    self.mark_seen_if_done(other_seat);
                    self.notice(
                        client_id,
                        format!("portal {other_idx}: already showing {}", row.name),
                    );
                    self.push_layout(true);
                    return Flow::Continue;
                }
                // Half-created pane, the same case the same-row arm below
                // handles: tracked in `panes` but absent from the tab tree, so
                // it shows the row to nobody. Reap it and drop its portal
                // rather than focusing a pane with no place on screen, then
                // fall through and open this reach fresh. Without the reap it
                // leaks a child process, and without the drop the entry keeps
                // blocking every later reach for this row.
                None => {
                    self.reap_pane(other_seat);
                    self.portals.remove(&other_idx);
                }
            }
        }
        let tier = agents_view::thread_reach(row.harness.as_deref(), row.attach_id.as_deref());
        let spawn_cwd = if row.cwd.is_empty() {
            self.session
                .squad(view.0)
                .map(|s| s.canonical_cwd().to_string())
                .unwrap_or_default()
        } else {
            row.cwd.clone()
        };
        // The tier's argv is built server-side, where the row set lives: the
        // client's reach command is tier-blind by design.
        let argv = match tier {
            Reach::Drive => {
                let id = row.attach_id.clone().expect("Drive implies attach_id");
                // The Drive argv is the canonical re-entry plan for a
                // claude row. `None` means the plan is resolving off-loop; the
                // replay carries a portal placement, which re-lands in this
                // reach with the verdict staged. It names THIS
                // portal, so an off-loop replay returns to the index the
                // operator reached, not to portal 0.
                let placement = crate::proto::PanePlacement {
                    portal: Some(portal_idx),
                    ..Default::default()
                };
                let Some((argv, _cd)) = self.attach_gesture_argv(client_id, &id, &placement) else {
                    return Flow::Continue;
                };
                argv
            }
            Reach::Follow => peek_argv(&row.name),
            Reach::Locate => locate_argv(&row),
        };
        let (rows, cols) = self
            .clients
            .iter()
            .find(|c| c.id == client_id)
            .map(|c| c.dims)
            // A passive observer's (0,0) sentinel must never size a pane -
            // fall back to the view rect (the control-path reach rides an
            // observer client).
            .filter(|(r, c)| *r > 0 && *c > 0)
            .unwrap_or((vp.rows, vp.cols));
        // Entry: take THIS portal, then verify against the live tree (the
        // diff-pane stale-id guard - a recorded pane closed by any other path
        // reads as closed and never wedges the portal). Only this index is
        // removed; every other portal is untouched by this reach.
        let slot = self.portals.remove(&portal_idx);
        // The seat's tab id, kept out of the stale-seat paths: a
        // fresh-open (below) prefers it when the tab still exists.
        let mut remembered_tab_id: Option<TabId> = None;
        if let Some(Portal {
            row_key: slot_row,
            seat: pid,
            tab: slot_tid,
        }) = slot
        {
            remembered_tab_id = Some(slot_tid);
            if self.panes.contains_key(&pid) {
                if let Some((sid, ti)) = self.session.find_pane(pid) {
                    let tid = self.session.squad(sid).expect("find_pane live").tabs[ti].id;
                    // Same ROW, not same key. The comparison lives in
                    // `row_matches_portal_key`, shared with the
                    // one-row-one-viewer check above, so the two readings of
                    // "is this the same row" cannot drift apart.
                    let same_row = row_matches_portal_key(&row, key, &slot_row);
                    // A same-row reach is a focus only when the seat
                    // holds a LIVE viewer. After the viewer's child died, the
                    // seat holds the idle-shell stand-in (no argv provenance):
                    // "already showing" would lie, so fall through to the
                    // repoint, which respawns the row's viewer in the same tab.
                    let seat_is_viewer = self.panes.get(&pid).is_some_and(|e| e.cmd.is_some());
                    if same_row && seat_is_viewer {
                        // Same row: "show me", never a toggle-close. Closing
                        // the pane is the ordinary close gesture. The slot
                        // was taken above; put it back - a focus is not a
                        // close. Caller geometry is ignored here too (the
                        // seat keeps its place), visibly.
                        if caller_geometry {
                            self.notice(client_id, "a portal takes no split, target, or anchor");
                        }
                        self.portals.insert(
                            portal_idx,
                            Portal {
                                row_key: slot_row,
                                seat: pid,
                                tab: slot_tid,
                            },
                        );
                        self.set_view(client_id, sid, tid);
                        if let Some(tab) = self.viewed_tab_mut((sid, tid)) {
                            tab.focus = pid;
                        }
                        self.mark_seen_if_done(pid);
                        self.notice(
                            client_id,
                            format!("thread pane: already showing {}", row.name),
                        );
                        self.push_layout(true);
                        return Flow::Continue;
                    }
                    // Repoint to the new row. Spawn-first, so a failure
                    // leaves the slot pane, the layout, and the recorded slot
                    // exactly as they were. The displacement guard the
                    // open-here path needs does not apply here: the slot pane
                    // is the dedicated pane, displaceable by construction.
                    //
                    // The repoint keeps the portal's geometry - the
                    // seat is replaced IN PLACE, the tab never moves - so
                    // caller geometry is ignored. The notice is what makes
                    // that a decision rather than a silent drop (AC2-REG).
                    if caller_geometry {
                        self.notice(client_id, "a portal takes no split, target, or anchor");
                    }
                    let permit = match crate::process_admission::admit_pane(0, None) {
                        Ok(p) => p,
                        Err(error) => {
                            self.portals.insert(
                                portal_idx,
                                Portal {
                                    row_key: slot_row,
                                    seat: pid,
                                    tab: slot_tid,
                                },
                            );
                            self.notice(client_id, format!("thread pane failed: {error}"));
                            return Flow::Continue;
                        }
                    };
                    let new_pid = match self
                        .spawn_pane_cmd_with_permit(&argv, rows, cols, &spawn_cwd, permit)
                    {
                        Ok(p) => p,
                        Err(e) => {
                            self.portals.insert(
                                portal_idx,
                                Portal {
                                    row_key: slot_row,
                                    seat: pid,
                                    tab: slot_tid,
                                },
                            );
                            self.notice(client_id, format!("thread pane failed: {e}"));
                            return Flow::Continue;
                        }
                    };
                    self.name_thread_viewer_pane(new_pid, &row, &tier);
                    let Some(tab) = self.viewed_tab_mut((sid, tid)) else {
                        self.reap_pane(new_pid);
                        self.portals.insert(
                            portal_idx,
                            Portal {
                                row_key: slot_row,
                                seat: pid,
                                tab: slot_tid,
                            },
                        );
                        self.notice(client_id, "thread pane: the tab closed under the repoint");
                        return Flow::Continue;
                    };
                    if !tree::replace_leaf(tab, pid, new_pid) {
                        self.reap_pane(new_pid);
                        self.portals.insert(
                            portal_idx,
                            Portal {
                                row_key: slot_row,
                                seat: pid,
                                tab: slot_tid,
                            },
                        );
                        self.notice(client_id, "thread pane: its pane left the tree");
                        return Flow::Continue;
                    }
                    // Insert the new mapping BEFORE the reap: reap_pane drops
                    // every mapping onto the old pane, so the old row
                    // resurfaces watch-only while the new mapping survives.
                    if let Some(id) = row.attach_id.clone() {
                        self.attached.insert(id, new_pid);
                    }
                    // Reap-last: the displaced viewer dies, the session it
                    // showed keeps running daemon-hosted.
                    self.reap_pane(pid);
                    self.portals.insert(
                        portal_idx,
                        Portal {
                            row_key: key.to_string(),
                            seat: new_pid,
                            tab: tid,
                        },
                    );
                    self.set_view(client_id, sid, tid);
                    if let Some(tab) = self.viewed_tab_mut((sid, tid)) {
                        tab.focus = new_pid;
                    }
                    self.notice(
                        client_id,
                        format!("thread pane -> {} (portal {})", row.name, portal_idx),
                    );
                    self.push_layout(true);
                    return Flow::Continue;
                } else {
                    // Half-created pane (close_pane's same case): tracked in
                    // self.panes but absent from the tab tree. Reap it here
                    // too, so it can never leak a child process.
                    self.reap_pane(pid);
                }
            }
            // Stale slot (pane closed by any other path): open fresh below.
        }
        // Open fresh through the ordinary placement path: owner routing (the
        // squad whose owns_path matches the row cwd, else the viewed squad),
        // then the shared placement helper. The slot is recorded only after
        // placement succeeds, and NO squad member is persisted - the one
        // deliberate difference from the ordinary attach tail.
        //
        // A remembered seat tab (from the stale slot above) still
        // means something: when it survives, land the fresh viewer THERE, so
        // replacing a dead or displaced viewer never strands the viewport in
        // whatever tab the client happens to be viewing. Any miss - no
        // remembered tab, squad or tab gone - falls back to today's routing.
        //
        // With no surviving remembered tab, a fresh open has no
        // geometry to own yet, so the CALLER's placement is honored instead
        // of a server guess (AC1-HP). The remembered tab still wins over a
        // caller tab: precedence must not regress (AC3-REG).
        let remembered_tab = remembered_tab_id.and_then(|tid| {
            self.session
                .squads
                .iter()
                .find_map(|s| s.tabs.iter().find(|t| t.id == tid).map(|_| (s.id, tid)))
        });
        let owner = self.session.find_by_cwd(&spawn_cwd).unwrap_or(view.0);
        let (dest, effective) = match remembered_tab {
            Some((sid, tid)) => {
                let eff = PanePlacement {
                    tab: Some(crate::proto::TabSel::Id(tid)),
                    ..Default::default()
                };
                (Some(sid), eff)
            }
            None => {
                let dest = match self.resolve_placement_target(&placement.target, Some(owner)) {
                    Ok(d) => d,
                    Err(e) => {
                        self.notice(client_id, e);
                        return Flow::Continue;
                    }
                };
                // AC4-EDGE: a tab the server cannot resolve refuses BEFORE a
                // pane exists - never spawn-then-reap. `New` is born with the
                // pane in place_with and needs no pre-check.
                if let (Some(sid), Some(sel)) = (dest, placement.tab.as_ref()) {
                    if !matches!(sel, crate::proto::TabSel::New) {
                        if let Err(e) = self.resolve_tab_index(sid, sel) {
                            self.notice(client_id, e);
                            return Flow::Continue;
                        }
                    }
                }
                // The portal fields are this reach's ADDRESSING, not
                // placement geometry: place_with reads none of them, and a
                // portal inside a placement is the overlap the decode edge
                // used to forbid. Strip them so the effective placement is
                // pure geometry.
                let mut eff = placement.clone();
                eff.portal = None;
                eff.portal_new = false;
                eff.thread_pane = false;
                (dest, eff)
            }
        };
        let permit = match crate::process_admission::admit_pane(
            self.placement_pane_count(dest, &effective),
            effective.max_panes,
        ) {
            Ok(p) => p,
            Err(error) => {
                self.notice(client_id, format!("thread pane failed: {error}"));
                return Flow::Continue;
            }
        };
        let pid = match self.spawn_pane_cmd_with_permit(&argv, rows, cols, &spawn_cwd, permit) {
            Ok(p) => p,
            Err(e) => {
                self.notice(client_id, format!("thread pane failed: {e}"));
                return Flow::Continue;
            }
        };
        self.name_thread_viewer_pane(pid, &row, &tier);
        let (sid, tid, fell_back) = match self.place_with(dest, &spawn_cwd, pid, &effective) {
            Ok(landing) => landing,
            Err((_code, e)) => {
                self.notice(client_id, e);
                return Flow::Continue;
            }
        };
        if let Some(id) = row.attach_id.clone() {
            self.attached.insert(id, pid);
        }
        self.portals.insert(
            portal_idx,
            Portal {
                row_key: key.to_string(),
                seat: pid,
                tab: tid,
            },
        );
        self.set_view(client_id, sid, tid);
        if fell_back {
            self.notice(client_id, "tab full - opened as tab");
        }
        self.notice(
            client_id,
            format!("thread pane -> {} (portal {})", row.name, portal_idx),
        );
        self.push_layout(true);
        Flow::Continue
    }

    /// Title a freshly-opened thread-viewer pane by its registry row.
    pub(super) fn name_thread_viewer_pane(&mut self, pid: u64, row: &RegistryAgent, tier: &Reach) {
        self.claim_eligible.insert(pid);
        // Claude Drive panes use attach lookup; other tiers use the row name.
        if matches!(tier, Reach::Drive) && row.harness.as_deref() == Some("claude") {
            // The attach argv carries no FNO_AGENT_SELF; name_attached_pane
            // resolves the name from the live catalog the same way.
            if let Some(id) = row.attach_id.as_deref() {
                let (_, cd) = self.attach_account_ctx(id);
                self.name_attached_pane(pid, id, cd.as_deref());
            }
            return;
        }
        if let Some(entry) = self.panes.get_mut(&pid) {
            entry.name = Some(row.name.clone());
        }
    }

    /// The outside-the-TUI reach (`fno agents attach` with a live
    /// mux), run as the exact command a TUI reach runs: a synthetic OBSERVER
    /// client (0,0 - read-only, no squad or PTY of its own) whose reliable
    /// channel collects the notices, then the real AttachAgent portal
    /// command. One implementation, two doors, no drift; the observer is
    /// removed through the same Gone path a Detach takes, and the reply is
    /// the landing notice on success or the refusal's Err.
    ///
    /// `portal` is the index the caller named (`--portal N`, default
    /// 0). This door is the addressing surface an operator uses to put two
    /// threads side by side without touching the TUI.
    pub(super) fn portal_ctl(
        &mut self,
        name: &str,
        portal: u8,
        placement: PanePlacement,
        agents: Option<Vec<RegistryAgent>>,
        reply: ControlReply,
    ) {
        if let Some(rows) = agents {
            // Same source and cadence as the off-loop reader's tick; assigning
            // only guarantees the command resolves against the snapshot the
            // CLI just saw.
            self.agents = rows;
        }
        // Names are not unique; a name that matches two rows must refuse,
        // never pick, same as reach_portal's own guard. The count is over
        // LIVE PANELESS rows - the rows a reach could serve - so a hosted
        // or exited namesake never turns a reachable row into a refusal.
        let mut named_hits = self.agents.iter().filter(|a| row_answers_key(a, name));
        if let (Some(_), Some(_)) = (named_hits.next(), named_hits.next()) {
            let _ = reply.send(ServerMsg::Err {
                code: err_code::BAD_REQUEST,
                msg: "more than one row goes by that name - reach it by its pane".to_string(),
            });
            return;
        }
        // The row the reach would land on decides this door's shape: the
        // sync path drives the reach inline, a claude Drive row parks below.
        let live_row = self
            .agents
            .iter()
            .find(|a| {
                (a.name == name || a.attach_id.as_deref() == Some(name))
                    && a.mux.is_none()
                    && !a.exited
            })
            .cloned();
        // A row already pane-hosted has its viewport: answer with the location
        // instead of opening a second one - but only when no live paneless row
        // answers the key, the rows reach_portal serves. Another session's row
        // is that server's to view - saying so beats the reach's no-row
        // refusal, which would lie about a row the registry knows (the inline
        // attach this verb replaced attached it regardless of hosting session).
        if live_row.is_none() {
            let hosted = self.agents.iter().find(|a| {
                (a.name == name || a.attach_id.as_deref() == Some(name)) && a.mux.is_some()
            });
            if let Some(a) = hosted {
                let (sess, pane) = a.mux.as_ref().expect("checked");
                let where_at = if sess == &self.session_name {
                    "this session; focus it in the mux".to_string()
                } else {
                    format!("session {sess}; focus it in that session's mux")
                };
                let _ = reply.send(ServerMsg::Notice {
                    text: format!("{} hosts pane {pane} in {where_at}", a.name),
                });
                return;
            }
        }
        // A claude Drive row's argv is the canonical re-entry plan,
        // resolved OFF this loop. The TUI gesture hands that wait to a live
        // client whose replay re-enters in place; this door's observer is
        // disposable and its reply is one-shot, so parking is the only honest
        // shape: the observer stays registered, the reply waits in
        // `pending_thread_reply`, and the ReentryPlanReady replay runs the
        // reach with the verdict staged and answers through
        // `finish_pending_thread_reply`. Driving the reach inline would hit
        // the plan-pending return that emits nothing, and the harvest would
        // invent "no such agent: NAME" for a row the registry knows.
        let needs_plan = matches!(&live_row, Some(r)
            if r.attach_id.is_some()
                && r.harness.as_deref() == Some("claude")
                && self.reentry_verdict.is_none());
        if needs_plan && self.pending_thread_reply.is_some() {
            // One park at a time: the observer client id is the constant
            // CONTROL_CLIENT, so a second park would trample the first. The
            // parked reach finishes within the resolver's own bound.
            let _ = reply.send(ServerMsg::Err {
                code: err_code::BAD_REQUEST,
                msg: "a portal reach is still resolving; try again in a moment".to_string(),
            });
            return;
        }
        // `--portal new` resolves to the next free index HERE, before
        // any park, so the parked replay lands where the reply says. Same
        // allocator the TUI's new-portal gesture uses. The one-park guard
        // above keeps two concurrent claude reaches from taking one index.
        let portal = if placement.portal_new {
            match self.next_free_portal() {
                Some(idx) => idx,
                None => {
                    let _ = reply.send(ServerMsg::Err {
                        code: err_code::BAD_REQUEST,
                        msg: "no free portal: every index 0-255 is seated".to_string(),
                    });
                    return;
                }
            }
        } else {
            portal
        };
        let cwd = self
            .agents
            .iter()
            .find(|a| a.name == name || a.attach_id.as_deref() == Some(name))
            .map(|a| a.cwd.clone())
            .unwrap_or_default();
        let (tx, mut rx) = mpsc::channel::<ServerMsg>(256);
        const CONTROL_CLIENT: u64 = u64::MAX;
        self.attach(
            CONTROL_CLIENT,
            0,
            0,
            cwd,
            name.to_string(),
            tx,
            DirtyMap::default(),
            Arc::new(Notify::new()),
        );
        // Drop the observer's cold-attach snapshot (layout + frames): only
        // the reach's notice is the payload, and an empty buffer guarantees it
        // is never the message a full channel drops.
        while rx.try_recv().is_ok() {}
        // The verb's index is the authoritative portal; the decoded
        // placement carries only geometry. Overwriting the portal trio keeps
        // the reach's addressing in exactly one field, the way every pre-v66
        // caller already sent it. `portal` is the resolved index by
        // here, so a `new` reach names the index the server picked.
        let mut placement = placement;
        placement.portal = Some(portal);
        placement.portal_new = false;
        placement.thread_pane = false;
        if needs_plan {
            let row = live_row.expect("needs_plan implies a live row");
            let attach_id = row.attach_id.expect("needs_plan implies an attach id");
            self.resolve_reentry(
                CONTROL_CLIENT,
                &row.name,
                "attach",
                ReentrySpawnRequest::Attach {
                    attach_id,
                    placement,
                },
            );
            self.pending_thread_reply = Some(PendingThreadReply {
                client: CONTROL_CLIENT,
                name: name.to_string(),
                portal,
                rx,
                reply,
            });
            return;
        }
        self.command(
            CONTROL_CLIENT,
            Command::AttachAgent {
                id: name.to_string(),
                placement,
            },
        );
        // Harvest the notice(s) the reach emitted and tear the observer out
        // through Gone. Every path ends in at least one; a reach that refuses
        // caller geometry AND lands ends in two, joined here so the
        // reply still carries the landing.
        let landing = Self::harvest_portal_landing(&mut rx);
        let _ = self.self_tx.try_send(CoreMsg::Gone(CONTROL_CLIENT));
        let landed = self.portal_landed(name, portal);
        let _ = reply.send(portal_reply(landed, landing, name, portal));
    }

    /// Drain an observer channel into the joined landing text: notices in
    /// arrival order, every other frame skipped.
    fn harvest_portal_landing(rx: &mut mpsc::Receiver<ServerMsg>) -> Option<String> {
        let mut landing: Option<String> = None;
        loop {
            match rx.try_recv() {
                Ok(ServerMsg::Notice { text }) => {
                    landing = Some(match landing {
                        Some(prev) => format!("{prev}; {text}"),
                        None => text,
                    });
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        landing
    }

    /// Which portal shows `pane`, DERIVED from the open portals every
    /// time the rows are built. Nothing is stored per row: the row-to-pane
    /// relation is a pointer, so a row moving between portals stays ONE row
    /// whose index changes, and no row can carry an index that has gone stale.
    ///
    /// `None` means this pane is not a portal seat - never "unknown". The
    /// comparison is EQUALITY on the seat id, and the `Option` is matched
    /// rather than tested: pane ids allocate from zero (`next_pane_id`), so
    /// pane 0 is a valid seat and a truthiness test on it is the
    /// defect that made six live workers invisible.
    pub(super) fn portal_of(&self, pane: Option<u64>) -> Option<u8> {
        let pane = pane?;
        self.portals
            .iter()
            .find(|(_, portal)| portal.seat == pane)
            .map(|(idx, _)| *idx)
    }

    /// The portal index a seat's row may wear: `None` unless the seat's key
    /// answers exactly one live row. A held seat (its row is gone) or a
    /// dropped one (the key names two, or the viewer title names no row)
    /// wears no marker, so the sideline band and the portal picker tell the
    /// truth instead of guessing.
    pub(super) fn portal_marker(&self, pane: Option<u64>) -> Option<u8> {
        let idx = self.portal_of(pane)?;
        let key = self.portals.get(&idx)?.row_key.as_str();
        let named = self
            .agents
            .iter()
            .filter(|a| row_answers_key(a, key))
            .count();
        (named == 1).then_some(idx)
    }

    /// The lowest portal index nothing LIVE holds.
    ///
    /// Server-side on purpose. A client computing this from the rows it last
    /// rendered races every other client: two of them pick the same number and
    /// the second reach repoints the first one's brand-new portal. The server
    /// handles reaches one at a time, so allocating here cannot collide.
    ///
    /// Liveness, not presence, the same read `close_pane` uses: an entry whose
    /// pane closed elsewhere is stale, and its index is free to reuse. The
    /// reach's own stale-slot path then reads the leftover entry for its
    /// remembered tab, so reusing the index lands the new viewer where the old
    /// one was.
    ///
    /// `None` means every index holds a portal whose seat is live.
    /// The old saturation at `u8::MAX` was itself an occupied index, so a
    /// full space silently REPOINTED portal 255; the caller refuses instead.
    pub(super) fn next_free_portal(&self) -> Option<u8> {
        (0..=u8::MAX).find(|idx| {
            !self
                .portals
                .get(idx)
                .is_some_and(|portal| self.panes.contains_key(&portal.seat))
        })
    }

    /// A claude portal seat follows the session its viewer's OSC title names.
    ///
    /// A claude attach TUI can switch sessions inside its own TUI (the
    /// agent-view arrow) without fno learning it, so the seat's stored row
    /// goes stale. Every 1s tick: a title naming one free row repoints
    /// `row_key`, `attached` and the pane name to it; a title naming no
    /// single free row drops the claim, so no row wears a seat that shows
    /// something else. Only claude viewer seats are followed (`entry.cmd`
    /// gates on the attach program), and a title naming a row another
    /// portal shows never steals it.
    pub(super) fn follow_portal_viewer_titles(&mut self) {
        let viewer_cmd = cmd_from_argv(&attach_base(""));
        let candidates: Vec<(u8, u64, String)> = self
            .portals
            .iter()
            .filter_map(|(idx, portal)| {
                let entry = self.panes.get(&portal.seat)?;
                if entry.cmd != viewer_cmd {
                    return None;
                }
                let title = title_session_name(entry.vt.osc_title()?);
                // A glyph-only frame ("◐", mid-spinner) names no session; a
                // seat must not unclaim onto it.
                (!title.is_empty() && title.chars().any(char::is_alphanumeric))
                    .then(|| (*idx, portal.seat, title.to_string()))
            })
            .collect();
        let mut changed = false;
        for (idx, seat, title) in candidates {
            let named: Vec<&RegistryAgent> = self
                .agents
                .iter()
                .filter(|a| {
                    a.mux.is_none()
                        && !a.exited
                        && (a.name == title || a.harness_title.as_deref() == Some(title.as_str()))
                })
                .collect();
            let claim: Option<String> = match named.as_slice() {
                [row] => row.attach_id.as_deref().and_then(|id| {
                    let free = self.live_viewer_portal(row, id, idx).is_none()
                        && match self.attached.get(id) {
                            Some(p) => *p == seat || !self.panes.contains_key(p),
                            None => true,
                        };
                    free.then(|| id.to_string())
                }),
                _ => None,
            };
            // Skip when the seat already agrees: the one row the title names
            // is the seated row with the attach mapping in place, or an
            // unclaimed seat whose key is already the title text (stable, so
            // a title naming no row costs no layout push per tick).
            let agrees = match named.as_slice() {
                [row] => {
                    row_answers_key(row, &self.portals[&idx].row_key)
                        && row
                            .attach_id
                            .as_deref()
                            .is_some_and(|id| self.attached.get(id) == Some(&seat))
                }
                _ => false,
            } || (claim.is_none()
                && self.portals[&idx].row_key == title
                && !self.attached.values().any(|p| *p == seat));
            if agrees {
                continue;
            }
            self.attached.retain(|_, p| *p != seat);
            match claim {
                Some(id) => {
                    self.attached.insert(id.clone(), seat);
                    let (_, cd) = self.attach_account_ctx(&id);
                    self.portals.get_mut(&idx).expect("candidate idx").row_key = id.clone();
                    self.name_attached_pane(seat, &id, cd.as_deref());
                }
                None => {
                    self.portals.get_mut(&idx).expect("candidate idx").row_key = title.clone();
                    if let Some(entry) = self.panes.get_mut(&seat) {
                        entry.name = Some(title.clone());
                    }
                }
            }
            self.notice_all(format!("portal {idx} now shows {title}"));
            changed = true;
        }
        if changed {
            self.push_layout(true);
        }
    }

    /// One-row-one-viewer, shared by the reach and the landed check: the
    /// index of a portal other than `skip` whose key matches `row` under
    /// `key` and whose seat runs a live viewer. A stand-in shell is not a
    /// viewer of the row: its portal is free to be repointed, so it never
    /// blocks a reach and never counts as a landing.
    fn live_viewer_portal(&self, row: &RegistryAgent, key: &str, skip: u8) -> Option<u8> {
        self.portals
            .iter()
            .find(|(idx, portal)| {
                **idx != skip
                    && row_matches_portal_key(row, key, &portal.row_key)
                    && self
                        .panes
                        .get(&portal.seat)
                        .is_some_and(|entry| entry.cmd.is_some())
            })
            .map(|(idx, _)| *idx)
    }

    /// Row-aware landed check for the portal the caller NAMED: a slot keyed
    /// by the name or by the row's attach id, never a landing reported from
    /// some other index.
    ///
    /// A focus of the portal that already shows the row is also a landing:
    /// the caller's index stayed empty because the door focused the existing
    /// viewer instead, so the named slot reads empty while the reach landed.
    /// A fresh open reported at an index the caller did not get is still a
    /// refusal.
    pub(super) fn portal_landed(&self, name: &str, portal: u8) -> bool {
        let named_slot = self.portals.get(&portal).is_some_and(|p| {
            let k = p.row_key.as_str();
            k == name
                || self.agents.iter().any(|a| {
                    (a.attach_id.as_deref() == Some(k) && a.name == name)
                        || (a.name == k && a.attach_id.as_deref() == Some(name))
                })
        });
        if named_slot {
            return true;
        }
        self.agents
            .iter()
            .filter(|a| row_answers_key(a, name))
            .any(|row| {
                self.live_viewer_portal(row, name, portal)
                    .is_some_and(|idx| self.session.find_pane(self.portals[&idx].seat).is_some())
            })
    }

    /// Finish a parked control-door reach: harvest what the replayed reach
    /// emitted, tear the observer out through Gone, and answer the held
    /// reply with the reach's own verdict. The parked observer is the only
    /// thing the resolver's verdict can still land on, so this runs in BOTH
    /// ReentryPlanReady arms - a refused plan is a notice here, and the join
    /// reads it as the (false, Some) refusal it is.
    pub(super) fn finish_pending_thread_reply(&mut self, pending: PendingThreadReply) {
        let PendingThreadReply {
            client,
            name,
            portal,
            mut rx,
            reply,
        } = pending;
        let landing = Self::harvest_portal_landing(&mut rx);
        let _ = self.self_tx.try_send(CoreMsg::Gone(client));
        let landed = self.portal_landed(&name, portal);
        let _ = reply.send(portal_reply(landed, landing, &name, portal));
    }
}
