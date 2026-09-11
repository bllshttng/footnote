//! Which rows exist in the mux UI, and what each row's paint
//! verdict is. The row-set derivation and its receipt live here, named by
//! the question they answer; the file they left is shrink-only under the
//! file-budget gate. The module is a child of `server`, so the private
//! helpers the walk reads (`compose_subline`, `truth_basis`, `portal_of`,
//! ...) stay where they are.

use crate::proto::{AgentRow, AgentRowReceipt};

use super::*;

impl Core {
    pub(crate) fn agent_rows(&self) -> Vec<AgentRow> {
        let mut out = Vec::new();
        // Which registry agents a pane row already claimed (so they don't
        // double-render as watch-only). Indexed like `self.agents`.
        let mut consumed = vec![false; self.agents.len()];
        // Holder name -> pr_number, joining the live-claim holders map
        // (node -> holder) with the graph's node -> pr map. The row-name join
        // below is primary; this remains the fallback for harness-native claims
        // whose holder equals the worker name.
        let pr_by_holder: HashMap<&str, u64> = self
            .backlog_holders
            .iter()
            .filter_map(|(node, holder)| self.backlog_pr.get(node).map(|pr| (holder.as_str(), *pr)))
            .collect();
        let pr_from_name = |name: &str| -> Option<u64> {
            let node_id = agents_view::resolve_node_id(name, &self.backlog_pr)?;
            self.backlog_pr.get(&node_id).copied()
        };
        // A paneless row whose name resolves to a node inside an active
        // mission is grouped under that mission's synthetic squad, taking
        // precedence over the owns_path fallback below. A pane-hosted row
        // keeps its real session squad (it lives in an actual tab tree).
        let mission_squad_for = |name: &str| -> Option<u64> {
            let node_id = agents_view::resolve_node_id(name, &self.missions.node_to_epic)?;
            self.missions
                .node_to_epic
                .get(&node_id)
                .map(|epic| crate::mission_squad::mission_sid(epic))
        };

        // 1. Pane rows: one per live tab leaf, deterministic (squad -> tab ->
        //    pane order). Iterating the tree (not `self.agents`) is what makes a
        //    bare shell pane a first-class row.
        for squad in &self.session.squads {
            for tab in &squad.tabs {
                for pid in tree::leaves(&tab.root) {
                    // The registry entry hosting this pane, if any: the join
                    // lives in agent_rows_join (extracted from this
                    // budget-capped file); its module doc carries the
                    // recycled-pane-id rationale and the total order.
                    let matched = agent_rows_join::bind_agent_to_pane(
                        &self.agents,
                        &self.session_name,
                        pid,
                        &self.attached,
                        &|a| self.worker_pane_for_agent(a),
                    );
                    // One lookup: liveness AND the bare-pane label read the same
                    // entry (a tree leaf reaped from `panes` is dying, so it
                    // forces `exited` - the fact-beats-report rule the old join
                    // used).
                    let pane_entry = self.panes.get(&pid);
                    let pane_dead = pane_entry.is_none();
                    let row = match matched {
                        Some(i) => {
                            consumed[i] = true;
                            let a = &self.agents[i];
                            let exited = a.exited || pane_dead;
                            // A confirmed-gone pane is its own corroboration
                            // (fact beats badge) even when the registry row's
                            // own liveness read is Unmeasured -- only render
                            // the softer glyph when nothing here corroborates
                            // the exit at all.
                            let unmeasured = exited
                                && !pane_dead
                                && a.liveness == agents_view::Liveness::Unmeasured;
                            AgentRow {
                                harness: a.harness.clone(),
                                model: a.model.clone(),
                                route: a.route.clone(),
                                spawned_by_session: a.spawned_by_session.clone(),
                                harness_session_id: a.harness_session_id.clone(),
                                squad: Some(squad.id),
                                name: a.name.clone(),
                                pane_id: Some(pid),
                                // Derived every build from the open portals; the row
                                // stores no index of its own.
                                portal: self.portal_of(Some(pid)),
                                badge: if exited { None } else { a.badge },
                                reason: if exited { None } else { a.reason.clone() },
                                exited,
                                dnd: a.dnd,
                                unmeasured,
                                liveness_measured_at: a.liveness_measured_at,
                                harness_title: a.harness_title.clone(),
                                answerable: if exited { None } else { a.answerable.clone() },
                                // A pane-hosted row focuses its pane; the attach
                                // target never rides it (wire contract).
                                attach_id: None,
                                external: a.external,
                                tab: Some(tab.id),
                                seen: self.seen.contains(&pid),
                                // (x-6851 US3) cwd basename on every row so the
                                // sideline can flag a foreign-cwd join.
                                cwd_base: cwd_basename(&a.cwd),
                                tombstone: false,
                                subline: subline_with_title(a, self.compose_subline(&a.cwd)),
                                // Structural roster-dir tag wins (Locked
                                // Decision 6); else this pane's birth account.
                                account: a
                                    .account
                                    .clone()
                                    .or_else(|| pane_entry.and_then(|e| e.account.clone())),
                                updated_at: a.updated_at,
                                pr: pr_from_name(&a.name)
                                    .or_else(|| pr_by_holder.get(a.name.as_str()).copied()),
                                tail: self.compose_tail(a),
                                crown_level: a.crown_level,
                                crown_scope: a.crown_scope.clone(),
                                basis: self.truth_basis(a),
                                last_activity_age_s: self.truth_age(a),
                                resumable: false,
                                no_pane_reason: None,
                                // A registry-hosted pane's badge is its primary
                                // signal, but the vt reading is still real and
                                // one field away: keep it so the client can
                                // show activity when the badge goes quiet.
                                pane_activity: pane_entry.map(|e| e.vt.shell_activity()),
                                // (x-07c2) Decorative on a pane-hosted row (its
                                // reach focuses the pane); carried so the field
                                // never lies about the row's capability.
                                reach: agents_view::thread_reach(
                                    a.harness.as_deref(),
                                    a.attach_id.as_deref(),
                                ),
                            }
                        }
                        None => {
                            // Bare pane: labelled from its own entry (node > cmd
                            // > cwd-basename > "shell"), matching the navigator's
                            // pane labels (v22) so the two agree.
                            let e = pane_entry;
                            AgentRow {
                                harness: None,
                                model: None,
                                route: None,
                                spawned_by_session: None,
                                harness_session_id: None,
                                squad: Some(squad.id),
                                name: pane_label(
                                    e.and_then(|e| e.name.as_deref()),
                                    e.and_then(|e| e.node.as_deref()),
                                    e.map(|e| e.cwd.as_str()).unwrap_or(""),
                                    e.and_then(|e| e.cmd.as_deref()),
                                ),
                                pane_id: Some(pid),
                                // Derived every build from the open portals; the row
                                // stores no index of its own.
                                portal: self.portal_of(Some(pid)),
                                badge: None,
                                reason: None,
                                exited: pane_dead
                                    || e.is_some_and(|entry| entry.refused_worker.is_some()),
                                dnd: false,
                                unmeasured: false,
                                liveness_measured_at: None,
                                harness_title: None,
                                answerable: None,
                                attach_id: None,
                                external: false,
                                tab: Some(tab.id),
                                seen: self.seen.contains(&pid),
                                cwd_base: cwd_basename(e.map(|e| e.cwd.as_str()).unwrap_or("")),
                                tombstone: false,
                                subline: self
                                    .compose_subline(e.map(|e| e.cwd.as_str()).unwrap_or("")),
                                account: e.and_then(|e| e.account.clone()),
                                // A bare pane is not a registry worker: no
                                // claim, no pr, no transcript. But not-in-
                                // registry is not is-a-shell - it can be a
                                // full agent with a live workload, so the row
                                // carries the pane's OWN vt reading and the
                                // drain-path activity stamp (x-d401).
                                pane_activity: e.map(|e| e.vt.shell_activity()),
                                last_activity_age_s: e.map(|e| e.last_output.elapsed().as_secs()),
                                updated_at: None,
                                pr: None,
                                tail: None,
                                // A bare shell pane has no registry entry, so
                                // no crown and no reachability probe either.
                                crown_level: None,
                                crown_scope: None,
                                basis: None,
                                resumable: false,
                                no_pane_reason: None,
                                reach: Reach::Locate,
                            }
                        }
                    };
                    out.push(row);
                }
            }
        }

        // 2. Watch-only appendix: registry rows no pane claimed.
        for (i, a) in self.agents.iter().enumerate() {
            if consumed[i] {
                continue;
            }
            match &a.mux {
                Some((sess, pane)) => {
                    // A row hosted in ANOTHER session is that server's to render.
                    if sess != &self.session_name {
                        continue;
                    }
                    // A same-session mux row whose pane left the tree entirely
                    // (fully reaped) is a dangling exited row - preserve the old
                    // behaviour (`find_pane` -> None squad, `exited`).
                    // (x-5f7f) A same-session mux row whose pane left the
                    // tree is DEAD: its worker's pty is gone (a pane child of
                    // this server). Render it paneless rather than dangling -
                    // there is no pane to focus, and a paneless dead row can
                    // carry `resumable`, so tapping it resumes the session
                    // through the harness's own form instead of dead-ending
                    // on a focus at a pane that no longer exists.
                    let detached = self.detached_pane_for_agent(a);
                    let detached_live = detached.is_some_and(|pane| {
                        self.panes
                            .get(&pane)
                            .is_some_and(|entry| entry.pty.is_child_alive())
                    });
                    let resumable = !detached_live && self.row_resumable_in_session(a);
                    // Attribute the row to the squad holding its recorded
                    // membership FIRST (cwd ownership only as a fallback), so
                    // the panel shows it where ResumeAgent will actually place
                    // the pane - the two lookups must agree.
                    let squad = self
                        .member_squad_for_agent(a)
                        .or_else(|| self.session.find_by_cwd(&a.cwd));
                    out.push(AgentRow {
                        harness: a.harness.clone(),
                        model: a.model.clone(),
                        route: a.route.clone(),
                        spawned_by_session: a.spawned_by_session.clone(),
                        harness_session_id: a.harness_session_id.clone(),
                        squad,
                        name: a.name.clone(),
                        pane_id: None,
                        // No pane, so no portal seat. Absent means NOT shown
                        // through a portal, never "unknown".
                        portal: None,
                        badge: detached_live.then_some(a.badge).flatten(),
                        reason: detached_live.then(|| a.reason.clone()).flatten(),
                        exited: if detached.is_some() {
                            !detached_live
                        } else {
                            true
                        },
                        dnd: a.dnd,
                        unmeasured: false,
                        liveness_measured_at: None,
                        harness_title: a.harness_title.clone(),
                        answerable: None,
                        attach_id: None,
                        external: a.external,
                        tab: None,
                        seen: self.seen.contains(pane),
                        cwd_base: cwd_basename(&a.cwd),
                        tombstone: false,
                        subline: subline_with_title(a, self.compose_subline(&a.cwd)),
                        account: a.account.clone(),
                        updated_at: a.updated_at,
                        pr: pr_from_name(&a.name)
                            .or_else(|| pr_by_holder.get(a.name.as_str()).copied()),
                        tail: self.compose_tail(a),
                        crown_level: a.crown_level,
                        crown_scope: a.crown_scope.clone(),
                        basis: self.truth_basis(a),
                        last_activity_age_s: self.truth_age(a),
                        resumable,
                        no_pane_reason: if detached_live {
                            Some(AgentNoPaneReason::LivePaneless)
                        } else {
                            self.row_no_pane_reason_in_session(a)
                        },
                        // Dangling dead: the pane is gone, so no vt reading.
                        pane_activity: None,
                        reach: Reach::Locate,
                    })
                }
                None => {
                    // Truly paneless (bg/headless/daemon/roster). Its attach map
                    // pointed at no live pane (else a pane row claimed it), so it
                    // stays watch-only attachable - the AC1-FR revert.
                    let squad = self
                        .member_squad_for_agent(a)
                        .or_else(|| mission_squad_for(&a.name))
                        .or_else(|| self.session.find_by_cwd(&a.cwd));
                    // (x-6851 US3) Every row carries its cwd basename: an orphan
                    // uses it for the `~ elsewhere` disambiguation suffix
                    // (x-0090 AC2-UI), a squad-matched row for the foreign-cwd
                    // exception subline.
                    let cwd_base = cwd_basename(&a.cwd);
                    out.push(AgentRow {
                        harness: a.harness.clone(),
                        model: a.model.clone(),
                        route: a.route.clone(),
                        spawned_by_session: a.spawned_by_session.clone(),
                        harness_session_id: a.harness_session_id.clone(),
                        squad,
                        name: a.name.clone(),
                        pane_id: None,
                        portal: None,
                        badge: if a.exited { None } else { a.badge },
                        reason: if a.exited { None } else { a.reason.clone() },
                        exited: a.exited,
                        dnd: a.dnd,
                        unmeasured: a.liveness == agents_view::Liveness::Unmeasured,
                        liveness_measured_at: a.liveness_measured_at,
                        harness_title: a.harness_title.clone(),
                        answerable: if a.exited { None } else { a.answerable.clone() },
                        attach_id: if a.exited { None } else { a.attach_id.clone() },
                        external: a.external,
                        tab: None,
                        // A watch-only row has no pane to focus, so it is always
                        // unseen.
                        seen: false,
                        cwd_base,
                        tombstone: false,
                        subline: subline_with_title(a, self.compose_subline(&a.cwd)),
                        // The structural roster-dir tag: an isolated-account
                        // foreign row carries its source account here (piece 3).
                        account: a.account.clone(),
                        updated_at: a.updated_at,
                        pr: pr_from_name(&a.name)
                            .or_else(|| pr_by_holder.get(a.name.as_str()).copied()),
                        tail: self.compose_tail(a),
                        crown_level: a.crown_level,
                        crown_scope: a.crown_scope.clone(),
                        basis: self.truth_basis(a),
                        last_activity_age_s: self.truth_age(a),
                        resumable: self.row_resumable_in_session(a),
                        no_pane_reason: self.row_no_pane_reason_in_session(a),
                        // Watch-only paneless: no PTY, no vt reading.
                        pane_activity: None,
                        // (x-07c2) The load-bearing site: a paneless live row's
                        // reach decides what its gesture opens. The attach_id
                        // half of the input re-reads the registry row (not the
                        // wire row's exited-gated copy) because the tier
                        // describes the SESSION's capability, while the wire's
                        // attach_id is cleared on exit for gate reasons.
                        reach: agents_view::thread_reach(
                            a.harness.as_deref(),
                            a.attach_id.as_deref(),
                        ),
                    })
                }
            }
        }
        // 3.  Tombstone members DECORATE the row their registry entry
        //    already produced; they no longer mint rows of their own. The old
        //    synthesized `cc-<attach_id>` row was the third row-set reader the
        //    two stores could disagree through: a member joining no registry
        //    row was evidence of nothing, yet it rendered a ghost under its
        //    (live) squad. The registry is the one row-set source now:
        //    a tombstoned member joining an EXITED row marks that row dimmed
        //    + dismissable (still the dimmed dismissable affordance, under the row's
        //    real name); a member joining a LIVE row never dims it (fact beats
        //    a stale tombstone - the liveness kill criterion); a member joining
        //    nothing renders nothing (a stale member; the member-retirement
        //    path removes it at restore).
        for (&sid, members) in &self.squad_members {
            if self.session.squad(sid).is_none() {
                continue;
            }
            for m in members.iter().filter(|m| m.tombstone) {
                if self.attached.contains_key(&m.attach_id) {
                    continue;
                }
                let joined = self.agents.iter().find(|a| {
                    crate::squad_store::member_joins_row(
                        m,
                        a.attach_id.as_deref(),
                        a.harness_session_id.as_deref(),
                    )
                });
                let Some(a) = joined else {
                    continue;
                };
                if !a.exited {
                    continue;
                }
                if let Some(row) = out.iter_mut().find(|r| {
                    // Match the produced row by the same session identity the
                    // join used, never by name alone: exited and live
                    // generations can share a display name, and dimming the
                    // live one is the liveness kill criterion.
                    r.name == a.name
                        && r.harness_session_id.as_deref() == a.harness_session_id.as_deref()
                }) {
                    row.tombstone = true;
                    // The dismiss affordance needs the attach target; exited
                    // rows clear it on the wire (attach-catalog gate), so the
                    // member's copy rides in here.
                    row.attach_id = Some(m.attach_id.clone());
                    // The dead member renders under the squad its membership
                    // persisted, not wherever the registry row's cwd points.
                    row.squad = Some(sid);
                }
            }
        }
        // 4. External-lifecycle tombstone rows (x-7561): a persisted external
        //    record NOT currently live renders so `x` can act on it. The state
        //    maps onto the existing `exited` flag - stopped -> `exited` (rm);
        //    failed/unknown/stopping/removing -> `!exited` (stop / stop-retry),
        //    with the state as the row reason so an in-flight action is visible
        //    (AC1-UI). Deduped against live external rows (a still-live roster
        //    row wins; the record is stale until the next reconcile clears it).
        let live_ext: std::collections::HashSet<&str> = self
            .agents
            .iter()
            .filter(|a| a.external)
            .filter_map(|a| a.attach_id.as_deref())
            .collect();
        for r in &self.external_lifecycle {
            if live_ext.contains(r.attach_id.as_str()) {
                continue;
            }
            use crate::squad_store::ExternalState as S;
            let (exited, reason) = match r.state {
                S::Stopped => (true, None),
                S::Failed => (
                    false,
                    Some(r.reason.clone().unwrap_or_else(|| "stop failed".into())),
                ),
                S::Unknown => (false, Some("state unknown".to_string())),
                S::Stopping => (false, Some("stopping…".to_string())),
                S::Removing => (false, Some("removing…".to_string())),
            };
            let squad = mission_squad_for(&r.name).or_else(|| self.session.find_by_cwd(&r.cwd));
            // (x-6851 US3) Every row carries its cwd basename - including a
            // squad-matched external-lifecycle row, so its foreign-cwd subline
            // still renders (the "every row" wire contract; codex review).
            let cwd_base = cwd_basename(&r.cwd);
            out.push(AgentRow {
                // squads.json records no lane axes (ExternalLifecycle carries
                // none), so the row renders default-colored.
                harness: None,
                model: None,
                route: None,
                spawned_by_session: None,
                harness_session_id: None,
                squad,
                name: r.name.clone(),
                pane_id: None,
                portal: None,
                badge: None,
                reason,
                exited,
                dnd: false,
                unmeasured: false,
                liveness_measured_at: None,
                harness_title: None,
                answerable: None,
                // Carried on an exited row so the client can send RemoveExternal;
                // on a live-ish row it is the StopExternal target. Either way the
                // attach-catalog gate (attach_id + !exited) never treats a stopped
                // tombstone as attachable.
                attach_id: Some(r.attach_id.clone()),
                external: true,
                tab: None,
                seen: false,
                cwd_base,
                tombstone: false,
                subline: self.compose_subline(&r.cwd),
                account: None,
                // An external row is never joined (respawn/pr/tail are
                // fno-registry concerns); its state lives in its own daemon, so
                // those cells stay EMPTY rather than inferred (AC4-ERR).
                updated_at: None,
                pr: None,
                tail: None,
                // An external-daemon row is not an fno-registry worker: no
                // crown, and its liveness lives in its own daemon, so no
                // reachability reading either.
                crown_level: None,
                crown_scope: None,
                basis: None,
                last_activity_age_s: None,
                resumable: false,
                no_pane_reason: None,
                // An external-daemon row owns no PTY of this server.
                pane_activity: None,
                // An external-lifecycle tombstone has no capability data here;
                // a live external row renders through the watch-only arm above.
                reach: Reach::Locate,
            })
        }
        out
    }

