//! The new-agent composer's unit suite : editor semantics, focus
//! order, submit refusals, update correlation, and the render table. The
//! subprocess-boundary journeys live in the integration suite
//! (`tests/agent_launcher_journey.rs`).

use super::agent_launcher::{
    apply_launch_update, close, open, CatalogOutcome, Focus, HarnessChoice, LauncherEsc, Phase,
};
use super::*;
use crate::proto::agent_launch::{AgentLaunchUpdate, LaunchState};
use ratatui_core::buffer::Buffer as RtBuffer;
use ratatui_core::layout::Rect as RtRect;
use ratatui_core::style::Modifier;

fn plain_view() -> View {
    let mut v = two_pane_view();
    v.term = (24, 80);
    v
}

fn view_with_launcher() -> View {
    let mut v = plain_view();
    open(&mut v);
    v
}

fn catalog(names: &[(&str, bool, bool)]) -> Option<CatalogOutcome> {
    Some(CatalogOutcome::Ok(
        names
            .iter()
            .map(|(n, native, installed)| HarnessChoice {
                name: n.to_string(),
                native: *native,
                installed: *installed,
                models: Vec::new(),
                // Free-text surface by default: the effort chip stays
                // offered in tests that do not name a list.
                efforts: Some(Vec::new()),
                permission_modes: Some(Vec::new()),
            })
            .collect(),
        None,
    ))
}

fn sync_catalog(v: &mut View) {
    // Mirror the run loop's rx arm: land the catalog and sync the draft's
    // harness names.
    let names: Vec<String> = match &v.launcher_catalog {
        Some(CatalogOutcome::Ok(rows)) => rows.iter().map(|r| r.name.clone()).collect(),
        _ => vec![],
    };
    if let Some(l) = v.launcher.as_mut() {
        if l.draft.harnesses.is_empty() && !names.is_empty() {
            l.draft.harnesses = names;
        }
    }
}

fn type_message(v: &mut View, text: &str) {
    // Focus the message field from a fresh dock: Harness -> Project ->
    // Message.
    if let Some(l) = v.launcher.as_mut() {
        l.focus = Focus::Message;
    }
    let mut esc = LauncherEsc::default();
    let keys = esc.fold(text.as_bytes());
    v.launcher_esc = esc;
    let _ = keys;
    // Route the same bytes through the real key folder so editor state and
    // folding agree.
    let bytes = text.as_bytes().to_vec();
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(v, &bytes, &mut sock).await;
    });
}

#[test]
fn open_seeds_project_candidates_and_kicks_the_catalog_probe() {
    let mut v = plain_view();
    open(&mut v);
    assert!(v.launcher.is_some());
    assert!(v.catalog_want, "first open arms the catalog probe");
    // The client's own cwd is always the first project candidate.
    let own = std::env::current_dir().unwrap().display().to_string();
    let l = v.launcher.as_ref().unwrap();
    assert_eq!(l.draft.projects.first().map(String::as_str), Some(&own[..]));
}

#[test]
fn one_lone_esc_byte_closes_the_dock() {
    // The unit twin of the pty repro: the scanner flushes [0x1b] into
    // launcher_keys after its quiet window, and the dock's own carry must
    // release a trailing lone ESC as a bare Esc press, not re-buffer it.
    let mut v = view_with_launcher();
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, b"\x1b", &mut sock).await;
    });
    assert!(v.launcher.is_none(), "one lone Esc byte closes the dock");
    assert!(
        v.launcher_closed.is_some(),
        "the draft is retained for reopen"
    );
}

#[test]
fn arrow_reaches_the_dock_whole_or_scanner_rejoined() {
    // AC1-EDGE: an arrow in one chunk, or as the scanner-rejoined pair a
    // lone-Esc read plus a follow-up, moves the cursor / focus and never
    // reads as Esc.
    let mut v = view_with_launcher();
    if let Some(l) = v.launcher.as_mut() {
        l.focus = Focus::Message;
    }
    type_message(&mut v, "ab");
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, b"\x1b[A", &mut sock).await;
    });
    // Whole-chunk arrow: the cursor moves inside the editor, no close.
    assert!(v.launcher.is_some(), "an arrow never closes the dock");
    // The scanner rejoins an arrow into ONE chunk, so the fold sees
    // `\x1b` + `[A` in a single read and folds Up, never Esc.
    let mut esc = LauncherEsc::default();
    assert_eq!(
        esc.fold(b"\x1b[A"),
        vec![super::agent_launcher::LKey::Up],
        "a rejoined arrow folds Up, not Esc"
    );
}

