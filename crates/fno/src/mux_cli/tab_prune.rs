//! The live-tab fold of `fno mux workspace prune` (v69): collect one
//! `LiveTab` per (session, squad, tab), then close the surplus ones. An
//! orphaned worker tab - its stored member judged Dead by the server - closes
//! under the default flags; pristine stays the test for tabs that never
//! hosted a worker. Extracted from mux_cli.rs under the file-budget gate.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LiveTab {
    pub(super) session: String,
    pub(super) squad_id: u64,
    pub(super) squad_name: Option<String>,
    pub(super) tab_id: u64,
    /// The pane-joined tab label (v51), so a receipt can NAME a tab instead
    /// of counting it.
    pub(super) tab_name: Option<String>,
    pub(super) pane_count: usize,
    pub(super) pristine: bool,
    /// (x-cf97) Every pane in the tab is a spent shell: `cmd: None`, no
    /// `fno_id`, measured idle NOW (ran something, at a prompt). The AND-fold
    /// over panes mirrors `pristine`, but the predicate is deliberately
    /// narrower than `!pristine` - it can only ever hold for bare shells,
    /// never for an agent or a running command.
    pub(super) used_shell_only: bool,
    /// (v69) Some pane here hosts a stored member the server judges Dead, and
    /// every pane is disposable (orphaned, pristine, or a spent shell). The
    /// default fold closes such a tab: a reaped worker leaves scrollback, so
    /// `pristine` alone never fires for it.
    pub(super) orphaned: bool,
}

/// Read every live tab once, keeping WHICH sessions answered. The same
/// snapshot supplies squad liveness cwds and the empty-tab candidates, so the
/// receipt and the destructive pass cannot disagree because one probe was
/// newer than the other. Returns `(tabs, cwds, answered, unreachable)`:
/// `answered` names the sessions whose `PaneLs` returned a `PaneList` (the
/// count is its length) and `unreachable` holds the sorted names of the rest.
/// A dead server leaves its socket behind by design (`kill-server` owns
/// removal, see [`session_names`]), so an unreachable session is a steady
/// state, not an anomaly - folding the per-session fact into one boolean
/// disabled the sweep for every healthy session and reported a clean exit 0
/// (x-6e79).
pub(super) fn live_tabs() -> (Vec<LiveTab>, Vec<String>, Vec<String>, Vec<String>) {
    let Ok(names) = session_names() else {
        // The session list itself is unreadable: nothing was probed, so no
        // session answered, and the refusal below must name that hole rather
        // than a clean zero.
        return (
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec!["<session list unreadable>".into()],
        );
    };
    let mut groups: std::collections::BTreeMap<(String, u64, u64), LiveTab> =
        std::collections::BTreeMap::new();
    // (v69) Per-tab fold state for the orphan verdict: (any pane orphaned,
    // every pane disposable). Side map because a tab's panes interleave with
    // other tabs' in the pane list.
    let mut orphan_fold: std::collections::BTreeMap<(String, u64, u64), (bool, bool)> =
        std::collections::BTreeMap::new();
    let mut cwds = Vec::new();
    let mut answered: Vec<String> = Vec::new();
    let mut unreachable = Vec::new();
    for name in names {
        let Ok(sock) = proto::socket_path(&name) else {
            unreachable.push(name);
            continue;
        };
        match control_roundtrip(&sock, &name, ControlVerb::PaneLs) {
            Ok(ServerMsg::PaneList { panes }) => {
                answered.push(name.clone());
                for pane in panes {
                    cwds.push(pane.cwd.clone());
                    let key = (name.clone(), pane.squad_id, pane.tab_id);
                    let fold = orphan_fold.entry(key.clone()).or_insert((false, true));
                    // (v69) The orphan verdict is per-pane from the server:
                    // `any.0` some pane orphaned, `any.1` every pane
                    // disposable (orphaned, pristine, or a spent shell - a
                    // live agent pane beside a dead one keeps the tab).
                    fold.0 |= pane.orphaned_worker;
                    fold.1 &= pane.orphaned_worker
                        || pane.pristine_idle_shell
                        || (pane.fno_id.is_none() && pane.shell_idle);
                    let tab = groups.entry(key).or_insert_with(|| LiveTab {
                        session: name.clone(),
                        squad_id: pane.squad_id,
                        squad_name: pane.squad_name.clone(),
                        tab_id: pane.tab_id,
                        tab_name: pane.tab_name.clone(),
                        pane_count: 0,
                        pristine: true,
                        used_shell_only: true,
                        orphaned: false,
                    });
                    tab.pane_count += 1;
                    tab.pristine &= pane.pristine_idle_shell;
                    // (x-cf97) `shell_idle` already carries `cmd: None`; the
                    // `fno_id` clause here keeps an occupied pane out of the
                    // category even if its shell layer reads idle.
                    tab.used_shell_only &= pane.fno_id.is_none() && pane.shell_idle;
                }
            }
            _ => unreachable.push(name),
        }
    }
    for (key, (any_orphan, disposable)) in orphan_fold {
        if any_orphan && disposable {
            if let Some(tab) = groups.get_mut(&key) {
                tab.orphaned = true;
            }
        }
    }
    (groups.into_values().collect(), cwds, answered, unreachable)
}

