//! The x-e3cc test families: the hold clock on every reaper hold, the
//! release verb's rulings and refusals. Same-module helpers resolve through
//! the parent glob; gc_receipts' fixtures are `pub(super)`.

use super::gc_receipts::*;
use super::*;
use crate::gc_sweep::{self, GcSummary, GraphRead};
use std::collections::HashMap;

// ── x-e3cc: every hold carries an age, a basis and an escalation ─────────

fn find_hold<'a>(summary: &'a GcSummary, id: &str) -> &'a crate::gc_sweep::Hold {
    summary
        .holds
        .iter()
        .find(|h| h.id == id)
        .unwrap_or_else(|| panic!("no hold for {id} in {:?}", summary.holds))
}

fn assert_hold_line(summary: &GcSummary, dry_run: bool, id: &str, needle: &str) {
    let text = crate::reap_render::render_reap(summary, false, dry_run);
    let line = text
        .lines()
        .find(|l| l.contains(&format!("kept {id} (")) || l.contains(&format!("held {id} (")))
        .unwrap_or_else(|| panic!("no hold line for {id} in:\n{text}"));
    assert!(line.contains(needle), "line for {id}: {line}");
}

/// AC2-HP: one row held under each of sources disagree, transcript
/// unresolved, open do row and needs live stop in one dry run; every hold
/// line found by its id carries `[held ` and an age basis, and the summary's
/// `holds` projection answers for all four.
#[test]
fn ac2_hp_every_hold_line_carries_an_age_and_basis() {
    let home = tmp_home("gc-ac2-hp");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "quiet.jsonl", 2 * 3600);
    state::update_registry(&home.registry_json(), |r| {
        // sources disagree: sessions names x-aaaa, the registry field x-cccc.
        let mut conf = ask_row("conf-row", None);
        conf.short_id = "confrow".into();
        conf.node = Some("x-cccc".into());
        conf.harness_session_id = Some("sess-conf".into());
        conf.origin = Some("spawn".into());
        r.entries.push(conf);
        // transcript unresolved: the age seam answers nothing for this row.
        let mut tu = ask_row("tu-row", None);
        tu.short_id = "turow".into();
        tu.harness_session_id = Some("sess-tu".into());
        tu.origin = Some("spawn".into());
        r.entries.push(tu);
        // open do row on a done node.
        let mut odr = ask_row("odr-row", None);
        odr.short_id = "odrrow".into();
        odr.harness_session_id = Some("sess-odr".into());
        odr.origin = Some("spawn".into());
        r.entries.push(odr);
        // needs live stop: a claude row a dry run cannot promise a stop for.
        let nls = claude_worker_row("nls-row", "nlsrow");
        r.entries.push(nls);
    })
    .unwrap();

    let summary = evidence_sweep(
        &home,
        &emitter,
        900,
        true,
        graph_read(
            &[
                ("sess-conf", "x-aaaa", "done"),
                ("x-cccc", "x-cccc", "done"),
                ("sess-odr", "N1", "done"),
                ("sess-tu", "N2", "done"),
                ("nlsrow-1111-2222-3333-444444444444", "N3", "done"),
            ],
            &[("sess-odr", "N1")],
        ),
        &|e| match e.harness_session_id.as_deref() {
            Some("sess-tu") => None,
            _ => Some(vec![quiet.clone()]),
        },
        no_agents(),
        &|_| false,
    );

    let ids: Vec<&str> = summary.holds.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(ids.len(), 4, "holds: {:?}", summary.holds);
    for (id, reason, basis) in [
        ("confrow", "sources disagree", "transcript quiet"),
        ("turow", "transcript unresolved", "row created"),
        ("odrrow", "open do row on done node", "transcript quiet"),
        ("nlsrow", "needs live stop", "transcript quiet"),
    ] {
        let h = find_hold(&summary, id);
        assert!(h.reason.contains(reason), "{id}: {:?}", h);
        assert!(h.age_s.is_some(), "{id}: {:?}", h);
        assert_eq!(h.age_basis, basis, "{id}: {:?}", h);
        // The TU line renders main's x-1b90 clock (`for {age}`); the other
        // holds render the [held ...] suffix.
        if id == "turow" {
            assert_hold_line(&summary, true, id, "transcript unresolved for");
        } else {
            assert_hold_line(&summary, true, id, "[held ");
        }
    }
}

/// AC2-HP, the real-run twin: a `stop refused` hold carries its clock too.
#[test]
fn ac2_hp_stop_refused_hold_carries_an_age() {
    let home = tmp_home("gc-ac2-stop");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "quiet.jsonl", 2 * 3600);
    state::update_registry(&home.registry_json(), |r| {
        let row = claude_worker_row("stop-row", "stoprow");
        r.entries.push(row);
    })
    .unwrap();

    let summary = evidence_sweep(
        &home,
        &emitter,
        900,
        false,
        graph_read(
            &[("stoprow-1111-2222-3333-444444444444", "N1", "done")],
            &[],
        ),
        &|_| Some(vec![quiet.clone()]),
        no_agents(),
        &|_| false,
    );

    assert!(
        !summary.stop_refused.is_empty(),
        "{:?}",
        summary.stop_refused
    );
    let id = &summary.stop_refused[0].0;
    let h = find_hold(&summary, id);
    assert_eq!(h.reason, "stop refused");
    assert_eq!(h.age_basis, "transcript quiet");
    assert_hold_line(&summary, false, id, "[held ");
}

/// AC2-ESC: a hold aged 5401s past a 5400s threshold is escalated in the
/// JSON and its text line names the release verb.
#[test]
fn ac2_esc_escalated_hold_names_the_release_verb() {
    let home = tmp_home("gc-ac2-esc");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    state::update_registry(&home.registry_json(), |r| {
        let mut row = ask_row("esc-row", None);
        row.short_id = "escrow".into();
        row.node = Some("x-cccc".into());
        row.harness_session_id = Some("sess-esc".into());
        row.origin = Some("spawn".into());
        r.entries.push(row);
    })
    .unwrap();

    let graph = graph_read(
        &[("sess-esc", "x-aaaa", "done"), ("x-cccc", "x-cccc", "done")],
        &[],
    );
    let mut summary = gc_sweep::run(
        &home,
        &emitter,
        900,
        true,
        7,
        &move |_| graph.clone(),
        &|_| None,
        &uniform_ages(5401),
        &|_| true,
        &|_| crate::daemon::CascadeOutcome::NotApplicable,
        &|_e| crate::daemon::CascadeOutcome::NotApplicable,
        &no_agents,
        &|_| (None, None),
        &|_| None,
    );
    summary.mark_escalated(std::time::Duration::from_secs(5400));

    let h = find_hold(&summary, "escrow");
    assert_eq!(h.reason, "sources disagree");
    assert_eq!(h.age_s, Some(5401));
    assert!(h.escalated, "{h:?}");
    let text = crate::reap_render::render_reap(&summary, false, true);
    assert!(
        text.contains(&format!("fno agents reap --release escrow")),
        "text: {text}"
    );
    let json_text = crate::reap_render::render_reap(&summary, true, true);
    assert!(
        json_text.contains("\"escalated\":true"),
        "json: {json_text}"
    );
    let h2 = find_hold(&summary, "escrow");
    assert!(h2.escalated, "{h2:?}");
}