#[test]
fn esc_hides_and_reopening_restores_the_draft() {
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true), ("codex", true, true)]);
    sync_catalog(&mut v);
    type_message(&mut v, "ship it");
    let revision_before = v.launcher.as_ref().unwrap().draft.revision;
    close(&mut v);
    assert!(v.launcher.is_none(), "dock hidden");
    assert!(v.launcher_closed.is_some(), "draft retained");
    // Reopen: same draft, same revision, nothing lost.
    open(&mut v);
    let l = v.launcher.as_ref().unwrap();
    assert_eq!(l.draft.message, "ship it");
    assert_eq!(l.draft.revision, revision_before);
    assert_eq!(l.draft.harnesses, vec!["claude", "codex"]);
}

#[test]
fn tab_walks_the_chip_order_and_skips_collapsed_pins() {
    let mut v = view_with_launcher();
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
    // Harness -> Project -> Model -> Effort -> More -> Message (5 tabs, the
    // two pins are collapsed).
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, b"\t\t\t\t\t", &mut sock).await;
    });
    assert_eq!(
        v.launcher.as_ref().unwrap().focus,
        Focus::Message,
        "Harness -> Project -> Model -> Effort -> More -> Message (5 tabs)"
    );
    // A sixth tab reaches Launch.
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, b"\t", &mut sock).await;
    });
    assert_eq!(v.launcher.as_ref().unwrap().focus, Focus::Launch);
    // Expanding the pins: Tab past More lands on Permission, then Placement.
    if let Some(l) = v.launcher.as_mut() {
        l.draft.expanded = true;
        l.focus = Focus::More;
    }
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, b"\t\t", &mut sock).await;
    });
    assert_eq!(
        v.launcher.as_ref().unwrap().focus,
        Focus::Placement,
        "expanded: More -> Permission -> Placement"
    );
}

#[test]
fn enter_inserts_newline_in_message_and_never_submits() {
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true)]);
    sync_catalog(&mut v);
    type_message(&mut v, "line one");
    let before = v.launcher.as_ref().unwrap().armed;
    type_message(&mut v, "\nline two");
    let l = v.launcher.as_ref().unwrap();
    assert_eq!(l.draft.message, "line one\nline two");
    assert_eq!(before, None);
    assert_eq!(l.armed, None, "Enter inside the message never submits");
}

#[test]
fn bracketed_paste_is_data_even_with_selector_shaped_bytes() {
    let mut v = view_with_launcher();
    type_message(&mut v, "");
    // A paste whose payload looks like an arrow sequence must land as text.
    let payload = b"\x1b[200~\x1b[Ahello\x1b[201~";
    type_message(&mut v, std::str::from_utf8(payload).unwrap());
    let l = v.launcher.as_ref().unwrap();
    assert!(
        l.draft.message.contains("\x1b[Ahello"),
        "pasted bytes are data: {}",
        l.draft.message
    );
}

#[test]
fn split_utf8_sequence_across_chunks_is_not_wedged() {
    let mut esc = LauncherEsc::default();
    // The emoji U+1F600 is four bytes; split after the first.
    let full = "\u{1f600}".as_bytes();
    let first = esc.fold(&full[..1]);
    assert!(first.is_empty(), "an incomplete sequence waits");
    let rest = esc.fold(&full[1..]);
    assert_eq!(rest, vec![super::agent_launcher::LKey::Char('\u{1f600}')]);
}

#[test]
fn submit_refuses_unavailable_harness_pre_wire_with_draft_intact() {
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[
        ("claude", true, true),
        ("gemini", true, false),
        ("hermes", false, true),
    ]);
    sync_catalog(&mut v);
    // Select the not-installed one: claude -> gemini -> hermes via Right on
    // the harness field.
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, b"\x1b[C\x1b[C", &mut sock).await;
    });
    assert_eq!(v.launcher.as_ref().unwrap().draft.harness(), "hermes");
    // Enter on Launch: refused BEFORE any wire message, with the reason.
    if let Some(l) = v.launcher.as_mut() {
        l.focus = Focus::Launch;
    }
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, b"\r", &mut sock).await;
    });
    let l = v.launcher.as_ref().unwrap();
    match &l.phase {
        Phase::Refused { reason, .. } => {
            assert!(
                reason.contains("no native fno spawn") || reason.contains("not installed"),
                "reason: {reason}"
            );
        }
        other => panic!("expected a pre-wire refusal, got {other:?}"),
    }
    assert_eq!(l.draft.message, "", "draft untouched");
    assert!(sock.is_empty(), "nothing went on the wire");
}