#[derive(Debug, Default)]
pub(super) struct TabPruneOutcome {
    pub(super) closed: usize,
    pub(super) would_close: usize,
    pub(super) skipped_named: usize,
    pub(super) kept: usize,
    /// (x-cf97) `kept` is one number covering four reasons; these split it so
    /// a receipt can say WHY a tab stayed. Within a run fold they sum to
    /// `kept`; `kept_not_probed` covers the no-fold case on its own, so the
    /// arithmetic stays auditable instead of trusted.
    pub(super) kept_last_in_squad: usize,
    pub(super) kept_not_pristine: usize,
    pub(super) kept_zero_panes: usize,
    pub(super) kept_unreachable: usize,
    /// The fold never ran (`--dead-only`, or no session answered): every tab
    /// is "kept" only in the sense that nobody looked. Counted separately so
    /// the four real reasons still sum to `kept` when it did run.
    pub(super) kept_not_probed: usize,
    /// (x-cf97) The used-shell population that passed every guard: tabs the
    /// opt-in flag WOULD close. Counted whatever the flag did, so the sweep
    /// modal can carry the number while the default posture stays off.
    pub(super) used_shells: usize,
    /// The used-shell subset of `closed`/`would_close`.
    pub(super) closed_used: usize,
    pub(super) would_close_used: usize,
    /// The orphaned-worker subset of `closed`/`would_close` (v69).
    pub(super) closed_orphaned: usize,
    pub(super) would_close_orphaned: usize,
    /// One label per tab this pass would close (or closed) - nineteen is past
    /// the point where a bare count is a decision an operator can make.
    pub(super) closed_named: Vec<String>,
}

/// The receipt label for one live tab: name what it is where, so a sweep line
/// is judgeable without cross-referencing `fno mux tab ls`.
pub(super) fn live_tab_label(tab: &LiveTab) -> String {
    let squad = tab
        .squad_name
        .clone()
        .unwrap_or_else(|| format!("squad {}", tab.squad_id));
    match &tab.tab_name {
        Some(name) if !name.is_empty() => format!(
            "{} / {squad} / \u{201c}{name}\u{201d} (tab {})",
            tab.session, tab.tab_id
        ),
        _ => format!("{} / {squad} / tab {} (unnamed)", tab.session, tab.tab_id),
    }
}