/// AC2-EDGE: a `transcript unresolved` row whose `created_at` does not
/// parse is `unmeasured` and never escalates.
#[test]
fn ac2_edge_unmeasured_hold_never_escalates() {
    let home = tmp_home("gc-ac2-unmeasured");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    state::update_registry(&home.registry_json(), |r| {
        let mut row = ask_row("unm-row", None);
        row.short_id = "unmrow".into();
        row.harness_session_id = Some("sess-unm".into());
        row.origin = Some("spawn".into());
        row.created_at = "not-a-stamp".into();
        r.entries.push(row);
    })
    .unwrap();

    let graph = graph_read(&[("sess-unm", "N1", "done")], &[]);
    let mut summary = gc_sweep::run(
        &home,
        &emitter,
        900,
        true,
        7,
        &move |_| graph.clone(),
        &|_| None,
        &staged_ages(&|_| None),
        &|_| true,
        &|_| crate::daemon::CascadeOutcome::NotApplicable,
        &|_e| crate::daemon::CascadeOutcome::NotApplicable,
        &no_agents,
        &|_| (None, None),
        &|_| None,
    );
    summary.mark_escalated(std::time::Duration::from_secs(5400));

    let h = find_hold(&summary, "unmrow");
    assert!(h.age_s.is_none(), "{h:?}");
    assert_eq!(h.age_basis, "unmeasured");
    assert!(!h.escalated, "{h:?}");
    // The projection carries the unmeasured basis even though the TU line
    // renders main's clock.
    assert_hold_line(&summary, true, "unmrow", "transcript unresolved for");
}

/// AC2-EDGE: an open do row blocked by an unrecorded additional PR names
/// the settle blocker in its hold line.
#[test]
fn ac2_edge_open_do_hold_names_the_settle_blocker() {
    let home = tmp_home("gc-ac2-odr");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    state::update_registry(&home.registry_json(), |r| {
        let mut row = ask_row("odr-row", None);
        row.short_id = "odrrow".into();
        row.harness_session_id = Some("sess-odr2".into());
        row.origin = Some("spawn".into());
        r.entries.push(row);
    })
    .unwrap();

    let mut graph = graph_read(&[("sess-odr2", "N1", "done")], &[("sess-odr2", "N1")]);
    graph
        .as_mut()
        .unwrap()
        .pr_state
        .insert("N1".to_string(), (None, 1, 1));
    let summary = gc_sweep::run(
        &home,
        &emitter,
        900,
        true,
        7,
        &move |_| graph.clone(),
        &|_| None,
        &uniform_ages(60),
        &|_| true,
        &|_| crate::daemon::CascadeOutcome::NotApplicable,
        &|_e| crate::daemon::CascadeOutcome::NotApplicable,
        &no_agents,
        &|_| (None, None),
        &|_| None,
    );

    let h = find_hold(&summary, "odrrow");
    assert_eq!(h.detail, "additional_prs: 1 of 1 not recorded merged");
    assert_hold_line(
        &summary,
        true,
        "odrrow",
        "additional_prs: 1 of 1 not recorded merged",
    );
}

// ── x-e3cc change 3: the release verb applies a ruling through the
// sweep's own door ────────────────────────────────────────────────────────

/// AC3-HP: a release retires the old transcript-unresolved row, keeps the
/// fresh hold, and keeps the active row - all in that one summary.
#[test]
fn ac3_hp_the_release_retires_the_ruled_row_and_keeps_the_rest() {
    let home = tmp_home("gc-ac3-hp");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let fresh = quiet_transcript(transcripts.path(), "fresh.jsonl", 30);
    let stamp = |ago_s: i64| {
        (chrono::Utc::now() - chrono::Duration::seconds(ago_s))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    };
    state::update_registry(&home.registry_json(), |r| {
        let mut old = ask_row("old-row", None);
        old.short_id = "oldrow".into();
        old.harness_session_id = Some("sess-old".into());
        old.origin = Some("spawn".into());
        old.created_at = stamp(7200);
        r.entries.push(old);
        let mut mid = ask_row("mid-row", None);
        mid.short_id = "midrow".into();
        mid.harness_session_id = Some("sess-mid".into());
        mid.origin = Some("spawn".into());
        mid.created_at = stamp(60);
        r.entries.push(mid);
        let mut act = ask_row("act-row", None);
        act.short_id = "actrow".into();
        act.harness_session_id = Some("sess-act".into());
        act.origin = Some("spawn".into());
        r.entries.push(act);
    })
    .unwrap();

    let graph = graph_read(
        &[
            ("sess-old", "N1", "done"),
            ("sess-mid", "N2", "done"),
            ("sess-act", "N3", "done"),
        ],
        &[],
    );
    let release = gc_sweep::Release {
        handle: "oldrow".to_string(),
        reason: "transcript unresolved".to_string(),
        detail: "absence is not quiet".to_string(),
    };
    let summary = gc_sweep::run_with_release(
        &home,
        &emitter,
        900,
        false,
        7,
        &move |_| graph.clone(),
        &|_| None,
        &staged_ages(&|e| match e.harness_session_id.as_deref() {
            Some("sess-act") => Some(vec![fresh.clone()]),
            _ => None,
        }),
        &|_| true,
        &|_| crate::daemon::CascadeOutcome::NotApplicable,
        &|_e| crate::daemon::CascadeOutcome::NotApplicable,
        &no_agents,
        &|_| (None, None),
        &|_| None,
        Some(&release),
    );

    let (id, basis) = &summary.retired[0];
    assert_eq!(id, "oldrow");
    assert!(
        basis.starts_with("released transcript unresolved held 2h"),
        "basis: {basis}"
    );
    assert!(
        summary
            .kept_transcript_unresolved
            .iter()
            .any(|h| h.id == "midrow"),
        "{:?}",
        summary.kept_transcript_unresolved
    );
    assert!(
        summary.kept_active.iter().any(|(id, _)| id == "actrow"),
        "{:?}",
        summary.kept_active
    );
}

/// AC3-EDGE, the stop family: the release satisfies the stop gate whether
/// or not absence confirms, the stop is still issued, and the basis names
/// `stop issued, unconfirmed`.
#[test]
fn ac3_edge_stop_release_issues_and_names_an_unconfirmed_stop() {
    let home = tmp_home("gc-ac3-stop");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "quiet.jsonl", 7200);
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(claude_worker_row("stop-row", "stoprow"));
    })
    .unwrap();

    let graph = graph_read(
        &[("stoprow-1111-2222-3333-444444444444", "N1", "done")],
        &[],
    );
    let release = gc_sweep::Release {
        handle: "stoprow".to_string(),
        reason: "needs live stop".to_string(),
        detail: "mode-dependent wording".to_string(),
    };
    let summary = gc_sweep::run_with_release(
        &home,
        &emitter,
        900,
        false,
        7,
        &move |_| graph.clone(),
        &|_| Some(vec![quiet.clone()]),
        &staged_ages(&|_| Some(vec![quiet.clone()])),
        &|_| false,
        &|_| crate::daemon::CascadeOutcome::NotApplicable,
        &|_e| crate::daemon::CascadeOutcome::NotApplicable,
        &no_agents,
        &|_| (None, None),
        &|_| None,
        Some(&release),
    );

    assert_eq!(summary.retired.len(), 1, "{:?}", summary.retired);
    assert!(
        summary.retired[0].1.contains("stop issued, unconfirmed"),
        "basis: {}",
        summary.retired[0].1
    );
}

