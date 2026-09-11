//! The x-e3cc test families: the hold clock on every reaper hold, the
//! release verb's rulings and refusals. Same-module helpers resolve through
//! the parent glob; gc_receipts' fixtures are `pub(super)`.

use super::gc_receipts::*;
use super::*;
use crate::gc_sweep::{self, GcSummary};

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
        assert_hold_line(&summary, true, id, "[held ");
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
        &no_agents,
        &|_| (None, None),
        &|_| None,
    );
    summary.mark_escalated(std::time::Duration::from_secs(5400));

    let h = find_hold(&summary, "unmrow");
    assert!(h.age_s.is_none(), "{h:?}");
    assert_eq!(h.age_basis, "unmeasured");
    assert!(!h.escalated, "{h:?}");
    assert_hold_line(&summary, true, "unmrow", "[held unmeasured]");
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
            .any(|id| id == "midrow"),
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