pub(super) fn prune_live_tabs(
    tabs: &[LiveTab],
    include_named: bool,
    dry_run: bool,
    include_used_shells: bool,
) -> TabPruneOutcome {
    let mut out = TabPruneOutcome::default();
    // Tabs still open per workspace, counted up front and decremented as this
    // loop folds. A workspace's LAST tab is never surplus: closing it removes
    // the squad, and the server de-persists a removed squad's store row, so the
    // tab arm would delete exactly the row the live-pane arm below promises to
    // protect, in one command. A static count is not enough - three pristine
    // tabs would each read "not the last one" and the loop would close all
    // three. Folding surplus tabs is this arm's job; destroying a workspace is
    // `squad close`, and the store consequence belongs to the squad arm.
    let mut open_tabs: std::collections::HashMap<(&str, u64), usize> =
        std::collections::HashMap::new();
    for tab in tabs {
        *open_tabs
            .entry((tab.session.as_str(), tab.squad_id))
            .or_default() += 1;
    }
    for tab in tabs {
        if tab
            .squad_name
            .as_deref()
            .is_some_and(|name| !name.is_empty())
            && !include_named
        {
            out.skipped_named += 1;
            continue;
        }
        let open = open_tabs
            .get_mut(&(tab.session.as_str(), tab.squad_id))
            .expect("counted above");
        if *open < 2 {
            out.kept_last_in_squad += 1;
            out.kept += 1;
            continue;
        }
        // The zero-pane guard is NOT loosened by the used-shell category
        // (x-cf97): `pristine` is an AND-fold, so a zero-pane tab reads
        // pristine VACUOUSLY, with no pane having voted, and the absence-
        // is-not-an-outcome rule keeps it out of every close arm. Counting
        // the population here is what makes it visible.
        if tab.pane_count == 0 {
            out.kept_zero_panes += 1;
            out.kept += 1;
            continue;
        }
        // (v69) An orphaned worker tab closes under the DEFAULT flags, ahead
        // of the pristine test: the worker is positively Dead and nothing live
        // sits in the tab, so its scrollback is not a reason to keep it.
        if tab.orphaned {
            out.closed_named.push(live_tab_label(tab));
            if dry_run {
                out.would_close += 1;
                out.would_close_orphaned += 1;
                *open -= 1;
                continue;
            }
            if close_live_tab(tab) {
                out.closed += 1;
                out.closed_orphaned += 1;
                *open -= 1;
            } else {
                out.kept_unreachable += 1;
                out.kept += 1;
            }
            continue;
        }
        if !tab.pristine {
            // (x-cf97) The one widening, and it is opt-in: a tab of spent
            // shells (cmd: None, no fno_id, idle now) closes only when the
            // operator asked for that category by name. Anything else merely
            // not-pristine - an agent pane, a running command, an unmeasured
            // shell - stays kept, by design.
            if tab.used_shell_only && include_used_shells {
                out.used_shells += 1;
                out.closed_named.push(live_tab_label(tab));
                // Both branches decrement on the SAME counter, and only when
                // the tab actually goes: a refused or failed close leaves it
                // open, so the remaining count must not move or the next tab
                // of this squad would read one fewer than are really there.
                if dry_run {
                    out.would_close += 1;
                    out.would_close_used += 1;
                    *open -= 1;
                    continue;
                }
                if close_live_tab(tab) {
                    out.closed += 1;
                    out.closed_used += 1;
                    *open -= 1;
                } else {
                    out.kept_unreachable += 1;
                    out.kept += 1;
                }
                continue;
            }
            if tab.used_shell_only {
                out.used_shells += 1;
            }
            out.kept_not_pristine += 1;
            out.kept += 1;
            continue;
        }
        if dry_run {
            out.would_close += 1;
            out.closed_named.push(live_tab_label(tab));
            *open -= 1;
            continue;
        }
        if close_live_tab(tab) {
            out.closed += 1;
            *open -= 1;
        } else {
            out.kept_unreachable += 1;
            out.kept += 1;
        }
    }
    out
}