/// AC3-EDGE, the changed hold: a release whose captured hold no longer
/// matches the row's current hold keeps the row and names the change.
#[test]
fn ac3_edge_a_changed_hold_refuses_the_release() {
    let home = tmp_home("gc-ac3-changed");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    state::update_registry(&home.registry_json(), |r| {
        let mut row = ask_row("chg-row", None);
        row.short_id = "chgrow".into();
        row.harness_session_id = Some("sess-chg".into());
        row.origin = Some("spawn".into());
        r.entries.push(row);
    })
    .unwrap();

    let graph = graph_read(&[("sess-chg", "N1", "done")], &[]);
    let release = gc_sweep::Release {
        handle: "chgrow".to_string(),
        reason: "sources disagree".to_string(),
        detail: "sessions x-aaaa vs registry x-cccc".to_string(),
    };
    let summary = gc_sweep::run_with_release(
        &home,
        &emitter,
        900,
        true,
        7,
        &move |_| graph.clone(),
        &|_| None,
        &staged_ages(&|_| None),
        &|_| true,
        &|_| crate::daemon::CascadeOutcome::NotApplicable,
        &|_e| crate::daemon::CascadeOutcome::NotApplicable,
        &no_agents,
        &|_| (None, None),
        &|_| None,
        Some(&release),
    );

    assert!(summary.retired.is_empty(), "{:?}", summary.retired);
    assert!(
        summary
            .release_refused
            .iter()
            .any(|line| line.contains("release refused: hold changed from sources disagree")),
        "{:?}",
        summary.release_refused
    );
    let text = crate::reap_render::render_reap(&summary, false, true);
    assert!(
        text.contains("release refused: hold changed"),
        "text: {text}"
    );
}

/// AC3-ERR, the fresh hold: the verb refuses with exit 2 and the named
/// line, built by the pure refusal builders the tests assert on.
#[test]
fn ac3_err_the_verb_refuses_a_fresh_hold_below_the_threshold() {
    let (dir, home) = staged_graph_home();
    stage_graph(
        dir.path(),
        json!([done_node(
            "N1",
            json!("merged"),
            json!([]),
            vec![open_do_row("claude", "sess-fresh")],
        )]),
    );
    let created = (chrono::Utc::now() - chrono::Duration::seconds(60))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    state::update_registry(&home.registry_json(), |r| {
        let mut row = ask_row("fresh-row", None);
        row.short_id = "freshrow".into();
        row.harness_session_id = Some("sess-fresh".into());
        row.origin = Some("spawn".into());
        row.created_at = created.clone();
        r.entries.push(row);
    })
    .unwrap();

    let code = crate::reap_release::run(&home, dir.path(), "freshrow");
    assert_eq!(code, 2);

    // The refusal line, asserted through its builder so the wording is
    // pinned without capturing stderr.
    let mut dry = gc_sweep_dry_run(&home, 900);
    dry.mark_escalated(std::time::Duration::from_secs(5400));
    let hold = dry.holds.iter().find(|h| h.id == "freshrow").unwrap();
    assert!(!hold.escalated);
    let message = crate::reap_release::fresh_hold_refusal(hold, 5400);
    assert!(
        message.contains("below agents.hold_escalate_after_s"),
        "{message}"
    );
}

/// AC3-ERR, open work: an escalated `sources disagree` hold whose witness
/// node reads ready refuses; a release never retires open work.
#[test]
fn ac3_err_the_verb_refuses_a_not_done_witness() {
    let (dir, home) = staged_graph_home();
    let sessions_node = json!({
        "id": "x-84b2",
        "status": "ready",
        "additional_prs": [],
        "sessions": [open_do_row("claude", "sess-wit")],
    });
    let registry_node = json!({
        "id": "x-cccc",
        "status": "done",
        "merge_status": "merged",
        "additional_prs": [],
        "sessions": [],
    });
    stage_graph(dir.path(), json!([sessions_node, registry_node]));
    let created = (chrono::Utc::now() - chrono::Duration::seconds(7200))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    state::update_registry(&home.registry_json(), |r| {
        let mut row = ask_row("wit-row", None);
        row.short_id = "witrow".into();
        row.node = Some("x-cccc".into());
        row.harness_session_id = Some("sess-wit".into());
        row.origin = Some("spawn".into());
        row.created_at = created.clone();
        r.entries.push(row);
    })
    .unwrap();

    let code = crate::reap_release::run(&home, dir.path(), "witrow");
    assert_eq!(code, 2);
    // And the refusal names the open node through the pure builder.
    let mut dry = gc_sweep_dry_run(&home, 900);
    dry.mark_escalated(std::time::Duration::from_secs(5400));
    eprintln!("HOLDS: {:?}", dry.holds);
    eprintln!("CONFLICTS: {:?}", dry.kept_node_conflict);
    eprintln!("OPENWORK: {:?}", dry.kept_open_work);
    let hold = dry.holds.iter().find(|h| h.id == "witrow").unwrap();
    assert_eq!(hold.reason, "sources disagree");
    let refusal = crate::reap_release::witness_refusal(&home, hold).unwrap();
    assert!(
        refusal.contains("x-84b2 reads ready; a release never retires open work"),
        "{refusal}"
    );
}

/// The conflict-release lift at apply time: work reads AllDone over the two
/// witness NODES (bare ids, never the "<source> <node>" hold strings), and
/// the basis names both. Found by the review sweep: the first draft pushed
/// the formatted dissent side into the nodes vec.
#[test]
fn ac3_the_conflict_release_reads_all_done_over_bare_witness_nodes() {
    let home = tmp_home("gc-ac3-conflict");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "quiet.jsonl", 7200);
    state::update_registry(&home.registry_json(), |r| {
        let mut row = ask_row("conf-row", None);
        row.short_id = "confrow".into();
        row.node = Some("x-cccc".into());
        row.harness_session_id = Some("sess-conf".into());
        row.origin = Some("spawn".into());
        r.entries.push(row);
    })
    .unwrap();

    let graph = graph_read(
        &[
            ("sess-conf", "x-aaaa", "done"),
            ("x-cccc", "x-cccc", "done"),
        ],
        &[],
    );
    let release = gc_sweep::Release {
        handle: "confrow".to_string(),
        reason: "sources disagree".to_string(),
        detail: "sessions x-aaaa vs registry x-cccc".to_string(),
    };
    let summary = gc_sweep::run_with_release(
        &home,
        &emitter,
        900,
        false,
        7,
        &move |_| graph.clone(),
        &|_| Some(vec![quiet.clone()]),
        &staged_ages(&|_| Some(vec![quiet.clone()])),
        &|_| true,
        &|_| crate::daemon::CascadeOutcome::NotApplicable,
        &|_e| crate::daemon::CascadeOutcome::NotApplicable,
        &no_agents,
        &|_| (None, None),
        &|_| None,
        Some(&release),
    );

    assert_eq!(summary.retired.len(), 1, "{:?}", summary.retired);
    let basis = &summary.retired[0].1;
    assert!(
        basis.starts_with("released sources disagree held 2h00m"),
        "basis: {basis}"
    );
    assert!(
        basis.contains("every named node done: x-aaaa, x-cccc"),
        "bare witness nodes, never the hold strings: {basis}"
    );
}

/// King specimen (2026-09-11): a deferred witness is parked, not open work.
/// The release gate accepts done and the inactive statuses; only ACTIVE
/// work refuses.
#[test]
fn the_witness_gate_releases_a_parked_witness_and_refuses_an_active_one() {
    let (dir, home) = staged_graph_home();
    stage_graph(
        dir.path(),
        json!([
            {"id": "x-done", "status": "done", "additional_prs": [], "sessions": []},
            {"id": "x-defer", "status": "deferred", "additional_prs": [], "sessions": []},
            {"id": "x-ready", "status": "ready", "additional_prs": [], "sessions": []},
        ]),
    );
    let hold_for = |detail: &str| gc_sweep::Hold {
        id: "row".to_string(),
        reason: "sources disagree",
        detail: detail.to_string(),
        age_s: Some(7200),
        age_basis: "row created",
        escalated: true,
    };

    let parked = crate::reap_release::witness_refusal(
        &home,
        &hold_for("sessions x-done vs registry x-defer"),
    );
    assert!(parked.is_none(), "deferred is not open work: {parked:?}");

    let active = crate::reap_release::witness_refusal(
        &home,
        &hold_for("sessions x-done vs registry x-ready"),
    )
    .unwrap();
    assert!(
        active.contains("x-ready reads ready; a release never retires open work"),
        "{active}"
    );
}

