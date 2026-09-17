//! The roster-side sweep (gap one): claude rows no fno registry row
//! names.
//!
//! Every removal door was keyed off an fno registry row, so a claude row
//! whose fno row is already gone was reachable by nothing. This sweep
//! iterates the CLAUDE ROSTER and asks, per row, whether anything still
//! owns it. An unmatched row is not automatically garbage: a human starts
//! sessions by hand and fno never spawned them, so the eligibility gates
//! are positive reasons to remove, never an absence.
//!
//! The gates, in order. The enumeration must be trusted: a snapshot that
//! failed outright keeps everything. No fno registry row that owns its
//! session may name the row, by session id, short id, or name: an owned
//! row is the registry sweep's business, and this sweep keeps it with that
//! reason (an adopted, uncrowned row owns nothing: see the scope rule
//! below). The shared
//! provenance verdict must resolve a provenance and read the node done,
//! with the PR confirm passing - the same function the registry sweep runs,
//! so the two sweeps cannot disagree about which rows are dead. The
//! transcript must read quiet past the grace, with an unreadable transcript
//! never quiet. Only then is the row removed from the claude active surface
//! through the same cascade `rm` walks, with the typed outcome recorded and
//! a receipt staged when none exists yet (an earlier retirement's receipt
//! is the record it already made; this pass's outcome is in the summary).
//! A history deletion never happens: the transcript survives the `rm`. The
//! receipt carries the resume command, but a removed claude background
//! session no longer resumes - the job state is gone with it.
//!
//! The scope (`agents.reap.roster_scope`) names the population that may
//! retire, as an operator setting: `off` retires nothing, `provenanced`
//! (the default) is the chain above, `all` widens to rows fno itself
//! spawned (sessions or registry provenance) whose work is open. The rule
//! under every value is positive: an unowned session retires only on an
//! fno-ownership marker - a sessions[] row fno wrote, or a reap receipt
//! an earlier retirement staged for the same session (the
//! leaked-retirement class: the sweep that leaked a session is the sweep
//! that staged its receipt). A name
//! pattern or a transcript mention is exactly how a hand-started session
//! acquires a phantom node, so weak provenance keeps. An adopted registry
//! row is a healer's or `fno agents adopt`'s note about a session, not
//! work fno itself spawned, so it does not shield its listed session; when
//! this sweep removes that session, the registry sweep's `origin_corpse`
//! exit retires the row on the next pass. A session with no marker at all
//! is unreachable by construction, not by default value, and a wrong
//! config cannot reach it.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::claude_roster::{ClaudeAgentRow, ClaudeAgentsSnapshot};
use crate::daemon::CascadeOutcome;
use crate::gc_sweep::{open_pr_verdict, provenance_verdict, GraphRead, OpenPrVerdict};
use crate::graph_store::WorkState;
use crate::state::RegistryEntry;

/// One roster row the pass judged, with the reason it landed where it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterJudgement {
    /// The row's short id (the identity `claude agents rm` takes).
    pub short_id: String,
    /// The resolved node, when provenance resolved one.
    pub node: Option<String>,
    /// Why the row was kept or retired, in both directions.
    pub reason: String,
    pub retired: bool,
}

#[derive(Debug, Default)]
pub struct RosterReapSummary {
    pub visited: usize,
    /// Duplicate listing rows collapsed before judging: one session can be
    /// minted more than once into a listing, and judging each copy is how
    /// one removal becomes three.
    pub deduped: usize,
    /// Rows the fno registry still names: the registry sweep's business.
    pub kept_owned: usize,
    /// An instrument this pass could not read (the claude roster
    /// enumeration, or the fno registry). Everything was kept, and the
    /// tick detail names the failed read instead of counting keeps.
    pub instrument_unread: bool,
    pub kept: Vec<RosterJudgement>,
    pub retired: Vec<RosterJudgement>,
    /// Apply-mode removals whose typed outcome did not confirm: named,
    /// never silent, retried by the next pass.
    pub refused: Vec<(String, String)>,
}

fn judgement(
    short_id: &str,
    node: Option<String>,
    reason: String,
    retired: bool,
) -> RosterJudgement {
    RosterJudgement {
        short_id: short_id.to_string(),
        node,
        reason,
        retired,
    }
}

/// Render the pass. Pure over the summary; every bucket appears at every
/// pass, zero counts included, so a silent pass is never confused with an
/// empty one.
pub fn render(summary: &RosterReapSummary, json_out: bool, dry_run: bool) -> String {
    if json_out {
        let rows = |rows: &Vec<RosterJudgement>| -> Vec<serde_json::Value> {
            rows.iter()
                .map(|j| {
                    serde_json::json!({
                        "id": j.short_id,
                        "node": j.node,
                        "reason": j.reason,
                        "retired": j.retired,
                    })
                })
                .collect()
        };
        return format!(
            "{}\n",
            serde_json::json!({
                "visited": summary.visited,
                "deduped": summary.deduped,
                "kept_owned": summary.kept_owned,
                "kept": rows(&summary.kept),
                "retired": rows(&summary.retired),
                "refused": summary
                    .refused
                    .iter()
                    .map(|(id, r)| serde_json::json!({"id": id, "reason": r}))
                    .collect::<Vec<_>>(),
                "dry_run": dry_run,
            })
        );
    }
    let verb = if dry_run { "would retire" } else { "retired" };
    let mut out = format!(
        "{verb} {} of {} roster row(s)\n",
        summary.retired.len(),
        summary.visited
    );
    if summary.deduped > 0 {
        out.push_str(&format!(
            "  deduped {} duplicate listing row(s)\n",
            summary.deduped
        ));
    }
    for j in &summary.retired {
        let node = j.node.as_deref().unwrap_or("-");
        out.push_str(&format!("  {verb} {} ({}: {node})\n", j.short_id, j.reason));
    }
    for (id, reason) in &summary.refused {
        out.push_str(&format!("  refused {id} ({reason})\n"));
    }
    for j in &summary.kept {
        out.push_str(&format!("  kept {} ({})\n", j.short_id, j.reason));
    }
    out
}

