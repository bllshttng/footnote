//! How `fno mux workspace restore` walks its members : the split
//! (classify on the core loop, resolve claude re-entry plans off it) and
//! the bulk apply half that walks every candidate through
//! [`super::Core::resume_one`]. Extracted from server.rs under the
//! file-budget gate - the code the change touched moves with it.

use super::*;

impl super::Core {
    /// `fno mux workspace restore`, phase 1: split the run. A dry
    /// run never resolves plans (it reports classifications and spawns
    /// nothing), and a run with no claude members needs none, so both apply
    /// inline. Otherwise the claude members' re-entry plans resolve OFF the
    /// core loop (the BatchPlansReady shape) and the apply half re-enters
    /// with the verdicts in hand - no bare claude resume on this axis, and
    /// no per-member resolver wait landing on the loop.
    pub(super) fn workspace_restore_start(
        &mut self,
        dry_run: bool,
        harness: Option<String>,
        reply: ControlReply,
    ) {
        // The off-loop registry reader ticks independently and may never have
        // run in a session nobody has watched (a headless reboot-restore).
        // This verb's classification IS a registry read, so it reads the file
        // itself rather than refuse members whose rows exist on disk.
        if let Some(rows) = restore_registry_rows() {
            self.agents = rows;
        }
        if dry_run {
            self.workspace_restore_apply(
                true,
                harness,
                HashMap::new(),
                Ok(revival_gate::unbounded_headroom()),
                reply,
            );
            return;
        }
        let claude_names: Vec<String> = self
            .restore_candidates(harness.as_deref())
            .into_iter()
            .filter(|(_, m)| m.harness.as_deref() == Some("claude"))
            .map(|(name, _)| name)
            .collect();
        let portal_names = portal_reach::portals_needing_claude_plan(self);
        // Tests without the gate override keep the bulk restore synchronous
        // and subprocess-free; every real run (and every test that asks the
        // gate) probes OFF the core loop below.
        #[cfg(test)]
        if !revival_gate::gate_ask_live() && claude_names.is_empty() && portal_names.is_empty() {
            self.workspace_restore_apply(
                false,
                harness,
                HashMap::new(),
                Ok(revival_gate::unbounded_headroom()),
                reply,
            );
            return;
        }
        let core_tx = self.self_tx.clone();
        tokio::spawn(async move {
            let headroom = revival_gate::probe_headroom().await;
            let plans = if claude_names.is_empty() && portal_names.is_empty() {
                HashMap::new()
            } else {
                agent_actions::resolve_restore_plans(claude_names, portal_names).await
            };
            let _ = core_tx
                .send(CoreMsg::WorkspaceRestoreApply {
                    dry_run,
                    harness,
                    plans,
                    headroom,
                    reply,
                })
                .await;
        });
    }

