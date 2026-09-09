//! The roster-side sweep (x-aad0, gap one): claude rows no fno registry row
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
//! failed outright keeps everything. No fno registry row may name the row,
//! by session id, short id, or name: an owned row is the registry sweep's
//! business, and this sweep keeps it with that reason. The shared
//! provenance verdict must resolve a provenance and read the node done,
//! with the PR confirm passing - the same function the registry sweep runs,
//! so the two sweeps cannot disagree about which rows are dead. The
//! transcript must read quiet past the grace, with an unreadable transcript
//! never quiet. Only then is the row removed from the claude active surface
//! through the same cascade `rm` walks, with the typed outcome recorded and
//! a receipt staged when none exists yet (an earlier retirement's receipt
//! is the record it already made; this pass's outcome is in the summary).
//! A history deletion never happens: the transcript survives, resume still
//! opens it.
//!
//! The scope (`agents.reap.roster_scope`) names the population that may
//! retire, as an operator setting: `off` retires nothing, `provenanced`
//! (the default) is the chain above, `all` widens to resolved rows with
//! open work. One rule sits under every value: a row that resolves to no
//! fno node is never retirable at any scope. An operator session names no
//! fno node, so a hand-started session is unreachable by construction, not
//! by default value, and a wrong config cannot reach it.

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::claude_roster::{ClaudeAgentRow, ClaudeAgentsSnapshot};
use crate::daemon::CascadeOutcome;
use crate::gc::transcript_age_s;
use crate::gc_sweep::{provenance_verdict, GraphRead};
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
pub fn run(
    home: &crate::paths::AgentsHome,
    grace_secs: i64,
    scope: crate::agents_config::RosterScope,
    dry_run: bool,
    roster: &ClaudeAgentsSnapshot,
    registry: &[RegistryEntry],
    read_graph: &dyn Fn() -> Option<GraphRead>,
    transcripts: &dyn Fn(&RegistryEntry) -> Option<Vec<PathBuf>>,
    now: i64,
    remove: &dyn Fn(&RegistryEntry) -> CascadeOutcome,
) -> RosterReapSummary {
    let mut summary = RosterReapSummary::default();
    let rows: Vec<ClaudeAgentRow> = match roster {
        ClaudeAgentsSnapshot::Known { rows, .. } => rows.clone(),
        ClaudeAgentsSnapshot::Unknown { rows, warnings } => {
            if rows.is_empty() {
                // The enumeration failed outright: keep everything. A sweep
                // that cannot see the roster removes nothing.
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
    let owned: BTreeSet<String> = registry
        .iter()
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
        let mut entry = RegistryEntry::new(row.session_id.clone(), crate::state::Lineage::none());
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
        let verdict = provenance_verdict(&entry, sid, graph, hits.as_deref());
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
        // population to resolved rows with open work; `provenanced` keeps
        // them. NoProvenance keeps at EVERY scope: an operator session
        // names no fno node, so this keep is by construction, not by
        // default value.
        let basis: String = match &verdict.work {
            WorkState::AllDone { nodes } => format!(
                "every named node done: {} (via {})",
                nodes.join(", "),
                verdict
                    .route
                    .source
                    .map(|s| s.as_str())
                    .unwrap_or("sessions")
            ),
            WorkState::Open { node: n, status }
                if scope == crate::agents_config::RosterScope::All =>
            {
                format!(
                    "open work {n} {status} at roster scope all (via {})",
                    verdict
                        .route
                        .source
                        .map(|s| s.as_str())
                        .unwrap_or("sessions")
                )
            }
            WorkState::Open { node: n, status } => {
                summary.kept.push(judgement(
                    &ident,
                    Some(n.clone()),
                    format!("open work: {n} {status}"),
                    false,
                ));
                continue;
            }
            WorkState::NoProvenance => {
                summary.kept.push(judgement(
                    &ident,
                    None,
                    crate::gc::KeepReason::NoProvenance.as_str().to_string(),
                    false,
                ));
                continue;
            }
        };
        // The quiet gate: an unresolved transcript is never quiet, and the
        // age rides the reason so a keep is auditable.
        let age = transcript_age_s(hits.as_deref(), now);
        match age {
            None => summary.kept.push(judgement(
                &ident,
                node,
                "transcript unresolved".into(),
                false,
            )),
            Some(age) if age <= grace_secs => summary.kept.push(judgement(
                &ident,
                node,
                format!("active: transcript written {age}s ago"),
                false,
            )),
            Some(_) => {
                if dry_run {
                    summary.retired.push(judgement(&ident, node, basis, true));
                } else {
                    let outcome = remove(&entry);
                    if outcome.satisfies_applied() {
                        summary.retired.push(judgement(&ident, node, basis, true));
                        write_receipt(home, &entry, &outcome);
                    } else {
                        summary.refused.push((
                            ident.clone(),
                            format!("the native removal did not confirm ({})", outcome.as_str()),
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
fn write_receipt(home: &crate::paths::AgentsHome, entry: &RegistryEntry, outcome: &CascadeOutcome) {
    let receipt = match crate::receipt::build_reap_receipt(entry, None) {
        Ok(mut receipt) => {
            if crate::receipt::reap_receipt_path(home, &receipt).exists() {
                return;
            }
            receipt.removed_by = Some("roster-reap".to_string());
            receipt.effects = vec![outcome.effect_record("active-surface")];
            let _ = crate::receipt::write_reap_receipt(home, &receipt);
        }
        Err(_) => {}
    };
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
    let registry = crate::state::load_registry(&home.registry_json()).unwrap_or_default();
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
            pr_state: HashMap::from([(node.to_string(), (Some("merged".into()), 0))]),
            ..Default::default()
        }
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

    fn no_home() -> crate::paths::AgentsHome {
        crate::paths::AgentsHome::at(std::path::Path::new("/nonexistent-roster-reap"))
    }

    // The whole positive chain: unmatched, resolved by name, done, merged,
    // quiet -> would retire with the basis naming the route.
    #[test]
    fn unmatched_done_quiet_row_would_retire_via_name() {
        let dir = tmpdir("retire");
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
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert_eq!(summary.retired.len(), 1, "{:?}", summary.kept);
        assert_eq!(summary.retired[0].node.as_deref(), Some("x-aaaa"));
        assert!(
            summary.retired[0].reason.contains("via name"),
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
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert_eq!(summary.kept_owned, 1);
        assert!(summary.retired.is_empty());
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
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert!(summary.retired.is_empty());
        assert!(summary.kept[0].reason.contains("open work"), "{summary:?}");
    }

    // No provenance keeps the row: the positive reason is named, never an
    // absence dressed as a removal.
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
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert!(summary.retired.is_empty());
        assert!(
            summary.kept[0].reason.contains("no provenance"),
            "{summary:?}"
        );
    }

    // A fresh transcript keeps the row: liveness outranks doneness.
    #[test]
    fn fresh_transcript_keeps_a_done_node_row() {
        let dir = tmpdir("fresh");
        let transcript = dir.join("sid-1.jsonl");
        std::fs::write(&transcript, "{\"message\":{}}\n").unwrap();
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
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert!(summary.retired.is_empty());
        assert!(summary.kept[0].reason.contains("active"), "{summary:?}");
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
            &|| Some(graph_done("x-aaaa")),
            &|_e| Some(vec![transcript.clone()]),
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::Failed("rm exited 3".into()),
        );
        assert!(summary.retired.is_empty());
        assert_eq!(summary.refused.len(), 1);
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
            &|| Some(graph_done("x-aaaa")),
            &|_e| Some(vec![transcript.clone()]),
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
    // retire candidate. At `off` the sweep does not even judge the row, so
    // the reason names the scope; at `provenanced` and `all` it names the
    // missing provenance.
    #[test]
    fn no_provenance_row_is_never_a_candidate_at_any_scope() {
        let rows = vec![row("ab12cd34", Some("sid-1"), Some("hand-typed-name"))];
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
            &|| Some(graph_done("x-aaaa")),
            &|_e| Some(vec![transcript.clone()]),
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert_eq!(summary.retired.len(), 1, "{summary:?}");
        assert!(summary.retired[0].reason.contains("every named node done"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // `all` is exercised beyond the default: an OPEN-node row quiet past the
    // grace retires at `all` and is kept at `provenanced`.
    #[test]
    fn all_scope_retires_an_open_node_row_the_default_keeps() {
        let dir = tmpdir("scope-all");
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
            crate::daemon::now_epoch_secs(),
            &|_| CascadeOutcome::NotApplicable,
        );
        assert_eq!(at_all.retired.len(), 1, "{at_all:?}");
        assert!(
            at_all.retired[0].reason.contains("roster scope all"),
            "{at_all:?}"
        );
        let at_default = run(
            &no_home(),
            900,
            RosterScope::Provenanced,
            true,
            &roster(rows),
            &[],
            &|| Some(g.clone()),
            &|_e| Some(vec![transcript.clone()]),
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
            pr_state: HashMap::from([("x-aaaa".to_string(), (Some("open".to_string()), 0))]),
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
}
