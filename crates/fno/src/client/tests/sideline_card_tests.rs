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

fn lead_and_worker() -> Vec<AgentRow> {
    let mut lead = agent_row("lead-a", 4, Some(AgentBadge::Working), false);
    lead.harness = Some("claude".into());
    lead.crown_level = Some(2);
    lead.crown_scope = Some("fno".into());
    lead.harness_session_id = Some("sess-lead".into());
    let mut w1 = agent_row("w1", 5, Some(AgentBadge::Working), false);
    w1.harness = Some("claude".into());
    w1.pr = Some(42);
    w1.tail = Some("**one message**".into());
    w1.lineage_kind = Some("child".into());
    w1.spawned_by_session = Some("sess-lead".into());
    w1.harness_session_id = Some("sess-w1".into());
    vec![lead, w1]
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
            let row = display_i - offset + 1; // the strip row owns row 0
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
            let row = display_i - offset + 1; // the strip row owns row 0
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
    let v = card_view(lead_and_worker());
    let (rows, depths) = v.display_rows_with_depths();
    let names: Vec<String> = rows
        .iter()
        .map(|r| match r {
            DisplayRow::TableHead => "head",
            DisplayRow::Sel(s) if s.tab.is_none() => "band",
            DisplayRow::Agent(_) => "agent",
            DisplayRow::CardDetail(..) => "detail",
            DisplayRow::CardMetrics(..) => "metrics",
            DisplayRow::CardRule => "rule",
            _ => "other",
        })
        .map(String::from)
        .collect();
    assert!(
        names.len() >= 9
            && names[..9]
                == [
                    "head", "band", "agent", "detail", "metrics", "rule", "agent", "detail",
                    "metrics"
                ],
        "{names:?}"
    );
    assert!(
        depths.iter().all(|d| *d == 0),
        "every depth is 0: {depths:?}"
    );
}

