//! The portal reach: the `fno mux thread` control verb (`portal_ctl`) and the
//! AttachAgent portal arm it drives (`reach_portal`), extracted from the
//! parent under the file-budget gate - the code the change touched moves with
//! it.

use super::*;

pub(super) fn row_matches_portal_key(row: &RegistryAgent, key: &str, portal_key: &str) -> bool {
    portal_key == key || row.attach_id.as_deref() == Some(portal_key) || portal_key == row.name
}

impl Core {
    /// (x-07c2) Reach `key` (an attach id for a claude row, a registry name
    /// for every other harness) through portal `portal`. The tier is
    /// capability-computed (`agents_view::thread_reach`): Drive runs the
    /// account-wrapped attach argv, Follow tails the transcript with
    /// `fno agents peek --follow`, Locate renders the self-teaching screen.
    ///
    /// (x-8f9d) Resolution order is per INDEX, and every arm touches only its
    /// own portal: no portal at `portal` opens one through the ordinary
    /// placement path; a portal at `portal` on another row repoints it in
    /// place (the open-here mechanic: spawn-first, `tree::replace_leaf`,
    /// reap-last - the geometry never moves); a portal at `portal` on this row
    /// focuses it; a recorded pane the tree no longer knows reads as absent.
    /// NEVER persists a squad member and is never rebuilt by restore: a pane
    /// binds a session to geometry, a thread binds a session to a row.
    ///
    /// (x-9b60) The geometry decision lives HERE, after the slot lookup that
    /// knows whether index N is occupied - not at the decode edge, which
    /// cannot see occupancy. `here` is refused in both cases (a portal mints
    /// its own seat pane; open-here repoints the sender's focused pane).
    /// Everything else the caller named is IGNORED, visibly, when the portal
    /// already has a live seat (a portal owns its geometry; x-d545's
    /// remembered tab steers the replacement), and HONORED on a fresh open,
    /// where there is no geometry to own yet.
    pub(super) fn reach_portal(
        &mut self,
        client_id: u64,
        view: (u64, TabId),
        vp: Rect,
        portal_idx: u8,
        key: &str,
        placement: &PanePlacement,
    ) -> Flow {
        // (x-9b60) open-here is never a portal, in either case below. Refused
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
        let mut hits = self.agents.iter().filter(|a| {
            a.mux.is_none() && !a.exited && (a.attach_id.as_deref() == Some(key) || a.name == key)
        });
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
                self.notice(client_id, "no such agent");
                return Flow::Continue;
            }
        };
        // (x-8f9d) ONE ROW, ONE VIEWER. A reach for a row that ANOTHER portal
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
        if let Some((other_idx, other_seat, other_tab)) = self
            .portals
            .iter()
            .find(|(idx, portal)| {
                **idx != portal_idx
                    && row_matches_portal_key(&row, key, &portal.row_key)
                    // A stand-in shell is not a viewer of the row: its portal
                    // is free to be repointed, so it never blocks this reach.
                    && self
                        .panes
                        .get(&portal.seat)
                        .is_some_and(|entry| entry.cmd.is_some())
            })
            .map(|(idx, portal)| (*idx, portal.seat, portal.tab))
        {
            match self.session.find_pane(other_seat) {
                Some((sid, _)) => {
                    // (x-9b60) This focus ignores caller geometry; saying so
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
                // (x-d285) The Drive argv is the canonical re-entry plan for a
                // claude row. `None` means the plan is resolving off-loop; the
                // replay carries a portal placement, which re-lands in this
                // reach with the verdict staged. (x-8f9d) It names THIS
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
        // (x-d545) The seat's tab id, kept out of the stale-seat paths: a
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
                    // (x-d545) A same-row reach is a focus only when the seat
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
                        // seat keeps its place), visibly (x-9b60).
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
                    // (x-9b60) The repoint keeps the portal's geometry - the
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
                    self.notice(client_id, format!("thread pane -> {}", row.name));
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
        // (x-d545) A remembered seat tab (from the stale slot above) still
        // means something: when it survives, land the fresh viewer THERE, so
        // replacing a dead or displaced viewer never strands the viewport in
        // whatever tab the client happens to be viewing. Any miss - no
        // remembered tab, squad or tab gone - falls back to today's routing.
        //
        // (x-9b60) With no surviving remembered tab, a fresh open has no
        // geometry to own yet, so the CALLER's placement is honored instead
        // of a server guess (AC1-HP). The remembered tab still wins over a
        // caller tab: x-d545's precedence must not regress (AC3-REG).
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
        self.notice(client_id, format!("thread pane -> {}", row.name));
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

    /// (x-07c2) The outside-the-TUI reach (`fno agents attach` with a live
    /// mux), run as the exact command a TUI reach runs: a synthetic OBSERVER
    /// client (0,0 - read-only, no squad or PTY of its own) whose reliable
    /// channel collects the notices, then the real AttachAgent portal
    /// command. One implementation, two doors, no drift; the observer is
    /// removed through the same Gone path a Detach takes, and the reply is
    /// the landing notice on success or the refusal's Err.
    ///
    /// (x-8f9d) `portal` is the index the caller named (`--portal N`, default
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
        // never pick, same as reach_portal's own guard.
        let mut named_hits = self
            .agents
            .iter()
            .filter(|a| a.name == name || a.attach_id.as_deref() == Some(name));
        if let (Some(_), Some(_)) = (named_hits.next(), named_hits.next()) {
            let _ = reply.send(ServerMsg::Err {
                code: err_code::BAD_REQUEST,
                msg: "more than one row goes by that name - reach it by its pane".to_string(),
            });
            return;
        }
        // A row already pane-hosted has its viewport: answer with the location
        // instead of opening a second one. Another session's row is that
        // server's to view - saying so beats the reach's "no such agent",
        // which would lie about a row the registry knows (the inline attach
        // this verb replaced attached it regardless of hosting session).
        let hosted = self
            .agents
            .iter()
            .find(|a| (a.name == name || a.attach_id.as_deref() == Some(name)) && a.mux.is_some());
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
        // (x-9b60) The verb's index is the authoritative portal; the decoded
        // placement carries only geometry. Overwriting the portal trio keeps
        // the reach's addressing in exactly one field, the way every pre-v66
        // caller already sent it.
        let mut placement = placement;
        placement.portal = Some(portal);
        placement.portal_new = false;
        placement.thread_pane = false;
        self.command(
            CONTROL_CLIENT,
            Command::AttachAgent {
                id: name.to_string(),
                placement,
            },
        );
        // Harvest the notice(s) the reach emitted and tear the observer out
        // through Gone. Every path ends in at least one; a reach that refuses
        // caller geometry AND lands (x-9b60) ends in two, joined here so the
        // reply still carries the landing.
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
        let _ = self.self_tx.try_send(CoreMsg::Gone(CONTROL_CLIENT));
        // Row-aware, not key-aware: a focus on a portal keyed by the attach id
        // (the TUI door) reached through the registry name (this door) leaves
        // it keyed by the attach id - that is a landing, not a refusal.
        // (x-8f9d) Read the portal the caller named, not any portal: landing
        // in a DIFFERENT index would be a refusal reported as success.
        let landed = self.portals.get(&portal).is_some_and(|p| {
            let k = p.row_key.as_str();
            k == name
                || self.agents.iter().any(|a| {
                    (a.attach_id.as_deref() == Some(k) && a.name == name)
                        || (a.name == k && a.attach_id.as_deref() == Some(name))
                })
        });
        let msg = match (landed, landing) {
            (true, Some(text)) => ServerMsg::Notice { text },
            (true, None) => ServerMsg::Notice {
                text: format!("thread pane -> {name}"),
            },
            (false, Some(text)) => ServerMsg::Err {
                code: err_code::BAD_REQUEST,
                msg: text,
            },
            (false, None) => ServerMsg::Err {
                code: err_code::BAD_REQUEST,
                msg: format!("no such agent: {name}"),
            },
        };
        let _ = reply.send(msg);
    }
}
