//! The sideline card layout acceptance family: three-line cards behind
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
    [agent_i, detail_i, detail_i + 1]
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
    [agent_i, detail_i, detail_i + 1]
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

#[test]
fn card_mode_expands_each_agent_into_three_lines_without_padding_rows() {
    // AC3: each card is Agent, CardDetail, CardMetrics; no blank rows.
    let v = card_view(king_and_worker());
    let (rows, depths) = v.display_rows_with_depths();
    let names: Vec<String> = rows
        .iter()
        .map(|r| match r {
            DisplayRow::TableHead => "head",
            DisplayRow::Sel(s) if s.tab.is_none() => "band",
            DisplayRow::Agent(_) => "agent",
            DisplayRow::CardDetail(..) => "detail",
            DisplayRow::CardMetrics(..) => "metrics",
            _ => "other",
        })
        .map(String::from)
        .collect();
    assert!(
        names.len() >= 8
            && names[..8]
                == ["head", "band", "agent", "detail", "metrics", "agent", "detail", "metrics"],
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
fn card_frame_paints_identity_then_model_and_metrics_on_distinct_lines() {
    // Three lines: identity; model, lead, activity and age; context, compactions and cost. The status glyph stays on line 1.
    let mut agents = king_and_worker();
    agents[1].context_used_pct = Some(26);
    agents[1].compaction_count = Some(3);
    agents[1].session_cost_cents = Some(42);
    agents[1].started_at = Some(crate::digest_overlay::now_secs() - 10800);
    agents[1].node = Some("x-4310".into());
    agents[1].model = Some("gpt-6.1-sol".into());
    agents[0].model = Some("claude-opus-5-5".into());
    let mut v = card_view(agents);
    v.term = (30, 140);
    v.sideline_width = 80;
    let frame = v.compose();
    let text = frame_text(&frame);
    let (king_i, _) = card_rows_for(&v, "king-a");
    let (worker_i, _) = card_rows_for(&v, "w1");
    let cols = frame.cols as usize;
    let width = v.sideline_paint_w() - 1;
    let offset = v.sideline_offset();
    assert!(
        frame.cells[(king_i - offset) * cols..(king_i - offset) * cols + width]
            .iter()
            .all(|cell| cell.bg == Color::Default)
    );
    assert!(
        frame.cells[(worker_i - offset) * cols..(worker_i - offset) * cols + width]
            .iter()
            .all(|cell| cell.bg == v.theme.sel)
    );
    assert!(text.contains("x-4310"), "{text:?}");
    assert!(!text.contains("Work") && !text.contains(" up "), "{text:?}");
    let head = text.lines().next().unwrap_or_default();
    assert!(
        head.contains("node · PR") && !head.contains("last msg"),
        "the card head names the card's own cells: {head:?}"
    );
    v.layout.agents[1].context_used_pct = Some(129);
    let over_frame = v.compose();
    let over_window = frame_text(&over_frame);
    assert!(over_window.contains("▂▃▃▄ 129%"), "{over_window:?}");
    v.layout.agents[1].context_used_pct = None;
    let unmeasured = frame_text(&v.compose());
    assert!(unmeasured.contains("???? ? · ?c · ?"), "{unmeasured:?}");
    assert!(text.contains("w1"), "{text:?}");
    assert!(text.contains("#42"), "{text:?}");
    assert!(text.contains("claude/opus"), "{text:?}");
    assert!(text.contains("king-a"), "{text:?}");
    assert!(text.contains("one message"), "{text:?}");
    assert!(text.contains("26%"), "{text:?}");
    assert!(
        text.contains("▂▃▃▄ 26% · 3c · $0.42"),
        "the compact sparkline line matches its display contract: {text:?}"
    );
    assert!(text.contains("3c") && text.contains("$0.42"), "{text:?}");
    // The king's own card shows its crown scope, not a king name.
    assert!(text.contains("fno"), "{text:?}");
    // Line 2's segment join: harness/model, then the king handle.
    assert!(text.contains("codex/gpt-6.1-sol \u{b7} king-a"), "{text:?}");
}

#[test]
fn card_slug_drops_node_and_model_and_the_node_taps_open() {
    // The name loses the node and model the card shows in their own
    // columns; a name that was only `t-<node>-<model>` reads the node slug.
    let mut a = agent_row("t-x-5316-opus", 5, Some(AgentBadge::Working), false);
    a.node = Some("x-5316".into());
    a.model = Some("claude-opus-5-5".into());
    let card = |id: &str, slug: &str| crate::proto::BacklogCard {
        id: id.into(),
        slug: slug.into(),
        priority: "p2".into(),
        state: crate::proto::CardState::InFlight,
        pane_id: None,
        attach_id: None,
        where_hint: None,
        project: None,
        lane: None,
        plan_path: None,
        head: false,
    };
    assert_eq!(
        card_line::slug(&a, &[card("x-5316", "gc-sweep")]),
        "t-gc-sweep"
    );
    assert_eq!(
        card_line::slug(&a, &[]),
        "t-x-5316",
        "no slug keeps the node"
    );
    a.name = "t-x4fb5-glm".into();
    a.node = Some("x-4fb5".into());
    a.model = Some("glm-5.3-flash[1m]".into());
    assert_eq!(card_line::slug(&a, &[card("x-4fb5", "status")]), "t-status");
    assert_eq!(card_line::harness_model(&a), Some("glm-5.3-flash".into()));
    a.name = "t-cards-org-opus".into();
    a.model = Some("claude-opus-5-5".into());
    a.harness = Some("claude".into());
    assert_eq!(card_line::slug(&a, &[]), "t-cards-org");
    assert_eq!(card_line::harness_model(&a), Some("claude/opus".into()));
    // Identity fields share a right-aligned line and remain whole tap fields.
    a.context_used_pct = Some(28);
    assert_eq!(card_line::node_span(&a, 30), Some(24..30));
    assert_eq!(card_line::node_span(&a, 21), Some(15..21));
    assert_eq!(card_line::node_span(&a, 5), None);
    // The node is a tap target where it is painted.
    let mut agents = king_and_worker();
    agents[1].node = Some("x-4310".into());
    let mut v = card_view(agents);
    v.term = (30, 140);
    v.sideline_width = 80;
    let (agent_i, _) = card_rows_for(&v, "w1");
    let frame = v.compose();
    let row = agent_i - v.sideline_offset();
    let line = row_text(&frame, row, v.sideline_paint_w() - 1);
    assert!(line.contains("x-4310 · #42"), "adjacent identity: {line:?}");
    let col = line.chars().position(|c| c == 'x').expect("node painted") as u16 + 2;
    assert!(
        matches!(v.chrome_hit(row as u16, col), Some(ChromeHit::OpenNode(id)) if id == "x-4310"),
        "{line:?}"
    );
    assert!(
        !matches!(v.chrome_hit(row as u16, 3), Some(ChromeHit::OpenNode(_))),
        "the glyph still focuses the row"
    );
    let chars: Vec<char> = line.chars().collect();
    let pr_col = chars
        .windows(3)
        .position(|w| w[0] == '#' && w[1] == '4' && w[2] == '2')
        .expect("PR painted") as u16
        + 1;
    assert!(
        matches!(v.chrome_hit(row as u16, pr_col), Some(ChromeHit::OpenPr(url)) if url.ends_with("/pull/42")),
        "PR tap opens its own link: {line:?}"
    );
    let node_span =
        card_line::node_span(&v.layout.agents[1], v.sideline_paint_w() - 1).expect("node range");
    let pr_span =
        card_line::pr_span(&v.layout.agents[1], v.sideline_paint_w() - 1).expect("PR range");
    assert_eq!(
        node_span.end + 3,
        pr_span.start,
        "identity fields are adjacent"
    );
    let cols = frame.cols as usize;
    let glyph = status_glyph(agent_lattice_state(&v.layout.agents[1]));
    let glyph_at = chars
        .iter()
        .position(|c| *c == glyph)
        .expect("status glyph");
    assert_eq!(
        frame.cells[row * cols + pr_span.start].fg,
        frame.cells[row * cols + glyph_at].fg,
        "PR keeps the status glyph color"
    );
    assert_ne!(
        frame.cells[row * cols + node_span.start].flags & cell_flags::INVERSE,
        0,
        "node uses inverse contrast"
    );
}

#[test]
fn card_detail_click_routes_to_the_agent_above() {
    // AC5: row_action on a CardDetail equals row_action one row up.
    let v = card_view(king_and_worker());
    let rows = v.painted_rows();
    let detail_i = rows
        .iter()
        .position(|r| matches!(r, DisplayRow::CardDetail(..)))
        .expect("a card detail row exists");
    let agent_action = v.row_action(detail_i - 1);
    let detail_action = v.row_action(detail_i);
    let (Some(ChromeHit::Cmds(a)), Some(ChromeHit::Cmds(b))) = (agent_action, detail_action) else {
        panic!("both rows resolve to Cmds");
    };
    assert_eq!(a, b, "the click routes to the Agent's commands");
}

#[test]
fn hovering_line_two_or_selecting_line_one_bands_both_card_lines() {
    for select in [false, true] {
        let mut v = card_view(king_and_worker());
        v.term = (30, 140);
        v.sideline_width = 80;
        let (agent_i, detail_i) = card_rows_for(&v, "w1");
        if select {
            v.selector = Some(agent_i);
        } else {
            v.hover_row = Some(detail_i);
        }
        let frame = v.compose();
        let width = v.sideline_paint_w().saturating_sub(1);
        let line = "#".repeat(width);
        assert_eq!(
            card_highlight_snapshot(&v, &frame, agent_i, detail_i),
            format!("{line}\n{line}\n{line}"),
            "select={select}: the snapshot covers every cell and column boundary on both lines"
        );
    }
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
    // hover pair - never INVERSE. The status and word columns of line 1
    // keep the row's lane accent on the band (the operator's color ruling).
    let mut v = card_view(king_and_worker());
    v.term = (30, 140);
    v.sideline_width = 80;
    let (agent_i, detail_i) = card_rows_for(&v, "w1");
    v.hover_row = Some(agent_i);
    let frame = v.compose();
    let cols = frame.cols as usize;
    let text_w = v.sideline_paint_w().saturating_sub(1);
    let offset = v.sideline_offset();
    let (band_fg, band_bg, _) = crate::theme::band_style(&v.theme);
    let rects = v.worker_column_rects(text_w as u16);
    let in_col =
        |j: usize, c: usize| j >= rects[c].x as usize && j < (rects[c].x + rects[c].width) as usize;
    let rows = v.painted_rows();
    let pr_span = match rows.get(agent_i) {
        Some(DisplayRow::Agent(a)) => card_line::pr_span(a, text_w),
        _ => None,
    };
    for display_i in [agent_i, detail_i, detail_i + 1] {
        let row = display_i - offset;
        for (j, cell) in frame.cells[row * cols..row * cols + text_w]
            .iter()
            .enumerate()
        {
            assert_eq!(cell.bg, band_bg, "one background everywhere");
            let keeps_identity_color =
                display_i == agent_i && pr_span.as_ref().is_some_and(|span| span.contains(&j));
            if !(display_i == agent_i && (in_col(j, 0) || in_col(j, 2))) && !keeps_identity_color {
                assert_eq!(cell.fg, band_fg, "accent band text");
            }
            assert_eq!(
                cell.flags & (cell_flags::INVERSE | cell_flags::DIM),
                0,
                "no INVERSE and no DIM inside the band"
            );
        }
    }
}

#[test]
fn a_foreign_cwd_shows_inline_in_parens_and_never_adds_a_row() {
    // d-36438ea4: a member with a different project or worktree path shows
    // it inline in parens after the slug, only when it fits whole; the dim
    // Sub line is gone. At a width that fits, the card's line 1 reads
    // `slug (cwd)`; at a width that does not, the parens drop and the row
    // is still one line.
    let mut agents = king_and_worker();
    agents[1].cwd_base = Some("elsewhere".into());
    let mut v = card_view(agents);
    v.term = (30, 140);
    v.sideline_width = 80;
    let frame_text = crate::vt::frame_text(&v.compose());
    assert!(
        frame_text.contains("w1 (elsewhere)"),
        "the cwd rides inline after the slug: {frame_text}"
    );
    // Each Agent owns its detail and metrics rows without spacer rows.
    let rows = v.display_rows();
    for (i, r) in rows.iter().enumerate() {
        if matches!(r, DisplayRow::Agent(_)) {
            assert!(
                matches!(rows.get(i + 1), Some(DisplayRow::CardDetail(_))),
                "agent row {i} lost its detail line"
            );
            assert!(
                matches!(rows.get(i + 2), Some(DisplayRow::CardMetrics(_))),
                "agent row {i} lost its metrics line"
            );
        }
    }
}

#[test]
fn chosen_card_paints_accent_across_both_lines() {
    // x-b5b8: the focused card wears the same surface band as selection -
    // accent text on the sel surface, never a full brand fill. The lane
    // accent survives on the status and word columns of line 1.
    let mut v = card_view(king_and_worker());
    v.term = (30, 140);
    v.sideline_width = 80;
    v.layout.focus = 5;
    let (agent_i, detail_i) = card_rows_for(&v, "w1");
    let frame = v.compose();
    let (band_fg, band_bg, _) = crate::theme::band_style(&v.theme);
    let cols = frame.cols as usize;
    let text_w = v.sideline_paint_w().saturating_sub(1);
    let offset = v.sideline_offset();
    let rects = v.worker_column_rects(text_w as u16);
    let in_col =
        |j: usize, c: usize| j >= rects[c].x as usize && j < (rects[c].x + rects[c].width) as usize;
    let rows = v.painted_rows();
    let pr_span = match rows.get(agent_i) {
        Some(DisplayRow::Agent(a)) => card_line::pr_span(a, text_w),
        _ => None,
    };
    for display_i in [agent_i, detail_i, detail_i + 1] {
        let row = display_i - offset;
        for (j, cell) in frame.cells[row * cols..row * cols + text_w]
            .iter()
            .enumerate()
        {
            assert_eq!(cell.bg, band_bg, "the surface band fills the card line");
            let keeps_identity_color =
                display_i == agent_i && pr_span.as_ref().is_some_and(|span| span.contains(&j));
            if !(display_i == agent_i && (in_col(j, 0) || in_col(j, 2))) && !keeps_identity_color {
                assert_eq!(cell.fg, band_fg, "the band's accent text everywhere");
            }
            assert_eq!(
                cell.flags & (cell_flags::INVERSE | cell_flags::DIM),
                0,
                "no INVERSE and no DIM inside the band"
            );
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
    let (_, band_bg, _) = crate::theme::band_style(&v.theme);
    let cols = frame.cols as usize;
    let text_w = v.sideline_paint_w().saturating_sub(1);
    let offset = v.sideline_offset();
    for display_i in [agent_i, detail_i, detail_i + 1] {
        let row = display_i - offset;
        for cell in &frame.cells[row * cols..row * cols + text_w] {
            assert_eq!(cell.bg, band_bg, "the band wins on hover");
        }
    }
}

#[test]
fn a_named_theme_bands_on_its_surface_and_never_paints_a_signal_across_a_row() {
    // The named-theme band pair is stamp-on-sel: neutral text on the sel
    // surface. No signal color fills a banded row's text - the highlight
    // pass alone restores a lane accent, on the glyph and state word
    // only. (A red brand band across every cell was the original bug;
    // the monochrome ruling made brand equal to text, so the old
    // fg != brand guard would fire on every text cell and retired with
    // the failure mode it guarded.)
    let mut v = card_view(king_and_worker());
    v.theme = crate::theme::Theme::from_name("footnote-superscript").0;
    v.term = (30, 140);
    v.sideline_width = 80;
    let (agent_i, detail_i) = card_rows_for(&v, "w1");
    v.hover_row = Some(agent_i);
    let frame = v.compose();
    let (band_fg, _, _) = crate::theme::band_style(&v.theme);
    assert_eq!(band_fg, v.theme.stamp, "the band text is the neutral stamp");
    let cols = frame.cols as usize;
    let text_w = v.sideline_paint_w().saturating_sub(1);
    let offset = v.sideline_offset();
    for display_i in [agent_i, detail_i, detail_i + 1] {
        let row = display_i - offset;
        for cell in &frame.cells[row * cols..row * cols + text_w] {
            assert_eq!(cell.bg, v.theme.sel, "the band is the sel surface");
            assert_ne!(cell.fg, v.theme.needs_you, "no signal fills a banded row");
        }
    }
}

#[test]
fn card_pr_and_age_activity_keep_distinct_right_side_fields() {
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
    let age_at = detail_cells
        .windows(3)
        .position(|run| [run[0].c, run[1].c, run[2].c] == ['4', '2', 's'])
        .expect("age is visible");

    assert_eq!(pr_end, width - 1, "PR is flush with the panel edge");
    assert!(age_at < width - 6, "age precedes the activity summary");
    let pr_tail = agent_cells[width - 6..]
        .iter()
        .map(|cell| cell.c)
        .collect::<String>();
    let detail = detail_cells.iter().map(|cell| cell.c).collect::<String>();
    assert_eq!(pr_tail, "   #42");
    assert!(
        detail.contains("42s") && detail.ends_with("message"),
        "{detail:?}"
    );
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
    let rects = v.worker_column_rects(width as u16);
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
fn list_mode_keeps_identity_and_unknown_measurements_visible() {
    let mut agents = king_and_worker();
    agents[0].last_activity_age_s = Some(42);
    agents[1].last_activity_age_s = Some(42);
    let mut v = card_view(agents);
    v.sideline_layout = sideline_color::SidelineLayout::List;
    v.term = (30, 140);
    v.sideline_width = 80;
    let frame = v.compose();

    let text = frame_text(&frame);
    assert!(text.contains("king-a"), "{text:?}");
    assert!(text.contains("w1"), "{text:?}");
    assert!(text.contains("ctx"), "{text:?}");
    assert!(text.contains("up"), "{text:?}");
    for name in ["king-a", "w1"] {
        assert!(
            text.lines()
                .any(|line| line.contains(name) && line.contains("●Work")),
            "Working row {name} keeps its still spinner beside the state word: {text:?}"
        );
    }
    let now = crate::digest_overlay::now_secs();
    assert_eq!(row_meter::ctx_cell(None), "-");
    assert_eq!(row_meter::ctx_cell(Some(0)), "    0%");
    assert_eq!(row_meter::ctx_cell(Some(100)), "███ 100%");
    assert_eq!(row_meter::ctx_cell(Some(129)), "███ 129%");
    assert_eq!(row_meter::ctx_bar(None), format!("{:<13}", "-"));
    assert_eq!(row_meter::ctx_bar(Some(0)), "         0%  ");
    assert_eq!(row_meter::ctx_bar(Some(28)), "██▎      28% ");
    assert_eq!(row_meter::up_cell(None, now), "-");
    assert_eq!(row_meter::up_cell(Some(now + 1), now), "0s");
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
    for name in [
        "footnote-paper",
        "terminal",
        "catppuccin",
        "tokyo-night",
        "gruvbox",
    ] {
        let (t, warn) = crate::theme::Theme::from_name(name);
        assert!(warn.is_none(), "{name} must ship without a warning");
        themes.push(t);
    }
    themes
}

#[test]
fn band_pairs_hold_luminance_contrast_on_dark_and_light() {
    // x-cd1c D1: every band is an explicit fg+bg pair with no INVERSE and no
    // DIM. The one band (selection, hover, and focus share it) clears the 3:1
    // transient-affordance floor. `terminal` resolves its Indexed legs
    // against each lens theme's real palette; named themes paint fixed RGB
    // pairs, which the named-theme lens themes judge directly.
    for t in shipped_mux_themes() {
        let (fg, bg, flags) = crate::theme::band_style(&t);
        assert_eq!(flags, 0, "no INVERSE and no DIM inside the band");
        assert!(fg != Color::Default, "band fg must be explicit");
        assert!(bg != Color::Default, "band bg must be explicit");
        for lens in crate::frame_html::THEMES {
            let ratio = lens_contrast(fg, bg, lens);
            assert!(
                ratio >= 3.0,
                "{} band on {}: {ratio:.2}:1 (floor 3.0)",
                t.name,
                lens.name
            );
        }
    }
}

#[test]
fn the_dark_anchor_is_the_luminance_pick_on_every_accent() {
    // x-cd1c D1: dark text on light accents is not taste, it is measurement.
    // On a light terminal the default fg loses to the dark anchor on every
    // shipped accent, which is the pick the band records.
    let mut accents = vec![crate::theme::Theme::default_theme().brand];
    for name in ["catppuccin", "tokyo-night", "gruvbox"] {
        let (t, _) = crate::theme::Theme::from_name(name);
        accents.push(t.brand);
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
    for display_i in [agent_i, detail_i, detail_i + 1] {
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
            // x-b5b8: chosen and hover share the ONE surface band, so the
            // chosen card clears the same 3:1 transient-affordance floor
            // (its sampled status cell also carries the lane accent now).
            assert!(ratio >= 3.0, "chosen band on {}: {ratio:.2}:1", lens.name);
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