#[test]
fn stale_update_cannot_overwrite_a_newer_draft() {
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true)]);
    sync_catalog(&mut v);
    // Arm request 1, then edit the draft (a newer revision).
    if let Some(l) = v.launcher.as_mut() {
        l.armed = Some(1);
        l.phase = Phase::Submitting { request_id: 1 };
    }
    let update = AgentLaunchUpdate {
        request_id: 1,
        state: LaunchState::Refused {
            reason: "capacity".into(),
        },
    };
    apply_launch_update(&mut v, update);
    assert!(matches!(
        v.launcher.as_ref().unwrap().phase,
        Phase::Refused { .. }
    ));
    // A STALE id (2 never armed by this dock) applies nowhere: the dock's
    // newer draft state survives.
    let stale = AgentLaunchUpdate {
        request_id: 2,
        state: LaunchState::Refused {
            reason: "bogus".into(),
        },
    };
    apply_launch_update(&mut v, stale);
    if let Some(l) = v.launcher.as_ref() {
        assert!(matches!(l.phase, Phase::Refused { ref reason, .. } if reason == "capacity"));
    }
}

#[test]
fn unknown_outcome_blocks_retry_until_dismiss() {
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true)]);
    sync_catalog(&mut v);
    if let Some(l) = v.launcher.as_mut() {
        l.armed = Some(1);
    }
    let update = AgentLaunchUpdate {
        request_id: 1,
        state: LaunchState::Unknown {
            reason: "launch timed out".into(),
        },
    };
    apply_launch_update(&mut v, update);
    assert!(matches!(
        v.launcher.as_ref().unwrap().phase,
        Phase::Unknown { .. }
    ));
    // The attempt is remembered BEYOND the dock (close + reopen).
    close(&mut v);
    open(&mut v);
    assert!(v.launch_attempt.is_some(), "attempt outlives the dock");
    // Dismiss is the explicit action that resolves the block; the draft
    // survives and launch is possible again.
    if let Some(l) = v.launcher.as_mut() {
        l.focus = Focus::Dismiss;
    }
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, b"\r", &mut sock).await;
    });
    assert!(matches!(v.launcher.as_ref().unwrap().phase, Phase::Editing));
}

#[test]
fn launched_pane_update_returns_the_focus_pane_and_the_seed_note() {
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true)]);
    sync_catalog(&mut v);
    if let Some(l) = v.launcher.as_mut() {
        l.armed = Some(1);
    }
    let update = AgentLaunchUpdate {
        request_id: 1,
        state: LaunchState::Launched {
            name: "quiet-otter".into(),
            pane: Some(7),
            seed_delivered: Some(false),
        },
    };
    let pane = apply_launch_update(&mut v, update);
    assert_eq!(pane, Some(7), "the requesting client focuses the new pane");
    match &v.launcher.as_ref().unwrap().phase {
        Phase::Launched {
            name, seed_note, ..
        } => {
            assert_eq!(name, "quiet-otter");
            assert_eq!(*seed_note, Some("interactive launch; no seed sent"));
        }
        other => panic!("expected Launched, got {other:?}"),
    }
    // The arm is released: a deliberate retry can arm a fresh request.
    assert_eq!(v.launcher.as_ref().unwrap().armed, None);
}

#[test]
fn chip_row_paints_fields_and_launch_on_one_row() {
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true)]);
    sync_catalog(&mut v);
    let l = v.launcher.as_ref().unwrap();
    let area = RtRect::new(0, 0, 40, 3);
    let rects = l.dock_layout_rects(&v, area);
    let labels: Vec<(String, u16)> = rects
        .chips
        .iter()
        .filter(|(_, _, r)| r.y == area.y)
        .map(|(_, s, r)| (s.clone(), r.x))
        .collect();
    let row_text: String = labels.iter().map(|(s, x)| format!("{s}@{x} ")).collect();
    assert!(
        row_text.contains("claude@"),
        "harness chip carries the catalog name: {row_text}"
    );
    let launch_x = labels.iter().find(|(s, _)| s == "Launch").map(|(_, x)| *x);
    let Some(launch_x) = launch_x else {
        panic!("launch chip on the primary row: {row_text}");
    };
    let after: Vec<_> = labels.iter().filter(|(_, x)| *x > launch_x).collect();
    assert!(after.is_empty(), "nothing right of Launch: {row_text}");
    assert_eq!(rects.chips.len(), labels.len(), "collapsed: no pin chips");
}

