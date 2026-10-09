//! The script path's pane spawn (`pane run`): target resolution and
//! admission on the core loop, the spawn off it, placement back on it.

use super::pane_spawn::{SpawnOrder, KEEPER_ROAD};
use super::*;

/// What `pane run` resolved before the spawn, carried to its placement.
pub(super) struct RunPanePlan {
    squad_key: String,
    cwd: String,
    placement: PanePlacement,
    worker: Option<String>,
    dest: Option<u64>,
    create_name: Option<String>,
}

/// The receipt fields `pane run` echoes, captured before the placement moves.
pub(super) struct RunPaneReceipt {
    wants: bool,
    anchor: Option<u64>,
    direction: Option<Dir>,
    fallback: PlacementFallback,
}

impl RunPaneReceipt {
    /// ANY selector placement (tab or anchor) gets the receipt: the bounded
    /// pane lane verifies placement by re-reading `pane ls`, and this receipt
    /// is the only record of where the server actually committed the pane.
    pub(super) fn of(placement: &PanePlacement) -> Self {
        RunPaneReceipt {
            wants: placement.tab.is_some() || placement.at.is_some(),
            anchor: placement.at,
            direction: placement.split,
            fallback: placement.fallback,
        }
    }
}

impl Core {
    /// The script path's spawn (`pane run`): validate and resolve the
    /// target on the loop, spawn off it, and hand the placed pane id (or the
    /// refusal) to `tail` on the loop. A refusal before the spawn calls
    /// `tail` at once.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_pane_then(
        &mut self,
        squad_key: String,
        cwd: String,
        argv: Vec<String>,
        rows: u16,
        cols: u16,
        claim: bool,
        placement: PanePlacement,
        worker: Option<String>,
        tail: impl FnOnce(&mut Core, Result<u64, (u32, String)>) -> Flow + Send + 'static,
    ) -> Flow {
        match self.run_pane_prepare(squad_key, cwd, argv, rows, cols, placement, worker) {
            Ok((order, plan)) => self.spawn_then(order, None, move |core, spawned| {
                let placed = match spawned {
                    Ok(pid) => core.run_pane_place(pid, claim, plan),
                    Err(e) => Err((err_code::SPAWN_FAILED, e)),
                };
                tail(core, placed)
            }),
            Err(refusal) => tail(self, Err(refusal)),
        }
    }

    /// Everything `pane run` decides before a pane exists. A bad target
    /// refuses here, with no pane.
    #[allow(clippy::too_many_arguments)]
    fn run_pane_prepare(
        &mut self,
        squad_key: String,
        cwd: String,
        argv: Vec<String>,
        rows: u16,
        cols: u16,
        placement: PanePlacement,
        worker: Option<String>,
    ) -> Result<(SpawnOrder, RunPanePlan), (u32, String)> {
        let mut placement = placement;
        placement.max_panes = Some(crate::process_admission::configured_pane_group_max(
            placement.max_panes,
        ));
        // The worker name reaches the store and later keys a resume,
        // so the SERVER re-validates it before any pane exists - the CLI gate
        // covers one caller, the control socket is reachable by any client.
        if let Some(name) = &worker {
            if !crate::squad_store::valid_worker_name(name) {
                return Err((
                    err_code::BAD_REQUEST,
                    "worker name must be a registry name ([A-Za-z0-9._-], <=64 chars)".into(),
                ));
            }
        }
        if let Some(refusal) = placement_fit::refuse_fit_with_geometry(&placement) {
            return Err(refusal);
        }
        // Create-if-absent lives ONLY here on the script path (Locked 7): a `pane run --squad
        // <name>` for a not-yet-existing squad mints one so lanes group by project; AttachAgent / UI targets
        // stay fail-closed. Only an UNKNOWN name is creatable (blank / unknown id still error). Resolved
        // pre-spawn so a bad target refuses with no pane.
        let (dest, create_name): (Option<u64>, Option<String>) = match &placement.target {
            PaneTarget::SquadName(name) => {
                let n = name.trim();
                if n.is_empty() {
                    return Err((
                        err_code::BAD_REQUEST,
                        "workspace name cannot be blank".into(),
                    ));
                }
                match self.resolve_placement_target(&placement.target, None) {
                    Ok(d) => (d, None),
                    // Coupled to resolve_placement_target's error text: a name matching NO squad is
                    // creatable; an ambiguous name (2+ matches) still errors - never silently pick one.
                    Err(e) if e.starts_with("no such workspace") => (None, Some(n.to_string())),
                    Err(e) => return Err((err_code::BAD_REQUEST, e)),
                }
            }
            _ => {
                // `CurrentRoute` here defaults to the squad owning the spawn's
                // CWD, not the active/gazed-at squad that `resolve_squad`
                // (the tab and layout verbs) passes for the same token. Both
                // defaults are deliberate; the pair is not interchangeable.
                let current = self.session.find_by_cwd(&squad_key);
                let dest = self
                    .resolve_placement_target(&placement.target, current)
                    .map_err(|e| (err_code::BAD_REQUEST, e))?;
                (dest, None)
            }
        };
        let pane_count = self.placement_pane_count(dest, &placement);
        let permit = crate::process_admission::admit_tab(pane_count, placement.max_panes)
            .map_err(|e| (err_code::SPAWN_FAILED, e.to_string()))?;
        // The worker path is the keeper path: a recorded member's pane
        // outlives this server. Everything else spawns inline.
        let theme = osc_reply::theme_at(&cwd);
        let mut spawn_argv = argv.clone();
        if let Some(worker) = worker.as_deref() {
            if agent_self_from_argv(&spawn_argv).is_none() {
                let mut wrapped = vec![
                    "env".to_string(),
                    format!("COLORFGBG={}", osc_reply::colorfgbg(&theme)),
                    format!("FNO_AGENT_SELF={worker}"),
                ];
                wrapped.extend(spawn_argv);
                spawn_argv = wrapped;
            }
        }
        // A claude pane on a light ground launches with the plugin's shipped
        // footnote-paper theme: the OSC 11 answer resolves `auto` to light,
        // and the custom theme keeps the dark prompt band the stock light
        // theme loses. `--settings` is per-session; settings.json is never
        // touched. Skipped when the argv already names a settings file - a
        // duplicate flag would let the theme blob win and drop the user's
        // file (parsers take the last occurrence). Appended last: claude's
        // parser takes flags after any positional, and the spawn argv is
        // never a shell string.
        if argv_runs_claude(&spawn_argv)
            && crate::theme::is_light(&theme)
            && !spawn_argv
                .iter()
                .any(|a| a == "--settings" || a.starts_with("--settings="))
        {
            spawn_argv.push("--settings".to_string());
            spawn_argv.push("{\"theme\":\"custom:fno:footnote-paper\"}".to_string());
        }
        // Every spawned pane takes the keeper road in production, worker or
        // not; unit fixtures keep today's split (short-lived fixtures can
        // exit before a keeper answers Identify).
        let keeper = KEEPER_ROAD || worker.is_some();
        let order = SpawnOrder {
            argv: spawn_argv,
            rows,
            cols,
            cwd: cwd.clone(),
            permit,
            keeper,
        };
        Ok((
            order,
            RunPanePlan {
                squad_key,
                cwd,
                placement,
                worker,
                dest,
                create_name,
            },
        ))
    }

    /// The tail after the spawn landed: claim eligibility, then placement
    /// (minting a create-if-absent squad), then the layout push.
    fn run_pane_place(
        &mut self,
        pid: u64,
        claim: bool,
        plan: RunPanePlan,
    ) -> Result<u64, (u32, String)> {
        let RunPanePlan {
            squad_key,
            cwd,
            placement,
            worker,
            mut dest,
            mut create_name,
        } = plan;
        // A second `pane run --squad <name>` landing first may have minted
        // the squad this one meant to create: join it, never mint a twin.
        if let Some(name) = create_name.clone() {
            if let Ok(Some(sid)) = self.resolve_placement_target(&PaneTarget::SquadName(name), None)
            {
                dest = Some(sid);
                create_name = None;
            }
        }
        if claim {
            // Writer-claim ELIGIBILITY, set only at agent spawn (Locked 5).
            // The claim itself is acquired per-burst via PaneClaim.
            self.claim_eligible.insert(pid);
        }
        if let Some(name) = create_name {
            // Origins = the spawn's repo root, so same-project lanes converge here. persist_squad
            // write-through is non-blocking: a failed write degrades restore, not the live session.
            let sid = self.next_squad_id;
            self.next_squad_id += 1;
            let tid = self.session.mint_tab_id();
            self.session.add_squad(
                sid,
                vec![squad_key.clone()],
                Some(name),
                Tab {
                    name: None,
                    id: tid,
                    root: Node::Leaf(pid),
                    focus: pid,
                },
            );
            self.squad_members.insert(sid, Vec::new());
            self.pre_restore_squads.insert(sid);
            if let Some(worker) = &worker {
                self.record_worker_member(sid, worker, pid, &cwd, None);
            }
            self.persist_squad(sid);
        } else {
            // v41: place_with honors placement.tab / placement.at; it
            // falls through to place_spawned_pane on the pre-v41 no-tab/no-anchor
            // path, and reaps `pid` on any hard error so a bad anchor never
            // orphans a pane.
            let (sid, _tid, _) = self.place_with(dest, &squad_key, pid, &placement)?;
            if let Some(worker) = &worker {
                self.record_worker_member(sid, worker, pid, &cwd, None);
            }
        }
        // Keep any attached client's view consistent; a script-only session
        // has no clients, so this is then a cheap no-op.
        self.push_layout(true);
        Ok(pid)
    }

    /// The `pane run` reply: the pane id, plus its committed squad and tab
    /// when the caller placed by selector.
    pub(super) fn pane_run_reply(
        &self,
        outcome: Result<u64, (u32, String)>,
        receipt: RunPaneReceipt,
    ) -> ServerMsg {
        let pane_id = match outcome {
            Ok(pane_id) => pane_id,
            Err((code, msg)) => return ServerMsg::Err { code, msg },
        };
        let placement = receipt.wants.then(|| {
            let (sid, tid, tab_name, tab_ordinal) = self
                .session
                .find_pane(pane_id)
                .and_then(|(sid, ti)| {
                    self.session
                        .squad(sid)
                        .and_then(|s| s.tab_dict(ti))
                        .map(|d| (sid, d.tab_id, d.name, Some(d.ordinal)))
                })
                .unwrap_or((0, 0, None, None));
            ResolvedPlacement {
                anchor: receipt.anchor.unwrap_or(0),
                direction: receipt.direction.unwrap_or(Dir::Down),
                fallback: receipt.fallback,
                squad: sid,
                tab: tid,
                tab_name,
                tab_ordinal,
            }
        });
        ServerMsg::PaneSpawned { pane_id, placement }
    }

    /// The synchronous form unit fixtures assert against: under `cfg(test)`
    /// the spawn and its tail run inline, so the outcome is in hand on return.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_pane(
        &mut self,
        squad_key: String,
        cwd: String,
        argv: Vec<String>,
        rows: u16,
        cols: u16,
        claim: bool,
        placement: PanePlacement,
        worker: Option<String>,
    ) -> Result<u64, (u32, String)> {
        let slot = Arc::new(Mutex::new(None));
        let fill = Arc::clone(&slot);
        self.run_pane_then(
            squad_key,
            cwd,
            argv,
            rows,
            cols,
            claim,
            placement,
            worker,
            move |_, outcome| {
                *fill.lock().unwrap() = Some(outcome);
                Flow::Continue
            },
        );
        let outcome = slot.lock().unwrap().take();
        outcome.expect("a test spawn runs inline")
    }
}
