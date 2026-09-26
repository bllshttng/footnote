//! The read model's tests, mounted by backlog_model.rs.

use super::*;
use crate::proto::AgentBadge;
use serde_json::json;

fn row(harness_session: Option<&str>) -> AgentRow {
    let mut v = json!({
        "squad": null, "name": "w1", "pane_id": null,
        "badge": null, "reason": null, "exited": false
    });
    if let Some(s) = harness_session {
        v["harness_session_id"] = json!(s);
    }
    serde_json::from_value(v).expect("a minimal roster row decodes")
}

#[test]
fn lanes_query_refuses_unknown_and_parses_known_values() {
    let p = vec![("lanes".to_string(), "sideways".to_string())];
    assert!(Query::from_pairs(&p).is_err(), "unknown lanes refused");
    let p = vec![("lanes".to_string(), "epic".to_string())];
    assert!(matches!(
        Query::from_pairs(&p).unwrap().lanes,
        LanesBy::Epic
    ));
    let p = vec![
        ("t".to_string(), "secret".to_string()),
        ("node".to_string(), "x-1".to_string()),
        ("all".to_string(), "".to_string()),
    ];
    let q = Query::from_pairs(&p).unwrap();
    assert!(q.all, "empty all reads true");
    assert!(q.project.is_empty());
}

#[test]
fn repeated_filter_pairs_accumulate_any_of_sets() {
    let p = vec![
        ("status".to_string(), "ready".to_string()),
        ("status".to_string(), "idea".to_string()),
        ("status".to_string(), "ready".to_string()),
        ("priority".to_string(), "p1".to_string()),
        ("status".to_string(), "".to_string()),
    ];
    let q = Query::from_pairs(&p).unwrap();
    assert_eq!(q.status, ["ready", "idea"], "dup dropped, blank skipped");
    assert_eq!(q.priority, ["p1"]);
}

#[test]
fn any_of_status_filter_keeps_either_status() {
    let rows = vec![
        json!({"id": "x-a", "slug": "a", "status": "ready", "priority": "p2"}),
        json!({"id": "x-b", "slug": "b", "status": "idea", "priority": "p2"}),
        json!({"id": "x-c", "slug": "c", "status": "done", "priority": "p2"}),
    ];
    let inp = fixture(rows);
    let q = Query {
        status: vec!["ready".into(), "idea".into()],
        ..Default::default()
    };
    let b = board(&inp, &q);
    let kept: Vec<&str> = b.lanes[0]
        .cells
        .iter()
        .flat_map(|c| c.cards.iter().map(|card| card.id.as_str()))
        .collect();
    assert_eq!(kept, ["x-a", "x-b"], "either status stays");
}

#[test]
fn status_tiles_count_the_pre_filter_set() {
    // The tiles double as filters, so their counts stay the whole scoped
    // board's: selecting a status must yield exactly what the tile said.
    let rows = vec![
        json!({"id": "x-a", "slug": "a", "status": "ready", "priority": "p2"}),
        json!({"id": "x-b", "slug": "b", "status": "ready", "priority": "p2"}),
        json!({"id": "x-c", "slug": "c", "status": "idea", "priority": "p2"}),
        json!({"id": "x-d", "slug": "d", "status": "done", "priority": "p2"}),
    ];
    let inp = fixture(rows);
    let q = Query {
        status: vec!["done".into()],
        ..Default::default()
    };
    let b = board(&inp, &q);
    let tiles: Vec<(String, usize)> = b
        .stats
        .statuses
        .iter()
        .map(|t| (t.status.clone(), t.total))
        .collect();
    assert_eq!(
        tiles,
        [("done".into(), 1), ("idea".into(), 1), ("ready".into(), 2)],
        "counts are the scoped board's, not the filtered answer's"
    );
    assert_eq!(b.stats.totals.iter().filter(|c| c.total > 0).count(), 1);
}