#[test]
fn chips_paint_in_the_popup_control_vocabulary() {
    // Change 2: the popup's control grammar, not raw DIM (a dim word reads
    // as a caption). Unfocused chips are filled Body blocks (INVERSE under
    // the terminal theme), the focused chip is the BodySel cut-out, Launch
    // wears the esc-chip role, adjacent chips sit a default-styled blank
    // column apart, and every picker chip ends in the dropdown caret.
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true)]);
    sync_catalog(&mut v);
    if let Some(l) = v.launcher.as_mut() {
        l.focus = Focus::Effort;
    }
    let l = v.launcher.as_ref().unwrap();
    let area = RtRect::new(0, 0, 80, 3);
    let rects = l.dock_layout_rects(&v, area);
    let mut buf = RtBuffer::empty(area);
    l.paint(&v, &mut buf, area);
    for (f, _, r) in &rects.chips {
        let focused = *f == Focus::Effort;
        assert!(
            !buf[(r.x, r.y)].modifier.contains(Modifier::DIM),
            "chip {f:?} must not read as a dim caption"
        );
        assert_eq!(
            buf[(r.x, r.y)].modifier.contains(Modifier::REVERSED),
            !focused,
            "chip {f:?} filled-block vs focused cut-out"
        );
        if super::agent_launcher::is_picker_chip(*f) {
            let last = buf[(r.x + r.width - 1, r.y)].symbol().to_string();
            assert_eq!(last, "\u{25be}", "picker chip {f:?} ends in the caret");
        }
    }
    // Adjacent same-row chips are separated by one default-styled column.
    for pair in rects.chips.windows(2) {
        let (f0, _, r0) = (&pair[0].0, &pair[0].1, pair[0].2);
        let (f1, _, r1) = (&pair[1].0, &pair[1].1, pair[1].2);
        if r0.y == r1.y {
            let gap_x = r0.x + r0.width;
            assert!(gap_x < r1.x, "chips {f0:?} and {f1:?} need a gap column");
            let gap = &buf[(gap_x, r0.y)];
            assert!(
                gap.modifier.is_empty(),
                "the gap column between {f0:?} and {f1:?} stays default-styled"
            );
        }
    }
}
#[test]
fn footer_names_the_lifecycle_and_the_refusal_reason() {
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true)]);
    sync_catalog(&mut v);
    if let Some(l) = v.launcher.as_mut() {
        l.armed = Some(1);
    }
    apply_launch_update(
        &mut v,
        AgentLaunchUpdate {
            request_id: 1,
            state: LaunchState::Refused {
                reason: "spawn gate: no free slot".into(),
            },
        },
    );
    let footer = v.launcher.as_ref().unwrap().footer();
    assert!(
        footer.contains("refused") && footer.contains("no free slot"),
        "footer: {footer}"
    );
}

#[test]
fn wrap_message_hard_wraps_by_display_width() {
    let chunks = super::agent_launcher::wrap_message("abcdefghij", 4);
    let got: Vec<(usize, &str)> = chunks.iter().map(|(o, s)| (*o, s.as_str())).collect();
    assert_eq!(
        got,
        vec![(0, "abcd"), (4, "efgh"), (8, "ij")],
        "hard wrap at 4 columns"
    );
    let chunks = super::agent_launcher::wrap_message("ab\n\ncd", 10);
    let got: Vec<(usize, &str)> = chunks.iter().map(|(o, s)| (*o, s.as_str())).collect();
    assert_eq!(
        got,
        vec![(0, "ab"), (3, ""), (4, "cd")],
        "blank line is its own chunk"
    );
    let chunks = super::agent_launcher::wrap_message("a王b", 2);
    assert_eq!(chunks[1].1, "王", "wide glyph starts the next chunk");
    assert!(
        chunks.iter().all(|(_, s)| s
            .chars()
            .map(|c| usize::from(unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)))
            .sum::<usize>()
            <= 2),
        "no chunk wider than 2 columns"
    );
}

#[test]
fn wrapped_cursor_lands_on_the_row_holding_the_char() {
    let (r, c) = super::agent_launcher::wrapped_cursor("abcdefghij", 9, 4);
    assert_eq!((r, c), (2, 1), "cursor 9 at width 4 is row 2 col 1");
    let (r, c) = super::agent_launcher::wrapped_cursor("ab\ncd", 2, 10);
    assert_eq!((r, c), (0, 2), "cursor on the newline ends its row");
    let (r, c) = super::agent_launcher::wrapped_cursor("abcdefgh", 8, 4);
    assert_eq!((r, c), (1, 4), "cursor at the very end");
}