    /// `fno mux workspace restore`, phase 2: walk every candidate
    /// through [`super::Core::resume_one`] and report one row per member. A claude
    /// member whose plan refused (or never resolved) refuses with the
    /// resolver's own reason - never a bare claude resume - and
    /// `reentry_verdict` is cleared around every attempt so one member's
    /// verdict can never leak into the next one's argv.
    pub(super) fn workspace_restore_apply(
        &mut self,
        dry_run: bool,
        harness: Option<String>,
        mut plans: HashMap<String, Result<ReentryVerdict, String>>,
        mut headroom: Result<revival_gate::ProbeHeadroom, String>,
        reply: ControlReply,
    ) {
        use self::portal_reach::RESTORE_CLIENT;
        let candidates = self.restore_candidates(harness.as_deref());
        // A worker NAME the store holds more than once (distinct sessions,
        // one display name - a supported state) refuses up front instead of
        // reaching resume_one: the second twin would find the first one's
        // pane through the name-only map and report "focused" while its own
        // session was never restored.
        let mut name_counts: HashMap<String, usize> = HashMap::new();
        for (name, _) in &candidates {
            *name_counts.entry(name.clone()).or_default() += 1;
        }
        let dims = (crate::vt::DEFAULT_ROWS, crate::vt::DEFAULT_COLS);
        // A reap receipt is the session's death record: a member it preserves
        // must not read as a live restore candidate, or restore resurrects a
        // session the fleet deliberately retired.
        let retired = crate::restore_gate::retired_receipt_session_ids().unwrap_or_default();
        let mut rows = Vec::with_capacity(candidates.len());
        for (name, member) in candidates {
            if let Some(reason) =
                crate::restore_gate::retired_refusal(member.harness_session_id.as_deref(), &retired)
            {
                rows.push(restore_route_gate::refused_row(
                    name,
                    member.harness.clone(),
                    reason,
                ));
                continue;
            }
            if name_counts.get(name.as_str()).copied().unwrap_or(0) > 1 {
                rows.push(restore_route_gate::refused_row(
                    name,
                    member.harness.clone(),
                    "member name is ambiguous in the store; resume by exact session id".into(),
                ));
                continue;
            }
            let harness_name = member.harness.clone();
            // a routed codex member refuses before any staging. A pane
            // spawn's only env channel is an argv prefix (visible in ps), so it
            // cannot carry the route's key; `fno agents resume` is the door
            // that restores the route, and the member is marked refused here.
            if harness_name.as_deref() == Some("codex") {
                let row = self.agents.iter().find(|a| {
                    agent_harness_session_id(a)
                        == member
                            .harness_session_id
                            .as_deref()
                            .filter(|s| !s.is_empty())
                });
                if let Some(reason) =
                    row.and_then(|row| restore_route_gate::member_routed_codex_refusal(row, &name))
                {
                    rows.push(restore_route_gate::refused_row(name, harness_name, reason));
                    continue;
                }
            }
            // A claude member without a resolvable plan refuses here instead
            // of firing a stray off-loop resolution from the bulk path; the
            // single gesture keeps its own replay behavior.
            if !dry_run && harness_name.as_deref() == Some("claude") {
                match plans.get(&name) {
                    Some(Ok(_)) => {
                        self.reentry_verdict = plans.remove(&name).and_then(|r| r.ok());
                    }
                    Some(Err(reason)) => {
                        rows.push(restore_route_gate::refused_row(
                            name,
                            harness_name,
                            reason.clone(),
                        ));
                        continue;
                    }
                    None => {
                        rows.push(restore_route_gate::refused_row(
                            name,
                            harness_name,
                            "claude re-entry plan unresolved; resume it from the agent panel"
                                .into(),
                        ));
                        continue;
                    }
                }
            }
            // A dead gate refuses every remaining candidate; the row keeps
            // the gate's own message. This sits after the cheap refusals so
            // a retired or ambiguous member keeps its specific reason.
            if let Err(reason) = &headroom {
                rows.push(restore_route_gate::refused_row(
                    name,
                    harness_name,
                    reason.clone(),
                ));
                continue;
            }
            let structural = member_structural_refusal(&member);
            // Pre-stage the sync render so the non-claude arm never
            // fires the off-loop resolution per bulk member: bulk restore
            // stays byte-identical to before.
            if harness_name.as_deref().is_some_and(|h| h != "claude") {
                self.staged_resume_argv = resume_argv_for(
                    harness_name.as_deref().unwrap_or(""),
                    member.harness_session_id.as_deref().unwrap_or(""),
                )
                .ok();
            }
            // Only a member with headroom left stages an admission; a
            // member at zero reaches resume_one unarmed, so its refusal
            // below is the choke's own "spawn gate not asked".
            let spends = !dry_run && matches!(&headroom, Ok(h) if h.left > 0);
            if spends {
                self.revival_admission = Some((name.clone(), std::time::Instant::now()));
            }
            let outcome =
                self.resume_one(&name, Some(member), RESTORE_CLIENT, (0, 0), dims, dry_run);
            self.revival_admission = None;
            self.staged_resume_argv = None;
            self.reentry_verdict = None;
            // Only a resumed member spends: a focused member started no
            // worker, and a refused one reached no pane at all.
            if matches!(outcome, ResumeOutcome::Resumed { .. }) {
                if let Ok(h) = &mut headroom {
                    h.left = h.left.saturating_sub(1);
                }
            }
            let row = match outcome {
                ResumeOutcome::Resumed {
                    pane,
                    squad,
                    tab,
                    notice,
                } => RestoreRow {
                    member: name,
                    harness: harness_name,
                    squad,
                    portal: None,
                    outcome: "resumed".into(),
                    pane: Some(pane),
                    tab: Some(tab),
                    reason: None,
                    notice,
                },
                ResumeOutcome::Focused { pane, squad, tab } => RestoreRow {
                    member: name,
                    harness: harness_name,
                    squad,
                    portal: None,
                    outcome: "focused".into(),
                    pane: Some(pane),
                    tab: Some(tab),
                    reason: None,
                    notice: None,
                },
                ResumeOutcome::Refused { reason } => {
                    // A member the headroom spent reaches the choke unarmed;
                    // its refusal becomes the rerun sentence with the
                    // probe's own numbers. A member refused for its own
                    // structural gap keeps that specific reason.
                    if let (Ok(h), true) = (&headroom, reason.contains("spawn gate not asked")) {
                        if h.left == 0 && !dry_run {
                            rows.push(restore_route_gate::refused_row(
                                name,
                                harness_name,
                                rerun_sentence(h),
                            ));
                            continue;
                        }
                    }
                    // The member's own structural gap outranks the generic
                    // gesture notice in the REPORT: a no-form harness or a
                    // `missing` session id is the specific reason AC5-ERR
                    // demands. The gates themselves already ran.
                    restore_route_gate::refused_row(
                        name,
                        harness_name,
                        structural.unwrap_or(reason),
                    )
                }
                ResumeOutcome::PlanPending => restore_route_gate::refused_row(
                    name,
                    harness_name,
                    "claude re-entry plan unresolved; resume it from the agent panel".into(),
                ),
                ResumeOutcome::Planned => RestoreRow {
                    member: name,
                    harness: harness_name,
                    squad: 0,
                    portal: None,
                    outcome: "planned".into(),
                    pane: None,
                    tab: None,
                    reason: None,
                    notice: None,
                },
            };
            rows.push(row);
        }
        rows.extend(portal_reach::portal_restore_rows(self, dry_run, &mut plans));
        let resumed = rows.iter().filter(|r| r.outcome == "resumed").count();
        if resumed > 0 {
            self.push_layout(true);
        }
        if !dry_run {
            // Every refusal reaches the attached clients by name (AC5-ERR),
            // and a zero-inclusive summary answers "did restore do anything"
            // without reading a pane count.
            for row in rows.iter().filter(|r| r.outcome == "refused") {
                self.notice_all(format!(
                    "workspace restore: {} could not be resumed: {}",
                    row.member,
                    row.reason.as_deref().unwrap_or("no reason given"),
                ));
            }
            let focused = rows.iter().filter(|r| r.outcome == "focused").count();
            let refused = rows.iter().filter(|r| r.outcome == "refused").count();
            self.notice_all(format!(
                "workspace restore: {resumed} resumed, {focused} focused, {refused} refused"
            ));
        }
        let _ = reply.send(ServerMsg::WorkspaceRestored { rows });
    }
}

/// The at-cap refusal a member reads when the probe granted no headroom
/// and the choke stopped its spawn: the probe's own numbers plus the
/// rerun remedy.
fn rerun_sentence(h: &revival_gate::ProbeHeadroom) -> String {
    format!(
        "spawn gate: {} of max_live {} live; rerun fno mux workspace restore when a worker finishes",
        h.slots, h.cap
    )
}
