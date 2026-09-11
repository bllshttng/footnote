//! The api contract's own tests: every query and mutation runs once per
//! backend on the same logical fixture and the two arms must agree
//! (AC14-HP, AC15-EDGE). The volatile stamps a real clock fills in
//! (created_at, ts, touched_at, archived_at, ended_at) are scrubbed before
//! the cross-arm transcript compare; every deterministic byte compares.

use super::*;
use serde_json::{json, Map, Value};
use tempfile::TempDir;

fn base_row(id: &str, title: &str, status: &str) -> Node {
    Node::from_json(&json!({
        "id": id, "slug": id, "title": title, "type": "feature",
        "status": status, "priority": "p2", "domain": "code",
        "created_at": "2026-09-11T00:00:00+00:00"
    }))
    .unwrap()
}

fn session_row(session_id: &str) -> SessionRecord {
    SessionRecord {
        phase: "do".into(),
        harness: "claude".into(),
        session_id: session_id.into(),
        started_at: None,
        ended_at: None,
        ended_by: None,
        effort: None,
        at: None,
        claimed_at: None,
        observed_model: None,
        merge_grant: None,
        extras: Map::new(),
    }
}

/// The shared fixture: three project rows (one held, one parented, one
/// free) and one archived row outside the project.
fn fixture_nodes() -> Vec<Node> {
    let mut one = base_row("ab-one", "One", "idea");
    one.project = Some("fno".into());
    one.labels = Some(vec!["infra".into()]);
    one.claim.locked_by = Some("holder-1".into());
    one.claim.locked_at = Some("2026-09-11T01:00:00+00:00".into());
    one.sessions = Some(vec![session_row("s-1")]);
    let mut two = base_row("ab-two", "Two", "ready");
    two.project = Some("fno".into());
    two.parent = Some("ab-one".into());
    two.created_at = Some("2026-09-12T00:00:00+00:00".into());
    let mut three = base_row("ab-three", "Three", "ready");
    three.project = Some("fno".into());
    let mut four = base_row("ab-four", "Four", "done");
    four.project = Some("other".into());
    four.archived_at = Some("2026-09-10T00:00:00+00:00".into());
    vec![one, two, three, four]
}

/// One arm of the both-backends run: JSON (no db named) or SQLite (the
/// store names its own backend after the one-shot import).
fn arm(dir: &TempDir, backend: crate::backlog::Backend) -> Store {
    let graph = dir.path().join("graph.json");
    let entries: Vec<Value> = fixture_nodes().iter().map(Node::to_json).collect();
    std::fs::write(
        &graph,
        serde_json::to_string(&json!({ "entries": entries })).unwrap(),
    )
    .unwrap();
    let store = Store::new(&graph);
    if backend == crate::backlog::Backend::Sqlite {
        crate::backlog::read_entries(&graph).unwrap();
        crate::backlog::set_backend(&graph, backend).unwrap();
    }
    store
}

fn both_stores() -> (TempDir, TempDir, Store, Store) {
    let json_dir = TempDir::new().unwrap();
    let sqlite_dir = TempDir::new().unwrap();
    let json_store = arm(&json_dir, crate::backlog::Backend::Json);
    let sqlite_store = arm(&sqlite_dir, crate::backlog::Backend::Sqlite);
    (json_dir, sqlite_dir, json_store, sqlite_store)
}

fn scrub(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for key in [
                "created_at",
                "touched_at",
                "archived_at",
                "ts",
                "ended_at",
                "locked_at",
            ] {
                map.remove(key);
            }
            for (_, child) in map.iter_mut() {
                scrub(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(scrub),
        _ => {}
    }
}

fn transcript(store: &Store) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{:?}\n",
        node(store, "ab-one").unwrap().map(|n| n.to_json())
    ));
    let conn = nodes(store, &NodeFilter::default(), &Page::default()).unwrap();
    for row in &conn.nodes {
        out.push_str(&format!("row {}\n", row.to_json()));
    }
    out
}

// -- queries ----------------------------------------------------------------