#[test]
fn list_view_answers_uncapped_cells() {
    // One more card than CELL_CAP: the kanban cell truncates at the cap and
    // says so, the list answers every row.
    let rows: Vec<Value> = (0..CELL_CAP + 1)
        .map(|i| {
            json!({
                "id": format!("x-{i}"), "slug": format!("s{i}"),
                "status": "ready", "priority": "p2"
            })
        })
        .collect();
    let inp = fixture(rows);
    let qk = Query::from_pairs(&vec![]).unwrap();
    let b = board(&inp, &qk);
    let drawn: usize = b.lanes[0].cells.iter().map(|c| c.cards.len()).sum();
    assert_eq!(drawn, CELL_CAP, "kanban capped at the cell cap");
    let q = Query {
        view: View::List,
        ..Default::default()
    };
    let b = board(&inp, &q);
    let drawn: usize = b.lanes[0].cells.iter().map(|c| c.cards.len()).sum();
    assert_eq!(drawn, CELL_CAP + 1, "list uncapped");
}

#[test]
fn cards_carry_created_at_for_the_list_rows() {
    let rows = vec![json!({
        "id": "x-a", "slug": "a", "status": "ready",
        "priority": "p2", "created_at": "2026-09-24T18:00:00Z"
    })];
    let inp = fixture(rows);
    let b = board(&inp, &Query::default());
    let card = b.lanes[0]
        .cells
        .iter()
        .flat_map(|c| c.cards.iter())
        .next()
        .expect("the fixture's one card is on the board");
    assert_eq!(card.created_at.as_deref(), Some("2026-09-24T18:00:00Z"));
}

fn epic_fixture() -> Vec<Value> {
    vec![
        json!({"id": "x-e", "status": "in_progress", "priority": "p1", "title": "The epic"}),
        json!({"id": "x-c", "status": "ready", "priority": "p1", "parent": "x-e"}),
        json!({"id": "x-q", "status": "ready", "priority": "p2", "parent": "x-e", "queued_at": "t"}),
        json!({"id": "x-loose", "status": "ready", "priority": "p2"}),
        json!({"id": "x-d", "status": "done", "priority": "p2", "parent": "x-e", "completed_at": "2026-09-02"}),
    ]
}

#[test]
fn epic_lanes_hold_six_cells_in_column_order() {
    // AC5-HP: cards in several columns, one epic, lanes=epic.
    let inp = fixture(epic_fixture());
    let q = Query {
        lanes: LanesBy::Epic,
        ..Default::default()
    };
    let b = board(&inp, &q);
    assert_eq!(b.schema, 1);
    assert!(b.errors.is_empty());
    let epic_lane = b
        .lanes
        .iter()
        .find(|l| l.key == "x-e")
        .expect("the epic lane");
    assert_eq!(epic_lane.cells.len(), 6);
    for (i, col) in KANBAN_COLUMNS.iter().enumerate() {
        assert_eq!(epic_lane.cells[i].column, *col);
    }
    assert_eq!(
        epic_lane.cells[0].cards[0].id, "x-e",
        "the epic sits in its own lane"
    );
    assert_eq!(epic_lane.cells[1].cards[0].id, "x-c");
    assert_eq!(epic_lane.cells[4].column, "Triage");
    assert_eq!(epic_lane.cells[4].cards[0].id, "x-q");
    assert_eq!(epic_lane.cells[5].total, 1, "uncapped total");
    assert!(
        b.lanes.iter().any(|l| l.key.is_empty()),
        "a loose lane exists"
    );
}