#[test]
fn card_age_sort_orders_workers_inside_a_lead_group() {
    // The user report: sorted by age, a lead's workers read
    // 12m, 12m, 10m, 41m, 23s in the card view. The card path now runs the
    // same run sort the extended table uses, workers order inside their
    // lead's group, leads keep their group order.
    let ages = [("w-old", 720u64), ("w-new", 60), ("w-mid", 600)];
    let mut agents = Vec::new();
    let mut lead = agent_row("lead-a", 4, Some(AgentBadge::Working), false);
    lead.crown_level = Some(2);
    lead.crown_scope = Some("fno".into());
    lead.harness_session_id = Some("sess-lead".into());
    lead.last_activity_age_s = Some(10);
    agents.push(lead);
    for (name, age) in ages {
        let mut w = agent_row(name, 5, Some(AgentBadge::Working), false);
        w.lineage_kind = Some("child".into());
        w.spawned_by_session = Some("sess-lead".into());
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
                DisplayRow::Agent(a) if a.name != "lead-a" => Some(a.name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            got,
            vec!["w-new", "w-mid", "w-old"],
            "workers order by the sort key inside the lead group at {density:?}"
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
    // Three lines: identity; model, lead and ages; context, compactions, cost, tokens and message. The status glyph stays on line 1.
    let mut agents = lead_and_worker();
    agents[1].context_used_pct = Some(26);
    agents[1].compaction_count = Some(3);
    agents[1].session_cost_cents = Some(42);
    agents[1].session_tokens = Some(12_345);
    agents[1].started_at = Some(crate::digest_overlay::now_secs() - 10800);
    agents[1].last_activity_age_s = Some(36);
    agents[1].node = Some("x-4310".into());
    agents[1].model = Some("gpt-6.1-sol".into());
    // The activity fixture: four intervals, the last idle, so the ramp
    // scales to the card's own max (8) and grades one warn, one error.
    agents[1].activity = Some(vec![(2, 0), (4, 1), (8, 4), (0, 0)]);
    agents[0].model = Some("claude-opus-5-5".into());
    agents[0].crown_title = Some("Lead of mux".into());
    let mut v = card_view(agents);
    v.term = (30, 140);
    v.sideline_width = 80;
    let frame = v.compose();
    let text = frame_text(&frame);
    let (lead_i, _) = card_rows_for(&v, "lead-a");
    let (worker_i, _) = card_rows_for(&v, "w1");
    let cols = frame.cols as usize;
    let width = v.sideline_paint_w() - 1;
    let offset = v.sideline_offset();
    assert!(
        frame.cells[(lead_i - offset) * cols..(lead_i - offset) * cols + width]
            .iter()
            .all(|cell| cell.bg == Color::Default)
    );
    assert!(
        frame.cells[(worker_i - offset) * cols..(worker_i - offset) * cols + width]
            .iter()
            .all(|cell| cell.bg == Color::Default),
        "no zebra: a resting card keeps the plain ground"
    );
    assert!(text.contains("x-4310"), "{text:?}");
    assert!(!text.contains("Work") && !text.contains(" up "), "{text:?}");
    assert!(text.contains("3h · 36s"), "{text:?}");
    let head = text.lines().nth(1).unwrap_or_default(); // under the strip row
    assert!(
        head.contains("node · PR") && !head.contains("last msg"),
        "the card head names the card's own cells: {head:?}"
    );
    v.layout.agents[1].context_used_pct = Some(129);
    let over_frame = v.compose();
    let over_window = frame_text(&over_frame);
    assert!(over_window.contains("▁▁▁▁▃▅█▁ · 129%"), "{over_window:?}");
    // The ramp math: heights scale to the card's own max over the served
    // intervals, the max reads the full block, and each cell's failed share
    // grades its color kind (0 ok, 1 warn, 2 error).
    let cell = card_line::activity_cell(&v.layout.agents[1], card_line::CardGraph::Activity)
        .expect("served intervals draw");
    assert_eq!(
        cell.text,
        "\u{2581}\u{2581}\u{2581}\u{2581}\u{2583}\u{2585}\u{2588}\u{2581}"
    );
    assert_eq!(cell.kinds, vec![0, 0, 0, 0, 0, 1, 2, 0]);
    // The slot stays blank before two intervals, whatever they hold.
    v.layout.agents[1].activity = Some(vec![(3, 0)]);
    assert!(
        card_line::activity_cell(&v.layout.agents[1], card_line::CardGraph::Activity).is_none()
    );
    v.layout.agents[1].activity = Some(vec![(0, 0)]);
    assert!(
        card_line::activity_cell(&v.layout.agents[1], card_line::CardGraph::Activity).is_none()
    );
    v.layout.agents[1].activity = Some(vec![]);
    assert!(
        card_line::activity_cell(&v.layout.agents[1], card_line::CardGraph::Activity).is_none()
    );
    // Context mode: the steady fill bar of the context percent, bare of
    // the number (the number sits beside it), for the whole session.
    v.layout.agents[1].activity = Some(vec![(3, 0)]);
    v.layout.agents[1].context_used_pct = Some(26);
    let bar = card_line::activity_cell(&v.layout.agents[1], card_line::CardGraph::Context)
        .expect("the context bar draws from the first reading");
    assert_eq!(
        bar.text, "\u{2588}\u{258d}   ",
        "26% fills 11 of 40 eighths"
    );
    assert_eq!(
        bar.kinds,
        vec![0, 0, 0, 0, 0],
        "the context bar never grades"
    );
    // The painter wears each ramp cell in its kind's theme color: the
    // fixture's majority-failed max cell paints error, the clean pads ok.
    let theme = v.theme;
    let mut saw_ok = false;
    let mut saw_error = false;
    for c in &frame.cells {
        if c.c == '\u{2588}' && c.fg == theme.error {
            saw_error = true;
        }
        if c.c == '\u{2581}' && c.fg == theme.ok {
            saw_ok = true;
        }
    }
    assert!(saw_error, "the majority-failed cell wears the error color");
    assert!(saw_ok, "the clean cell wears the ok color");
    v.layout.agents[1].context_used_pct = None;
    v.layout.agents[1].activity = None;
    v.layout.agents[1].compaction_count = None;
    v.layout.agents[1].session_cost_cents = None;
    v.layout.agents[1].session_tokens = None;
    // The pulse contract is a fresh row's: past 10s a Loading field holds a
    // static dash (row_meter's own tests pin the boundary).
    v.layout.agents[1].started_at = Some(crate::digest_overlay::now_secs());
    let unmeasured = frame_text(&v.compose());
    assert!(
        unmeasured.contains("░░░░ · ░░░ · ░░░░░░░░"),
        "a claude card whose fold has not landed pulses every field: {unmeasured:?}"
    );
    // Past 10s the fold-less row gives up the pulse: static dashes at the
    // fields' own widths, so the line never reflows.
    v.layout.agents[1].started_at = Some(crate::digest_overlay::now_secs() - 11);
    let gave_up = frame_text(&v.compose());
    assert!(
        gave_up.contains("-    · -   · -"),
        "a row past 10s holds static dashes: {gave_up:?}"
    );
    assert!(text.contains("w1"), "{text:?}");
    assert!(text.contains("#42"), "{text:?}");
    assert!(text.contains("opus · Lead of mux"), "{text:?}");
    assert!(text.contains("lead-a"), "{text:?}");
    assert!(text.contains("one message"), "{text:?}");
    assert!(text.contains("26%"), "{text:?}");
    assert!(
        text.contains("▁▁▁▁▃▅█▁ · 26% · 3c · 12.3k tok · one message"),
        "the compact metrics line matches its display contract: {text:?}"
    );
    assert!(text.contains("3c") && text.contains("~$0.42"), "{text:?}");
    // A worker names its lead, and a teamed row names its role.
    assert!(text.contains("gpt-6.1-sol · lead-a"), "{text:?}");
    // Classification contract, beyond the w1 paint above: a bare pane (no
    // harness) has nothing that could ever land, so every field hides; a
    // teamed lead never prices; a claude row's unserved fields are loading
    // skeletons, never `?`.
    let hidden = |c: &card_line::MetricCell| matches!(c, card_line::MetricCell::Hidden);
    let mut bare = agent_row("w9", 6, Some(AgentBadge::Working), false);
    assert!(
        card_line::metric_cells(&bare, card_line::CardGraph::Activity)
            .iter()
            .all(hidden)
    );
    bare.harness = Some("claude".into());
    bare.harness_session_id = Some("sess-w9".into());
    bare.crown_level = Some(2);
    bare.context_used_pct = Some(26);
    bare.session_tokens = Some(999);
    bare.session_cost_cents = Some(77);
    let cells = card_line::metric_cells(&bare, card_line::CardGraph::Activity);
    assert!(
        matches!(cells[0], card_line::MetricCell::Hidden),
        "activity stays blank until two intervals land, it never fakes a line"
    );
    assert!(matches!(&cells[3], card_line::MetricCell::Value(v) if v == "999 tok"));
    // Cost left the metrics line (it rides line 2, served-only): a crowned
    // lead's session_cost_cents never reach this line at all.
    // A codex row keeps the populated-paint contract off its unreportable
    // fields: context and compactions hide even when the wire carries them,
    // while its tokens land and its activity stays blank until two
    // intervals exist.
    let mut cx = agent_row("w10", 7, Some(AgentBadge::Working), false);
    cx.harness = Some("codex".into());
    cx.harness_session_id = Some("sess-w10".into());
    cx.context_used_pct = Some(40);
    cx.session_tokens = Some(500);
    let cells = card_line::metric_cells(&cx, card_line::CardGraph::Activity);
    assert!(
        hidden(&cells[1]) && hidden(&cells[2]),
        "codex never reports context or compactions"
    );
    assert!(hidden(&cells[0]));
    assert!(matches!(&cells[3], card_line::MetricCell::Value(v) if v == "500 tok"));
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
        link: None,
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
    let mut agents = lead_and_worker();
    agents[1].node = Some("x-4310".into());
    let mut v = card_view(agents);
    v.term = (30, 140);
    v.sideline_width = 80;
    let (agent_i, _) = card_rows_for(&v, "w1");
    let frame = v.compose();
    let row = agent_i - v.sideline_offset() + 1; // the strip row owns row 0
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
        frame.cells[row * cols + node_span.start].fg,
        frame.cells[row * cols + glyph_at].fg,
        "node ID matches the animated status glyph"
    );
    assert_eq!(
        frame.cells[row * cols + node_span.start].flags & cell_flags::INVERSE,
        0,
        "node uses the status color directly"
    );
    assert_eq!(
        frame.cells[row * cols + pr_span.start].fg,
        v.theme.brand,
        "PR number uses the theme's brand accent, kept distinct from the lane signal"
    );
    assert_ne!(
        frame.cells[row * cols + pr_span.start].fg,
        frame.cells[row * cols + glyph_at].fg,
        "PR number is visually separate from the lane signal"
    );
}

#[test]
fn card_detail_click_routes_to_the_agent_above() {
    // AC5: row_action on a CardDetail equals row_action one row up.
    let v = card_view(lead_and_worker());
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
        let mut v = card_view(lead_and_worker());
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
    let mut hover = card_view(lead_and_worker());
    hover.term = (30, 140);
    hover.sideline_width = 80;
    let (agent_i, detail_i) = card_rows_for(&hover, "w1");
    hover.hover_row = Some(detail_i);
    let hover_frame = hover.compose();
    let hover_cells = card_pair_cells(&hover, &hover_frame, agent_i, detail_i);

    let mut selected = card_view(lead_and_worker());
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
    let mut v = card_view(lead_and_worker());
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
    let (node_span, pr_span) = match rows.get(agent_i) {
        Some(DisplayRow::Agent(a)) => {
            let spans = card_line::identity_spans(a, text_w);
            (spans.node, spans.pr)
        }
        _ => (None, None),
    };
    for display_i in [agent_i, detail_i, detail_i + 1] {
        let row = display_i - offset + 1; // the strip row owns row 0
        for (j, cell) in frame.cells[row * cols..row * cols + text_w]
            .iter()
            .enumerate()
        {
            assert_eq!(cell.bg, band_bg, "one background everywhere");
            let keeps_identity_color = display_i == agent_i
                && (node_span.as_ref().is_some_and(|span| span.contains(&j))
                    || pr_span.as_ref().is_some_and(|span| span.contains(&j)));
            if !(display_i == agent_i && in_col(j, 0)) && !keeps_identity_color {
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
    let mut agents = lead_and_worker();
    agents[1].cwd_base = Some("elsewhere".into());
    let mut v = card_view(agents);
    v.term = (30, 140);
    v.sideline_width = 80;
    let frame_text = crate::vt::frame_text(&v.compose());
    assert!(
        frame_text.contains("w1 (elsewhere)"),
        "the cwd rides inline after the slug: {frame_text}"
    );
    // A squad-less row whose cwd base repeats its own node id prints no
    // parenthetical (the user's noise case); one from an arbitrary directory
    // keeps the context (the wire contract's purpose).
    let orphan_node = {
        let mut a = agent_row("orphan-node", 9, Some(AgentBadge::Working), false);
        a.squad = None;
        a.node = Some("x-fcb4".into());
        a.cwd_base = Some("x-fcb4".into());
        a
    };
    let orphan_dir = {
        let mut a = agent_row("orphan-dir", 10, Some(AgentBadge::Working), false);
        a.squad = None;
        a.cwd_base = Some("footnote".into());
        a
    };
    let mut v2 = card_view(vec![lead_and_worker()[0].clone(), orphan_node, orphan_dir]);
    v2.term = (30, 140);
    v2.sideline_width = 80;
    v2.expand_pull_sections();
    let text2 = crate::vt::frame_text(&v2.compose());
    assert!(
        !text2.contains("orphan-node (x-fcb4)"),
        "a cwd base that repeats the node id prints nothing: {text2}"
    );
    assert!(
        text2.contains("orphan-dir (footnote)"),
        "a real directory base keeps its context: {text2}"
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
fn a_teamed_rows_node_worktree_cwd_never_tags_the_name_with_its_node() {
    // The x-54bb leak: a node-backed codex thread inherits its spawner's
    // workspace, so its own node worktree reads as a FOREIGN cwd and the
    // card rendered `t-<node> (x-<node>)` - the node twice on one line.
    // The parenthetical drops whenever the cwd base repeats the row's node,
    // teamed or not; a genuinely foreign directory still tags.
    let mut leak = agent_row("t-x64d4", 11, Some(AgentBadge::Working), false);
    leak.harness = Some("codex".into());
    leak.node = Some("x-64d4".into());
    leak.cwd_base = Some("x-64d4".into());
    let mut foreign = agent_row("t-else", 12, Some(AgentBadge::Working), false);
    foreign.harness = Some("codex".into());
    foreign.cwd_base = Some("elsewhere".into());
    let mut v = card_view(vec![leak, foreign]);
    v.term = (30, 140);
    v.sideline_width = 80;
    let text = crate::vt::frame_text(&v.compose());
    assert!(
        !text.contains("(x-64d4)"),
        "a cwd base that repeats the node id tags no teamed name: {text}"
    );
    assert!(
        text.contains("t-else (elsewhere)"),
        "a real foreign directory keeps its context: {text}"
    );
}

#[test]
fn chosen_card_fills_all_three_lines_with_the_accent_and_a_left_bar() {
    // The operator's 2026-10-04 ruling: the focused card fills all 3 lines
    // with the theme accent plus a left bar, unmistakable against resting
    // neighbors; the old surface-band selection is retired for cards. The
    // fill is monochrome: the node and PR spans read in the fill's base tone
    // too, because brand-on-brand text would vanish (the composed contrast
    // test pins the fill at 3:1 on both lens themes).
    let mut v = card_view(lead_and_worker());
    v.term = (30, 140);
    v.sideline_width = 80;
    v.layout.focus = 5;
    let (agent_i, detail_i) = card_rows_for(&v, "w1");
    let frame = v.compose();
    let (fill_fg, fill_bg, _) = crate::theme::chosen_card_style(&v.theme);
    let cols = frame.cols as usize;
    let text_w = v.sideline_paint_w().saturating_sub(1);
    let offset = v.sideline_offset();
    for display_i in [agent_i, detail_i, detail_i + 1] {
        let row = display_i - offset + 1; // the strip row owns row 0
        for cell in &frame.cells[row * cols..row * cols + text_w] {
            assert_eq!(cell.bg, fill_bg, "the accent fill covers the card line");
            assert_eq!(cell.fg, fill_fg, "the fill's base text everywhere");
            assert_eq!(
                cell.flags & (cell_flags::INVERSE | cell_flags::DIM),
                0,
                "no INVERSE and no DIM inside the fill"
            );
        }
        assert_eq!(
            frame.cells[row * cols].c,
            '\u{258e}',
            "the left bar leads the card line"
        );
    }
    // A resting neighbor keeps the plain ground: no zebra, no fill.
    let (lead_agent_i, lead_detail_i) = card_rows_for(&v, "lead-a");
    for display_i in [lead_agent_i, lead_detail_i, lead_detail_i + 1] {
        let row = display_i - offset + 1;
        for cell in &frame.cells[row * cols..row * cols + text_w] {
            assert_eq!(cell.bg, Color::Default, "no zebra and no fill next door");
        }
    }
}

#[test]
fn hovering_the_chosen_card_keeps_the_chosen_color_on_all_three_lines() {
    let mut v = card_view(lead_and_worker());
    v.term = (30, 140);
    v.sideline_width = 80;
    v.layout.focus = 5;
    let (agent_i, detail_i) = card_rows_for(&v, "w1");
    v.hover_row = Some(detail_i);
    let frame = v.compose();
    let (_, fill_bg, _) = crate::theme::chosen_card_style(&v.theme);
    let cols = frame.cols as usize;
    let text_w = v.sideline_paint_w().saturating_sub(1);
    let offset = v.sideline_offset();
    for display_i in [agent_i, detail_i, detail_i + 1] {
        let row = display_i - offset + 1; // the strip row owns row 0
        for cell in &frame.cells[row * cols..row * cols + text_w] {
            assert_eq!(cell.bg, fill_bg, "the chosen fill wins on hover");
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
    let mut v = card_view(lead_and_worker());
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
        let row = display_i - offset + 1; // the strip row owns row 0
        for cell in &frame.cells[row * cols..row * cols + text_w] {
            assert_eq!(cell.bg, v.theme.sel, "the band is the sel surface");
            assert_ne!(cell.fg, v.theme.needs_you, "no signal fills a banded row");
        }
    }
}

#[test]
fn card_pr_and_created_activity_ages_keep_right_side_fields() {
    let mut agents = lead_and_worker();
    agents[1].last_activity_age_s = Some(42);
    let mut v = card_view(agents);
    v.term = (30, 140);
    v.sideline_width = 80;
    let (agent_i, detail_i) = card_rows_for(&v, "w1");
    let frame = v.compose();
    let offset = v.sideline_offset();
    let cols = frame.cols as usize;
    let width = v.sideline_paint_w() - 1;
    let agent_row = agent_i - offset + 1; // the strip row owns row 0
    let detail_row = detail_i - offset + 1;
    let agent_cells = &frame.cells[agent_row * cols..agent_row * cols + width];
    let detail_cells = &frame.cells[detail_row * cols..detail_row * cols + width];
    let pr_end = agent_cells
        .windows(3)
        .rposition(|run| [run[0].c, run[1].c, run[2].c] == ['#', '4', '2'])
        .expect("PR is visible")
        + 2;
    detail_cells
        .windows(3)
        .position(|run| [run[0].c, run[1].c, run[2].c] == ['4', '2', 's'])
        .expect("age is visible");

    assert_eq!(pr_end, width - 1, "PR is flush with the panel edge");
    let pr_tail = agent_cells[width - 6..]
        .iter()
        .map(|cell| cell.c)
        .collect::<String>();
    let detail = detail_cells.iter().map(|cell| cell.c).collect::<String>();
    assert_eq!(pr_tail, "   #42");
    assert!(detail.ends_with("– · 42s"), "{detail:?}");
}

#[test]
fn regular_card_snapshot_shows_a_pr_when_it_fits() {
    let mut agents = lead_and_worker();
    agents[1].last_activity_age_s = Some(42);
    let mut v = card_view(agents);
    set_density(&mut v, Density::Regular);
    v.term = (30, 140);
    let (agent_i, _) = card_rows_for(&v, "w1");
    let frame = v.compose();
    let row = agent_i - v.sideline_offset() + 1; // the strip row owns row 0
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
    let mut agents = lead_and_worker();
    agents[1].pr = Some(1_234_567);
    let mut v = card_view(agents);
    set_density(&mut v, Density::Regular);
    v.term = (30, 140);
    let (agent_i, _) = card_rows_for(&v, "w1");
    let frame = v.compose();
    let row = agent_i - v.sideline_offset() + 1; // the strip row owns row 0
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
    let mut agents = lead_and_worker();
    agents[0].last_activity_age_s = Some(42);
    agents[1].last_activity_age_s = Some(42);
    let mut v = card_view(agents);
    v.sideline_layout = sideline_color::SidelineLayout::List;
    v.term = (30, 140);
    v.sideline_width = 80;
    let frame = v.compose();

    let text = frame_text(&frame);
    assert!(text.contains("lead-a"), "{text:?}");
    assert!(text.contains("w1"), "{text:?}");
    assert!(text.contains("ctx"), "{text:?}");
    assert!(text.contains("up"), "{text:?}");
    for name in ["lead-a", "w1"] {
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
fn lead_label_walk_stops_on_a_lineage_cycle() {
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
    assert_eq!(v.lead_label(xr), None);
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
    // card) and hover band (the lead's card) clear their floors on a dark and
    // a light lens theme. The bands are explicit pairs, so the lens needs no
    // inverse/dim modeling to judge them.
    let mut v = card_view(lead_and_worker());
    v.term = (30, 140);
    v.sideline_width = 80;
    v.layout.focus = 5; // w1 is the chosen card
    let (agent_i, detail_i) = card_rows_for(&v, "w1");
    let (lead_i, lead_detail_i) = card_rows_for(&v, "lead-a");
    v.hover_row = Some(lead_i);
    let frame = v.compose();
    let cols = frame.cols as usize;
    let text_w = v.sideline_paint_w().saturating_sub(1);
    let offset = v.sideline_offset();
    let mut chosen_cells: Vec<crate::proto::Cell> = Vec::new();
    let mut hover_cells: Vec<crate::proto::Cell> = Vec::new();
    for display_i in [agent_i, detail_i, detail_i + 1] {
        let row = display_i - offset + 1; // the strip row owns row 0
        chosen_cells.push(frame.cells[row * cols]);
        chosen_cells.push(frame.cells[row * cols + text_w - 1]);
    }
    for display_i in [lead_i, lead_detail_i] {
        let row = display_i - offset + 1; // the strip row owns row 0
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
    let mut v = card_view(lead_and_worker());
    v.term = (30, 140);
    v.sideline_width = 80;
    let (_, detail_i) = card_rows_for(&v, "w1");
    let frame = v.compose();
    let cols = frame.cols as usize;
    let text_w = v.sideline_paint_w().saturating_sub(1);
    let row = detail_i - v.sideline_offset() + 1; // the strip row owns row 0
    let cells = &frame.cells[row * cols..row * cols + text_w];
    let painted = cells.iter().filter(|c| c.c != ' ').count();
    assert!(painted > 0, "detail row has text");
    for cell in cells.iter().filter(|c| c.c != ' ') {
        assert_eq!(cell.fg, Color::Indexed(8), "line 2 fg follows the palette");
        assert_eq!(cell.flags & cell_flags::DIM, 0, "no DIM on line 2");
    }
}

// ---- a card's whole span answers the menu gestures ----------------------

#[tokio::test]
async fn a_long_press_on_each_card_line_opens_the_agents_menu() {
    // The card's detail and metrics lines carry their agent's identity, so
    // a press there arms the hold and a 600ms hold opens the agent's menu
    // from any line of the card - the same span the tap path resolves.
    let (agent_i, detail_i) = card_rows_for(&card_view(lead_and_worker()), "lead-a");
    for display_i in [agent_i, detail_i, detail_i + 1] {
        let mut v = card_view(lead_and_worker());
        let id = v
            .row_identity(display_i)
            .expect("every card line has an identity");
        assert_eq!(id, "agent:lead-a", "card lines identify as their agent");
        v.press_hold = Some((display_i, id, Instant::now() - Duration::from_millis(600)));
        let term_row = (display_i - v.sideline_offset() + 1) as u16;
        let mut buf: Vec<u8> = Vec::new();
        let mut carry = Vec::<u8>::new();
        crate::client::handle_stdin(
            &mut v,
            &mut Scanner::default(),
            &mut carry,
            format!("\x1b[<0;7;{}m", term_row + 1).as_bytes(),
            &mut buf,
        )
        .await
        .unwrap();
        assert!(
            matches!(
                v.row_menu.as_ref().map(|m| &m.target),
                Some(super::MenuTarget::Agent(ident)) if ident.name == "lead-a"
            ),
            "line {display_i}: the card's menu opened: {:?}",
            v.row_menu.as_ref().map(|m| &m.target)
        );
        assert!(
            buf.is_empty(),
            "line {display_i}: no click action rode the long press"
        );
    }
}

#[tokio::test]
async fn a_right_press_on_each_card_line_opens_the_agents_menu() {
    // Same span for right-click, the no-config path for terminals that
    // swallow the hold. The inter-card rule stays inert: no menu opens.
    let (agent_i, detail_i) = card_rows_for(&card_view(lead_and_worker()), "lead-a");
    for display_i in [agent_i, detail_i, detail_i + 1] {
        let mut v = card_view(lead_and_worker());
        let term_row = (display_i - v.sideline_offset() + 1) as u16;
        let mut scanner = Scanner::default();
        let mut buf: Vec<u8> = Vec::new();
        let mut carry = Vec::<u8>::new();
        crate::client::handle_stdin(
            &mut v,
            &mut scanner,
            &mut carry,
            format!("\x1b[<2;7;{}M", term_row + 1).as_bytes(),
            &mut buf,
        )
        .await
        .unwrap();
        assert!(
            matches!(
                v.row_menu.as_ref().map(|m| &m.target),
                Some(super::MenuTarget::Agent(ident)) if ident.name == "lead-a"
            ),
            "line {display_i}: the card's menu opened: {:?}",
            v.row_menu.as_ref().map(|m| &m.target)
        );
    }
    let rule_i = card_view(lead_and_worker())
        .painted_rows()
        .iter()
        .position(|row| matches!(row, DisplayRow::CardRule))
        .expect("adjacent cards paint a rule");
    let mut v = card_view(lead_and_worker());
    let term_row = (rule_i - v.sideline_offset() + 1) as u16;
    let mut scanner = Scanner::default();
    let mut buf: Vec<u8> = Vec::new();
    let mut carry = Vec::<u8>::new();
    crate::client::handle_stdin(
        &mut v,
        &mut scanner,
        &mut carry,
        format!("\x1b[<2;7;{}M", term_row + 1).as_bytes(),
        &mut buf,
    )
    .await
    .unwrap();
    assert!(v.row_menu.is_none(), "the rule row stays inert");
}