/// d-81c6da7e AC3-HP: an idea-node planner hold ages on the same clock the
/// other reaper holds use, reads `escalated` past the threshold, and its
/// reason is the string a release answers.
#[test]
fn an_idea_planner_hold_ages_and_escalates_past_the_threshold() {
    let (dir, home) = staged_graph_home();
    stage_graph(
        dir.path(),
        json!([{
            "id": "x-idea",
            "status": "idea",
            "sessions": [{
                "phase": "blueprint",
                "harness": "codex",
                "session_id": "s-idea",
                "started_at": "2026-09-01T00:00:00Z",
            }],
        }]),
    );
    crate::state::update_registry(&home.registry_json(), |r| {
        let mut e = state::RegistryEntry::default();
        e.name = "bp-x-idea".into();
        e.short_id = "bp-x-idea".into();
        e.origin = Some("spawn".into());
        e.harness = Some("codex".into());
        e.harness_session_id = Some("s-idea".into());
        e.created_at = "2026-09-01T00:00:00Z".into();
        r.entries.push(e);
    })
    .unwrap();
    let mut summary = gc_sweep::run(
        &home,
        &EventEmitter::new(home.events_jsonl(), "daemon"),
        900,
        true,
        7,
        &crate::gc_sweep::read_graph_entries,
        &|_| None,
        &uniform_ages(5401),
        &|_| true,
        &|_| crate::daemon::CascadeOutcome::NotApplicable,
        &|_e| crate::daemon::CascadeOutcome::NotApplicable,
        &no_agents,
        &|_| (None, None),
        &|_| None,
    );
    summary.mark_escalated(std::time::Duration::from_secs(5400));
    let h = find_hold(&summary, "bp-x-idea");
    assert_eq!(h.reason, "planning assignment not finished by this session");
    assert_eq!(h.age_s, Some(5401));
    assert!(h.escalated, "{h:?}");
}

/// d-81c6da7e AC3-EDGE: the release lifts the marker question by ruling -
/// the row retires past the 1200 s planner grace, and the basis carries
/// the release prefix plus the `released` marker.
#[test]
fn gc_sweep_release_retires_a_released_planner() {
    let (dir, home) = staged_graph_home();
    stage_graph(
        dir.path(),
        json!([{
            "id": "x-idea",
            "status": "idea",
            "sessions": [{
                "phase": "blueprint",
                "harness": "codex",
                "session_id": "s-idea",
                "started_at": "2026-09-01T00:00:00Z",
            }],
        }]),
    );
    crate::state::update_registry(&home.registry_json(), |r| {
        let mut e = state::RegistryEntry::default();
        e.name = "bp-x-idea".into();
        e.short_id = "bp-x-idea".into();
        e.origin = Some("spawn".into());
        e.harness = Some("codex".into());
        e.harness_session_id = Some("s-idea".into());
        e.created_at = "2026-09-01T00:00:00Z".into();
        r.entries.push(e);
    })
    .unwrap();
    let release = gc_sweep::Release {
        handle: "bp-x-idea".to_string(),
        reason: "planning assignment not finished by this session".to_string(),
        detail: "x-idea idea: no close and no plan written by this session".to_string(),
    };
    let summary = gc_sweep::run_with_release(
        &home,
        &EventEmitter::new(home.events_jsonl(), "daemon"),
        900,
        false,
        7,
        &crate::gc_sweep::read_graph_entries,
        &|_| None,
        &uniform_ages(5401),
        &|_| true,
        &|_| crate::daemon::CascadeOutcome::NotApplicable,
        &|_e| crate::daemon::CascadeOutcome::NotApplicable,
        &no_agents,
        &|_| (None, None),
        &|_| None,
        Some(&release),
    );
    assert_eq!(summary.retired.len(), 1, "{:?}", summary.retired);
    assert_eq!(summary.retired[0].0, "bp-x-idea", "{:?}", summary.retired);
    let basis = &summary.retired[0].1;
    assert!(
        basis.starts_with("released planning assignment not finished by this session held "),
        "basis: {basis}"
    );
    assert!(
        basis.contains("planning finished on x-idea: released"),
        "basis: {basis}"
    );
}

/// AC4-HP: the unevaluated gate renders in both formats, named, and
/// never reads as a retirement - the text says the gate was not evaluated
/// and apply may still refuse, the JSON exposes the same id and reason, and
/// the header count stays sourced from `retired` alone.
#[test]
fn ac4_hp_dry_run_unverified_renders_in_text_and_json_without_counting_retired() {
    let s = GcSummary {
        dry_run_unverified: vec![(
            "w1".to_string(),
            "active-surface removal was not evaluated".to_string(),
        )],
        ..Default::default()
    };
    let text = crate::reap_render::render_reap(&s, false, true);
    assert!(
        text.contains(
            "  held w1 (dry-run did not evaluate: active-surface removal was not evaluated; apply may still refuse)\n"
        ),
        "{text}"
    );
    assert!(!text.contains("would retire w1"), "{text}");
    assert!(
        text.starts_with("would retire 0 row(s)"),
        "the header counts only summary.retired: {text}"
    );
    let out = crate::reap_render::render_reap(&s, true, true);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        v["dry_run_unverified"],
        serde_json::json!([
            {"id": "w1", "reason": "active-surface removal was not evaluated"}
        ])
    );
    assert_eq!(v["retired"], serde_json::json!([]));
    assert_eq!(v["dry_run"], serde_json::json!(true));
}

/// The release refusal names the dry-run-only bucket: a row held out of
/// `retired` by an unevaluated gate is a positive statement about the row's
/// state, never an unnamed keep (codex P2 on this branch). The seam is
/// [`row_bucket_in`] because production seams cannot stage the terminal
/// roster evidence an unverified row needs.
#[test]
fn the_release_refusal_names_the_unverified_gate() {
    let dry = GcSummary {
        dry_run_unverified: vec![(
            "w1".to_string(),
            "active-surface removal was not evaluated".to_string(),
        )],
        ..Default::default()
    };
    let home = tmp_home("release-unverified");
    assert_eq!(
        crate::reap_release::row_bucket_in(&home, &dry, "w1"),
        "dry-run-unverified (a retirement gate was not evaluated)"
    );
    // The arm must not swallow the fallthrough: a handle no bucket names
    // and no registry row answers still reads as unnamed.
    assert_eq!(
        crate::reap_release::row_bucket_in(&home, &dry, "ghost"),
        "no registry row names this handle"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

// ── the open-PR keep through the sweep ──────────────────────────────────────

/// A stopped claude spawn row whose session has a do row on an in_review
/// node carrying pr_number 1943 (merge_status unrecorded): the sweep keeps
/// the row under `open pr`, projects a hold with a clock, and exposes the
/// nudge-ladder row. A peer without a do row on the node changes nothing.
#[test]
fn ac1_hp_open_pr_keep_survives_a_terminal_state_through_the_sweep() {
    let home = tmp_home("gc-open-pr");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "quiet.jsonl", 2 * 3600);
    state::update_registry(&home.registry_json(), |r| {
        let mut row = claude_worker_row("pr-row", "cccc9999");
        row.origin = Some("spawn".into());
        r.entries.push(row);
    })
    .unwrap();
    let sid = "cccc9999-1111-2222-3333-444444444444";
    let graph = Some(GraphRead {
        index: HashMap::from([(
            sid.to_string(),
            vec![("x-node".to_string(), "in_review".to_string())],
        )]),
        work_index: HashMap::from([(
            sid.to_string(),
            vec![("x-node".to_string(), "in_review".to_string())],
        )]),
        statuses: HashMap::from([("x-node".to_string(), "in_review".to_string())]),
        pr_state: HashMap::from([("x-node".to_string(), (None, 0, 0))]),
        pr_number: HashMap::from([("x-node".to_string(), Some(1943))]),
        do_nodes: HashMap::from([(
            sid.to_string(),
            std::collections::HashSet::from(["x-node".to_string()]),
        )]),
        // The row is quiet past the grace, so the keep asks the PR: stage
        // the open answer the sweep must read (no test touches the network).
        pr_reads: HashMap::from([("/tmp".to_string(), 1943u64)])
            .into_iter()
            .map(|(cwd, pr)| ((cwd, pr), Some(true)))
            .collect(),
        ..Default::default()
    });
    // The roster reads stopped: the exact state that reaped seven rows on
    // 2026-09-13 before the keep existed.
    let agents = crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
        crate::claude_roster::ClaudeAgentRow::new("cccc9999", Some("stopped")),
    ]);
    let summary = evidence_sweep(
        &home,
        &emitter,
        900,
        false,
        graph,
        &|_| Some(vec![quiet.clone()]),
        agents,
        &|_| true,
    );
    // The row handle is the short id when one is recorded.
    assert_eq!(
        summary.kept_open_pr,
        vec![("cccc9999".to_string(), "x-node".to_string())],
        "kept buckets: {:?}",
        summary.kept_open_work
    );
    let hold = find_hold(&summary, "cccc9999");
    assert_eq!(hold.reason, "open pr");
    assert_eq!(hold.detail, "x-node #1943");
    assert_eq!(summary.open_pr_rows.len(), 1, "{:?}", summary.open_pr_rows);
    let ladder_row = &summary.open_pr_rows[0];
    assert_eq!(ladder_row.node, "x-node");
    assert_eq!(ladder_row.pr, Some(1943));
    assert_eq!(ladder_row.session_id, sid);
    // A stopped roster state reads as not live: the ladder's resume arm.
    assert!(!ladder_row.live);
    assert!(!ladder_row.busy, "a stopped row is never mid-turn");
    assert_eq!(summary.retired, vec![], "nothing retires");
    std::fs::remove_dir_all(home.root()).ok();
}