/// The roster-side sweep. Every I/O seam is injected so a test stages the
/// world; production wiring is [`roster_reap`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    home: &crate::paths::AgentsHome,
    grace_secs: i64,
    scope: crate::agents_config::RosterScope,
    dry_run: bool,
    roster: &ClaudeAgentsSnapshot,
    registry: &[RegistryEntry],
    read_graph: &dyn Fn() -> Option<GraphRead>,
    transcripts: &dyn Fn(&RegistryEntry) -> Option<Vec<PathBuf>>,
    age_many: &dyn Fn(&[&RegistryEntry]) -> HashMap<String, Option<i64>>,
    _now: i64,
    remove: &dyn Fn(&RegistryEntry) -> CascadeOutcome,
) -> RosterReapSummary {
    let mut summary = RosterReapSummary::default();
    let rows: Vec<ClaudeAgentRow> = match roster {
        ClaudeAgentsSnapshot::Known { rows, .. } => rows.clone(),
        ClaudeAgentsSnapshot::Unknown { rows, warnings } => {
            if rows.is_empty() {
                // The enumeration failed outright: keep everything. A sweep
                // that cannot see the roster removes nothing.
                summary.instrument_unread = true;
                summary.kept = warnings
                    .iter()
                    .map(|w| judgement("", None, format!("roster unreadable: {w}"), false))
                    .collect();
                return summary;
            }
            rows.clone()
        }
    };
    summary.visited = rows.len();
    // One session can be minted more than once into a listing (measured:
    // three listing rows for one operator session). Judge each SESSION
    // once: the full harness session id is the identity, the short id the
    // fallback for a listing that omits it. First copy wins; the duplicates
    // are counted, never judged.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut unique: Vec<ClaudeAgentRow> = Vec::with_capacity(rows.len());
    for row in rows {
        let key = match row.session_id.as_deref().filter(|s| !s.is_empty()) {
            Some(sid) => sid.to_string(),
            None => row.short_id.clone(),
        };
        if !seen.insert(key) {
            summary.deduped += 1;
            continue;
        }
        unique.push(row);
    }
    let rows: Vec<ClaudeAgentRow> = unique;
    let graph = read_graph();
    // An adopted row is a fact a healer or `fno agents adopt` wrote ABOUT a
    // session, not work fno itself spawned: the registry sweep keeps the row
    // while the session is listed, so here it stops shielding and this
    // sweep's own gates judge the session. Removing the session is the
    // first tick; the registry sweep's `origin_corpse` exit retires the row
    // on the next pass. Every other entry still shields, including an
    // entry with no origin.
    let owned: BTreeSet<String> = registry
        .iter()
        .filter(|e| !(e.origin.as_deref() == Some("adopted") && e.crown_level.is_none()))
        .flat_map(|e| {
            e.aliases
                .iter()
                .map(String::as_str)
                .chain([e.name.as_str()])
                .chain([e.short_id.as_str()])
                .chain(e.harness_session_id.as_deref().into_iter())
                .map(str::to_string)
                .collect::<Vec<String>>()
        })
        .collect();
    // Pass one: every gate up to the quiet gate, judged per row; the rows
    // that survive it become the batch's age candidates.
    struct Candidate {
        entry: RegistryEntry,
        ident: String,
        node: Option<String>,
        basis: String,
        terminal: Option<String>,
        pid: Option<u32>,
    }
    let mut candidates: Vec<Candidate> = Vec::new();
    for row in &rows {
        let ident = row
            .session_id
            .clone()
            .or_else(|| row.name.clone())
            .unwrap_or_else(|| row.short_id.clone());
        // An owned row is the registry sweep's business.
        let owned_match = owned.contains(&row.short_id)
            || row
                .session_id
                .as_deref()
                .is_some_and(|sid| owned.contains(sid))
            || row.name.as_deref().is_some_and(|n| owned.contains(n));
        if owned_match {
            summary.kept_owned += 1;
            summary
                .kept
                .push(judgement(&ident, None, "owned by an fno row".into(), false));
            continue;
        }
        // Scope off retires nothing and judges nothing: no graph read, no
        // transcript probe.
        if scope == crate::agents_config::RosterScope::Off {
            summary
                .kept
                .push(judgement(&ident, None, "roster scope off".into(), false));
            continue;
        }
        // The synthetic identity: the roster row's own fields, shaped like
        // the registry entry the removal cascade and the store index read.
        // Built through the sanctioned constructor and then specialized -
        // the mint guard bars a default-based literal here, and the guard
        // is right that identity fields should be set on purpose.
        let mut entry = RegistryEntry::new(
            row.session_id.clone(),
            crate::state::Lineage::unproven("synthetic row for a read, never written"),
        );
        entry.name = row.name.clone().unwrap_or_else(|| row.short_id.clone());
        entry.short_id = row.short_id.clone();
        entry.harness = Some("claude".into());
        entry.cwd = row.cwd.clone().unwrap_or_default();
        entry.origin = Some("spawn".into());
        let Some(graph) = &graph else {
            summary
                .kept
                .push(judgement(&ident, None, "graph unreadable".into(), false));
            continue;
        };
        let hits = transcripts(&entry);
        let sid = entry.harness_session_id.as_deref().unwrap_or("").trim();
        let verdict = provenance_verdict(&entry, sid, graph, hits.as_deref(), None);
        let node = verdict.route.node.clone();
        // A hold (conflict or PR contradiction) names itself, and stays a
        // keep at every scope: contested truth is not a scope question.
        if let Some(hold) = &verdict.hold {
            summary
                .kept
                .push(judgement(&ident, node, hold.as_str().to_string(), false));
            continue;
        }
        // The basis names why the row may retire. `all` widens the
        // population to rows fno ITSELF spawned (the sessions join or the
        // registry) whose work is open; a name pattern or a transcript
        // mention is exactly how a hand-started operator session acquires a
        // phantom node, so weak provenance keeps even at `all`.
        // NoProvenance keeps at EVERY scope: an operator session names no
        // fno node, so this keep is by construction, not by default value.
        // open NODE state alone is not evidence a SESSION is alive.
        // A terminal harness state on the row itself, a parked or
        // never-started node, or a recorded merge the status lags release
        // the open-work hold the same way they do in the registry sweep -
        // INSIDE the population the scope already allows. `all` is the only
        // scope whose population includes open-work rows, so the release
        // retires there; at every other scope the row still keeps, and the
        // reason names the release so the operator can see what a wider
        // scope would do. Supersession is registry-only: roster rows carry
        // no created_at to order by.
        let via = verdict
            .route
            .source
            .map(|s| s.as_str())
            .unwrap_or("sessions");
        // The fno-ownership marker: a sessions[] row fno wrote, or a reap
        // receipt an earlier retirement staged for this session (the same
        // `<harness>-<session id>` key `write_receipt` files under - a
        // stat, not a receipt build, which would walk the transcript store
        // per row). A name pattern or a transcript mention is exactly how a
        // hand-started session acquires a phantom node, so weak provenance
        // is not a marker. Checked only where a retirement is possible.
        let strong_source = matches!(
            verdict.route.source,
            Some(crate::node_route::NodeSource::Sessions)
                | Some(crate::node_route::NodeSource::Registry)
        );
        let receipt_marker = || {
            entry
                .harness_session_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .is_some_and(|sid| {
                    crate::receipt::reap_receipt_path_for(home, entry.harness_name(), sid).exists()
                })
        };
        // the terminal read hoisted out of the Open arm. Every row
        // here is claude by construction, so `row.state` is in hand, and
        // recency must be able to yield to it exactly as the registry
        // sweep's grace_gate does.
        let terminal = row
            .state
            .as_deref()
            .filter(|s| crate::claude_roster::is_terminal_roster_state(s));
        let open_release: Option<String> = match &verdict.work {
            WorkState::Open { node: n, status } => {
                let inactive = crate::gc::INACTIVE_NODE_STATUSES.contains(&status.as_str());
                let merged = verdict.merged_but_open.is_some();
                if terminal.is_some() || inactive || merged {
                    Some(match (terminal, inactive, merged) {
                        (Some(state), _, _) => format!(
                            "session terminal: harness state {state} (via {via}); node {n} {status}"
                        ),
                        (None, true, _) => format!("node {n} is {status}, not active work"),
                        (None, false, true) => {
                            format!("node {n} {status}; recorded merge_status merged")
                        }
                        (None, false, false) => unreachable!("release required a positive fact"),
                    })
                } else {
                    None
                }
            }
            _ => None,
        };
        let basis: String = match &verdict.work {
            WorkState::AllDone { nodes } => {
                if strong_source || receipt_marker() {
                    format!("every named node done: {} (via {via})", nodes.join(", "),)
                } else {
                    summary.kept.push(judgement(
                        &ident,
                        node,
                        format!("no fno ownership marker (via {via})"),
                        false,
                    ));
                    continue;
                }
            }
            WorkState::Open { node: n, status } => {
                let scope_all_strong = scope == crate::agents_config::RosterScope::All
                    && (strong_source || receipt_marker());
                // The open-PR keep at scope all, asked through the one
                // predicate the registry sweep runs: the graph record names
                // the candidate and both sweeps read the same answer. No
                // GitHub reader is supplied here, so a candidate holds as it
                // always has - the record alone decides at this scope. The
                // default scope is unchanged.
                if scope_all_strong {
                    match open_pr_verdict(graph, sid, n, &entry.cwd, true, None) {
                        OpenPrVerdict::Holds { node, pr } | OpenPrVerdict::Unread { node, pr } => {
                            summary.kept.push(judgement(
                                &ident,
                                Some(node.clone()),
                                format!("open pr: {node} #{pr}"),
                                false,
                            ));
                            continue;
                        }
                        _ => {}
                    }
                }
                match &open_release {
                    Some(release) => {
                        if scope_all_strong {
                            release.clone()
                        } else {
                            summary.kept.push(judgement(
                                &ident,
                                Some(n.clone()),
                                format!("open work: {n} {status}; {release}"),
                                false,
                            ));
                            continue;
                        }
                    }
                    None if scope_all_strong => {
                        format!("open work {n} {status} at roster scope all (via {via})")
                    }
                    None => {
                        summary.kept.push(judgement(
                            &ident,
                            Some(n.clone()),
                            format!("open work: {n} {status}"),
                            false,
                        ));
                        continue;
                    }
                }
            }
            WorkState::NoProvenance => {
                // The registry sweep's decision with the roster's own
                // witness: a harness state of `done` is the row's own
                // finished report, so with a receipt marker it falls to the
                // quiet gate every other state takes below. Without the
                // marker - and for `stopped` and `failed`, which are not
                // that report - the row keeps.
                if terminal == Some("done") && receipt_marker() {
                    format!(
                        "no provenance; harness state {state} is the row's own finished report",
                        state = terminal.unwrap_or_default()
                    )
                } else {
                    summary.kept.push(judgement(
                        &ident,
                        None,
                        crate::gc::KeepReason::NoProvenance.as_str().to_string(),
                        false,
                    ));
                    continue;
                }
            }
        };
        candidates.push(Candidate {
            entry,
            ident,
            node,
            basis,
            terminal: terminal.map(str::to_string),
            pid: row.pid,
        });
    }

    // Pass two: ONE batched age call answers every candidate, keyed by
    // `row_handle` - the exact seam `gc_sweep::run` takes, whose production
    // default pages 24 handles per truth probe instead of paying one
    // subprocess per row. Judgements push in roster order.
    let refs: Vec<&RegistryEntry> = candidates.iter().map(|c| &c.entry).collect();
    let ages = age_many(&refs);
    for c in &candidates {
        let ident = c.ident.clone();
        let node = c.node.clone();
        let basis = c.basis.clone();
        let age = ages
            .get(&crate::gc::row_handle(&c.entry))
            .copied()
            .flatten();
        let pid_gone = c.pid.is_some_and(crate::daemon::pid_is_gone);
        match age {
            None => summary.kept.push(judgement(
                &ident,
                node,
                "transcript unresolved".into(),
                false,
            )),
            Some(age) if age <= grace_secs && !pid_gone && c.terminal.is_none() => {
                summary.kept.push(judgement(
                    &ident,
                    node,
                    format!("active: transcript written {age}s ago"),
                    false,
                ))
            }
            Some(age) => {
                // Name the early fire: a retirement INSIDE the grace window
                // went because the harness says the session finished, not
                // because the transcript aged out. A dead pid and a terminal
                // harness state are the two early-fire witnesses.
                let basis = if c.terminal.is_some() && age <= grace_secs {
                    format!(
                        "{basis}; session terminal: harness state {}",
                        c.terminal.as_deref().unwrap_or_default()
                    )
                } else {
                    basis
                };
                let basis = if pid_gone {
                    format!("{basis}; pid {} is gone", c.pid.unwrap_or(0))
                } else {
                    basis
                };
                if dry_run {
                    summary.retired.push(judgement(&ident, node, basis, true));
                } else {
                    let outcome = remove(&c.entry);
                    if outcome.satisfies_applied() {
                        write_receipt(home, &c.entry, &outcome, node.as_deref(), &basis);
                        summary.retired.push(judgement(&ident, node, basis, true));
                    } else {
                        // Name WHY the removal did not confirm, not just the
                        // outcome tag: the detail is what tells the operator
                        // whether the row is a race to re-run or a real
                        // refusal (sub-defect b).
                        let detail = outcome.detail().unwrap_or_else(|| "no detail".to_string());
                        summary.refused.push((
                            ident.clone(),
                            format!(
                                "the native removal did not confirm ({}: {detail})",
                                outcome.as_str()
                            ),
                        ));
                    }
                }
            }
        }
    }
    summary
}

