//! The sideline card layout acceptance family: two-line padded cards behind
//! `[sideline] layout = "card"`, list mode byte-identical.

use super::*;

/// An Extended-density view in card mode over the given agents.
fn card_view(agents: Vec<AgentRow>) -> View {
    let mut v = wide_view(agents);
    set_density(&mut v, Density::Extended);
    v.sideline_layout = sideline_color::SidelineLayout::Card;
    v
}

fn king_and_worker() -> Vec<AgentRow> {
    let mut king = agent_row("king-a", 4, Some(AgentBadge::Working), false);
    king.harness = Some("claude".into());
    king.crown_level = Some(2);
    king.crown_scope = Some("fno".into());
    king.harness_session_id = Some("sess-king".into());
    let mut w1 = agent_row("w1", 5, Some(AgentBadge::Working), false);
    w1.harness = Some("codex".into());
    w1.pr = Some(42);
    w1.tail = Some("**one message**".into());
    w1.lineage_kind = Some("child".into());
    w1.spawned_by_session = Some("sess-king".into());
    w1.harness_session_id = Some("sess-w1".into());
    vec![king, w1]
}

#[test]
fn card_mode_expands_each_agent_into_a_two_line_padded_card() {
    // AC3: two agents expand to Blank, Agent, CardDetail per card, one
    // blank shared between adjacent cards, every agent depth 0.
    let v = card_view(king_and_worker());
    let (rows, depths) = v.display_rows_with_depths();
    let names: Vec<String> = rows
        .iter()
        .map(|r| match r {
            DisplayRow::TableHead => "head",
            DisplayRow::Sel(s) if s.tab.is_none() => "band",
            DisplayRow::Blank => "blank",
            DisplayRow::Agent(_) => "agent",
            DisplayRow::CardDetail(_) => "detail",
            _ => "other",
        })
        .map(String::from)
        .collect();
    assert!(
        names.len() >= 9
            && names[..9]
                == [
                    "head", "band", "blank", "agent", "detail", "blank", "agent", "detail", "blank"
                ],
        "{names:?}"
    );
    assert!(
        depths.iter().all(|d| *d == 0),
        "every depth is 0: {depths:?}"
    );
}

#[test]
fn card_frame_paints_glyph_name_word_pr_on_line1_king_message_age_on_line2() {
    // AC6/AC7: working worker w1 with PR 42: line 1 = glyph, w1, Work, #42;
    // line 2 = harness, king handle, message, age. A worker with no crowned
    // ancestor has no king segment (and no empty `·  ·`).
    let mut v = card_view(king_and_worker());
    v.term = (30, 140);
    v.sideline_width = 80;
    let frame = v.compose();
    let text = frame_text(&frame);
    assert!(text.contains("w1"), "{text:?}");
    assert!(text.contains("#42"), "{text:?}");
    assert!(text.contains("claude"), "{text:?}");
    assert!(text.contains("king-a"), "{text:?}");
    assert!(text.contains("one message"), "{text:?}");
    // The king's own card shows its crown scope, not a king name.
    assert!(text.contains("fno"), "{text:?}");
    // Line 2's segment join: harness, then the king handle.
    assert!(text.contains("codex \u{b7} king-a"), "{text:?}");
}

#[test]
fn card_detail_click_routes_to_the_agent_above() {
    // AC5: row_action on a CardDetail equals row_action one row up.
    let v = card_view(king_and_worker());
    let rows = v.painted_rows();
    let detail_i = rows
        .iter()
        .position(|r| matches!(r, DisplayRow::CardDetail(_)))
        .expect("a card detail row exists");
    let agent_action = v.row_action(detail_i - 1);
    let detail_action = v.row_action(detail_i);
    let (Some(ChromeHit::Cmds(a)), Some(ChromeHit::Cmds(b))) = (agent_action, detail_action) else {
        panic!("both rows resolve to Cmds");
    };
    assert_eq!(a, b, "the click routes to the Agent's commands");
}

#[test]
fn king_label_walk_stops_on_a_lineage_cycle() {
    // AC8: two rows naming each other as parent terminate with None.
    let mut x = agent_row("x", 4, Some(AgentBadge::Working), false);
    x.lineage_kind = Some("child".into());
    x.spawned_by_session = Some("sess-y".into());
    x.harness_session_id = Some("sess-x".into());
    let mut y = agent_row("y", 5, Some(AgentBadge::Working), false);
    y.lineage_kind = Some("child".into());
    y.spawned_by_session = Some("sess-x".into());
    y.harness_session_id = Some("sess-y".into());
    let v = card_view(vec![x, y]);
    let rows = v.painted_rows();
    let xrow = rows.iter().find_map(|r| match r {
        DisplayRow::Agent(a) if a.name == "x" => Some(*a),
        _ => None,
    });
    let xr = xrow.expect("row x exists");
    assert_eq!(v.king_label(xr), None);
}