// ── the dead-worker keep (dead open work) through the sweep ────────────────

/// The staged shape every dead-open-work test varies: a spawn row whose
/// session has a do row on an in_progress node with no PR, a transcript
/// quiet past the grace, and a roster the test stages. The peer roster row
/// carries a pid so the listing provably carries pids: the missing pid on
/// the target row is then a death witness, not a missing column.
fn dead_work_sweep(
    tag: &str,
    roster: crate::claude_roster::ClaudeAgentsSnapshot,
    peer_row: bool,
) -> crate::gc_sweep::GcSummary {
    let home = tmp_home(tag);
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "quiet.jsonl", 2 * 3600);
    state::update_registry(&home.registry_json(), |r| {
        let mut row = claude_worker_row("dead-row", "cccc9999");
        row.origin = Some("spawn".into());
        r.entries.push(row);
        if peer_row {
            let mut peer = claude_worker_row("fresh-row", "dddd0000");
            peer.origin = Some("spawn".into());
            peer.created_at = "2026-09-26T00:00:00Z".into();
            r.entries.push(peer);
        }
    })
    .unwrap();
    let sid = "cccc9999-1111-2222-3333-444444444444";
    let mut index = HashMap::from([(
        sid.to_string(),
        vec![("x-node".to_string(), "in_progress".to_string())],
    )]);
    let mut do_nodes = HashMap::from([(
        sid.to_string(),
        std::collections::HashSet::from(["x-node".to_string()]),
    )]);
    if peer_row {
        let peer_sid = "dddd0000-1111-2222-3333-444444444444";
        index.insert(
            peer_sid.to_string(),
            vec![("x-node".to_string(), "in_progress".to_string())],
        );
        do_nodes.insert(
            peer_sid.to_string(),
            std::collections::HashSet::from(["x-node".to_string()]),
        );
    }
    let graph = Some(GraphRead {
        index: index.clone(),
        work_index: index,
        statuses: HashMap::from([("x-node".to_string(), "in_progress".to_string())]),
        pr_state: HashMap::from([("x-node".to_string(), (None, 0, 0))]),
        pr_number: HashMap::from([("x-node".to_string(), None)]),
        do_nodes,
        pr_reads: HashMap::new(),
        ..Default::default()
    });
    evidence_sweep(
        &home,
        &emitter,
        900,
        false,
        graph,
        &|_| Some(vec![quiet.clone()]),
        roster,
        &|_| true,
    )
}

/// AC2-HP: the worker's process is gone (blocked row, no pid, in a listing
/// that carries pids), the node is in_progress with no PR. The row is kept
/// under `dead open work`, projected into `dead_work_rows` with pr: null and
/// live: false, and nothing retires: law d-71d03643, resumed never stranded.
#[test]
fn ac2_hp_dead_worker_on_in_progress_node_is_kept_and_laddered() {
    let agents = crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
        crate::claude_roster::ClaudeAgentRow::new("cccc9999", Some("blocked")),
        crate::claude_roster::ClaudeAgentRow::new("peer0001", Some("working")).with_pid(Some(5001)),
    ]);
    let summary = dead_work_sweep("gc-dead-work", agents, false);
    assert!(
        summary
            .kept_open_work
            .iter()
            .any(|(id, node, status, _)| id == "cccc9999"
                && node == "x-node"
                && status == "in_progress"),
        "kept buckets: {:?}",
        summary.kept_open_work
    );
    let hold = find_hold(&summary, "cccc9999");
    assert_eq!(hold.reason, "dead open work");
    assert_eq!(summary.retired, vec![], "nothing retires");
    assert_eq!(
        summary.dead_work_rows.len(),
        1,
        "{:?}",
        summary.dead_work_rows
    );
    let dead = &summary.dead_work_rows[0];
    assert_eq!(dead.id, "cccc9999");
    assert_eq!(dead.node, "x-node");
    assert_eq!(dead.pr, None, "no PR exists yet");
    assert_eq!(dead.session_id, "cccc9999-1111-2222-3333-444444444444");
    assert!(!dead.live, "the ladder's Resume rung reads this");
    assert!(summary.open_pr_rows.is_empty(), "not an open-PR row");
}

/// AC2-EDGE, failed: the death of the worker does not finish the node's
/// work. A `failed` roster state keeps under dead open work and is
/// laddered, instead of releasing through the terminal arm.
#[test]
fn ac2_edge_a_failed_state_still_keeps_and_ladders_the_dead_worker() {
    let agents = crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
        crate::claude_roster::ClaudeAgentRow::new("cccc9999", Some("failed")),
        crate::claude_roster::ClaudeAgentRow::new("peer0001", Some("working")).with_pid(Some(5001)),
    ]);
    let summary = dead_work_sweep("gc-dead-work-failed", agents, false);
    assert_eq!(find_hold(&summary, "cccc9999").reason, "dead open work");
    assert_eq!(summary.dead_work_rows.len(), 1);
    assert_eq!(summary.retired, vec![]);
}

/// AC2-EDGE, done and stopped: those states take today's terminal paths.
/// `done` retires through the session-shaped release; a raw `stopped` (no
/// fno stop record) is terminal and retires too - both keep
/// `dead_work_rows` empty, because neither shape is a resume candidate.
#[test]
fn ac2_edge_done_and_stopped_states_take_todays_path() {
    let done = crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
        crate::claude_roster::ClaudeAgentRow::new("cccc9999", Some("done")),
        crate::claude_roster::ClaudeAgentRow::new("peer0001", Some("working")).with_pid(Some(5001)),
    ]);
    let summary = dead_work_sweep("gc-dead-work-done", done, false);
    assert_eq!(summary.dead_work_rows, vec![]);
    assert_eq!(summary.retired.len(), 1, "{:?}", summary.retired);

    let stopped = crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
        crate::claude_roster::ClaudeAgentRow::new("cccc9999", Some("stopped")),
        crate::claude_roster::ClaudeAgentRow::new("peer0001", Some("working")).with_pid(Some(5001)),
    ]);
    let summary = dead_work_sweep("gc-dead-work-stopped", stopped, false);
    assert_eq!(summary.dead_work_rows, vec![]);
    assert_eq!(summary.retired.len(), 1, "{:?}", summary.retired);
}

