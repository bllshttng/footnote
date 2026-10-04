//! Density-button behavior and spacing beside the notifications bell.

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
        lines[0].contains("🔔"),
        "bell remains visible: {:?}",
        lines[0]
    );
}

#[test]
fn density_button_glyph_sits_before_the_bell() {
    let v = wide_view(vec![agent_row("w", 4, Some(AgentBadge::Working), false)]);
    let pw = v.panel_w() as usize;
    let range = v.density_button_range(pw).unwrap();
    let bell = bell::button_range(&v, pw.saturating_sub(1));
    let frame = v.compose();
    let glyph_cell = &frame.cells[range.start];
    let pad_cell = &frame.cells[range.end - 1];
    assert_eq!(glyph_cell.c, density_glyph(v.density), "button glyph");
    assert_eq!(glyph_cell.flags, cell_flags::INVERSE, "glyph is clickable");
    assert_eq!(pad_cell.c, ' ', "plain pad before the gap");
    assert_eq!(pad_cell.flags, 0, "pad stays plain");
    assert_eq!(range.end.saturating_add(1), bell.start, "gap before bell");
    assert_eq!(bell.end, pw - 1, "bell ends before divider");
}