#[test]
fn dock_growth_counts_wrapped_rows_and_caps_at_a_third() {
    let mut v = view_with_launcher();
    v.term = (24, 80);
    // A 120-character line wraps to 4 rows at the editor's 38 wrap columns
    // (40 minus the prompt gutter): the bar grows to 1 chip + 4 message +
    // 1 footer = 6 and shrinks when it clears.
    if let Some(l) = v.launcher.as_mut() {
        l.draft.message = "x".repeat(120);
    }
    let l = v.launcher.as_ref().unwrap();
    let (total, editor) = l.dock_layout(60, 40);
    assert_eq!((total, editor), (6, 4), "grows with wrapped rows");
    if let Some(l) = v.launcher.as_mut() {
        l.draft.message.clear();
    }
    let l = v.launcher.as_ref().unwrap();
    assert_eq!(
        l.dock_layout(60, 40),
        (3, 1),
        "clearing gives the rows back"
    );
    // 800 characters against a 30-row panel: capped at a third (editor 8,
    // total 10) and the window holds the cursor row.
    if let Some(l) = v.launcher.as_mut() {
        l.draft.message = "y".repeat(20 * 40);
    }
    let l = v.launcher.as_ref().unwrap();
    let (total, editor) = l.dock_layout(30, 40);
    assert_eq!((total, editor), (10, 8), "capped at a third of the panel");
    let area = RtRect::new(0, 0, 40, 10);
    let rects = l.dock_layout_rects(&v, area);
    let (cur_row, _) =
        super::agent_launcher::wrapped_cursor(&l.draft.message, l.draft.cursor_chars, 38);
    assert!(
        cur_row >= rects.start_chunk && cur_row < rects.start_chunk + rects.editor_rows,
        "window holds the cursor row: cur {cur_row}, start {}, rows {}",
        rects.start_chunk,
        rects.editor_rows
    );
}

#[test]
fn dock_layout_floors_at_one_editor_line_on_tiny_panels() {
    let mut v = view_with_launcher();
    v.term = (24, 80);
    let l = v.launcher.as_ref().unwrap();
    // 8 rows: cap = 8/3 - 1 chip - 1 footer = 0, floored to 1.
    assert_eq!(l.dock_layout(8, 40), (3, 1));
}

#[test]
fn launch_is_dead_while_an_unknown_outcome_blocks_retry() {
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true)]);
    sync_catalog(&mut v);
    if let Some(l) = v.launcher.as_mut() {
        l.armed = Some(1);
        l.phase = Phase::Submitting { request_id: 1 };
    }
    apply_launch_update(
        &mut v,
        AgentLaunchUpdate {
            request_id: 1,
            state: LaunchState::Unknown {
                reason: "launch timed out".into(),
            },
        },
    );
    // Arm-released Unknown: the exact state the old gate leaked through.
    assert_eq!(v.launcher.as_ref().unwrap().armed, None);
    if let Some(l) = v.launcher.as_mut() {
        l.focus = Focus::Launch;
    }
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, b"\r", &mut sock).await;
    });
    assert!(matches!(
        v.launcher.as_ref().unwrap().phase,
        Phase::Unknown { .. }
    ));
    assert!(sock.is_empty(), "nothing went on the wire: {sock:?}");
}

/// A Starting attempt whose update is lost must stay escapable: the
/// Dismiss row is reachable during Submitting and returns the dock to
/// Editing (retry then arms a fresh id, one attempt each).
#[test]
fn submitting_dock_offers_dismiss_and_recovers_to_editing() {
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true)]);
    sync_catalog(&mut v);
    if let Some(l) = v.launcher.as_mut() {
        l.armed = Some(1);
        l.phase = Phase::Submitting { request_id: 1 };
    }
    // The dismiss chip exists while starting.
    let l = v.launcher.as_ref().unwrap();
    let rects = l.dock_layout_rects(&v, RtRect::new(0, 0, 40, 3));
    assert!(
        rects.chips.iter().any(|(f, _, _)| *f == Focus::Dismiss),
        "dismiss chip reachable while starting"
    );
    if let Some(l) = v.launcher.as_mut() {
        l.focus = Focus::Dismiss;
    }
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, b"\r", &mut sock).await;
    });
    assert!(matches!(v.launcher.as_ref().unwrap().phase, Phase::Editing));
    assert_eq!(v.launcher.as_ref().unwrap().armed, None);
}

/// A degraded catalog re-probes on the next open instead of sticking for
/// the session.
#[test]
fn degraded_catalog_reprobes_on_reopen() {
    let mut v = plain_view();
    v.launcher_catalog = Some(CatalogOutcome::Degraded("probe failed".into()));
    open(&mut v);
    assert!(v.catalog_want, "a degraded read re-arms the probe");
}