/// AC2-ERR: a newer live registry row on the same node releases the old
/// row exactly as before - the peer is the successor, and the stale row is
/// not laddered over it.
#[test]
fn ac2_err_a_live_newer_peer_releases_the_dead_row() {
    let agents = crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
        crate::claude_roster::ClaudeAgentRow::new("cccc9999", Some("blocked")),
        crate::claude_roster::ClaudeAgentRow::new("dddd0000", Some("working")).with_pid(Some(5002)),
    ]);
    let summary = dead_work_sweep("gc-dead-work-peer", agents, true);
    assert_eq!(summary.dead_work_rows, vec![]);
    assert_eq!(summary.retired.len(), 1, "{:?}", summary.retired);
}

/// A blocked row WITH a pid is a live session: it keeps under the ordinary
/// open-work reasons and is never laddered as a dead worker.
#[test]
fn a_blocked_row_with_a_pid_stays_open_work_and_is_not_laddered() {
    let agents = crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
        crate::claude_roster::ClaudeAgentRow::new("cccc9999", Some("blocked")).with_pid(Some(5003)),
        crate::claude_roster::ClaudeAgentRow::new("peer0001", Some("working")).with_pid(Some(5001)),
    ]);
    let summary = dead_work_sweep("gc-dead-work-alive", agents, false);
    assert_eq!(summary.dead_work_rows, vec![]);
    assert_eq!(summary.retired, vec![]);
    assert!(
        !summary.kept_open_work.is_empty(),
        "the ordinary open-work keep holds: {:?}",
        summary.kept_open_work
    );
}

// ── the open-PR keep asks the PR ─────────────────────────────────────────

/// A working roster row reads live AND busy: the row is mid-turn, so the
/// nudge ladder keeps it on Mail and never sends it to the resume that
/// would refuse it.
#[test]
fn ac3_working_roster_row_reads_live_and_busy() {
    let home = tmp_home("gc-open-pr-working");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "quiet.jsonl", 2 * 3600);
    state::update_registry(&home.registry_json(), |r| {
        let mut row = claude_worker_row("pr-row-working", "cccc9999");
        row.origin = Some("spawn".into());
        r.entries.push(row);
    })
    .unwrap();
    let sid = "cccc9999-1111-2222-3333-444444444444";
    let graph = Some(GraphRead {
        index: HashMap::from([(
            sid.to_string(),
            vec![("x-node".to_string(), "in_review".to_string())],
        )]),
        work_index: HashMap::from([(
            sid.to_string(),
            vec![("x-node".to_string(), "in_review".to_string())],
        )]),
        statuses: HashMap::from([("x-node".to_string(), "in_review".to_string())]),
        pr_state: HashMap::from([("x-node".to_string(), (None, 0, 0))]),
        pr_number: HashMap::from([("x-node".to_string(), Some(1943))]),
        do_nodes: HashMap::from([(
            sid.to_string(),
            std::collections::HashSet::from(["x-node".to_string()]),
        )]),
        pr_reads: HashMap::from([("/tmp".to_string(), 1943u64)])
            .into_iter()
            .map(|(cwd, pr)| ((cwd, pr), Some(true)))
            .collect(),
        ..Default::default()
    });
    // The roster reads working: mid-turn, live and busy at once.
    let agents = crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
        crate::claude_roster::ClaudeAgentRow::new("cccc9999", Some("working")),
    ]);
    let summary = evidence_sweep(
        &home,
        &emitter,
        900,
        false,
        graph,
        &|_| Some(vec![quiet.clone()]),
        agents,
        &|_| true,
    );
    assert_eq!(summary.open_pr_rows.len(), 1, "{:?}", summary.open_pr_rows);
    let ladder_row = &summary.open_pr_rows[0];
    assert!(ladder_row.live, "working is a non-terminal roster state");
    assert!(ladder_row.busy, "working is the mid-turn state");
    std::fs::remove_dir_all(home.root()).ok();
}

/// The candidate's graph: one in_review node this session drives, PR 4242.
fn open_candidate_graph() -> GraphRead {
    let mut g = graph_read(&[("sess-pr", "N1", "in_review")], &[]).unwrap();
    g.pr_number.insert("N1".into(), Some(4242));
    g.do_nodes.insert(
        "sess-pr".into(),
        std::collections::HashSet::from(["N1".into()]),
    );
    g
}

/// The read gate: a candidate inside the grace window is kept without the
/// read firing at all.
#[test]
fn open_pr_verdict_asks_only_a_quiet_candidate() {
    let g = open_candidate_graph();
    let mut calls = 0u32;
    {
        let mut reader = |_pr: u64, _cwd: &str| -> Option<bool> {
            calls += 1;
            Some(true)
        };
        let fresh =
            gc_sweep::open_pr_verdict(&g, "sess-pr", "N1", "/tmp", false, Some(&mut reader));
        assert!(
            matches!(fresh, gc_sweep::OpenPrVerdict::Holds { .. }),
            "{fresh:?}"
        );
    }
    assert_eq!(calls, 0, "a fresh candidate pays no read");
    let mut calls2 = 0u32;
    let mut reader2 = |_pr: u64, _cwd: &str| -> Option<bool> {
        calls2 += 1;
        Some(true)
    };
    let quiet = gc_sweep::open_pr_verdict(&g, "sess-pr", "N1", "/tmp", true, Some(&mut reader2));
    assert!(matches!(quiet, gc_sweep::OpenPrVerdict::Holds { .. }));
    assert_eq!(calls2, 1);
}

/// The three answers once the read fires: open holds, merged or closed
/// settles, and both a failed read and no reader hold under unread - never
/// a retirement on an unread answer. The attribution gate is unchanged: a
/// session that never drove the node is no candidate and pays no read.
#[test]
fn open_pr_verdict_settles_on_a_closed_answer() {
    let g = open_candidate_graph();
    let mut closed = |_pr: u64, _cwd: &str| -> Option<bool> { Some(false) };
    assert!(matches!(
        gc_sweep::open_pr_verdict(&g, "sess-pr", "N1", "/tmp", true, Some(&mut closed)),
        gc_sweep::OpenPrVerdict::Settled { .. }
    ));
    let mut failed = |_pr: u64, _cwd: &str| -> Option<bool> { None };
    assert!(matches!(
        gc_sweep::open_pr_verdict(&g, "sess-pr", "N1", "/tmp", true, Some(&mut failed)),
        gc_sweep::OpenPrVerdict::Unread { .. }
    ));
    assert!(matches!(
        gc_sweep::open_pr_verdict(&g, "sess-pr", "N1", "/tmp", true, None),
        gc_sweep::OpenPrVerdict::Unread { .. }
    ));
    let mut never = |_pr: u64, _cwd: &str| -> Option<bool> { panic!("no candidate pays a read") };
    assert!(matches!(
        gc_sweep::open_pr_verdict(&g, "sess-other", "N1", "/tmp", true, Some(&mut never)),
        gc_sweep::OpenPrVerdict::None
    ));
}