/// Stage a reap receipt for a roster-only row this pass positively removed,
/// unless an earlier retirement already staged one (its record is the
/// record it made; this pass's fresh outcome is in the summary).
fn write_receipt(
    home: &crate::paths::AgentsHome,
    entry: &RegistryEntry,
    outcome: &CascadeOutcome,
    node: Option<&str>,
    basis: &str,
) {
    // The event's `resumable` is the receipt's measured resume-evidence
    // verdict (the same basis the retirement sweep emits), never the fact
    // that a receipt happened to stage: a transcript gone from the store
    // reports `no-transcript` even though the receipt itself staged fine.
    let (receipt_staged, evidence) = match crate::receipt::build_reap_receipt(entry, None) {
        Ok(mut receipt) => {
            let evidence = crate::gc_sweep::resume_evidence_effect(&receipt);
            if crate::receipt::reap_receipt_path(home, &receipt).exists() {
                (true, evidence)
            } else {
                receipt.removed_by = Some("roster-reap".to_string());
                receipt.effects = vec![outcome.effect_record("active-surface")];
                let staged = crate::receipt::write_reap_receipt(home, &receipt).is_ok();
                (staged, evidence)
            }
        }
        Err(_) => (false, crate::gc_sweep::resume_evidence_effect_unbuilt()),
    };
    let emitter = crate::events::EventEmitter::new(home.events_jsonl(), "daemon");
    let _ = emitter.emit(
        "agent_row_reaped",
        &serde_json::json!({
            "short_id": entry.short_id,
            "name": entry.name,
            "node_id": node,
            "session_id": entry.harness_session_id,
            "termination_event": false,
            "harness": entry.harness_name(),
            "harness_session_id": entry.harness_session_id,
            "basis": basis,
            "resumable": evidence.outcome == "confirmed-removed",
            "resumable_basis": evidence.detail,
            "receipt_staged": receipt_staged,
            "remover": "roster-reap",
        }),
    );
}