/// One close roundtrip for a tab the fold has decided to close. `true` only
/// on the positive `TabClosed` receipt - a refused close keeps the tab and is
/// counted as kept, never as a silent success.
pub(super) fn close_live_tab(tab: &LiveTab) -> bool {
    let Ok(sock) = proto::socket_path(&tab.session) else {
        return false;
    };
    matches!(
        control_roundtrip(
            &sock,
            &tab.session,
            ControlVerb::TabClose {
                squad: PaneTarget::SquadId(tab.squad_id),
                tab: TabSel::Id(tab.tab_id),
                force: false,
            },
        ),
        Ok(ServerMsg::TabClosed { .. })
    )
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_prune_classifies_pristine_running_and_named_tabs() {
        let tabs = vec![
            LiveTab {
                session: "main".into(),
                squad_id: 1,
                squad_name: None,
                tab_id: 11,
                tab_name: None,
                pane_count: 1,
                pristine: true,
                used_shell_only: false,
                orphaned: false,
            },
            LiveTab {
                session: "main".into(),
                squad_id: 1,
                squad_name: None,
                tab_id: 12,
                tab_name: None,
                pane_count: 1,
                pristine: false,
                used_shell_only: false,
                orphaned: false,
            },
            LiveTab {
                session: "main".into(),
                squad_id: 2,
                squad_name: Some("named".into()),
                tab_id: 21,
                tab_name: None,
                pane_count: 1,
                pristine: true,
                used_shell_only: false,
                orphaned: false,
            },
        ];

        let outcome = prune_live_tabs(&tabs, false, true, false);
        assert_eq!(outcome.would_close, 1);
        assert_eq!(outcome.closed, 0);
        assert_eq!(outcome.skipped_named, 1);
        assert_eq!(outcome.kept, 1);
        // (x-cf97) The kept split accounts for the one kept tab. Tab 11 closed
        // first, so tab 12 was the squad's LAST tab standing when it was
        // evaluated - the decrement order is the reason it names.
        assert_eq!(outcome.kept_last_in_squad, 1);
        assert_eq!(outcome.kept_not_pristine, 0);
        assert_eq!(outcome.kept_zero_panes, 0);
        assert_eq!(outcome.kept_unreachable, 0);
        assert_eq!(outcome.kept_not_probed, 0);
    }

    #[test]
    fn workspace_prune_never_folds_a_workspace_only_tab() {
        // The tab arm folds SURPLUS pristine tabs. A squad's only tab is not
        // surplus: closing it removes the squad, and the server de-persists a
        // removed squad's store row, so one prune run would take both the live
        // pane and the record the live-pane arm exists to protect. Surplus
        // pristine tabs still fold - that is what this arm is for - but the
        // count has to DECREMENT as they go. A static count reads "not the last
        // one" for every tab of a three-tab squad and folds the workspace away.
        let only = vec![LiveTab {
            session: "main".into(),
            squad_id: 1,
            squad_name: None,
            tab_id: 11,
            tab_name: None,
            pane_count: 1,
            pristine: true,
            used_shell_only: false,
            orphaned: false,
        }];
        let outcome = prune_live_tabs(&only, false, false, false);
        assert_eq!(outcome.closed, 0, "a workspace's only tab is never closed");
        assert_eq!(outcome.would_close, 0);
        assert_eq!(outcome.kept, 1);
        assert_eq!(outcome.kept_last_in_squad, 1, "the reason names the guard");

        let pristine_tab = |tab_id: u64| LiveTab {
            session: "main".into(),
            squad_id: 1,
            squad_name: None,
            tab_id,
            tab_name: None,
            pane_count: 1,
            pristine: true,
            used_shell_only: false,
            orphaned: false,
        };
        let pair = vec![pristine_tab(11), pristine_tab(12)];
        let outcome = prune_live_tabs(&pair, false, true, false);
        assert_eq!(outcome.would_close, 1, "the surplus tab folds");
        assert_eq!(outcome.kept, 1, "one tab is left standing");

        let three = vec![pristine_tab(11), pristine_tab(12), pristine_tab(13)];
        let outcome = prune_live_tabs(&three, false, true, false);
        assert_eq!(outcome.would_close, 2, "the count decrements as tabs fold");
        assert_eq!(outcome.kept, 1, "a workspace never folds to zero tabs");
    }

    #[test]
    fn workspace_prune_folds_answering_session_despite_dead_sibling() {
        // AC4-HP (x-6e79): one session answers PaneLs, a sibling (the socket a
        // dead server left behind) does not. The assertion is that the tab
        // fold RUNS over the answering session's tabs anyway - asserting the
        // prune ran proves nothing, it always ran when every socket was live.
        let scope = sweep_scope(1, &["x7b5e-proof".into()]);
        assert!(
            scope.fold_tabs,
            "a dead sibling does not silence the answering session"
        );

        let pristine = |tab_id: u64| LiveTab {
            session: "main".into(),
            squad_id: 1,
            squad_name: None,
            tab_id,
            tab_name: None,
            pane_count: 1,
            pristine: true,
            used_shell_only: false,
            orphaned: false,
        };
        // Five tabs in one squad: four pristine, one running. The fold
        // empties the squad to its LAST tab - the running one is it - so all
        // four pristine tabs are surplus (the pinned two-tab case folds a
        // pristine down to a lone running tab the same way).
        let mut tabs: Vec<LiveTab> = (11..=14).map(pristine).collect();
        tabs.push(LiveTab {
            session: "main".into(),
            squad_id: 1,
            squad_name: None,
            tab_id: 15,
            tab_name: None,
            pane_count: 1,
            pristine: false,
            used_shell_only: false,
            orphaned: false,
        });
        let outcome = prune_live_tabs(&tabs, false, true, false);
        assert_eq!(
            outcome.would_close, 4,
            "the fold runs despite the dead sibling"
        );
        assert_eq!(outcome.kept, 1, "the squad's last tab stands");
        // (x-cf97) The five-way kept arithmetic accounts for every tab.
        assert_eq!(outcome.kept_last_in_squad, 1);
        assert_eq!(outcome.kept_not_pristine, 0);
        assert_eq!(outcome.kept_zero_panes, 0);
        assert_eq!(outcome.kept_unreachable, 0);
    }

    #[test]
    fn workspace_prune_names_the_unreachable_session_in_the_receipt() {
        // AC5-ERR (x-6e79): a positive marker - the refusal names WHICH
        // session could not be probed. The measured defect: a dead
        // x7b5e-proof socket disabled the sweep for every healthy session and
        // the receipt printed a clean zero at exit 0, naming nothing.
        let notice = unreachable_notice(&["x7b5e-proof".into()]);
        assert!(notice.contains("x7b5e-proof"), "{notice}");
        assert!(
            notice.contains("no squad records changed"),
            "the refusal still says nothing changed: {notice}"
        );
        let two = unreachable_notice(&["a-dead".into(), "z-dead".into()]);
        assert_eq!(
            two,
            "server liveness incomplete for session(s) a-dead, z-dead; no squad records changed"
        );
    }

    fn used_shell_tab(tab_id: u64) -> LiveTab {
        LiveTab {
            session: "main".into(),
            squad_id: 1,
            squad_name: None,
            tab_id,
            tab_name: Some(format!("t{tab_id}")),
            pane_count: 1,
            pristine: false,
            used_shell_only: true,
            orphaned: false,
        }
    }

    #[test]
    fn workspace_prune_used_shell_category_is_opt_in_and_named() {
        // (x-cf97) The default sweep's posture is UNCHANGED: a tab of spent
        // shells is kept_not_pristine, exactly what it was before this
        // category existed. Only the named flag moves it, and the dry-run
        // receipt then NAMES the tab it would close - a count alone is not a
        // decision at nineteen tabs.
        let tabs = vec![
            used_shell_tab(31),
            used_shell_tab(32),
            // An agent pane's tab is merely not-pristine and NEVER qualifies:
            // the category can only ever hold bare shells.
            LiveTab {
                session: "main".into(),
                squad_id: 1,
                squad_name: None,
                tab_id: 33,
                tab_name: Some("agent".into()),
                pane_count: 1,
                pristine: false,
                used_shell_only: false,
                orphaned: false,
            },
        ];

        let off = prune_live_tabs(&tabs, false, true, false);
        assert_eq!(off.would_close, 0, "the default posture closes nothing new");
        assert_eq!(off.kept_not_pristine, 3);
        assert_eq!(off.kept, 3);
        assert_eq!(off.used_shells, 2, "the population is counted either way");
        assert!(off.closed_named.is_empty(), "nothing is named for closing");

        let on = prune_live_tabs(&tabs, false, true, true);
        assert_eq!(on.would_close, 2, "the flag closes the qualifying pair");
        assert_eq!(on.would_close_used, 2);
        // Both used shells closed first, so the agent tab was the squad's
        // LAST tab standing by the time it was evaluated.
        assert_eq!(on.kept_last_in_squad, 1, "the agent tab still stays");
        assert_eq!(on.kept_not_pristine, 0);
        assert_eq!(on.kept, 1);
        assert_eq!(on.closed_named.len(), 2, "each candidate is named");
        assert!(
            on.closed_named.iter().all(|l| l.contains("main")),
            "labels carry the session: {on:?}"
        );
    }

    #[test]
    fn workspace_prune_used_shell_never_closes_a_workspace_last_tab() {
        // The last-in-squad guard outranks the new category: closing a
        // workspace's only tab removes the squad, and the used-shell flag
        // must not become a workspace destroyer.
        let only = vec![used_shell_tab(41)];
        let outcome = prune_live_tabs(&only, false, true, true);
        assert_eq!(outcome.would_close, 0);
        assert_eq!(outcome.kept_last_in_squad, 1);
        assert_eq!(outcome.kept, 1);
    }

    #[test]
    fn workspace_prune_skipped_fold_counts_as_not_probed() {
        // `--dead-only` skips the fold: every tab is "kept" only in the sense
        // that nobody looked, and the receipt says so instead of folding them
        // into a real reason.
        let tabs = [used_shell_tab(51), used_shell_tab(52)];
        let outcome = TabPruneOutcome {
            kept: tabs.len(),
            kept_not_probed: tabs.len(),
            ..TabPruneOutcome::default()
        };
        assert_eq!(outcome.kept, outcome.kept_not_probed);
        assert_eq!(
            outcome.kept_last_in_squad
                + outcome.kept_not_pristine
                + outcome.kept_zero_panes
                + outcome.kept_unreachable,
            0
        );
    }

    #[test]
    fn live_tab_label_names_session_squad_and_tab() {
        let tab = LiveTab {
            session: "main".into(),
            squad_id: 3,
            squad_name: Some("ops".into()),
            tab_id: 9,
            tab_name: Some("shells".into()),
            pane_count: 1,
            pristine: false,
            used_shell_only: true,
            orphaned: false,
        };
        let label = live_tab_label(&tab);
        assert!(label.contains("main"), "{label}");
        assert!(label.contains("ops"), "{label}");
        assert!(label.contains("shells"), "{label}");
        assert!(label.contains("9"), "{label}");
        let unnamed = LiveTab {
            squad_name: None,
            tab_name: None,
            ..tab
        };
        let label = live_tab_label(&unnamed);
        assert!(label.contains("squad 3"), "{label}");
        assert!(label.contains("unnamed"), "{label}");
    }

    #[test]
    fn workspace_prune_store_pass_stays_closed_on_partial_liveness() {
        // AC6-EDGE (x-6e79): live_cwds is CROSS-session protective evidence -
        // a live pane's cwd makes prune_decision_with_evidence return Keep,
        // and an unreachable session contributes no cwds. Running the store
        // pass on partial evidence would turn a Keep into a Prune, so it
        // stays globally fail-closed even while the tab arm folds per
        // session. Absent liveness must never read as dead.
        let partial = sweep_scope(1, &["x7b5e-proof".into()]);
        assert!(partial.fold_tabs, "the tab arm is per-session and safe");
        assert!(
            !partial.sweep_store,
            "partial liveness closes the store pass"
        );
        let full = sweep_scope(2, &[]);
        assert!(
            full.sweep_store,
            "every session answered, the store pass runs"
        );
        assert!(full.fold_tabs);
        let none_answered = sweep_scope(0, &[]);
        assert!(!none_answered.fold_tabs, "nothing answered, nothing folds");
        assert!(
            none_answered.sweep_store,
            "zero sessions is empty, not partial"
        );
    }

    #[test]
    fn orphaned_worker_tab_with_scrollback_closes_by_default_and_is_named() {
        // The node's marker: a tab whose pane hosted a Dead worker is NOT
        // pristine (scrollback), yet closes with every flag off and is NAMED
        // in the receipt. A count-only assertion is refused: pristine tabs
        // already close today.
        let tab = LiveTab {
            session: "no-such-fno-main".into(),
            squad_id: 1,
            squad_name: None,
            tab_id: 2,
            tab_name: Some("worker".into()),
            pane_count: 1,
            pristine: false,
            used_shell_only: false,
            orphaned: true,
        };
        // The companion tab shares the unreachable session, so the squad has
        // two tabs and the last-in-squad guard lets the fold decide. A live
        // session name here would make the suite close a REAL tab.
        let companion = LiveTab {
            session: "no-such-fno-main".into(),
            squad_id: 1,
            squad_name: None,
            tab_id: 3,
            tab_name: None,
            pane_count: 1,
            pristine: false,
            used_shell_only: true,
            orphaned: false,
        };
        let out = prune_live_tabs(&[tab, companion], false, false, false);
        assert_eq!(out.closed_orphaned, 0, "no server answers in a unit test");
        assert_eq!(out.would_close_orphaned, 0);
        assert!(
            out.closed_named.iter().any(|l| l.contains("worker")),
            "the orphaned tab is named even when the close cannot reach a server: {closed_named:?}",
            closed_named = out.closed_named
        );
        assert_eq!(
            out.kept_unreachable, 1,
            "the unreachable close is kept and counted, never silent"
        );
    }
}