#[test]
fn launcher_mouse_clicks_launch_and_submits() {
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true)]);
    sync_catalog(&mut v);
    let l = v.launcher.as_ref().unwrap();
    let pw = v.panel_w() as usize;
    let chrome = v.bottom_row_is_chrome() as usize;
    let body = 24 - chrome;
    let (total, _) = l.dock_layout(body, pw - 1);
    let top = body - total;
    let area = RtRect::new(0, top as u16, (pw - 1) as u16, total as u16);
    let rects = l.dock_layout_rects(&v, area);
    let (launch_y, launch_x) = rects
        .chips
        .iter()
        .find(|(f, _, _)| *f == Focus::Launch)
        .map(|(_, _, r)| (r.y, r.x))
        .unwrap();
    let rep = crate::mouse::MouseReport {
        kind: crate::proto::MouseKind::Press(crate::proto::MouseButton::Left),
        row: launch_y,
        col: launch_x,
        shift: false,
    };
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let consumed = super::agent_launcher::launcher_mouse(&mut v, rep, &mut sock)
            .await
            .unwrap();
        assert!(consumed, "a click on the Launch chip is consumed");
    });
    assert!(
        matches!(v.launcher.as_ref().unwrap().phase, Phase::Submitting { .. }),
        "submit path ran"
    );
    assert!(!sock.is_empty(), "request went on the wire");
}

#[test]
fn launcher_mouse_click_on_footer_is_unconsumed() {
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true)]);
    sync_catalog(&mut v);
    let l = v.launcher.as_ref().unwrap();
    let pw = v.panel_w() as usize;
    let chrome = v.bottom_row_is_chrome() as usize;
    let body = 24 - chrome;
    let (total, _) = l.dock_layout(body, pw - 1);
    let top = body - total;
    let footer_row = (top + total - 1) as u16;
    let rep = crate::mouse::MouseReport {
        kind: crate::proto::MouseKind::Press(crate::proto::MouseButton::Left),
        row: footer_row,
        col: 5,
        shift: false,
    };
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let consumed = super::agent_launcher::launcher_mouse(&mut v, rep, &mut sock)
            .await
            .unwrap();
        assert!(!consumed, "the footer row is never the dock's");
    });
}

#[test]
fn launcher_mouse_click_in_message_rect_focuses_the_editor() {
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true)]);
    sync_catalog(&mut v);
    let l = v.launcher.as_ref().unwrap();
    let pw = v.panel_w() as usize;
    let chrome = v.bottom_row_is_chrome() as usize;
    let body = 24 - chrome;
    let (total, _) = l.dock_layout(body, pw - 1);
    let top = body - total;
    let area = RtRect::new(0, top as u16, (pw - 1) as u16, total as u16);
    let rects = l.dock_layout_rects(&v, area);
    let rep = crate::mouse::MouseReport {
        kind: crate::proto::MouseKind::Press(crate::proto::MouseButton::Left),
        row: rects.message.y,
        col: rects.message.x,
        shift: false,
    };
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let consumed = super::agent_launcher::launcher_mouse(&mut v, rep, &mut sock)
            .await
            .unwrap();
        assert!(consumed, "a click in the message rect is consumed");
    });
    assert_eq!(v.launcher.as_ref().unwrap().focus, Focus::Message);
}

#[test]
fn esc_leaves_full_screen_and_retains_the_draft() {
    let mut v = view_with_launcher();
    v.sideline_full = true;
    type_message(&mut v, "keep me");
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, b"\x1b ", &mut sock).await;
    });
    assert!(!v.sideline_full, "Esc leaves full-screen sideline");
    assert!(v.launcher.is_none(), "composer closed");
    assert_eq!(
        v.launcher_closed.as_ref().unwrap().draft.message,
        "keep me",
        "draft retained"
    );
}

#[test]
fn chip_paint_truncates_with_an_ellipsis_inside_its_rect() {
    let area = RtRect::new(0, 0, 6, 1);
    let mut buf = RtBuffer::empty(area);
    let style = RtStyle::new();
    super::agent_launcher::paint_chip(&mut buf, area, "abcdefgh", style, false);
    let painted: String = (0..area.width)
        .map(|x| buf[(x, 0)].symbol().to_string())
        .collect();
    assert_eq!(painted.chars().count(), 6, "paint stays inside the rect");
    assert!(
        painted.ends_with('\u{2026}'),
        "truncated chip elides: {painted}"
    );
}