/// The sweep asks: a quiet candidate whose PR reads closed retires, an open
/// answer keeps the row with the hold named, an unread answer holds under
/// `pr state contradicts`, and a candidate inside the grace window is kept
/// without its staged answer ever being consulted.
#[test]
fn the_sweep_asks_the_pr_once_the_row_is_quiet() {
    let home = tmp_home("gc-openpr-ask");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "quiet.jsonl", 7200);
    let fresh = quiet_transcript(transcripts.path(), "fresh.jsonl", 30);
    state::update_registry(&home.registry_json(), |r| {
        for (name, short) in [
            ("closed-row", "closedrow"),
            ("open-row", "openrow"),
            ("failed-row", "failedrow"),
            ("fresh-row", "freshrow"),
        ] {
            let mut row = claude_worker_row(name, short);
            row.origin = Some("spawn".into());
            r.entries.push(row);
        }
    })
    .unwrap();

    let mut graph = graph_read(
        &[
            ("closedrow-1111-2222-3333-444444444444", "NC", "in_review"),
            ("openrow-1111-2222-3333-444444444444", "NO", "in_review"),
            ("failedrow-1111-2222-3333-444444444444", "NF", "in_review"),
            ("freshrow-1111-2222-3333-444444444444", "NR", "in_review"),
        ],
        &[],
    )
    .unwrap();
    for (node, pr) in [("NC", 101u64), ("NO", 102), ("NF", 103), ("NR", 104)] {
        graph.pr_number.insert(node.into(), Some(pr));
        graph.pr_state.insert(node.into(), (None, 0, 0));
    }
    for (sid, node) in [
        ("closedrow-1111-2222-3333-444444444444", "NC"),
        ("openrow-1111-2222-3333-444444444444", "NO"),
        ("failedrow-1111-2222-3333-444444444444", "NF"),
        ("freshrow-1111-2222-3333-444444444444", "NR"),
    ] {
        graph
            .do_nodes
            .insert(sid.into(), std::collections::HashSet::from([node.into()]));
    }
    // Staged answers keyed by (cwd, pr); the fixture rows cwd to /tmp.
    for (pr, answer) in [
        (101u64, Some(false)),
        (102, Some(true)),
        (103, None),
        (104, Some(false)),
    ] {
        graph.pr_reads.insert(("/tmp".into(), pr), answer);
    }

    let summary = evidence_sweep(
        &home,
        &emitter,
        900,
        false,
        Some(graph),
        &|e| match e.harness_session_id.as_deref() {
            Some("freshrow-1111-2222-3333-444444444444") => Some(vec![fresh.clone()]),
            _ => Some(vec![quiet.clone()]),
        },
        no_agents(),
        &|_| true,
    );

    let retired: Vec<&str> = summary.retired.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(retired, vec!["closedrow"], "retired: {:?}", summary.retired);
    let held: Vec<&str> = summary
        .kept_open_pr
        .iter()
        .map(|(id, _)| id.as_str())
        .collect();
    assert_eq!(held, vec!["openrow", "freshrow"], "{held:?}");
    let h = find_hold(&summary, "openrow");
    assert_eq!(h.reason, "open pr");
    assert_eq!(h.detail, "NO #102");
    assert_eq!(
        summary.kept_pr_contradicts,
        vec![(
            "failedrow".to_string(),
            "NF".to_string(),
            "pr 103 state unread".to_string()
        )],
        "{:?}",
        summary.kept_pr_contradicts
    );
    std::fs::remove_dir_all(home.root()).ok();
}

// ── an adopted row keeps only while there is a session to own it ────────

