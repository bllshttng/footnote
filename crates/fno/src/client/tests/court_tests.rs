//! (x-3cb3) The court panel's render: the three rules that keep it honest.
//!
//! A panel that can print a fabricated zero, hide the attribution gap, or
//! show a stale reading as a fresh one is worse than no panel, because an
//! operator acts on it. Each test below pins one of those.

use super::*;
use crate::court_overlay::{Census, Court};

fn measured_court() -> Court {
    // The payload measured on one machine, 2026-09-04.
    serde_json::from_str(
        r#"{
          "lane_count": 0,
          "per_lane_cpu_cores": 0.032,
          "per_lane_mem_gb": 0.396,
          "cost_source": "measured from the live roster's attributed footprint (50 live row(s))",
          "refused_reason": "",
          "census": {"kings": 5, "king_conflicts": 0, "workers": 45, "tests": 2,
                     "roster_rows": 50, "read_ms": 597, "attribution_gap": null},
          "arms": [
            {"name": "spawn load", "state": "measured",
             "value": {"load_1m": 107.3, "ceiling": 96.0, "status": "exceeded"}, "reason": ""},
            {"name": "whole-machine cpu", "state": "measured",
             "value": {"busy_fraction": 0.797, "capacity_cores": 12}, "reason": ""},
            {"name": "memory", "state": "measured",
             "value": {"free_fraction": 0.63, "available_gb": 64.9}, "reason": ""}
          ]
        }"#,
    )
    .expect("the measured payload parses")
}

fn open_with(court: Option<Court>) -> View {
    let mut view = two_pane_view();
    view.court = Some(Instant::now());
    view.court_fold_at = court.as_ref().map(|_| Instant::now());
    view.court_fold = court;
    view
}

#[test]
fn ac4_hp_renders_load_census_lanes_and_an_age() {
    let view = open_with(Some(measured_court()));
    let text = view.court_overlay_lines().join("\n");

    assert!(text.contains("load    107.3 / 96.0"), "{text}");
    assert!(text.contains("exceeded"), "{text}");
    assert!(text.contains("cpu     80% busy of 12 cores"), "{text}");
    assert!(text.contains("memory  64.9 GB available"), "{text}");
    assert!(
        text.contains("court   5 kings · 45 workers · 2 tests (50 rows)"),
        "{text}"
    );
    assert!(text.contains("lanes   0 more fit"), "{text}");
    assert!(text.contains("0.032 cores"), "{text}");
    // The age line is always present and always carries a duration.
    assert!(text.contains("read    0.0s ago"), "{text}");
}

#[test]
fn ac4_edge_a_failed_fold_opens_with_a_named_degrade_never_a_blank_panel() {
    let mut view = open_with(None);
    view.court_degraded = true;
    let lines = view.court_overlay_lines();

    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("fold failed"), "{lines:?}");
    assert!(lines[0].contains("10s"), "{lines:?}");
}

#[test]
fn a_pending_fold_says_so_rather_than_showing_zeroes() {
    let view = open_with(None);
    let lines = view.court_overlay_lines();

    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("reading the machine"), "{lines:?}");
    assert!(
        !lines[0].contains('0'),
        "a pending fold must show no counts"
    );
}

#[test]
fn ac4_edge2_a_refusal_carries_the_advisors_words_and_no_lane_number() {
    let mut court = measured_court();
    court.lane_count = None;
    court.refused_reason =
        "the machine arms cannot answer the lane question: memory dark (macmon not on PATH)"
            .to_string();
    let view = open_with(Some(court));
    let text = view.court_overlay_lines().join("\n");

    assert!(text.contains("lanes   REFUSED"), "{text}");
    assert!(text.contains("memory dark (macmon not on PATH)"), "{text}");
    assert!(!text.contains("more fit"), "{text}");
}

#[test]
fn a_refusal_with_no_reason_still_says_no_number_rather_than_printing_one() {
    let mut court = measured_court();
    court.lane_count = None;
    court.refused_reason = String::new();
    let view = open_with(Some(court));
    let text = view.court_overlay_lines().join("\n");

    assert!(text.contains("REFUSED"), "{text}");
    assert!(text.contains("without naming a reason"), "{text}");
}

#[test]
fn the_attribution_gap_gets_its_own_line_and_never_a_count() {
    // x-e040 made the gap honest. A panel that folded it into a count would
    // re-open the hole: the gap is a process-to-row failure and cannot
    // change how many rows exist.
    let mut court = measured_court();
    court.census.attribution_gap = Some(
        "11 pidless row(s) with no identity route (codex); 8 bg-socket row(s) missing".to_string(),
    );
    let view = open_with(Some(court));
    let text = view.court_overlay_lines().join("\n");

    assert!(text.contains("gap     11 pidless row(s)"), "{text}");
    assert!(text.contains("undercount, not headroom"), "{text}");
    // The counts are untouched by the gap.
    assert!(text.contains("5 kings · 45 workers"), "{text}");
}

#[test]
fn an_unreadable_registry_renders_unknown_never_a_fabricated_zero() {
    let mut court = measured_court();
    court.census = Census {
        tests: Some(3),
        ..Census::default()
    };
    let view = open_with(Some(court));
    let text = view.court_overlay_lines().join("\n");

    assert!(
        text.contains("court   unknown kings · unknown workers · 3 tests (unknown rows)"),
        "{text}"
    );
}

#[test]
fn a_stale_reading_says_stale_and_still_shows_its_numbers() {
    // The failure this closes, named in the node: an operator watching
    // "unknown" every second learns nothing and reaches for --force.
    let mut view = open_with(Some(measured_court()));
    view.court_fold_at = Some(Instant::now() - COURT_CACHE_TTL - Duration::from_secs(2));
    let text = view.court_overlay_lines().join("\n");

    assert!(text.contains("stale, refreshing"), "{text}");
    assert!(text.contains("court   5 kings"), "{text}");
    assert!(!text.contains("unknown"), "{text}");
}

#[test]
fn a_failed_refresh_over_a_good_reading_keeps_the_numbers_and_says_so() {
    let mut view = open_with(Some(measured_court()));
    view.court_degraded = true;
    let text = view.court_overlay_lines().join("\n");

    assert!(text.contains("last refresh failed"), "{text}");
    assert!(text.contains("court   5 kings"), "{text}");
}

#[test]
fn a_dark_load_arm_names_its_reason_rather_than_printing_a_number() {
    let court: Court = serde_json::from_str(
        r#"{"lane_count": null, "refused_reason": "arms dark",
            "census": {}, "arms": [{"name": "spawn load", "state": "dark",
            "value": null, "reason": "load average unreadable"}]}"#,
    )
    .expect("parses");
    let view = open_with(Some(court));
    let text = view.court_overlay_lines().join("\n");

    assert!(
        text.contains("load    unknown - load average unreadable"),
        "{text}"
    );
}

#[test]
fn a_king_conflict_is_warned_because_a_bare_count_hides_it() {
    let mut court = measured_court();
    court.census.king_conflicts = Some(2);
    let view = open_with(Some(court));
    let text = view.court_overlay_lines().join("\n");

    assert!(
        text.contains("warn    2 scope(s) held by more than one crown"),
        "{text}"
    );
}

#[test]
fn the_court_binding_is_c_in_global_and_dispatches_open_court() {
    let binding = crate::keys::key_bindings()
        .into_iter()
        .find(|b| b.action == "court")
        .expect("the court action is bound");

    assert_eq!(binding.key, b'C');
    assert_eq!(binding.event, crate::keys::Event::OpenCourt);
    assert!(matches!(binding.section, crate::keys::KeySection::Global));
}