#[test]
fn caret_survives_chip_truncation() {
    // AC2-EDGE: a chip rect narrower than its label ellipsizes the label and
    // keeps the caret visible - truncate the text, never the caret.
    let area = RtRect::new(0, 0, 5, 1);
    let mut buf = RtBuffer::empty(area);
    let style = RtStyle::new();
    super::agent_launcher::paint_chip(&mut buf, area, "long-label", style, true);
    let painted: String = (0..area.width)
        .map(|x| buf[(x, 0)].symbol().to_string())
        .collect();
    assert_eq!(
        painted.chars().last(),
        Some('\u{25be}'),
        "the caret stays visible: {painted}"
    );
}

#[test]
fn labels_name_their_field_and_never_say_default() {
    // R3 + AC3-UI: every empty pin reads "<harness> decides" (placement:
    // the door's thread default) and NO label anywhere contains the bare
    // word "default" - the operator could not tell which default was which.
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true)]);
    sync_catalog(&mut v);
    if let Some(l) = v.launcher.as_mut() {
        l.draft.expanded = true;
    }
    let l = v.launcher.as_ref().unwrap();
    let texts: Vec<String> = l.chip_texts(&v).into_iter().map(|(_, s)| s).collect();
    let joined = texts.join(" | ");
    for want in [
        "harness: claude",
        "model: claude decides",
        "effort: claude decides",
        "permission: claude decides",
        "where: thread",
    ] {
        assert!(
            texts.iter().any(|t| t.contains(want)),
            "want {want:?} in {joined}"
        );
    }
    assert!(
        !joined.split_whitespace().any(|w| w == "default"),
        "no bare default: {joined}"
    );
}

#[test]
fn model_picker_lists_catalog_rows_and_picking_one_pins_the_row() {
    // AC3-HP: the model picker lists "<harness> decides", then the claude
    // routing rows, then free text; picking a row sets the chip to the ROW
    // name (and change 5's wire field rides the row's model id).
    let mut v = view_with_launcher();
    let mut rows = catalog(&[("claude", true, true)]).unwrap();
    if let CatalogOutcome::Ok(choices, _) = &mut rows {
        choices[0].models = vec![
            super::agent_launcher::ModelChoice {
                name: "claude-opus-5".into(),
                model: "claude-opus-5".into(),
                verdict: "ok".into(),
            },
            super::agent_launcher::ModelChoice {
                name: "zai-flash".into(),
                model: "glm-5.3-flash[1m]".into(),
                verdict: "ok".into(),
            },
        ];
    }
    v.launcher_catalog = Some(rows);
    sync_catalog(&mut v);
    let l = v.launcher.as_mut().unwrap();
    l.focus = Focus::Model;
    assert!(super::agent_launcher::open_picker(l, &v), "picker opens");
    let picker = l.picker.as_ref().unwrap();
    let labels: Vec<String> = picker
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            crate::popup::PopupRow::Entry { label, enabled, .. } if *enabled => Some(label.clone()),
            _ => None,
        })
        .collect();
    assert!(
        labels.contains(&"claude decides".to_string()) && labels.contains(&"zai-flash".to_string()),
        "picker lists decides + rows: {labels:?}"
    );
    // Pick zai-flash: the chip reads `model: zai-flash`, the draft carries
    // the row's model id.
    let target = picker
        .popup
        .rows
        .iter()
        .position(
            |r| matches!(r, crate::popup::PopupRow::Entry { label, .. } if label == "zai-flash"),
        )
        .unwrap();
    let mut l = v.launcher.as_mut().unwrap();
    l.picker.as_mut().unwrap().popup.select(target);
    let action = l
        .picker
        .as_ref()
        .unwrap()
        .actions
        .get(target)
        .cloned()
        .flatten();
    super::agent_launcher::apply_picker_action(&mut l, &v, action.unwrap(), 0);
    v.launcher = Some(l.clone());
    let l = v.launcher.as_ref().unwrap();
    assert!(l.picker.is_none(), "commit closes the picker");
    assert_eq!(l.draft.model, "glm-5.3-flash[1m]");
    assert_eq!(l.draft.model_row.as_deref(), Some("zai-flash"));
}