/// The high path: an adopted claude row absent from a KNOWN roster
/// snapshot, named on a done-and-merged node, quiet past the grace, retires
/// through the normal pipeline - `kept_not_spawn` does not name it.
#[test]
fn an_adopted_corpse_absent_from_a_known_roster_retires() {
    let home = tmp_home("gc-corpse-hp");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "quiet.jsonl", 7200);
    state::update_registry(&home.registry_json(), |r| {
        let mut row = claude_worker_row("ac-row", "acrow000");
        row.origin = Some("adopted".into());
        r.entries.push(row);
    })
    .unwrap();

    let mut graph = graph_read(
        &[("acrow000-1111-2222-3333-444444444444", "NM", "done")],
        &[],
    )
    .unwrap();
    graph
        .pr_state
        .insert("NM".into(), (Some("merged".into()), 0, 0));
    // A KNOWN snapshot listing nothing: absence from it is positive death
    // evidence, the same predicate the rm live gate applies.
    let agents = crate::claude_roster::ClaudeAgentsSnapshot::known(vec![]);

    let summary = evidence_sweep(
        &home,
        &emitter,
        900,
        false,
        Some(graph),
        &|_| Some(vec![quiet.clone()]),
        agents,
        &|_| true,
    );

    assert_eq!(
        summary.retired.len(),
        1,
        "retired: {:?}, kept_not_spawn: {:?}",
        summary.retired,
        summary.kept_not_spawn
    );
    assert_eq!(summary.retired[0].0, "acrow000");
    assert!(
        summary.kept_not_spawn.is_empty(),
        "{:?}",
        summary.kept_not_spawn
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// The roster sweep's second tick (AC2-HP): once the roster sweep has
/// removed the session, the adopted row is absent from a known roster that
/// still lists OTHER sessions - positive absence, not an empty read - and
/// the registry sweep retires it through `origin_corpse`. `kept_not_spawn`
/// does not name it.
#[test]
fn an_adopted_row_retires_once_the_roster_sweep_removed_its_session() {
    let home = tmp_home("gc-corpse-second-tick");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "quiet.jsonl", 7200);
    state::update_registry(&home.registry_json(), |r| {
        let mut row = claude_worker_row("x42-row", "x42row00");
        row.origin = Some("adopted".into());
        r.entries.push(row);
    })
    .unwrap();

    let mut graph = graph_read(
        &[("x42row00-1111-2222-3333-444444444444", "NM", "done")],
        &[],
    )
    .unwrap();
    graph
        .pr_state
        .insert("NM".into(), (Some("merged".into()), 0, 0));
    // The roster the sweep would have left behind: one unrelated live
    // session, the adopted row's session gone - the roster sweep's removal.
    let agents = crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
        crate::claude_roster::ClaudeAgentRow::new("otherw", Some("idle")),
    ]);

    let summary = evidence_sweep(
        &home,
        &emitter,
        900,
        false,
        Some(graph),
        &|_| Some(vec![quiet.clone()]),
        agents,
        &|_| true,
    );

    assert_eq!(
        summary.retired.len(),
        1,
        "retired: {:?}, kept_not_spawn: {:?}",
        summary.retired,
        summary.kept_not_spawn
    );
    assert_eq!(summary.retired[0].0, "x42row00");
    assert!(
        summary.kept_not_spawn.is_empty(),
        "{:?}",
        summary.kept_not_spawn
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// The failed read: the same corpse row with a snapshot that reads unknown
/// keeps under `not a spawn row` - an unread instrument is never absence.
#[test]
fn an_adopted_row_with_an_unknown_snapshot_keeps() {
    let home = tmp_home("gc-corpse-unknown");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "quiet.jsonl", 7200);
    state::update_registry(&home.registry_json(), |r| {
        let mut row = claude_worker_row("au-row", "aurow000");
        row.origin = Some("adopted".into());
        r.entries.push(row);
    })
    .unwrap();

    let graph = graph_read(
        &[("aurow000-1111-2222-3333-444444444444", "NM", "done")],
        &[],
    );
    let summary = evidence_sweep(
        &home,
        &emitter,
        900,
        true,
        graph,
        &|_| Some(vec![quiet.clone()]),
        no_agents(),
        &|_| true,
    );

    assert_eq!(summary.retired, vec![], "{:?}", summary.retired);
    assert_eq!(
        summary.kept_not_spawn,
        vec![("aurow000".to_string(), "adopted".to_string())],
        "{:?}",
        summary.kept_not_spawn
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// Presence in a known snapshot keeps, whatever the node reads; and a codex
/// row (no claude roster, no pid) with no death marker keeps too - the
/// corpse predicate has exactly two legs and nothing else satisfies it.
#[test]
fn an_adopted_row_with_a_live_roster_row_or_no_probe_keeps() {
    let home = tmp_home("gc-corpse-live");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "quiet.jsonl", 7200);
    state::update_registry(&home.registry_json(), |r| {
        let mut present = claude_worker_row("pr-row", "prrow000");
        present.origin = Some("adopted".into());
        r.entries.push(present);
        let mut codex = ask_row("cx-row", None);
        codex.short_id = "cxrow000".into();
        codex.harness = Some("codex".into());
        codex.harness_session_id = Some("sess-cx".into());
        codex.origin = Some("adopted".into());
        r.entries.push(codex);
    })
    .unwrap();

    let graph = graph_read(
        &[
            ("prrow000-1111-2222-3333-444444444444", "NM", "done"),
            ("sess-cx", "NM", "done"),
        ],
        &[],
    );
    let agents = crate::claude_roster::ClaudeAgentsSnapshot::known(vec![
        crate::claude_roster::ClaudeAgentRow::new("prrow000", Some("idle")),
    ]);
    let summary = evidence_sweep(
        &home,
        &emitter,
        900,
        true,
        graph,
        &|_| Some(vec![quiet.clone()]),
        agents,
        &|_| true,
    );

    assert_eq!(summary.retired, vec![], "{:?}", summary.retired);
    let kept: Vec<&str> = summary
        .kept_not_spawn
        .iter()
        .map(|(id, _)| id.as_str())
        .collect();
    assert_eq!(kept.len(), 2, "{:?}", summary.kept_not_spawn);
    assert!(kept.contains(&"prrow000"), "{:?}", summary.kept_not_spawn);
    assert!(kept.contains(&"cxrow000"), "{:?}", summary.kept_not_spawn);
    std::fs::remove_dir_all(home.root()).ok();
}

// ── a row that resolved no node releases on its own done report ─────────

/// The high path: a quiet row whose inside-leg report reads done and that
/// fno never stopped retires with no node resolved. A still-working twin
/// keeps - and every kept no-provenance row now carries a hold with a
/// clock, so the release verb can reach it.
#[test]
fn a_no_provenance_row_releases_on_its_done_report_and_keeps_carry_a_clock() {
    let home = tmp_home("gc-np-done");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let transcripts = tempfile::tempdir().unwrap();
    let quiet = quiet_transcript(transcripts.path(), "quiet.jsonl", 7200);
    let leg_done = state::InsideLegReport {
        state: crate::state::InsideLegState::Done,
        seq: 3,
        reason: None,
        received_at: "2026-09-15T19:52:20Z".into(),
        ttl_ms: None,
    };
    let leg_working = state::InsideLegReport {
        state: crate::state::InsideLegState::Working,
        seq: 4,
        reason: None,
        received_at: "2026-09-15T19:52:20Z".into(),
        ttl_ms: None,
    };
    state::update_registry(&home.registry_json(), |r| {
        // The name carries no node token, the registry node field is empty,
        // and the graph names the session nowhere: no source resolves.
        let mut done = ask_row("standby-worker", None);
        done.short_id = "npdone01".into();
        done.harness_session_id = Some("sess-np-done".into());
        done.origin = Some("spawn".into());
        done.inside_leg = Some(leg_done);
        r.entries.push(done);
        let mut working = ask_row("midturn-worker", None);
        working.short_id = "npwork01".into();
        working.harness_session_id = Some("sess-np-work".into());
        working.origin = Some("spawn".into());
        working.inside_leg = Some(leg_working);
        r.entries.push(working);
    })
    .unwrap();

    let summary = evidence_sweep(
        &home,
        &emitter,
        900,
        false,
        graph_read(&[], &[]),
        &|_| Some(vec![quiet.clone()]),
        no_agents(),
        &|_| true,
    );

    assert_eq!(
        summary.retired.len(),
        1,
        "retired: {:?}, kept: {:?}",
        summary.retired,
        summary.kept_no_provenance
    );
    assert_eq!(summary.retired[0].0, "npdone01");
    assert_eq!(summary.kept_no_provenance, vec!["npwork01"]);
    let h = find_hold(&summary, "npwork01");
    assert!(h.reason.contains("no provenance"), "{h:?}");
    assert!(h.age_s.is_some_and(|a| a >= 7200), "{h:?}");
    assert_eq!(h.age_basis, "transcript quiet");
    std::fs::remove_dir_all(home.root()).ok();
}

// ── the receipt names a registry it cannot write ────────────────────────

/// A staged registry one version ahead of this binary: the summary carries
/// the skew and the rendered receipt names both versions and the refused
/// write, in text and JSON.
#[test]
fn a_forward_registry_names_itself_in_the_receipt() {
    let home = tmp_home("gc-skew-hp");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let forward = crate::state::REGISTRY_SCHEMA_VERSION + 1;
    let understood = crate::state::REGISTRY_SCHEMA_VERSION;
    let registry = serde_json::json!({
        "schema_version": forward,
        "agents": [{
            "name": "skew-row",
            "cwd": "/tmp",
            "status": "exited",
            "created_at": "2026-09-06T00:00:00Z",
            "harness": "claude",
            "harness_session_id": "sess-skew",
            "short_id": "skewrow1",
            "origin": "spawn",
        }],
    });
    std::fs::create_dir_all(home.root()).unwrap();
    std::fs::write(
        home.registry_json(),
        serde_json::to_string(&registry).unwrap(),
    )
    .unwrap();

    let graph = graph_read(&[("sess-skew", "N1", "done")], &[]);
    let summary = gc_sweep::run(
        &home,
        &emitter,
        900,
        true,
        7,
        &move |_| graph.clone(),
        &|_| None,
        &uniform_ages(7200),
        &|_| true,
        &|_| crate::daemon::CascadeOutcome::NotApplicable,
        &|_e| crate::daemon::CascadeOutcome::NotApplicable,
        &no_agents,
        &|_| (None, None),
        &|_| None,
    );

    assert_eq!(summary.schema_skew, Some((forward, understood)));
    let text = crate::reap_render::render_reap(&summary, false, true);
    assert!(
        text.contains(&format!(
            "schema v{forward} is ahead of the v{understood} this fno understands"
        )),
        "text: {text}"
    );
    assert!(
        text.contains("no retirement can be written"),
        "text must name the refused write: {text}"
    );
    let json_text = crate::reap_render::render_reap(&summary, true, true);
    assert!(
        json_text.contains(&format!(
            "\"schema_skew\":{{\"on_disk\":{forward},\"understood\":{understood}}}"
        )),
        "json: {json_text}"
    );
    std::fs::remove_dir_all(home.root()).ok();
}

/// A registry at this binary's own version carries no skew, and the
/// rendered text gains no line.
#[test]
fn a_registry_at_the_binary_version_carries_no_skew() {
    let home = tmp_home("gc-skew-none");
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(ask_row("plain-row", None));
    })
    .unwrap();

    let graph = graph_read(&[("plain-row-sess", "N1", "done")], &[]);
    let summary = gc_sweep::run(
        &home,
        &emitter,
        900,
        true,
        7,
        &move |_| graph.clone(),
        &|_| None,
        &uniform_ages(7200),
        &|_| true,
        &|_| crate::daemon::CascadeOutcome::NotApplicable,
        &|_e| crate::daemon::CascadeOutcome::NotApplicable,
        &no_agents,
        &|_| (None, None),
        &|_| None,
    );

    assert_eq!(summary.schema_skew, None);
    let text = crate::reap_render::render_reap(&summary, false, true);
    assert!(!text.contains("is ahead of the v"), "text: {text}");
    let json_text = crate::reap_render::render_reap(&summary, true, true);
    assert!(
        json_text.contains("\"schema_skew\":null"),
        "json: {json_text}"
    );
    std::fs::remove_dir_all(home.root()).ok();
}