/// The production shell: enumeration, registry, graph, transcripts and the
/// removal cascade from the live seams.
pub fn roster_reap(
    home: &crate::paths::AgentsHome,
    grace_secs: i64,
    scope: crate::agents_config::RosterScope,
    dry_run: bool,
) -> RosterReapSummary {
    let roster = crate::claude_roster::read_all_agents_union();
    let registry = match crate::state::load_registry(&home.registry_json()) {
        Ok(registry) => registry,
        Err(e) => {
            // The same fail-closed posture the registry sweep holds: a
            // registry that cannot be read cannot prove that none of its
            // rows owns a session, so this sweep removes nothing.
            let mut summary = RosterReapSummary::default();
            summary.instrument_unread = true;
            summary.kept = vec![judgement(
                "",
                None,
                format!("registry unreadable: {e}"),
                false,
            )];
            return summary;
        }
    };
    let store = std::cell::RefCell::new(crate::gc_inventory::HarnessStoreIndex::default());
    run(
        home,
        grace_secs,
        scope,
        dry_run,
        &roster,
        &registry.entries,
        &|| crate::gc_sweep::read_graph_entries(home),
        &|e| store.borrow_mut().matches(e),
        &crate::gc::probe_entry_ages,
        crate::daemon::now_epoch_secs(),
        &crate::gc_native::apply_active_surface_removal,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents_config::RosterScope;
    use crate::state::RegistryEntry;
    use std::collections::HashMap;

    fn row(short_id: &str, session_id: Option<&str>, name: Option<&str>) -> ClaudeAgentRow {
        ClaudeAgentRow {
            short_id: short_id.to_string(),
            state: Some("done".into()),
            session_id: session_id.map(str::to_string),
            name: name.map(str::to_string),
            cwd: Some("/work".into()),
            account: None,
            pid: None,
        }
    }

    fn roster(rows: Vec<ClaudeAgentRow>) -> ClaudeAgentsSnapshot {
        ClaudeAgentsSnapshot::Known {
            rows,
            warnings: Vec::new(),
        }
    }

    fn graph_done(node: &str) -> GraphRead {
        GraphRead {
            statuses: HashMap::from([(node.to_string(), "done".to_string())]),
            pr_state: HashMap::from([(node.to_string(), (Some("merged".into()), 0, 0))]),
            ..Default::default()
        }
    }

    /// `graph_done` with the sessions join naming the node: the session's
    /// provenance resolves through NodeSource::Sessions, an ownership
    /// marker.
    fn graph_done_via_sessions(node: &str, sids: &[&str]) -> GraphRead {
        let mut g = graph_done(node);
        for sid in sids {
            g.index.insert(
                sid.to_string(),
                vec![(node.to_string(), "done".to_string())],
            );
        }
        g.work_index = g.index.clone();
        g
    }

    fn quiet_transcript(dir: &std::path::Path, sid: &str) -> PathBuf {
        let path = dir.join(format!("{sid}.jsonl"));
        std::fs::write(&path, "{\"message\":{}}\n").unwrap();
        let old = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            - 10_000;
        let f = std::fs::File::options().append(true).open(&path).unwrap();
        f.set_modified(
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(old as u64),
        )
        .unwrap();
        path
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("roster-reap-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The test age seam: the age answers from the staged transcript
    /// files' mtimes, exactly what the pre-probe stat read.
    fn mtime_age(paths: &[PathBuf]) -> Option<i64> {
        paths
            .iter()
            .filter_map(|p| {
                let t = std::fs::metadata(p).ok()?.modified().ok()?;
                Some(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64)
            })
            .max()
            .map(|newest| {
                (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64)
                    .saturating_sub(newest)
            })
    }

    fn no_home() -> crate::paths::AgentsHome {
        crate::paths::AgentsHome::at(std::path::Path::new("/nonexistent-roster-reap"))
    }

    // AC4-HP: an unowned session whose provenance resolves only through the
    // row NAME (done, merged, quiet) keeps: a name pattern is exactly how a
    // hand-started session acquires a phantom node, so without an ownership
    // marker - no sessions join, no receipt - the keep names the missing
    // marker.
    #[test]
    fn an_unowned_session_with_weak_or_no_provenance_keeps_without_a_marker() {
        let dir = tmpdir("marker-weak");
        let transcript = quiet_transcript(&dir, "sid-1");
        let rows = vec![row("ab12cd34", Some("sid-1"), Some("target-x-aaaa-worker"))];
        let summary = run(
            &no_home(),
            900,
            RosterScope::Provenanced,
            true,
            &roster(rows),
            &[],
            &|| Some(graph_done("x-aaaa")),
            &|_e| Some(vec![transcript.clone()]),
            &|entries| {
                entries
                    .iter()
                    .map(|e| (crate::gc::row_handle(e), mtime_age(&[transcript.clone()])))
                    .collect::<HashMap<_, _>>()
            },
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert!(summary.retired.is_empty(), "{summary:?}");
        assert!(
            summary.kept[0]
                .reason
                .contains("no fno ownership marker (via name)"),
            "{summary:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // AC5-HP, the leaked-retirement class: a session with NO provenance in
    // harness state done, quiet past the grace, retires on the receipt an
    // earlier retirement staged for it.
    #[test]
    fn a_done_session_with_a_receipt_on_disk_retires() {
        let _env = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tmpdir("marker-receipt");
        let home = crate::paths::AgentsHome::at(dir.join("home"));
        home.ensure_root().unwrap();
        let transcript = quiet_transcript(&dir, "sid-1");
        let rows = vec![row("ab12cd34", Some("sid-1"), Some("hand-typed-name"))];
        // Stage the receipt under a hermetic HOME: the receipt builder reads
        // the harness store index for the resume locator.
        let store_home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", store_home.path());
        let mut entry = RegistryEntry::new(
            Some("sid-1".into()),
            crate::state::Lineage::unproven("test receipt staging"),
        );
        entry.harness = Some("claude".into());
        entry.short_id = "ab12cd34".into();
        let receipt = crate::receipt::build_reap_receipt(&entry, None).expect("receipt builds");
        crate::receipt::write_reap_receipt(&home, &receipt).unwrap();
        let summary = run(
            &home,
            900,
            RosterScope::Provenanced,
            true,
            &roster(rows),
            &[],
            &|| Some(GraphRead::default()),
            &|_e| Some(vec![transcript.clone()]),
            &|entries| {
                entries
                    .iter()
                    .map(|e| (crate::gc::row_handle(e), mtime_age(&[transcript.clone()])))
                    .collect::<HashMap<_, _>>()
            },
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        match &old_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        assert_eq!(summary.retired.len(), 1, "{summary:?}");
        assert!(
            summary.retired[0]
                .reason
                .contains("no provenance; harness state done"),
            "{summary:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // An owned row is the registry sweep's business.
    #[test]
    fn owned_row_is_kept_for_the_registry_sweep() {
        let rows = vec![row("ab12cd34", Some("sid-1"), Some("target-x-aaaa-worker"))];
        let mut entry = RegistryEntry::default();
        entry.name = "target-x-aaaa-worker".into();
        entry.harness_session_id = Some("sid-1".into());
        let summary = run(
            &no_home(),
            900,
            RosterScope::Provenanced,
            true,
            &roster(rows),
            &[entry],
            &|| Some(graph_done("x-aaaa")),
            &|_| None,
            &|_| HashMap::new(),
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert_eq!(summary.kept_owned, 1);
        assert!(summary.retired.is_empty());
    }

    // AC1-HP: an adopted, uncrowned registry row does NOT shield its listed
    // session. The session still needs its own gates - marker, quiet - but
    // the adopted keep is no longer one of them.
    #[test]
    fn an_adopted_row_does_not_shield_its_listed_session() {
        let dir = tmpdir("adopted-unshield");
        let transcript = quiet_transcript(&dir, "sid-1");
        let rows = vec![row("ab12cd34", Some("sid-1"), Some("target-x-aaaa-worker"))];
        let mut entry = RegistryEntry::default();
        entry.name = "adopted-worker".into();
        entry.short_id = "adopted-worker".into();
        entry.origin = Some("adopted".into());
        entry.harness = Some("claude".into());
        entry.harness_session_id = Some("sid-1".into());
        let summary = run(
            &no_home(),
            900,
            RosterScope::Provenanced,
            true,
            &roster(rows),
            &[entry],
            &|| Some(graph_done_via_sessions("x-aaaa", &["sid-1"])),
            &|_e| Some(vec![transcript.clone()]),
            &|entries| {
                entries
                    .iter()
                    .map(|e| (crate::gc::row_handle(e), mtime_age(&[transcript.clone()])))
                    .collect::<HashMap<_, _>>()
            },
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert_eq!(summary.kept_owned, 0, "{summary:?}");
        assert_eq!(summary.retired.len(), 1, "{summary:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // AC3-ERR: spawn, operator, crowned, and unstamped entries all still
    // shield their listed session; only the adopted-uncrowned carve-out
    // stops shielding.
    #[test]
    fn a_spawn_operator_crowned_or_unstamped_row_still_shields() {
        let rows = vec![row("ab12cd34", Some("sid-1"), Some("target-x-aaaa-worker"))];
        let mut spawn = RegistryEntry::default();
        spawn.name = "w-spawn".into();
        spawn.origin = Some("spawn".into());
        spawn.harness_session_id = Some("sid-1".into());
        let mut operator = RegistryEntry::default();
        operator.name = "w-operator".into();
        operator.origin = Some("operator".into());
        operator.harness_session_id = Some("sid-1".into());
        let mut crowned = RegistryEntry::default();
        crowned.name = "w-crowned".into();
        crowned.origin = Some("adopted".into());
        crowned.crown_level = Some(1);
        crowned.harness_session_id = Some("sid-1".into());
        let mut unstamped = RegistryEntry::default();
        unstamped.name = "w-unstamped".into();
        unstamped.harness_session_id = Some("sid-1".into());
        let summary = run(
            &no_home(),
            900,
            RosterScope::All,
            true,
            &roster(rows),
            &[spawn, operator, crowned, unstamped],
            &|| Some(graph_done_via_sessions("x-aaaa", &["sid-1"])),
            &|_| None,
            &|_| HashMap::new(),
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert_eq!(summary.kept_owned, 1, "{summary:?}");
        assert!(summary.retired.is_empty(), "{summary:?}");
    }

    // AC6-HP: the age seam is called ONCE with every candidate, and a
    // candidate the batch does not answer keeps as `transcript unresolved`.
    #[test]
    fn the_age_seam_is_called_once_for_every_candidate() {
        let dir = tmpdir("age-batch");
        let transcript = quiet_transcript(&dir, "sid-a");
        let rows = vec![
            row("aaaa1111", Some("sid-a"), Some("target-x-aaaa-worker-a")),
            row("bbbb2222", Some("sid-b"), Some("target-x-aaaa-worker-b")),
        ];
        let calls = std::cell::RefCell::new(0u32);
        let summary = {
            let calls_ref = &calls;
            run(
                &no_home(),
                900,
                RosterScope::Provenanced,
                true,
                &roster(rows),
                &[],
                &|| Some(graph_done_via_sessions("x-aaaa", &["sid-a", "sid-b"])),
                &|_e| Some(vec![transcript.clone()]),
                &move |entries: &[&RegistryEntry]| {
                    *calls_ref.borrow_mut() += 1;
                    entries
                        .iter()
                        .map(|e| {
                            let age = if crate::gc::row_handle(e) == "aaaa1111" {
                                Some(10_000i64)
                            } else {
                                None
                            };
                            (crate::gc::row_handle(e), age)
                        })
                        .collect::<HashMap<_, _>>()
                },
                crate::daemon::now_epoch_secs(),
                &|_| CascadeOutcome::NotApplicable,
            )
        };
        assert_eq!(*calls.borrow(), 1, "one batched call, not one per row");
        assert_eq!(summary.retired.len(), 1, "{summary:?}");
        let unresolved = summary
            .kept
            .iter()
            .find(|j| j.reason == "transcript unresolved")
            .expect("the unanswered candidate keeps");
        assert_eq!(unresolved.short_id, "sid-b", "{summary:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // a terminal harness state releases the open-work keep inside
    // the population the scope allows. At `all` with strong provenance the
    // row retires naming the session state; at `provenanced` it keeps, and
    // the reason names the release so the operator sees what a wider scope
    // would do.
    #[test]
    fn x2774_terminal_state_releases_open_work_inside_the_scope() {
        let dir = tmpdir("term");
        let transcript = quiet_transcript(&dir, "sid-1");
        let rows = vec![row("ab12cd34", Some("sid-1"), Some("target-x-bbbb-worker"))];
        let mut g = graph_done("x-aaaa");
        g.statuses.insert("x-bbbb".into(), "in_review".into());
        g.index.insert(
            "sid-1".to_string(),
            vec![("x-bbbb".to_string(), "in_review".to_string())],
        );
        g.work_index = g.index.clone();
        let summary = run(
            &no_home(),
            900,
            RosterScope::All,
            true,
            &roster(rows),
            &[],
            &|| Some(g.clone()),
            &|_e| Some(vec![transcript.clone()]),
            &|entries| {
                entries
                    .iter()
                    .map(|e| (crate::gc::row_handle(e), mtime_age(&[transcript.clone()])))
                    .collect::<HashMap<_, _>>()
            },
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert_eq!(summary.retired.len(), 1, "{summary:?}");
        assert!(
            summary.retired[0]
                .reason
                .starts_with("session terminal: harness state done"),
            "{summary:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // The open-PR keep at scope all: an open node whose PR is unmerged and
    // whose driver is THIS session keeps its row even on a terminal roster
    // state. The default scope keeps the row too, but under the unchanged
    // open-work reason.
    #[test]
    fn open_pr_keep_at_scope_all_beats_the_terminal_release() {
        let dir = tmpdir("open-pr");
        let transcript = quiet_transcript(&dir, "sid-1");
        let rows = vec![row("ab12cd34", Some("sid-1"), Some("target-x-bbbb-worker"))];
        let mut g = graph_done("x-aaaa");
        g.statuses.insert("x-bbbb".into(), "in_review".into());
        g.index.insert(
            "sid-1".to_string(),
            vec![("x-bbbb".to_string(), "in_review".to_string())],
        );
        g.work_index = g.index.clone();
        g.pr_number.insert("x-bbbb".into(), Some(1943));
        g.pr_state.insert("x-bbbb".into(), (None, 0, 0));
        g.do_nodes.insert(
            "sid-1".to_string(),
            std::collections::HashSet::from(["x-bbbb".to_string()]),
        );
        let summary = run(
            &no_home(),
            900,
            RosterScope::All,
            true,
            &roster(rows),
            &[],
            &|| Some(g.clone()),
            &|_e| Some(vec![transcript.clone()]),
            &|entries| {
                entries
                    .iter()
                    .map(|e| (crate::gc::row_handle(e), mtime_age(&[transcript.clone()])))
                    .collect::<HashMap<_, _>>()
            },
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert!(summary.retired.is_empty(), "{summary:?}");
        let kept = &summary.kept[0];
        assert!(kept.reason.contains("open pr: x-bbbb #1943"), "{summary:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // At the default scope, the same row keeps - but the reason names the
    // terminal state, so the hold is legible.
    #[test]
    fn x2774_terminal_state_names_itself_at_the_default_scope() {
        let dir = tmpdir("term-keep");
        let transcript = quiet_transcript(&dir, "sid-1");
        let rows = vec![row("ab12cd34", Some("sid-1"), Some("target-x-bbbb-worker"))];
        let mut g = graph_done("x-aaaa");
        g.statuses.insert("x-bbbb".into(), "in_review".into());
        g.index.insert(
            "sid-1".to_string(),
            vec![("x-bbbb".to_string(), "in_review".to_string())],
        );
        g.work_index = g.index.clone();
        let summary = run(
            &no_home(),
            900,
            RosterScope::Provenanced,
            true,
            &roster(rows),
            &[],
            &|| Some(g.clone()),
            &|_e| Some(vec![transcript.clone()]),
            &|entries| {
                entries
                    .iter()
                    .map(|e| (crate::gc::row_handle(e), mtime_age(&[transcript.clone()])))
                    .collect::<HashMap<_, _>>()
            },
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert!(summary.retired.is_empty(), "{summary:?}");
        let kept = summary
            .kept
            .iter()
            .find(|j| j.reason.contains("session terminal:"))
            .expect("the keep names the terminal state");
        assert!(
            kept.reason.contains("open work: x-bbbb in_review"),
            "{summary:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // An open node holds the row: the work is not done.
    #[test]
    fn open_node_keeps_the_row() {
        let mut g = graph_done("x-aaaa");
        g.statuses.insert("x-bbbb".into(), "in_progress".into());
        let rows = vec![row("ab12cd34", Some("sid-1"), Some("target-x-bbbb-worker"))];
        let summary = run(
            &no_home(),
            900,
            RosterScope::Provenanced,
            true,
            &roster(rows),
            &[],
            &|| Some(g.clone()),
            &|_| None,
            &|_| HashMap::new(),
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert!(summary.retired.is_empty());
        assert!(summary.kept[0].reason.contains("open work"), "{summary:?}");
    }

    // No provenance keeps the row: the positive reason is named, never an
    // absence dressed as a removal. The row's harness state reads done, but
    // with no receipt marker the done release never reaches the quiet gate.
    #[test]
    fn unresolved_provenance_keeps_the_row() {
        let rows = vec![row("ab12cd34", Some("sid-1"), Some("hand-typed-name"))];
        let summary = run(
            &no_home(),
            900,
            RosterScope::Provenanced,
            true,
            &roster(rows),
            &[],
            &|| Some(GraphRead::default()),
            &|_| None,
            &|_| HashMap::new(),
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert!(summary.retired.is_empty());
        assert!(
            summary.kept[0].reason.contains("no provenance"),
            "{summary:?}"
        );
    }

    // A fresh transcript does NOT save a row whose harness state reads
    // terminal: the roster's done is a finish line, not a turn boundary
    //. A working row inside grace still keeps.
    #[test]
    fn fresh_transcript_does_not_save_a_terminal_roster_row() {
        let dir = tmpdir("fresh");
        let transcript = dir.join("sid-1.jsonl");
        std::fs::write(&transcript, "{\"message\":{}}\n").unwrap();
        // The row() helper defaults to state done: the terminal fact wins
        // over the fresh transcript, and the basis names the early fire.
        let rows = vec![row("ab12cd34", Some("sid-1"), Some("target-x-aaaa-worker"))];
        let summary = run(
            &no_home(),
            900,
            RosterScope::Provenanced,
            true,
            &roster(rows),
            &[],
            &|| Some(graph_done_via_sessions("x-aaaa", &["sid-1"])),
            &|_e| Some(vec![transcript.clone()]),
            &|entries| {
                entries
                    .iter()
                    .map(|e| (crate::gc::row_handle(e), mtime_age(&[transcript.clone()])))
                    .collect::<HashMap<_, _>>()
            },
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert_eq!(summary.retired.len(), 1, "{summary:?}");
        assert!(
            summary.retired[0]
                .reason
                .contains("session terminal: harness state done"),
            "{summary:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // A roster the sweep cannot see removes nothing.
    #[test]
    fn unreadable_roster_keeps_everything() {
        let summary = run(
            &no_home(),
            900,
            RosterScope::Provenanced,
            true,
            &ClaudeAgentsSnapshot::unknown("claude exited 1"),
            &[],
            &|| Some(graph_done("x-aaaa")),
            &|_| None,
            &|_| HashMap::new(),
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert_eq!(summary.retired.len(), 0);
        assert!(summary.kept[0].reason.contains("roster unreadable"));
    }

    // A removal that does not confirm lands in refused, never in retired.
    #[test]
    fn unconfirmed_removal_refuses_named() {
        let dir = tmpdir("refuse");
        let transcript = quiet_transcript(&dir, "sid-1");
        let rows = vec![row("ab12cd34", Some("sid-1"), Some("target-x-aaaa-worker"))];
        let summary = run(
            &no_home(),
            900,
            RosterScope::Provenanced,
            false,
            &roster(rows),
            &[],
            &|| Some(graph_done_via_sessions("x-aaaa", &["sid-1"])),
            &|_e| Some(vec![transcript.clone()]),
            &|entries| {
                entries
                    .iter()
                    .map(|e| (crate::gc::row_handle(e), mtime_age(&[transcript.clone()])))
                    .collect::<HashMap<_, _>>()
            },
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::Failed("rm exited 3".into()),
        );
        assert!(summary.retired.is_empty());
        assert_eq!(summary.refused.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn roster_reap_emits_positive_event_for_confirmed_removal() {
        let dir = tmpdir("event-confirmed");
        let home = crate::paths::AgentsHome::at(dir.join("home"));
        home.ensure_root().unwrap();
        let transcript = quiet_transcript(&dir, "sid-event");
        let rows = vec![row(
            "feed1234",
            Some("sid-event"),
            Some("target-x-aaaa-event"),
        )];
        let summary = run(
            &home,
            900,
            RosterScope::Provenanced,
            false,
            &roster(rows),
            &[],
            &|| Some(graph_done_via_sessions("x-aaaa", &["sid-event"])),
            &|_| Some(vec![transcript.clone()]),
            &|entries| {
                entries
                    .iter()
                    .map(|e| (crate::gc::row_handle(e), mtime_age(&[transcript.clone()])))
                    .collect::<HashMap<_, _>>()
            },
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::Removed,
        );
        assert_eq!(summary.retired.len(), 1, "{summary:?}");
        let events = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
        let event = events
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|event| {
                event["type"] == "agent_row_reaped" && event["data"]["short_id"] == "feed1234"
            })
            .expect("confirmed roster removal must emit agent_row_reaped");
        assert_eq!(event["data"]["remover"], "roster-reap");
        assert!(event["data"]["basis"]
            .as_str()
            .is_some_and(|basis| basis.contains("every named node done")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn roster_reap_emits_no_reap_event_for_unconfirmed_removal() {
        let dir = tmpdir("event-refused");
        let home = crate::paths::AgentsHome::at(dir.join("home"));
        home.ensure_root().unwrap();
        let transcript = quiet_transcript(&dir, "sid-refused");
        let rows = vec![row(
            "fade5678",
            Some("sid-refused"),
            Some("target-x-aaaa-refused"),
        )];
        let summary = run(
            &home,
            900,
            RosterScope::Provenanced,
            false,
            &roster(rows),
            &[],
            &|| Some(graph_done_via_sessions("x-aaaa", &["sid-refused"])),
            &|_| Some(vec![transcript.clone()]),
            &|entries| {
                entries
                    .iter()
                    .map(|e| (crate::gc::row_handle(e), mtime_age(&[transcript.clone()])))
                    .collect::<HashMap<_, _>>()
            },
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::Failed("injected refusal".into()),
        );
        assert!(summary.retired.is_empty());
        assert_eq!(summary.refused.len(), 1);
        assert!(summary.refused[0].1.contains("failed"));
        let events = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
        assert!(!events.lines().any(|line| line.contains("fade5678")));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// AC2-ERR, roster side: the event's resumable verdict is measured off
    /// the receipt's resume-evidence, not the staging fact. A transcript
    /// present in the row's own store reads true with its basis; a missing
    /// one reads `no-transcript`, even though the receipt staged fine.
    #[test]
    fn roster_reap_measures_the_resumable_basis() {
        let _env = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tmpdir("resumable-basis");
        let home = crate::paths::AgentsHome::at(dir.join("home"));
        home.ensure_root().unwrap();
        // The real store: a temp HOME whose projects tree holds the
        // transcript for the PRESENT case only.
        let store_home = tempfile::tempdir().unwrap();
        let projects = store_home
            .path()
            .join(".claude")
            .join("projects")
            .join("work");
        std::fs::create_dir_all(&projects).unwrap();
        let staged = quiet_transcript(&projects, "present-1111-2222-3333-444444444444");
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", store_home.path());
        let present = row(
            "present1",
            Some("present-1111-2222-3333-444444444444"),
            Some("target-x-aaaa-present"),
        );
        let absent = row(
            "absent01",
            Some("absent-1111-2222-3333-444444444444"),
            Some("target-x-aaaa-absent"),
        );
        let summary = run(
            &home,
            900,
            RosterScope::Provenanced,
            false,
            &roster(vec![present, absent]),
            &[],
            &|| {
                Some(graph_done_via_sessions(
                    "x-aaaa",
                    &[
                        "present-1111-2222-3333-444444444444",
                        "absent-1111-2222-3333-444444444444",
                    ],
                ))
            },
            &|_| Some(vec![staged.clone()]),
            &|entries| {
                entries
                    .iter()
                    .map(|e| (crate::gc::row_handle(e), Some(10_000i64)))
                    .collect::<HashMap<_, _>>()
            },
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::Removed,
        );
        assert_eq!(summary.retired.len(), 2, "{summary:?}");
        let events = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
        let found: Vec<serde_json::Value> = events
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .filter(|event: &serde_json::Value| event["type"] == "agent_row_reaped")
            .collect();
        let present_event = found
            .iter()
            .find(|e| e["data"]["short_id"] == "present1")
            .expect("the present row emits its event");
        assert_eq!(present_event["data"]["resumable"], true, "{present_event}");
        assert_eq!(
            present_event["data"]["resumable_basis"], "transcript-present",
            "{present_event}"
        );
        let absent_event = found
            .iter()
            .find(|e| e["data"]["short_id"] == "absent01")
            .expect("the absent row emits its event");
        assert_eq!(absent_event["data"]["resumable"], false, "{absent_event}");
        assert_eq!(
            absent_event["data"]["resumable_basis"], "no-transcript",
            "{absent_event}"
        );
        match &old_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    // The renderer prints every bucket at every pass, zero included.
    #[test]
    fn render_names_every_bucket_even_at_zero() {
        let summary = RosterReapSummary::default();
        let out = render(&summary, false, true);
        assert!(out.contains("would retire 0 of 0 roster row(s)"), "{out}");
        let json_out = render(&summary, true, true);
        let v: serde_json::Value = serde_json::from_str(json_out.trim()).unwrap();
        for key in [
            "visited",
            "deduped",
            "kept_owned",
            "kept",
            "retired",
            "refused",
            "dry_run",
        ] {
            assert!(v.get(key).is_some(), "bucket {key} missing: {json_out}");
        }
    }

    // One session listed twice is judged once: the duplicate is counted in
    // `deduped`, never judged, never removed twice.
    #[test]
    fn duplicate_visit_never_judged_twice() {
        let dir = tmpdir("dedupe");
        let transcript = quiet_transcript(&dir, "sid-1");
        let rows = vec![
            row("ab12cd34", Some("sid-1"), Some("target-x-aaaa-worker")),
            row("ef56ab78", Some("sid-1"), Some("target-x-aaaa-copy")),
        ];
        let summary = run(
            &no_home(),
            900,
            RosterScope::Provenanced,
            true,
            &roster(rows),
            &[],
            &|| Some(graph_done_via_sessions("x-aaaa", &["sid-1"])),
            &|_e| Some(vec![transcript.clone()]),
            &|entries| {
                entries
                    .iter()
                    .map(|e| (crate::gc::row_handle(e), mtime_age(&[transcript.clone()])))
                    .collect::<HashMap<_, _>>()
            },
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert_eq!(summary.retired.len(), 1);
        assert_eq!(summary.deduped, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Scope. The knob names the population that may retire;
    // no scope reaches a row with no provenance. ---

    // THE law under every setting: a session with no fno node is never a
    // retire candidate unless its own done report took it to the quiet
    // gate. This row is still working, so the missing provenance keeps it
    // at every scope. At `off` the sweep does not even judge the row, so
    // the reason names the scope.
    #[test]
    fn no_provenance_row_is_never_a_candidate_at_any_scope() {
        let mut r = row("ab12cd34", Some("sid-1"), Some("hand-typed-name"));
        r.state = Some("working".into());
        let rows = vec![r];
        let scopes = [RosterScope::Off, RosterScope::Provenanced, RosterScope::All];
        for scope in scopes {
            let summary = run(
                &no_home(),
                900,
                scope,
                true,
                &roster(rows.clone()),
                &[],
                &|| Some(GraphRead::default()),
                &|_| None,
                &|_| HashMap::new(),
                crate::daemon::now_epoch_secs(),
                &|_| CascadeOutcome::NotApplicable,
            );
            assert!(
                summary.retired.is_empty(),
                "scope {scope:?} retired a no-provenance row: {summary:?}"
            );
            let reason = &summary.kept[0].reason;
            if scope == RosterScope::Off {
                assert!(reason.contains("roster scope off"), "{summary:?}");
            } else {
                assert!(reason.contains("no provenance"), "{summary:?}");
            }
        }
    }

    // The default is the stated contract, not an accident: a provenanced,
    // done, quiet row retires.
    #[test]
    fn provenanced_scope_retires_a_done_quiet_row() {
        let dir = tmpdir("scope-default");
        let transcript = quiet_transcript(&dir, "sid-1");
        let rows = vec![row("ab12cd34", Some("sid-1"), Some("target-x-aaaa-worker"))];
        let summary = run(
            &no_home(),
            900,
            RosterScope::Provenanced,
            true,
            &roster(rows),
            &[],
            &|| Some(graph_done_via_sessions("x-aaaa", &["sid-1"])),
            &|_e| Some(vec![transcript.clone()]),
            &|entries| {
                entries
                    .iter()
                    .map(|e| (crate::gc::row_handle(e), mtime_age(&[transcript.clone()])))
                    .collect::<HashMap<_, _>>()
            },
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert_eq!(summary.retired.len(), 1, "{summary:?}");
        assert!(summary.retired[0].reason.contains("every named node done"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // `all` is exercised beyond the default - but only for rows fno itself
    // spawned. A row whose open node was resolved by NAME (a pattern match,
    // exactly how a hand-started session acquires a phantom node) stays
    // kept even at `all`; the same open row resolved by the sessions join
    // retires.
    #[test]
    fn all_scope_keeps_a_name_provenanced_open_node_row() {
        let dir = tmpdir("scope-all-weak");
        let transcript = quiet_transcript(&dir, "sid-1");
        let rows = vec![row("ab12cd34", Some("sid-1"), Some("target-x-aaaa-worker"))];
        let mut g = graph_done("x-aaaa");
        g.statuses.insert("x-aaaa".into(), "in_progress".into());
        let at_all = run(
            &no_home(),
            900,
            RosterScope::All,
            true,
            &roster(rows.clone()),
            &[],
            &|| Some(g.clone()),
            &|_e| Some(vec![transcript.clone()]),
            &|entries| {
                entries
                    .iter()
                    .map(|e| (crate::gc::row_handle(e), mtime_age(&[transcript.clone()])))
                    .collect::<HashMap<_, _>>()
            },
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert!(at_all.retired.is_empty(), "{at_all:?}");
        assert!(at_all.kept[0].reason.contains("open work"), "{at_all:?}");
        // The same row retires at the default too: done work retires on the
        // sessions-join marker, so the only `all`-only slack is the open
        // work above.
        g.statuses.insert("x-aaaa".into(), "done".into());
        g.index
            .insert("sid-1".into(), vec![("x-aaaa".into(), "done".into())]);
        g.work_index = g.index.clone();
        let at_default = run(
            &no_home(),
            900,
            RosterScope::Provenanced,
            true,
            &roster(rows),
            &[],
            &|| Some(g.clone()),
            &|_e| Some(vec![transcript.clone()]),
            &|entries| {
                entries
                    .iter()
                    .map(|e| (crate::gc::row_handle(e), mtime_age(&[transcript.clone()])))
                    .collect::<HashMap<_, _>>()
            },
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert_eq!(at_default.retired.len(), 1, "{at_default:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // The `all` widening fires for a sessions-provenanced open row: the
    // reverse join names the session, its node is open, the transcript is
    // quiet past the grace.
    #[test]
    fn all_scope_retires_a_spawn_provenanced_open_node_row() {
        let dir = tmpdir("scope-all-sessions");
        let transcript = quiet_transcript(&dir, "sid-1");
        // A non-terminal state: the widening itself is under test here. A
        // done state would take the session-terminal release instead
        // (x2774_terminal_state_releases_open_work_inside_the_scope).
        let mut live = row("ab12cd34", Some("sid-1"), Some("target-x-aaaa-worker"));
        live.state = Some("working".into());
        let rows = vec![live];
        let mut g = graph_done("x-aaaa");
        g.statuses.insert("x-aaaa".into(), "in_progress".into());
        // No recorded merge on the open node: the widening itself is under
        // test, not the merge-lag release.
        g.pr_state.insert("x-aaaa".into(), (None, 0, 0));
        g.index
            .insert("sid-1".into(), vec![("x-aaaa".into(), "do".into())]);
        g.work_index = g.index.clone();
        let at_all = run(
            &no_home(),
            900,
            RosterScope::All,
            true,
            &roster(rows.clone()),
            &[],
            &|| Some(g.clone()),
            &|_e| Some(vec![transcript.clone()]),
            &|entries| {
                entries
                    .iter()
                    .map(|e| (crate::gc::row_handle(e), mtime_age(&[transcript.clone()])))
                    .collect::<HashMap<_, _>>()
            },
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert_eq!(at_all.retired.len(), 1, "{at_all:?}");
        assert!(
            at_all.retired[0].reason.contains("roster scope all"),
            "{at_all:?}"
        );
        assert!(
            at_all.retired[0].reason.contains("via sessions"),
            "{at_all:?}"
        );
        // The identical world is kept at the default: this is the widening.
        let at_default = run(
            &no_home(),
            900,
            RosterScope::Provenanced,
            true,
            &roster(rows),
            &[],
            &|| Some(g.clone()),
            &|_e| Some(vec![transcript.clone()]),
            &|entries| {
                entries
                    .iter()
                    .map(|e| (crate::gc::row_handle(e), mtime_age(&[transcript.clone()])))
                    .collect::<HashMap<_, _>>()
            },
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert!(at_default.retired.is_empty(), "{at_default:?}");
        assert!(
            at_default.kept[0].reason.contains("open work"),
            "{at_default:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // `off` keeps even a fully eligible row, and names the scope.
    #[test]
    fn off_scope_keeps_a_fully_eligible_row() {
        let dir = tmpdir("scope-off");
        let transcript = quiet_transcript(&dir, "sid-1");
        let rows = vec![row("ab12cd34", Some("sid-1"), Some("target-x-aaaa-worker"))];
        let summary = run(
            &no_home(),
            900,
            RosterScope::Off,
            true,
            &roster(rows),
            &[],
            &|| Some(graph_done("x-aaaa")),
            &|_e| Some(vec![transcript.clone()]),
            &|entries| {
                entries
                    .iter()
                    .map(|e| (crate::gc::row_handle(e), mtime_age(&[transcript.clone()])))
                    .collect::<HashMap<_, _>>()
            },
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert!(summary.retired.is_empty(), "{summary:?}");
        assert!(
            summary.kept[0].reason.contains("roster scope off"),
            "{summary:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // A hold (PR contradiction here) stays a keep at `all`: contested truth
    // is not a scope question.
    #[test]
    fn hold_stays_kept_at_all_scope() {
        let dir = tmpdir("scope-hold");
        let transcript = quiet_transcript(&dir, "sid-1");
        let rows = vec![row("ab12cd34", Some("sid-1"), Some("target-x-aaaa-worker"))];
        let g = GraphRead {
            statuses: HashMap::from([("x-aaaa".to_string(), "done".to_string())]),
            pr_state: HashMap::from([("x-aaaa".to_string(), (Some("open".to_string()), 0, 0))]),
            ..Default::default()
        };
        let summary = run(
            &no_home(),
            900,
            RosterScope::All,
            true,
            &roster(rows),
            &[],
            &|| Some(g.clone()),
            &|_e| Some(vec![transcript.clone()]),
            &|entries| {
                entries
                    .iter()
                    .map(|e| (crate::gc::row_handle(e), mtime_age(&[transcript.clone()])))
                    .collect::<HashMap<_, _>>()
            },
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert!(summary.retired.is_empty(), "{summary:?}");
        assert!(
            summary.kept[0].reason.contains("pr state contradicts"),
            "{summary:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A fresh transcript (inside grace, unlike `quiet_transcript`).
    fn fresh_transcript(dir: &std::path::Path, sid: &str) -> PathBuf {
        let path = dir.join(format!("{sid}.jsonl"));
        std::fs::write(&path, "{\"message\":{}}\n").unwrap();
        let f = std::fs::File::options().append(true).open(&path).unwrap();
        f.set_modified(std::time::SystemTime::now()).unwrap();
        path
    }

    /// the roster-side twin of the grace_gate conjunct. An AllDone
    /// row reading done with a transcript 60s old retires and names the
    /// early fire; reading working it keeps with the active line.
    #[test]
    fn xb7f8_terminal_state_overrides_recency_at_the_roster_sweep() {
        let dir = tmpdir("term-recency");
        let transcript = fresh_transcript(&dir, "sid-1");
        let rows = vec![row("ab12cd34", Some("sid-1"), Some("target-x-aaaa-worker"))];
        let summary = run(
            &no_home(),
            900,
            RosterScope::Provenanced,
            true,
            &roster(rows),
            &[],
            &|| Some(graph_done_via_sessions("x-aaaa", &["sid-1"])),
            &|_e| Some(vec![transcript.clone()]),
            &|entries| {
                entries
                    .iter()
                    .map(|e| (crate::gc::row_handle(e), mtime_age(&[transcript.clone()])))
                    .collect::<HashMap<_, _>>()
            },
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert_eq!(summary.retired.len(), 1, "{summary:?}");
        assert!(
            summary.retired[0]
                .reason
                .contains("session terminal: harness state done"),
            "{summary:?}"
        );
        std::fs::remove_dir_all(&dir).ok();

        let dir = tmpdir("term-recency-keep");
        let transcript = fresh_transcript(&dir, "sid-1");
        let mut working = row("ab12cd34", Some("sid-1"), Some("target-x-aaaa-worker"));
        working.state = Some("working".into());
        let rows = vec![working];
        let summary = run(
            &no_home(),
            900,
            RosterScope::Provenanced,
            true,
            &roster(rows),
            &[],
            &|| Some(graph_done_via_sessions("x-aaaa", &["sid-1"])),
            &|_e| Some(vec![transcript.clone()]),
            &|entries| {
                entries
                    .iter()
                    .map(|e| (crate::gc::row_handle(e), mtime_age(&[transcript.clone()])))
                    .collect::<HashMap<_, _>>()
            },
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert!(summary.retired.is_empty(), "{summary:?}");
        assert!(
            summary.kept[0]
                .reason
                .starts_with("active: transcript written "),
            "{summary:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