#[test]
fn degraded_inventory_leaves_free_text_and_names_the_failure() {
    // AC3-EDGE: when the inventory read fails, the model picker shows a
    // disabled entry carrying the reason and a working `type a model...`;
    // the harness chip still lists every catalog harness.
    let mut v = view_with_launcher();
    v.launcher_catalog = Some(CatalogOutcome::Ok(
        vec![HarnessChoice {
            name: "claude".into(),
            native: true,
            installed: true,
            models: Vec::new(),
            efforts: Some(Vec::new()),
            permission_modes: Some(Vec::new()),
        }],
        Some("routing inventory unavailable".into()),
    ));
    sync_catalog(&mut v);
    let l = v.launcher.as_mut().unwrap();
    l.focus = Focus::Model;
    assert!(super::agent_launcher::open_picker(l, &v));
    let picker = l.picker.as_ref().unwrap();
    let disabled: Vec<&str> = picker
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            crate::popup::PopupRow::Entry {
                label,
                enabled: false,
                ..
            } => Some(label.as_str()),
            _ => None,
        })
        .collect();
    let enabled: Vec<&str> = picker
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            crate::popup::PopupRow::Entry {
                label,
                enabled: true,
                ..
            } => Some(label.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        disabled.iter().any(|d| d.contains("unavailable")),
        "the failure is named: {disabled:?}"
    );
    assert!(
        enabled.contains(&"type a model..."),
        "free text still works: {enabled:?}"
    );
    assert!(
        !v.launcher.as_ref().unwrap().draft.harnesses.is_empty(),
        "harness choices survive the degraded model list"
    );
}

#[test]
fn placement_picker_offers_thread_views_and_the_one_pane_entry() {
    // The operator's placement ruling: a thread view, not a pane. The chip
    // offers thread (the door default), thread split beside, thread new tab
    // - each sent as --substrate thread --portal N with --split/--tab - and
    // keeps one pane entry for args a thread cannot carry.
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true)]);
    sync_catalog(&mut v);
    let l = v.launcher.as_mut().unwrap();
    l.focus = Focus::Placement;
    l.draft.expanded = true;
    assert!(super::agent_launcher::open_picker(l, &v));
    let labels: Vec<String> = l
        .picker
        .as_ref()
        .unwrap()
        .popup
        .rows
        .iter()
        .filter_map(|r| match r {
            crate::popup::PopupRow::Entry { label, .. } => Some(label.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        labels,
        vec![
            "thread",
            "thread split beside",
            "thread new tab",
            "pane: active tab",
        ]
    );
    // Committing split beside records the placement; request() turns it
    // into --substrate thread --portal N --split right.
    let mut l = v.launcher.as_mut().unwrap();
    super::agent_launcher::apply_picker_action(
        &mut l,
        &v,
        super::agent_launcher::PickerAction::Place(
            super::agent_launcher::Placement::ThreadSplitBeside,
        ),
        2,
    );
    v.launcher = Some(l.clone());
    let l = v.launcher.as_ref().unwrap();
    assert_eq!(l.draft.placement_portal, 2);
    let texts: Vec<String> = l.chip_texts(&v).into_iter().map(|(_, s)| s).collect();
    assert!(
        texts
            .iter()
            .any(|t| t.contains("where: thread split beside")),
        "the chip shows the picked view: {texts:?}"
    );
}

#[test]
fn editor_paints_prompt_marker_and_empty_draft_placeholder() {
    // The operator's scope add: a visible input marker before the first
    // message row, and dim placeholder text on an empty draft.
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true)]);
    sync_catalog(&mut v);
    let l = v.launcher.as_ref().unwrap();
    let area = RtRect::new(0, 0, 80, 5);
    let rects = l.dock_layout_rects(&v, area);
    let mut buf = RtBuffer::empty(area);
    l.paint(&v, &mut buf, area);
    let glyph: String = (0..2)
        .map(|x| {
            buf[(rects.message.x + x, rects.message.y)]
                .symbol()
                .to_string()
        })
        .collect();
    assert!(
        glyph.contains('\u{276f}'),
        "prompt marker painted: {glyph:?}"
    );
    let row: String = (0..40)
        .map(|x| buf[(x, rects.message.y)].symbol().to_string())
        .collect();
    assert!(
        row.contains("/fno:target <node> or a task"),
        "placeholder on an empty draft: {row:?}"
    );
    // Typing replaces the placeholder and keeps the marker.
    let mut l = v.launcher.as_mut().unwrap();
    l.focus = Focus::Message;
    drop(l);
    let mut v_mut = v;
    type_message(&mut v_mut, "ship it");
    let l = v_mut.launcher.as_ref().unwrap();
    let mut buf = RtBuffer::empty(area);
    l.paint(&v_mut, &mut buf, area);
    let row: String = (0..40)
        .map(|x| buf[(x, rects.message.y)].symbol().to_string())
        .collect();
    assert!(row.contains("ship it"), "draft paints: {row:?}");
    assert!(!row.contains("/fno:target"), "placeholder gone: {row:?}");
}
