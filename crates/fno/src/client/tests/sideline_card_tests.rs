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

fn card_rows_for(view: &View, name: &str) -> (usize, usize) {
    let rows = view.painted_rows();
    let agent = rows
        .iter()
        .position(|row| matches!(row, DisplayRow::Agent(a) if a.name == name))
        .expect("agent card exists");
    let detail = rows
        .iter()
        .position(|row| matches!(row, DisplayRow::CardDetail(a) if a.name == name))
        .expect("card detail exists");
    (agent, detail)
}

fn card_highlight_snapshot(view: &View, frame: &Frame, agent_i: usize, detail_i: usize) -> String {
    let cols = frame.cols as usize;
    let text_w = view.sideline_paint_w().saturating_sub(1);
    let offset = view.sideline_offset();
    [agent_i, detail_i]
        .into_iter()
        .map(|display_i| {
            let row = display_i - offset;
            frame.cells[row * cols..row * cols + text_w]
                .iter()
                .map(|cell| {
                    if cell.flags & cell_flags::INVERSE == cell_flags::INVERSE {
                        '#'
                    } else {
                        '.'
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn card_pair_cells(view: &View, frame: &Frame, agent_i: usize, detail_i: usize) -> Vec<Cell> {
    let cols = frame.cols as usize;
    let text_w = view.sideline_paint_w().saturating_sub(1);
    let offset = view.sideline_offset();
    [agent_i, detail_i]
        .into_iter()
        .flat_map(|display_i| {
            let row = display_i - offset;
            frame.cells[row * cols..row * cols + text_w].iter().cloned()
        })
        .collect()
}

fn row_text(frame: &Frame, row: usize, width: usize) -> String {
    let cols = frame.cols as usize;
    frame.cells[row * cols..row * cols + width]
        .iter()
        .map(|cell| cell.c)
        .collect()
}

fn frame_cell_snapshot_digest(cells: &[Cell]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for cell in cells {
        let encoded = format!(
            "{:?}:{:?}:{:?}:{:02x}\0",
            cell.c, cell.fg, cell.bg, cell.flags
        );
        for byte in encoded.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    hash
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
fn hovering_card_line_two_snapshots_one_solid_highlight_on_both_lines() {
    let mut v = card_view(king_and_worker());
    v.term = (30, 140);
    v.sideline_width = 80;
    let (agent_i, detail_i) = card_rows_for(&v, "w1");
    v.hover_row = Some(detail_i);

    let frame = v.compose();
    let width = v.sideline_paint_w().saturating_sub(1);
    let line = "#".repeat(width);
    assert_eq!(
        card_highlight_snapshot(&v, &frame, agent_i, detail_i),
        format!("{line}\n{line}"),
        "hover snapshot includes every cell and column boundary on both lines"
    );
}

#[test]
fn selecting_card_line_one_snapshots_one_solid_highlight_on_both_lines() {
    let mut v = card_view(king_and_worker());
    v.term = (30, 140);
    v.sideline_width = 80;
    let (agent_i, detail_i) = card_rows_for(&v, "w1");
    v.selector = Some(agent_i);

    let frame = v.compose();
    let width = v.sideline_paint_w().saturating_sub(1);
    let line = "#".repeat(width);
    assert_eq!(
        card_highlight_snapshot(&v, &frame, agent_i, detail_i),
        format!("{line}\n{line}"),
        "selection snapshot includes every cell and column boundary on both lines"
    );
}

#[test]
fn hover_and_selection_share_the_same_card_cell_snapshot() {
    let mut hover = card_view(king_and_worker());
    hover.term = (30, 140);
    hover.sideline_width = 80;
    let (agent_i, detail_i) = card_rows_for(&hover, "w1");
    hover.hover_row = Some(detail_i);
    let hover_frame = hover.compose();
    let hover_cells = card_pair_cells(&hover, &hover_frame, agent_i, detail_i);

    let mut selected = card_view(king_and_worker());
    selected.term = (30, 140);
    selected.sideline_width = 80;
    let (selected_agent_i, selected_detail_i) = card_rows_for(&selected, "w1");
    selected.selector = Some(selected_agent_i);
    let selected_frame = selected.compose();
    let selected_cells = card_pair_cells(
        &selected,
        &selected_frame,
        selected_agent_i,
        selected_detail_i,
    );

    assert_eq!(
        hover_cells, selected_cells,
        "hover and click use one paint path"
    );
}

#[test]
fn card_pr_and_age_snapshots_share_the_panel_right_edge() {
    let mut agents = king_and_worker();
    agents[1].last_activity_age_s = Some(42);
    let mut v = card_view(agents);
    v.term = (30, 140);
    v.sideline_width = 80;
    let (agent_i, detail_i) = card_rows_for(&v, "w1");
    let frame = v.compose();
    let offset = v.sideline_offset();
    let cols = frame.cols as usize;
    let width = v.sideline_paint_w() - 1;
    let agent_row = agent_i - offset;
    let detail_row = detail_i - offset;
    let agent_cells = &frame.cells[agent_row * cols..agent_row * cols + width];
    let detail_cells = &frame.cells[detail_row * cols..detail_row * cols + width];
    let pr_end = agent_cells
        .windows(3)
        .rposition(|run| [run[0].c, run[1].c, run[2].c] == ['#', '4', '2'])
        .expect("PR is visible")
        + 2;
    let age_end = detail_cells
        .windows(3)
        .rposition(|run| [run[0].c, run[1].c, run[2].c] == ['4', '2', 's'])
        .expect("age is visible")
        + 2;

    assert_eq!(pr_end, age_end, "the two line snapshots share a right edge");
}

#[test]
fn regular_card_snapshot_shows_a_pr_when_it_fits() {
    let mut agents = king_and_worker();
    agents[1].last_activity_age_s = Some(42);
    let mut v = card_view(agents);
    set_density(&mut v, Density::Regular);
    v.term = (30, 140);
    let (agent_i, _) = card_rows_for(&v, "w1");
    let frame = v.compose();
    let row = agent_i - v.sideline_offset();
    let width = v.sideline_paint_w() - 1;
    let line = row_text(&frame, row, width);

    assert!(line.ends_with("#42"), "Regular card snapshot: {line:?}");
}

#[test]
fn regular_card_snapshot_omits_a_pr_that_would_overwrite_identity() {
    let mut agents = king_and_worker();
    agents[1].pr = Some(1_234_567);
    let mut v = card_view(agents);
    set_density(&mut v, Density::Regular);
    v.term = (30, 140);
    let (agent_i, _) = card_rows_for(&v, "w1");
    let frame = v.compose();
    let row = agent_i - v.sideline_offset();
    let width = v.sideline_paint_w() - 1;
    let line = row_text(&frame, row, width);
    let cols = frame.cols as usize;
    let rects = sideline_column_rects(width as u16);
    let status = frame.cells
        [row * cols + rects[0].x as usize..row * cols + (rects[0].x + rects[0].width) as usize]
        .iter()
        .map(|cell| cell.c)
        .collect::<String>();
    let name = frame.cells
        [row * cols + rects[1].x as usize..row * cols + (rects[1].x + rects[1].width) as usize]
        .iter()
        .map(|cell| cell.c)
        .collect::<String>();

    assert!(
        status.chars().any(|ch| ch != ' '),
        "card status remains visible: {status:?}"
    );
    assert!(
        name.contains("w1"),
        "card identity remains visible: {name:?}"
    );
    assert!(
        !line.contains("#1234567"),
        "oversized PR is omitted: {line:?}"
    );
}

#[test]
fn list_mode_matches_its_frozen_frame_cell_snapshot() {
    let mut agents = king_and_worker();
    agents[0].last_activity_age_s = Some(42);
    agents[1].last_activity_age_s = Some(42);
    let mut v = card_view(agents);
    v.sideline_layout = sideline_color::SidelineLayout::List;
    v.term = (30, 140);
    v.sideline_width = 80;
    let frame = v.compose();

    assert_eq!(
        frame_cell_snapshot_digest(&frame.cells),
        0x71346fda85d3f813,
        "List frame-cell snapshot"
    );
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
