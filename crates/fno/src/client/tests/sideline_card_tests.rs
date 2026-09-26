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
                .map(|cell| if cell.bg != Color::Default { '#' } else { '.' })
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
fn card_age_sort_orders_workers_inside_a_king_group() {
    // The user report: sorted by age, a king's workers read
    // 12m, 12m, 10m, 41m, 23s in the card view. The card path now runs the
    // same run sort the extended table uses, workers order inside their
    // king's group, kings keep their group order.
    let ages = [("w-old", 720u64), ("w-new", 60), ("w-mid", 600)];
    let mut agents = Vec::new();
    let mut king = agent_row("king-a", 4, Some(AgentBadge::Working), false);
    king.crown_level = Some(2);
    king.crown_scope = Some("fno".into());
    king.harness_session_id = Some("sess-king".into());
    king.last_activity_age_s = Some(10);
    agents.push(king);
    for (name, age) in ages {
        let mut w = agent_row(name, 5, Some(AgentBadge::Working), false);
        w.lineage_kind = Some("child".into());
        w.spawned_by_session = Some("sess-king".into());
        w.harness_session_id = Some(format!("sess-{name}"));
        w.last_activity_age_s = Some(age);
        agents.push(w);
    }
    for density in [Density::Extended, Density::Regular] {
        let mut v = card_view(agents.clone());
        set_density(&mut v, density);
        v.agent_sort = AgentSort {
            column: AgentSortColumn::Age,
            direction: SortDirection::Ascending,
        };
        let (rows, _) = v.display_rows_with_depths();
        let got: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                DisplayRow::Agent(a) if a.name != "king-a" => Some(a.name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            got,
            vec!["w-new", "w-mid", "w-old"],
            "workers order by the sort key inside the king group at {density:?}"
        );
        // The painted age is the value sorted on: the card detail of the
        // oldest worker reads the humanized value of its measured age.
        let now = crate::digest_overlay::now_secs();
        let w_old = rows
            .iter()
            .find_map(|r| match r {
                DisplayRow::CardDetail(a) if a.name == "w-old" => Some(a),
                _ => None,
            })
            .expect("w-old card detail exists");
        let detail = v.card_detail_text(w_old, now, 80);
        assert!(
            detail.ends_with("12m"),
            "painted age must read the sorted-on field: {detail:?}"
        );
    }
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
fn hovered_card_paints_one_background_across_both_lines_including_gaps() {
    // Per-cell background, not text: every cell of both lines carries the
    // same band, the column gaps included. The pair is the theme's explicit
    // hover pair - never INVERSE.
    let mut v = card_view(king_and_worker());
    v.term = (30, 140);
    v.sideline_width = 80;
    let (agent_i, detail_i) = card_rows_for(&v, "w1");
    v.hover_row = Some(agent_i);
    let frame = v.compose();
    for cell in card_pair_cells(&v, &frame, agent_i, detail_i) {
        assert_eq!(cell.bg, Color::Indexed(0), "one background everywhere");
        assert_eq!(cell.fg, Color::Indexed(3), "accent band text");
        assert_eq!(cell.flags, 0, "no INVERSE and no DIM inside the band");
    }
}

#[test]
fn chosen_card_paints_accent_across_both_lines() {
    let mut v = card_view(king_and_worker());
    v.term = (30, 140);
    v.sideline_width = 80;
    v.layout.focus = 5;
    let (agent_i, detail_i) = card_rows_for(&v, "w1");
    let frame = v.compose();
    let accent = v.theme.accent;
    let cols = frame.cols as usize;
    let text_w = v.sideline_paint_w().saturating_sub(1);
    let offset = v.sideline_offset();
    for display_i in [agent_i, detail_i] {
        let row = display_i - offset;
        for cell in &frame.cells[row * cols..row * cols + text_w] {
            assert_eq!(cell.bg, accent, "the chosen color fills the card line");
            assert_eq!(cell.fg, crate::theme::BAND_TEXT, "dark band text");
            assert_eq!(cell.flags, 0, "no INVERSE and no DIM inside the band");
        }
    }
}

#[test]
fn hovering_the_chosen_card_keeps_the_chosen_color_on_both_lines() {
    let mut v = card_view(king_and_worker());
    v.term = (30, 140);
    v.sideline_width = 80;
    v.layout.focus = 5;
    let (agent_i, detail_i) = card_rows_for(&v, "w1");
    v.hover_row = Some(detail_i);
    let frame = v.compose();
    let accent = v.theme.accent;
    let cols = frame.cols as usize;
    let text_w = v.sideline_paint_w().saturating_sub(1);
    let offset = v.sideline_offset();
    for display_i in [agent_i, detail_i] {
        let row = display_i - offset;
        for cell in &frame.cells[row * cols..row * cols + text_w] {
            assert_eq!(cell.bg, accent, "the chosen color wins on hover");
        }
    }
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
    let pr_tail = agent_cells[width - 6..]
        .iter()
        .map(|cell| cell.c)
        .collect::<String>();
    let age_tail = detail_cells[width - 6..]
        .iter()
        .map(|cell| cell.c)
        .collect::<String>();
    assert_eq!(format!("{pr_tail}\n{age_tail}"), "   #42\n   42s");
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
    assert_eq!(
        line.chars().skip(width - 6).collect::<String>(),
        "   #42",
        "PR occupies the same six-column right-edge slot as age"
    );
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

    // Re-frozen when the bracket crown tag left the sideline: the registry
    // label is the name, so the tag cells are gone.
    assert_eq!(
        frame_cell_snapshot_digest(&frame.cells),
        0x8f03cc1b0244586,
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

// The x-cd1c contrast family: every highlight band paints an explicit pair
// whose readability the WCAG lens holds on a dark AND a light terminal, and
// the unhighlighted rows' colored texts follow the terminal palette.

fn lens_cell(fg: Color, bg: Color) -> crate::proto::Cell {
    crate::proto::Cell {
        c: 'x',
        fg,
        bg,
        flags: 0,
    }
}

fn lens_contrast(fg: Color, bg: Color, lens: crate::frame_html::Theme) -> f64 {
    crate::frame_html::contrast_ratio(&lens_cell(fg, bg), lens)
}

fn shipped_mux_themes() -> Vec<crate::theme::Theme> {
    let mut themes = vec![crate::theme::Theme::default_theme()];
    for name in ["catppuccin", "tokyo-night", "gruvbox"] {
        let (t, warn) = crate::theme::Theme::from_name(name);
        assert!(warn.is_none(), "{name} must ship without a warning");
        themes.push(t);
    }
    themes
}

#[test]
fn band_pairs_hold_luminance_contrast_on_dark_and_light() {
    // x-cd1c D1: every band is an explicit fg+bg pair with no INVERSE and no
    // DIM. The CHOSEN band clears the 4.5:1 body-text floor (dark text on a
    // light accent); the hover band clears the 3:1 bar (transient affordance,
    // same text the row carries unhovered). `terminal` resolves its Indexed
    // legs against each lens theme's real palette; named themes paint fixed
    // RGB pairs, which the named-theme lens themes judge directly.
    for t in shipped_mux_themes() {
        for (chosen, floor) in [(true, 4.5), (false, 3.0)] {
            let (fg, bg, flags) = crate::theme::band_style(chosen, &t);
            assert_eq!(flags, 0, "no INVERSE and no DIM inside the band");
            assert!(fg != Color::Default, "band fg must be explicit");
            assert!(bg != Color::Default, "band bg must be explicit");
            for lens in crate::frame_html::THEMES {
                let ratio = lens_contrast(fg, bg, lens);
                assert!(
                    ratio >= floor,
                    "{} chosen={chosen} on {}: {ratio:.2}:1 (floor {floor})",
                    t.name,
                    lens.name
                );
            }
        }
    }
}

#[test]
fn the_dark_anchor_is_the_luminance_pick_on_every_accent() {
    // x-cd1c D1: dark text on light accents is not taste, it is measurement.
    // On a light terminal the default fg loses to the dark anchor on every
    // shipped accent, which is the pick the band records.
    let mut accents = vec![crate::theme::Theme::default_theme().accent];
    for name in ["catppuccin", "tokyo-night", "gruvbox"] {
        let (t, _) = crate::theme::Theme::from_name(name);
        accents.push(t.accent);
    }
    let light_fg = Color::Rgb(0xf2, 0xf2, 0xf2);
    for bg in accents {
        let dark = lens_contrast(Color::Rgb(0, 0, 0), bg, crate::frame_html::LIGHT);
        let light = lens_contrast(light_fg, bg, crate::frame_html::LIGHT);
        assert!(dark >= 4.5, "accent {bg:?}: dark anchor {dark:.2}:1");
        assert!(
            dark > light,
            "accent {bg:?}: dark anchor {dark:.2} must beat light {light:.2}"
        );
    }
}

#[test]
fn composed_bands_hold_contrast_on_dark_and_light_frames() {
    // x-cd1c D1, end to end: the REAL painter's chosen band (the worker's
    // card) and hover band (the king's card) clear their floors on a dark and
    // a light lens theme. The bands are explicit pairs, so the lens needs no
    // inverse/dim modeling to judge them.
    let mut v = card_view(king_and_worker());
    v.term = (30, 140);
    v.sideline_width = 80;
    v.layout.focus = 5; // w1 is the chosen card
    let (agent_i, detail_i) = card_rows_for(&v, "w1");
    let (king_i, king_detail_i) = card_rows_for(&v, "king-a");
    v.hover_row = Some(king_i);
    let frame = v.compose();
    let cols = frame.cols as usize;
    let text_w = v.sideline_paint_w().saturating_sub(1);
    let offset = v.sideline_offset();
    let mut chosen_cells: Vec<crate::proto::Cell> = Vec::new();
    let mut hover_cells: Vec<crate::proto::Cell> = Vec::new();
    for display_i in [agent_i, detail_i] {
        let row = display_i - offset;
        chosen_cells.push(frame.cells[row * cols]);
        chosen_cells.push(frame.cells[row * cols + text_w - 1]);
    }
    for display_i in [king_i, king_detail_i] {
        let row = display_i - offset;
        hover_cells.push(frame.cells[row * cols]);
        hover_cells.push(frame.cells[row * cols + text_w - 1]);
    }
    for lens in crate::frame_html::THEMES {
        for cell in &chosen_cells {
            let ratio = crate::frame_html::contrast_ratio(cell, lens);
            assert_eq!(cell.flags & cell_flags::INVERSE, 0);
            assert!(ratio >= 4.5, "chosen band on {}: {ratio:.2}:1", lens.name);
        }
        for cell in &hover_cells {
            let ratio = crate::frame_html::contrast_ratio(cell, lens);
            assert_eq!(cell.flags & cell_flags::INVERSE, 0);
            assert!(ratio >= 3.0, "hover band on {}: {ratio:.2}:1", lens.name);
        }
    }
}

#[test]
fn unhighlighted_rows_read_on_a_light_terminal() {
    // x-cd1c D2: the colored texts on ordinary rows follow the terminal
    // palette. Built-in lane colors resolve to ANSI indexed colors (never
    // hard RGB), the lane fg stays visible on a light scheme, and the card's
    // dim gray line 2 is index 8 with no DIM flag: dim on dark, readable on
    // white.
    let lanes: [(Option<&str>, Option<&str>, Option<&str>); 7] = [
        (Some("codex"), None, None),
        (Some("agy"), None, None),
        (Some("opencode"), None, None),
        (None, None, Some("zai")),
        (None, None, Some("openai")),
        (None, None, Some("anthropic")),
        (None, None, Some("openrouter")),
    ];
    for lens in crate::frame_html::THEMES {
        for (harness, model, route) in lanes {
            let fg = crate::sideline_color::resolve_lane_color(harness, model, route, None)
                .expect("builtin lanes resolve");
            assert!(
                matches!(fg, Color::Indexed(_)),
                "lane {harness:?}/{route:?} must be ANSI named, got {fg:?}"
            );
            if lens.name == crate::frame_html::LIGHT.name {
                let ratio = lens_contrast(fg, Color::Default, lens);
                assert!(
                    ratio >= 2.5,
                    "lane {harness:?}/{route:?} on {lens:?}: {ratio:.2}:1"
                );
            }
        }
        // Card line 2: index 8, DIM flag gone.
        let gray = lens_cell(Color::Indexed(8), Color::Default);
        let ratio = crate::frame_html::contrast_ratio(&gray, lens);
        if lens.name == crate::frame_html::LIGHT.name {
            assert!(ratio >= 4.5, "line 2 gray on light: {ratio:.2}:1");
        } else {
            assert!(
                ratio >= 2.0,
                "line 2 gray still reads on {}: {ratio:.2}:1",
                lens.name
            );
        }
    }
    // Through the real painter: the detail row carries the palette gray
    // without the DIM flag.
    let mut v = card_view(king_and_worker());
    v.term = (30, 140);
    v.sideline_width = 80;
    let (_, detail_i) = card_rows_for(&v, "w1");
    let frame = v.compose();
    let cols = frame.cols as usize;
    let text_w = v.sideline_paint_w().saturating_sub(1);
    let row = detail_i - v.sideline_offset();
    let cells = &frame.cells[row * cols..row * cols + text_w];
    let painted = cells.iter().filter(|c| c.c != ' ').count();
    assert!(painted > 0, "detail row has text");
    for cell in cells.iter().filter(|c| c.c != ' ') {
        assert_eq!(cell.fg, Color::Indexed(8), "line 2 fg follows the palette");
        assert_eq!(cell.flags & cell_flags::DIM, 0, "no DIM on line 2");
    }
}
