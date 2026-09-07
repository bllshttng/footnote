//! The status-glyph test family: which glyph/accent a row or pane
//! renders for its state. Lives outside client_tests.rs for the same
//! reason the nav family does.

use super::*;

#[test]
fn keys_modal_legend_lists_severity_glyphs_in_header_order() {
    // (x-b5d1 AC1) The legend section is generated from the lattice table,
    // so the modal's rows must equal SEVERITY_ORDER through lattice_glyph,
    // each with a non-empty label - the same glyphs the header band
    // counts, in the order the band lists them.
    let m = super::build_keys_modal();
    let rows = &m.popup.rows;
    let start = rows
        .iter()
        .position(|r| matches!(r, PopupRow::Header(h) if h == "sideline glyphs"))
        .expect("the legend section renders in the keys modal");
    let legend = &rows[start + 1..start + 1 + SEVERITY_ORDER.len()];
    for (row, &s) in legend.iter().zip(SEVERITY_ORDER.iter()) {
        match row {
            PopupRow::Entry {
                glyph,
                label,
                hint: _,
                enabled: false,
            } => {
                assert_eq!(*glyph, lattice_glyph(s).0.to_string());
                assert!(!label.is_empty(), "{s:?} must carry a label");
            }
            other => panic!("legend rows are inert entries, got {other:?}"),
        }
    }
    assert!(matches!(
        rows.get(start + 1 + SEVERITY_ORDER.len()),
        Some(PopupRow::Rule) | None
    ));
}

#[test]
fn lattice_glyphs_are_pairwise_distinct_and_single_cell() {
    use LatticeState::*;
    let states = [
        Working, Idle, Blocked, DoneUnseen, Exited, Unmeasured, Empty,
    ];
    let glyphs: Vec<char> = states.iter().map(|&s| lattice_glyph(s).0).collect();
    // Pairwise distinct: every state pair reads differently by GLYPH alone,
    // so a monochrome/weak-BOLD terminal never collapses two states
    // (AC1-ERR / AC1-EDGE).
    for (i, a) in glyphs.iter().enumerate() {
        for b in &glyphs[i + 1..] {
            assert_ne!(a, b, "lattice glyphs must be pairwise distinct");
        }
    }
    // Single-cell width (AC1-EDGE): `glyph_cols` is the renderer's own width
    // authority, and the codepoint stays out of the astral/emoji planes so
    // no terminal renders it double-width (row alignment can never break).
    for g in &glyphs {
        assert_eq!(glyph_cols(*g), 1, "lattice glyph {g:?} must be single-cell");
        assert!(
            (*g as u32) < 0x1F000,
            "lattice glyph {g:?} must not be an astral emoji"
        );
    }
}

#[test]
fn pane_activity_folds_to_working_empty_idle_or_unmeasured_never_blind_idle() {
    // (x-d401, AC1-HP/EDGE) The render fold. With no badge, the pane's own
    // OSC 133 reading decides: Running -> Working, Idle -> Idle, Empty ->
    // Empty, and Unmeasured or absent -> Unmeasured. The old `None =>
    // Idle` fold rendered four working panes and thirty empty shells as
    // the same circle; the absent reading must render as the marked
    // absence `?`, never as a measured idle.
    use crate::vt::ShellActivity as SA;
    assert_eq!(
        pane_state(None, false, Some(SA::Running)),
        PaneState::Working
    );
    assert_eq!(pane_state(None, false, Some(SA::Idle)), PaneState::Idle);
    assert_eq!(pane_state(None, false, Some(SA::Empty)), PaneState::Empty);
    assert_eq!(
        pane_state(None, false, Some(SA::Unmeasured)),
        PaneState::Unmeasured,
        "an un-integrated pane reads as no-reading, not idle"
    );
    assert_eq!(
        pane_state(None, false, None),
        PaneState::Unmeasured,
        "an absent field reads as no-reading, not idle"
    );
    // A present badge still wins: the registry worker's own report beats
    // the vt reading for its row.
    assert_eq!(
        pane_state(Some(AgentBadge::Working), false, Some(SA::Idle)),
        PaneState::Working
    );
}

#[test]
fn unmeasured_pane_renders_the_question_glyph_not_the_idle_circle() {
    // (x-d401, AC1-EDGE) The positive control: `?` is a marker only the
    // no-reading outcome produces, and it is pinned as DISTINCT from the
    // idle glyph it used to be confused with.
    assert_eq!(lattice_glyph(pane_to_lattice(PaneState::Unmeasured)).0, '?');
    assert_ne!(
        lattice_glyph(pane_to_lattice(PaneState::Idle)).0,
        lattice_glyph(pane_to_lattice(PaneState::Unmeasured)).0,
        "the no-reading glyph must differ from the idle glyph"
    );
}

