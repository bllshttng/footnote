//! Density-button behavior and spacing at the strip's right end; the bell
//! now lives on the mux tab bar.

use super::*;

#[test]
fn density_button_click_routes_to_the_cycle() {
    let v = wide_view(vec![agent_row("w", 4, Some(AgentBadge::Working), false)]);
    let range = v.density_button_range(v.panel_w() as usize).unwrap();
    assert!(matches!(
        v.chrome_hit(0, range.start as u16 + 1),
        Some(ChromeHit::CycleDensity)
    ));
    assert!(!matches!(
        v.chrome_hit(1, range.start as u16 + 1),
        Some(ChromeHit::CycleDensity)
    ));
    assert_eq!(
        crate::keys::resolve_chord(b'B'),
        crate::keys::Event::CycleDensity
    );
    assert_eq!(
        crate::keys::resolve_chord(b'o'),
        crate::keys::Event::ToggleAgentSort
    );
}

#[test]
fn density_button_preserves_the_top_header_rollup() {
    let mut v = wide_view(vec![agent_row("b", 5, Some(AgentBadge::Blocked), false)]);
    v.density = Density::Regular;
    let lines: Vec<String> = frame_text(&v.compose())
        .lines()
        .map(str::to_string)
        .collect();
    assert!(lines[1].contains('▲'), "rollup survives: {:?}", lines[1]);
    assert!(
        lines[0].contains(density_glyph(Density::Regular)),
        "density button remains visible: {:?}",
        lines[0]
    );
    assert!(
        lines[0].contains('\u{f0f3}'),
        "bell glyph shows in the tab bar: {:?}",
        lines[0]
    );
}

#[test]
fn the_strip_presents_tabs_density_and_the_tab_bar_bell() {
    let v = wide_view(vec![agent_row("w", 4, Some(AgentBadge::Working), false)]);
    let pw = v.panel_w() as usize;
    let range = v.density_button_range(pw).unwrap();
    let bell = bell::button_range(&v);
    let frame = v.compose();
    let glyph_cell = &frame.cells[range.start];
    let pad_cell = &frame.cells[range.end - 1];
    assert_eq!(glyph_cell.c, density_glyph(v.density), "button glyph");
    assert_eq!(glyph_cell.flags, cell_flags::INVERSE, "glyph is clickable");
    assert_eq!(pad_cell.c, ' ', "plain pad before the gap");
    assert_eq!(pad_cell.flags, 0, "pad stays plain");
    assert_eq!(range.end, pw - 1, "the density button ends the strip");
    // One column in from the edge: the last bell cell sits beside
    // the pane border's top-right corner, never on it.
    assert_eq!(
        bell.end,
        v.term.1 as usize - 1,
        "the bell ends one column in"
    );
    assert!(
        matches!(
            v.chrome_hit(0, (v.term.1 - 2) as u16),
            Some(ChromeHit::Bell(bell::Hit::Toggle))
        ),
        "the tab-bar bell toggles"
    );
    // The strip tabs read as tabs (the operator's 2026-10-04 ruling): the
    // active word wears the brand with a bold underline, the resting one
    // sits muted - and the words right-align ahead of the density button.
    let words = v.top_row_words();
    let spans = v.top_row_spans();
    let mut last_end = 0;
    for ((start, w, view), (word, _)) in spans.iter().zip(words.iter()) {
        assert_eq!(*w, word.chars().count(), "span width matches the word");
        last_end = last_end.max(start + w);
        for j in *start..*start + *w {
            let cell = &frame.cells[j];
            if v.sideline_view == *view {
                assert_eq!(cell.fg, v.theme.brand, "active tab wears the brand");
                assert_eq!(
                    cell.flags & (cell_flags::BOLD | cell_flags::UNDERLINE),
                    cell_flags::BOLD | cell_flags::UNDERLINE,
                    "active tab is bold and underlined"
                );
            } else {
                assert_eq!(
                    cell.fg,
                    crate::theme::dim_fg(&v.theme),
                    "resting tab is muted"
                );
            }
        }
    }
    assert!(last_end < range.start, "the words end before the button");
    // At the supported 9-column panel the words yield to the density seat:
    // the last word ends before the button's range, so the button's cells
    // route to the density cycle, not the Messages switch.
    let mut v9 = wide_view(vec![agent_row("w", 4, Some(AgentBadge::Working), false)]);
    v9.sideline_width = 9;
    let range9 = v9.density_button_range(v9.panel_w() as usize).unwrap();
    let spans9 = v9.top_row_spans();
    let last_end9 = spans9.iter().map(|(s, w, _)| s + w).max().unwrap();
    assert!(
        last_end9 <= range9.start,
        "words end before the density seat at the 9-column panel: spans {spans9:?} vs {range9:?}"
    );
}