#[test]
fn project_and_priority_filters_narrow_cards_and_totals() {
    // AC6-HP.
    let rows = vec![
        json!({"id": "x-1", "status": "ready", "priority": "p1", "project": "fno"}),
        json!({"id": "x-2", "status": "ready", "priority": "p2", "project": "fno"}),
        json!({"id": "x-3", "status": "ready", "priority": "p1", "project": "other"}),
    ];
    let inp = fixture(rows);
    let q = Query {
        project: vec!["fno".into()],
        priority: vec!["p1".into()],
        ..Default::default()
    };
    let b = board(&inp, &q);
    let cards: Vec<&str> = b
        .lanes
        .iter()
        .flat_map(|l| l.cells.iter())
        .flat_map(|c| c.cards.iter())
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(cards, vec!["x-1"]);
    let now = b.stats.open.iter().find(|t| t.column == "Now").unwrap();
    assert_eq!(now.total, 1, "totals count the filtered set only");
}

#[test]
fn node_lists_children_blockers_and_live_sessions() {
    // AC7-HP.
    let mut rows = vec![
        json!({"id": "x-top", "status": "ready", "priority": "p1", "blocked_by": ["x-blk"]}),
        json!({"id": "x-kid1", "status": "ready", "priority": "p2", "parent": "x-top"}),
        json!({"id": "x-kid2", "status": "ready", "priority": "p2", "parent": "x-top"}),
        json!({"id": "x-blk", "status": "ready", "priority": "p2"}),
    ];
    rows[0]["sessions"] = json!([
        {"phase": "do", "session_id": "s-live"},
        {"phase": "do", "session_id": "s-dead"},
        {"phase": "plan"}
    ]);
    rows[3]["blocked_by"] = json!(["x-top"]);
    let mut agents = vec![row(Some("s-live"))];
    agents[0].pane_id = Some(3);
    let mut inp = fixture(rows);
    inp.agents = agents;
    let view = node(&inp, "x-top").expect("the node resolves");
    assert_eq!(view.children.len(), 2);
    assert_eq!(view.blocked_by.len(), 1);
    assert_eq!(view.blocks.len(), 1, "x-blk is blocked by x-top");
    assert_eq!(view.sessions.len(), 3);
    assert_eq!(view.sessions[0].action, "attach");
    assert_eq!(view.sessions[0].agent.as_deref(), Some("w1"));
    assert_eq!(view.sessions[1].action, "none");
    assert!(view.card.blocked, "x-top has an open dependency");
}

#[test]
fn done_cells_cap_at_20_with_uncapped_totals() {
    // AC10-EDGE.
    let mut rows = Vec::new();
    for i in 0..900 {
        rows.push(json!({
            "id": format!("x-d{i}"),
            "status": "done",
            "priority": "p2",
            "completed_at": format!("2026-09-{:02}", 1 + (i % 20))
        }));
    }
    let inp = fixture(rows);
    let q = Query {
        all: true,
        ..Default::default()
    };
    let b = board(&inp, &q);
    let done_total: usize = b.lanes.iter().map(|l| l.cells[5].total).sum();
    let done_cards: usize = b.lanes.iter().map(|l| l.cells[5].cards.len()).sum();
    assert_eq!(done_total, 900, "uncapped Done totals sum");
    assert_eq!(done_cards, 20, "the single Done cell caps at 20");
    let newest = &b.lanes[0].cells[5].cards[0];
    assert_eq!(newest.id, "x-d19", "newest completed_at first");
}

#[test]
fn external_backend_names_its_gaps() {
    // AC16-EDGE.
    let rows = vec![json!({"id": "g-1", "status": "ready", "priority": "p2"})];
    let mut inp = fixture(rows);
    inp.backend = "github".into();
    let b = board(&inp, &Query::default());
    let features: Vec<&str> = b.unavailable.iter().map(|u| u.feature).collect();
    assert!(features.contains(&"card moves"));
    assert!(features.contains(&"size filter"));
    assert!(features.contains(&"details"));
    assert!(b.facets.sizes.is_empty());
    for u in &b.unavailable {
        assert!(
            u.reason.contains("github"),
            "reason names the backend: {}",
            u.reason
        );
    }
}