#[test]
fn pristine_shell_and_live_workload_render_different_glyphs() {
    // (x-d401, AC1-ERR) The reported bug as its own assertion: an empty
    // shell tab and a pane running cargo test must not share a glyph.
    use crate::vt::ShellActivity as SA;
    let empty = lattice_glyph(pane_to_lattice(pane_state(None, false, Some(SA::Empty)))).0;
    let running = lattice_glyph(pane_to_lattice(pane_state(None, false, Some(SA::Running)))).0;
    assert_ne!(empty, running);
}

#[test]
fn lattice_accent_only_on_blocked() {
    use LatticeState::*;
    // The accent is reserved for needs-attention (Blocked); every other
    // state stays default-colored (US6 invariant). The accent is now the
    // theme's pick (x-f75e); pass it in and expect Blocked to wear it.
    assert_eq!(lattice_style(Blocked, LATTICE_ACCENT).fg, LATTICE_ACCENT);
    for s in [Working, Idle, DoneUnseen, Exited, Unmeasured] {
        assert_eq!(
            lattice_style(s, LATTICE_ACCENT).fg,
            Color::Default,
            "{s:?} must not carry the accent"
        );
    }
    // Attention is never dimmed (AC1-UI): Blocked is BOLD, not DIM.
    assert_eq!(lattice_glyph(Blocked).1 & cell_flags::DIM, 0);
}

#[test]
fn agent_row_done_respects_seen_bit() {
    // A Done pane the operator has NOT viewed holds the bold `✓`; once seen
    // (x-4328) it folds to Idle `○`, matching the nav/tab rollup paths, so a
    // viewed-done row never shows a stale needs-attention marker.
    let unseen = tab_agent(None, Some(AgentBadge::Done), false);
    assert_eq!(agent_lattice_state(&unseen), LatticeState::DoneUnseen);
    assert_eq!(lattice_glyph(agent_lattice_state(&unseen)).0, '✓');
    let seen = AgentRow {
        portal: None,
        harness: None,
        model: None,
        route: None,
        seen: true,
        ..tab_agent(None, Some(AgentBadge::Done), false)
    };
    assert_eq!(agent_lattice_state(&seen), LatticeState::Idle);
    assert_eq!(lattice_glyph(agent_lattice_state(&seen)).0, '○');
}

#[test]
fn agent_row_unmeasured_renders_a_distinct_dim_glyph_from_exited() {
    // x-9de7: `exited` alone no longer says "dead" - `unmeasured` marks
    // the uncorroborated case, and it must draw a DIFFERENT glyph than a
    // confirmed exit, because the operator's routing decision turns on
    // telling the two apart at a glance (dead => respawn is safe,
    // unmeasured => look before you spawn).
    let confirmed_dead = tab_agent(None, None, true);
    assert_eq!(agent_lattice_state(&confirmed_dead), LatticeState::Exited);
    assert_eq!(lattice_glyph(agent_lattice_state(&confirmed_dead)).0, '✗');

    let uncorroborated = AgentRow {
        portal: None,
        harness: None,
        model: None,
        route: None,
        unmeasured: true,
        ..tab_agent(None, None, true)
    };
    assert_eq!(
        agent_lattice_state(&uncorroborated),
        LatticeState::Unmeasured
    );
    assert_eq!(lattice_glyph(agent_lattice_state(&uncorroborated)).0, '?');

    // The two glyphs must never collide.
    assert_ne!(
        lattice_glyph(agent_lattice_state(&confirmed_dead)).0,
        lattice_glyph(agent_lattice_state(&uncorroborated)).0
    );

    // `unmeasured` on a LIVE row (exited: false) is inert - a live row is
    // never rendered Unmeasured just because some upstream sentinel left
    // the bit set. (x-d401: the row now carries an explicit activity
    // reading, so this proves the BIT is inert; a live row with NO reading
    // at all renders Unmeasured for the absence, which is the new
    // predicate, not the old bug.)
    let live_with_stale_bit = AgentRow {
        portal: None,
        harness: None,
        model: None,
        route: None,
        unmeasured: true,
        pane_activity: Some(crate::vt::ShellActivity::Idle),
        ..tab_agent(None, None, false)
    };
    assert_ne!(
        agent_lattice_state(&live_with_stale_bit),
        LatticeState::Unmeasured
    );
}
