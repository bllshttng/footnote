//! The sideline name-fit acceptance family: short columns retain the suffix
//! that distinguishes worker scopes and generations.

use super::*;

#[test]
fn extended_table_keeps_distinguishing_name_suffixes() {
    let first = agent_row(
        "blueprinter-fno-8bef7b",
        4,
        Some(AgentBadge::Working),
        false,
    );
    let second = agent_row(
        "blueprinter-etl-631fd8",
        5,
        Some(AgentBadge::Working),
        false,
    );
    let mut v = wide_view(vec![first, second]);
    set_density(&mut v, Density::Extended);
    v.term = (24, MIN_EXTENDED_PANEL_W + MIN_CONTENT_COLS + 3);

    let text = frame_text(&v.compose());
    assert!(text.contains("8bef7b"), "first suffix was hidden:\n{text}");
    assert!(text.contains("631fd8"), "second suffix was hidden:\n{text}");
}

#[test]
fn extended_table_prioritizes_name_suffix_over_context() {
    let mut agent = agent_row(
        "blueprinter-fno-8bef7b",
        4,
        Some(AgentBadge::Working),
        false,
    );
    agent.dnd = true;
    agent.reason = Some("a long context reason".into());
    let mut v = wide_view(vec![agent]);
    set_density(&mut v, Density::Extended);
    v.term = (24, MIN_EXTENDED_PANEL_W + MIN_CONTENT_COLS + 3);

    let text = frame_text(&v.compose());
    assert!(
        text.contains("8bef7b"),
        "identity suffix must survive context overflow:\n{text}"
    );
}