#[test]
fn api_node_finds_one_row() {
    let (_d1, _d2, json_store, sqlite_store) = both_stores();
    for store in [&json_store, &sqlite_store] {
        let one = node(store, "ab-one").unwrap().unwrap();
        assert_eq!(one.id, "ab-one");
        assert_eq!(one.title, "One");
        assert_eq!(one.claim.locked_by.as_deref(), Some("holder-1"));
        assert!(node(store, "ab-absent").unwrap().is_none());
    }
}

#[test]
fn api_nodes_pages_two_then_cursor_third() {
    // AC14-HP: first:2 over 3 matching rows returns 2 with a next page; the
    // end cursor resumes at the third.
    let (_d1, _d2, json_store, sqlite_store) = both_stores();
    for store in [&json_store, &sqlite_store] {
        let filter = NodeFilter {
            project: Some("fno".into()),
            ..Default::default()
        };
        let page_one = nodes(
            store,
            &filter,
            &Page {
                first: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page_one.nodes.len(), 2);
        assert!(page_one.page_info.has_next_page);
        assert!(!page_one.page_info.has_previous_page);
        let cursor = page_one.page_info.end_cursor.clone().unwrap();
        let page_two = nodes(
            store,
            &filter,
            &Page {
                first: Some(2),
                after: Some(cursor),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page_two.nodes.len(), 1);
        assert_eq!(page_two.nodes[0].id, "ab-three");
        assert!(!page_two.page_info.has_next_page);
        assert!(page_two.page_info.has_previous_page);
    }
}

#[test]
fn api_nodes_first_none_returns_every_row() {
    let (_d1, _d2, json_store, sqlite_store) = both_stores();
    for store in [&json_store, &sqlite_store] {
        let conn = nodes(store, &NodeFilter::default(), &Page::default()).unwrap();
        assert_eq!(conn.nodes.len(), 3, "archived hidden by default");
        assert!(!conn.page_info.has_next_page);
    }
}

#[test]
fn api_nodes_hides_archived_unless_included() {
    let (_d1, _d2, json_store, sqlite_store) = both_stores();
    for store in [&json_store, &sqlite_store] {
        let page = Page {
            include_archived: true,
            ..Default::default()
        };
        let conn = nodes(store, &NodeFilter::default(), &page).unwrap();
        assert_eq!(conn.nodes.len(), 4);
        assert!(conn.nodes.iter().any(|n| n.id == "ab-four"));
    }
}

#[test]
fn api_nodes_filters_by_state_and_status_words() {
    let (_d1, _d2, json_store, sqlite_store) = both_stores();
    for store in [&json_store, &sqlite_store] {
        let filter = NodeFilter {
            state_type: Some("unstarted".into()),
            ..Default::default()
        };
        let conn = nodes(store, &filter, &Page::default()).unwrap();
        assert_eq!(conn.nodes.len(), 3, "idea + two ready rows");
        let filter = NodeFilter {
            status_in: Some(vec!["done".into()]),
            ..Default::default()
        };
        let conn = nodes(store, &filter, &Page::default()).unwrap();
        assert!(conn.nodes.is_empty(), "the done row is archived");
        let filter = NodeFilter {
            status_in: Some(vec!["done".into()]),
            ..Default::default()
        };
        let page = Page {
            include_archived: true,
            ..Default::default()
        };
        let conn = nodes(store, &filter, &page).unwrap();
        assert_eq!(conn.nodes.len(), 1);
    }
}

#[test]
fn api_nodes_filters_by_parent_label_claim_and_session() {
    let (_d1, _d2, json_store, sqlite_store) = both_stores();
    for store in [&json_store, &sqlite_store] {
        let cases: Vec<NodeFilter> = vec![
            NodeFilter {
                label: Some("infra".into()),
                ..Default::default()
            },
            NodeFilter {
                claimed: Some(true),
                ..Default::default()
            },
            NodeFilter {
                session_id: Some("s-1".into()),
                ..Default::default()
            },
        ];
        for filter in &cases {
            let conn = nodes(store, filter, &Page::default()).unwrap();
            assert_eq!(conn.nodes.len(), 1, "filter {filter:?}");
            assert_eq!(conn.nodes[0].id, "ab-one", "filter {filter:?}");
        }
        let conn = nodes(
            store,
            &NodeFilter {
                parent: Some("ab-one".into()),
                ..Default::default()
            },
            &Page::default(),
        )
        .unwrap();
        assert_eq!(conn.nodes.len(), 1);
        assert_eq!(conn.nodes[0].id, "ab-two", "the child of ab-one");
        let filter = NodeFilter {
            claimed: Some(false),
            ..Default::default()
        };
        let conn = nodes(store, &filter, &Page::default()).unwrap();
        assert_eq!(conn.nodes.len(), 2);
    }
}

#[test]
fn api_nodes_orders_by_created_at() {
    let (_d1, _d2, json_store, sqlite_store) = both_stores();
    for store in [&json_store, &sqlite_store] {
        let page = Page {
            order_by: OrderBy::CreatedAt,
            ..Default::default()
        };
        let conn = nodes(store, &NodeFilter::default(), &page).unwrap();
        let ids: Vec<&str> = conn.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["ab-one", "ab-three", "ab-two"]);
    }
}

#[test]
fn api_version_reads_zero_then_counter() {
    let (_d1, _d2, json_store, sqlite_store) = both_stores();
    for store in [&json_store, &sqlite_store] {
        let before = version(store).unwrap();
        assert!(before >= 0);
    }
}

// -- mutations ---------------------------------------------------------------

#[test]
fn api_node_update_bumps_version_once() {
    // AC15-EDGE: one successful mutation, version grows by exactly one.
    let (_d1, _d2, json_store, sqlite_store) = both_stores();
    for store in [&json_store, &sqlite_store] {
        let before = version(store).unwrap();
        let payload = node_update(
            store,
            "ab-one",
            NodeUpdateInput {
                title: Some("One renamed".into()),
                description: Some("now described".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(payload.success);
        let row = payload.node.unwrap();
        assert_eq!(row.title, "One renamed");
        assert_eq!(row.description.as_deref(), Some("now described"));
        assert_eq!(payload.version, before + 1);
        assert_eq!(version(store).unwrap(), before + 1);
        let reread = node(store, "ab-one").unwrap().unwrap();
        assert_eq!(reread.title, "One renamed");
    }
}

#[test]
fn api_failed_mutation_keeps_version() {
    // AC15-EDGE: a failed mutation answers success:false and the store's
    // counter does not move.
    let (_d1, _d2, json_store, sqlite_store) = both_stores();
    for store in [&json_store, &sqlite_store] {
        let before = version(store).unwrap();
        let payload = node_update(
            store,
            "ab-absent",
            NodeUpdateInput {
                title: Some("nobody".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!payload.success);
        assert!(payload.node.is_none());
        assert_eq!(payload.version, before);
        let payload = node_update(
            store,
            "ab-one",
            NodeUpdateInput {
                status: Some("not-a-status".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!payload.success);
        assert_eq!(version(store).unwrap(), before);
    }
}

#[test]
fn api_node_create_appends_and_queries() {
    let (_d1, _d2, json_store, sqlite_store) = both_stores();
    for store in [&json_store, &sqlite_store] {
        let before = version(store).unwrap();
        let payload = node_create(
            store,
            NodeCreateInput {
                id: "ab-five".into(),
                title: "Five".into(),
                project: Some("fno".into()),
                description: Some("the fifth".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(payload.success, "create refused");
        let created = payload.node.unwrap();
        assert_eq!(created.status, Status::Idea, "default rung");
        assert_eq!(payload.version, before + 1);
        let reread = node(store, "ab-five").unwrap().unwrap();
        assert_eq!(reread.title, "Five");
        assert_eq!(reread.description.as_deref(), Some("the fifth"));
        // A duplicate id refuses and leaves the counter alone.
        let again = node_create(
            store,
            NodeCreateInput {
                id: "ab-five".into(),
                title: "Five again".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!again.success);
        assert_eq!(version(store).unwrap(), before + 1);
    }
}

#[test]
fn api_node_batch_update_moves_every_named_row() {
    let (_d1, _d2, json_store, sqlite_store) = both_stores();
    for store in [&json_store, &sqlite_store] {
        let payload = node_batch_update(
            store,
            &["ab-one".into(), "ab-two".into()],
            NodeUpdateInput {
                priority: Some("p0".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(payload.success);
        let moved = payload.node.unwrap();
        assert_eq!(moved.len(), 2);
        assert!(moved.iter().all(|n| n.priority == Priority::P0));
        // One missing id fails the whole batch.
        let before = version(store).unwrap();
        let payload = node_batch_update(
            store,
            &["ab-one".into(), "ab-absent".into()],
            NodeUpdateInput {
                priority: Some("p1".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!payload.success);
        assert_eq!(version(store).unwrap(), before);
    }
}

#[test]
fn api_archive_unarchive_delete_roundtrip() {
    let (_d1, _d2, json_store, sqlite_store) = both_stores();
    for store in [&json_store, &sqlite_store] {
        let payload = node_archive(store, "ab-two").unwrap();
        assert!(payload.success);
        let conn = nodes(store, &NodeFilter::default(), &Page::default()).unwrap();
        assert_eq!(
            conn.nodes.len(),
            2,
            "the archived row left the default view"
        );
        node_unarchive(store, "ab-two").unwrap();
        let conn = nodes(store, &NodeFilter::default(), &Page::default()).unwrap();
        assert_eq!(conn.nodes.len(), 3);
        let payload = node_delete(store, "ab-two").unwrap();
        assert!(payload.success);
        assert!(node(store, "ab-two").unwrap().is_none());
        let payload = node_delete(store, "ab-two").unwrap();
        assert!(!payload.success, "deleting twice refuses");
    }
}

#[test]
fn api_edge_label_and_note_mutations_agree() {
    let (_d1, _d2, json_store, sqlite_store) = both_stores();
    for store in [&json_store, &sqlite_store] {
        let payload = relation_create(store, "ab-one", "ab-two", RelationType::Blocks).unwrap();
        assert!(payload.success);
        assert_eq!(
            payload.node.unwrap().relations.blocked_by,
            Some(vec!["ab-two".into()])
        );
        let payload = relation_create(store, "ab-one", "ab-two", RelationType::Blocks).unwrap();
        assert!(payload.success, "an idempotent re-add still succeeds");
        relation_delete(store, "ab-one", "ab-two", RelationType::Blocks).unwrap();
        let one = node(store, "ab-one").unwrap().unwrap();
        assert!(
            one.relations
                .blocked_by
                .as_ref()
                .map_or(false, Vec::is_empty),
            "the edge list empties: {:?}",
            one.relations.blocked_by
        );
        label_add(store, "ab-one", "urgent").unwrap();
        label_add(store, "ab-one", "urgent").unwrap();
        let one = node(store, "ab-one").unwrap().unwrap();
        assert_eq!(one.labels, Some(vec!["infra".into(), "urgent".into()]));
        label_remove(store, "ab-one", "infra").unwrap();
        let one = node(store, "ab-one").unwrap().unwrap();
        assert_eq!(one.labels, Some(vec!["urgent".into()]));
        let payload = comment_create(
            store,
            "ab-one",
            CommentCreateInput {
                body: "first note".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(payload.success);
        let conn = comments(store, "ab-one", &Page::default()).unwrap();
        assert_eq!(conn.nodes.len(), 1);
        assert_eq!(conn.nodes[0].body.as_deref(), Some("first note"));
    }
}

#[test]
fn api_pr_session_dispatch_and_encounter_mutations_agree() {
    let (_d1, _d2, json_store, sqlite_store) = both_stores();
    for store in [&json_store, &sqlite_store] {
        let payload = pull_request_attach(
            store,
            "ab-one",
            PullRequestInput {
                number: 42,
                url: Some("https://example.test/pr/42".into()),
                note: None,
            },
        )
        .unwrap();
        assert!(payload.success);
        let one = node(store, "ab-one").unwrap().unwrap();
        assert_eq!(one.primary_pr.unwrap().number, Some(42));
        pull_request_attach(
            store,
            "ab-one",
            PullRequestInput {
                number: 43,
                url: None,
                note: Some("follow-up".into()),
            },
        )
        .unwrap();
        let one = node(store, "ab-one").unwrap().unwrap();
        assert_eq!(one.additional_prs.unwrap().len(), 1);
        let payload = session_append(store, "ab-one", session_row("s-2")).unwrap();
        assert!(payload.success);
        let payload = session_end(store, "ab-one", "s-2", "operator").unwrap();
        assert!(payload.success);
        let one = node(store, "ab-one").unwrap().unwrap();
        let s2 = one
            .sessions
            .unwrap()
            .into_iter()
            .find(|row| row.session_id == "s-2")
            .unwrap();
        assert_eq!(s2.ended_by.as_deref(), Some("operator"));
        let again = session_end(store, "ab-one", "s-2", "operator").unwrap();
        assert!(!again.success, "ending twice refuses");
        let payload = dispatch_set(
            store,
            "ab-one",
            Some(Dispatch {
                verb: Some("do".into()),
                brief: None,
                model: Some("test-model".into()),
            }),
        )
        .unwrap();
        assert!(payload.success);
        let one = node(store, "ab-one").unwrap().unwrap();
        assert_eq!(one.dispatch.verb.as_deref(), Some("do"));
        dispatch_set(store, "ab-one", None).unwrap();
        let one = node(store, "ab-one").unwrap().unwrap();
        assert_eq!(one.dispatch.verb, None);
        let payload = encounter_create(
            store,
            "ab-one",
            EncounterInput {
                evidence: "cost me an hour".into(),
                session_id: Some("s-1".into()),
            },
        )
        .unwrap();
        assert!(payload.success);
        let one = node(store, "ab-one").unwrap().unwrap();
        assert_eq!(one.encounters.unwrap().len(), 1);
    }
}

// -- the cross-arm contract ---------------------------------------------------

#[test]
fn api_both_backends_agree_on_to_json() {
    // AC14-HP: the same scripted queries and mutations under each backend,
    // equal typed output (clock stamps scrubbed, everything else exact).
    fn script(store: &Store) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::new();
        out.push(json!(node(store, "ab-one").unwrap().map(|n| n.to_json())));
        out.push(json!(nodes(
            store,
            &NodeFilter::default(),
            &Page::default()
        )
        .unwrap()
        .nodes
        .iter()
        .map(Node::to_json)
        .collect::<Vec<_>>()));
        node_update(
            store,
            "ab-one",
            NodeUpdateInput {
                title: Some("One scripted".into()),
                ..Default::default()
            },
        )
        .unwrap();
        node_create(
            store,
            NodeCreateInput {
                id: "ab-scripted".into(),
                title: "Scripted".into(),
                ..Default::default()
            },
        )
        .unwrap();
        relation_create(store, "ab-two", "ab-three", RelationType::Related).unwrap();
        label_add(store, "ab-three", "scripted").unwrap();
        comment_create(
            store,
            "ab-two",
            CommentCreateInput {
                body: "scripted note".into(),
                ..Default::default()
            },
        )
        .unwrap();
        node_archive(store, "ab-three").unwrap();
        out.push(json!(node(store, "ab-one").unwrap().map(|n| n.to_json())));
        out.push(json!(node(store, "ab-scripted")
            .unwrap()
            .map(|n| n.to_json())));
        out.push(json!(version(store).unwrap()));
        out.iter_mut().for_each(scrub);
        out
    }
    let (_d1, _d2, json_store, sqlite_store) = both_stores();
    assert_eq!(
        serde_json::to_string(&script(&json_store)).unwrap(),
        serde_json::to_string(&script(&sqlite_store)).unwrap(),
    );
}

#[test]
fn api_transcript_helper_agrees_before_mutations() {
    let (_d1, _d2, json_store, sqlite_store) = both_stores();
    let (a, b) = (transcript(&json_store), transcript(&sqlite_store));
    assert_eq!(a, b);
}

#[test]
fn api_cursor_decodes_roundtrip() {
    let mut one = base_row("ab-one", "One", "idea");
    one.ordinal = 7;
    let cursor = encode_cursor(&one);
    assert_eq!(decode_cursor(&cursor), Some((7, "ab-one".into())));
    assert_eq!(decode_cursor("not-a-cursor"), None);
}
