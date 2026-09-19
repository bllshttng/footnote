//! View-wiring tests for the node detail overlay: the Enter-on-card open,
//! the Esc key flow, and what a refusal notice looks like through the same
//! notice channel everything else uses. The pure logic (fold parse, action
//! derivation, king lookup, render) is unit-tested in
//! `client/node_detail.rs`; these drive the View integration.

use super::tests::two_pane_view;
use super::*;

fn card(id: &str, slug: &str) -> BacklogCard {
    serde_json::from_str::<BacklogCard>(&format!(
        r#"{{"id":"{id}","slug":"{slug}","priority":"p1","state":"Ready"}}"#
    ))
    .expect("a minimal card decodes")
}

/// The display-row index of the card with `id`, the cursor Enter acts on.
fn card_row(view: &mut View, id: &str) -> usize {
    view.expand_pull_sections();
    view.display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::Card(c) if c.id == id))
        .expect("the card is in the catalog")
}

#[tokio::test]
async fn enter_on_a_card_opens_the_detail_overlay() {
    let mut view = two_pane_view();
    view.layout.backlog = vec![card("x-1", "feat")];
    let cur = card_row(&mut view, "x-1");
    view.selector = Some(cur);

    // A Vec<u8> is the wire sink; the open writes nothing.
    let mut sock = Vec::new();
    selector_apply_row_action(&mut view, cur, &mut sock)
        .await
        .unwrap();

    let nd = view
        .node_detail
        .as_ref()
        .expect("Enter on a card opened the overlay");
    assert_eq!(nd.node_id, "x-1");
    assert!(nd.want, "the open arms a fold");
    assert!(
        view.selector.is_some(),
        "the selector stays open underneath"
    );
}

#[tokio::test]
async fn esc_closes_and_selector_survives() {
    let mut view = two_pane_view();
    view.layout.backlog = vec![card("x-1", "feat")];
    view.selector = Some(card_row(&mut view, "x-1"));
    node_detail::open_for(&mut view, "x-1".into());

    let mut sock = Vec::new();
    node_detail::detail_keys(&mut view, &[0x1b], &mut sock)
        .await
        .unwrap();

    assert!(view.node_detail.is_none(), "Esc closed the overlay");
    assert!(view.selector.is_some(), "the selector survives underneath");
}

#[tokio::test]
async fn a_dim_row_answers_with_its_reason_and_launches_nothing() {
    let mut view = two_pane_view();
    view.layout.backlog = vec![card("x-1", "feat")];
    // A session whose id joins NO registry row: activation must notice,
    // never launch.
    node_detail::open_for(&mut view, "x-1".into());
    view.node_detail = Some(node_detail::NodeDetailOverlay {
        node_id: "x-1".into(),
        detail: Some(
            serde_json::from_str(
                r#"{"id":"x-1","slug":"feat","sessions":[
                {"session_id":"cccccccc-2"}]}"#,
            )
            .unwrap(),
        ),
        error: None,
        inflight: false,
        want: false,
        gen: 1,
        sel: 0,
    });

    let mut sock = Vec::new();
    node_detail::detail_keys(&mut view, &[b'a'], &mut sock)
        .await
        .unwrap();

    assert!(
        view.notice
            .as_ref()
            .map(|(s, _)| s.contains("no registry row"))
            .unwrap_or(false),
        "the refusal is the notice, got: {:?}",
        view.notice
    );
}