    ///  The `fno mux rows` receipt: `agent_rows` trimmed to the
    /// row-set facts, each decorated with the server's paint verdict. The
    /// verdict names what the SERVER knows: a `no_pane_reason` text, the
    /// tombstone marker, or `None` (would paint; a non-paint on screen is a
    /// client-side fold, which is client state the server cannot see). The
    /// receipt distinguishes "present but suppressed" from "absent from the
    /// published set" - the reading the plan demanded before any render fix.
    pub(crate) fn agent_rows_receipt(&self) -> Vec<AgentRowReceipt> {
        self.agent_rows()
            .into_iter()
            .map(|row| {
                let reason: Option<String> = row
                    .no_pane_reason
                    .map(|r| Self::no_pane_reason_text(r).to_string())
                    .or_else(|| {
                        row.tombstone
                            .then(|| "tombstoned member; dismissable".into())
                    })
                    .or_else(|| row.exited.then(|| "exited; renders dim".into()));
                AgentRowReceipt {
                    name: row.name.clone(),
                    harness: row.harness.clone(),
                    squad: row
                        .squad
                        .map(|sid| (sid, self.session.squad(sid).and_then(|sq| sq.name.clone()))),
                    pane: row.pane_id,
                    exited: row.exited,
                    tombstone: row.tombstone,
                    reason,
                    resumable: Some(row.resumable),
                }
            })
            .collect()
    }
}