#[test]
fn a_board_facts_failure_degrades_to_created_at_order() {
    // AC23-ERR.
    let rows = vec![
        json!({"id": "x-late", "status": "ready", "priority": "p2", "created_at": "2026-09-02"}),
        json!({"id": "x-early", "status": "ready", "priority": "p2", "created_at": "2026-09-01"}),
    ];
    let mut inp = fixture(rows);
    inp.order = Vec::new();
    inp.errors.push(
        "board order unavailable: the store keeper predates the board mode; run fno doctor update"
            .into(),
    );
    let b = board(&inp, &Query::default());
    assert!(b
        .errors
        .iter()
        .any(|e| e.contains("board order unavailable")));
    assert_eq!(b.lanes.len(), 1);
    let cards: Vec<&str> = b.lanes[0].cells[2]
        .cards
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(cards, vec!["x-early", "x-late"], "created_at order");
}

#[test]
fn moved_helpers_still_answer() {
    let mut king = row(None);
    king.name = "kd".into();
    king.crown_level = Some(2);
    king.crown_scope = Some("x-9".into());
    assert_eq!(
        king_of(&[king.clone()], "x-9", None, None),
        Some(("kd".into(), 2))
    );
    assert_eq!(
        king_of(std::slice::from_ref(&king), "x-1", Some("x-9"), None),
        Some(("kd".into(), 2))
    );
    assert_eq!(king_of(&[row(None)], "x-1", None, None), None);
    assert_eq!(
        session_action(None),
        SessionAction::Dim("no registry row".into())
    );
    let mut a = row(Some("s1"));
    a.pane_id = Some(3);
    assert_eq!(session_action(Some(&a)), SessionAction::Attach);
    let mut a = row(Some("s1"));
    a.resumable = true;
    assert_eq!(session_action(Some(&a)), SessionAction::Resume);
    let mut a = row(Some("s1"));
    a.resumable = true;
    a.badge = Some(AgentBadge::Done);
    assert_eq!(session_action(Some(&a)), SessionAction::Dim("done".into()));
}

#[test]
fn snapshot_errors_and_staleness_ride_inputs_errors() {
    // AC8-HP: the snapshot parse copies each errors line into Inputs.errors
    // with the stale stamp first; rows stay the entries.
    let doc = json!({
        "backend": "github",
        "stale_since": "2026-09-24T10:00:00Z",
        "entries": [{"id": "E-1", "status": "ready", "priority": "p2"}],
        "errors": ["closed window failed: gh down"],
    });
    let (rows, errors) = parse_snapshot_doc(&doc);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "E-1");
    assert_eq!(
        errors,
        vec![
            "tracker snapshot stale since 2026-09-24T10:00:00Z".to_string(),
            "closed window failed: gh down".to_string(),
        ]
    );
}

#[test]
fn backlog_view_column_rule_follows_the_authority() {
    // AC21-HP rows, through the model's card path and the rule itself.
    use crate::backlog_view::kanban_column;
    let in_progress = json!({"id": "a", "status": "in_progress", "priority": "p3"});
    let claimed_ready = json!({"id": "b", "status": "ready", "priority": "p2"});
    let done_status = json!({"id": "c", "status": "done", "priority": "p2"});
    let epic_child = json!({"id": "d", "status": "ready", "priority": "p2", "parent": "x-e"});
    assert_eq!(
        kanban_column(&in_progress, false, false, None),
        Some("In Progress")
    );
    assert_eq!(
        kanban_column(&claimed_ready, true, false, None),
        Some("In Progress")
    );
    assert_eq!(
        kanban_column(&done_status, false, false, None),
        Some("Done")
    );
    assert_eq!(
        kanban_column(&epic_child, false, true, None),
        Some("In Progress")
    );
    assert_eq!(
        kanban_column(
            &json!({"id": "e", "status": "ready", "priority": "p2"}),
            false,
            false,
            Some("p1")
        ),
        Some("Now"),
        "effective priority promotes the column"
    );
}
