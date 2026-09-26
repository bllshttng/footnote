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

struct WireVersionFixture(std::path::PathBuf);

impl Drop for WireVersionFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn current_wire_fixture() -> (String, WireVersionFixture) {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let session = format!("x50ed-wire-fixture-{}-{nonce}", std::process::id());
    let socket = crate::proto::socket_path(&session).unwrap();
    let sidecar = crate::proto::version_sidecar_path(&socket);
    std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
    std::fs::write(&sidecar, "91\n").unwrap();
    (session, WireVersionFixture(sidecar))
}

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
                models_error: None,
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
        Some(CatalogOutcome::Ok(rows, _)) => rows.iter().map(|r| r.name.clone()).collect(),
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
fn tab_walks_the_field_order() {
    let mut v = view_with_launcher();
    let mut one_provider = catalog(&[("claude", true, true)]).unwrap();
    if let CatalogOutcome::Ok(rows, _) = &mut one_provider {
        rows[0].models.push(super::agent_launcher::ModelChoice {
            name: "anthropic/claude-sonnet".into(),
            model: "anthropic/claude-sonnet".into(),
            route: String::new(),
            provider: Some("anthropic".into()),
            verdict: "ok".into(),
        });
    }
    v.launcher_catalog = Some(one_provider);
    sync_catalog(&mut v);
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
    // With one configured provider the provider tab is absent, so the bar
    // is Harness -> Model -> Project -> Mode -> Where -> Flags -> Message:
    // six tabs land on Message, and the next one wraps to Harness.
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, b"\t\t\t\t\t\t", &mut sock).await;
    });
    assert_eq!(
        v.launcher.as_ref().unwrap().focus,
        Focus::Message,
        "six visible tabs reach Message"
    );
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, b"\t", &mut sock).await;
    });
    assert_eq!(
        v.launcher.as_ref().unwrap().focus,
        Focus::Harness,
        "the tab bar wraps"
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
    // Point the draft at the not-installed one; the submission door refuses
    // it AT SUBMIT with its own reason (the body lists selectable rows only).
    if let Some(l) = v.launcher.as_mut() {
        let idx = l
            .draft
            .harnesses
            .iter()
            .position(|h| h == "hermes")
            .unwrap();
        l.draft.harness_idx = idx;
        l.focus = Focus::Message;
    }
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
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
    // The attempt is remembered BEYOND the sheet (close + reopen).
    close(&mut v);
    open(&mut v);
    assert!(v.launch_attempt.is_some(), "attempt outlives the sheet");
    // Esc is the explicit cancel while pending: the block resolves, the
    // draft survives, the sheet stays open, and launch is possible again.
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, b"\x1b", &mut sock).await;
    });
    assert!(matches!(v.launcher.as_ref().unwrap().phase, Phase::Editing));
    assert!(v.launcher.is_some(), "the canceling Esc does not close");
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
    // ^j (the launch key) is inert until the operator cancels explicitly.
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, b"\n", &mut sock).await;
    });
    assert!(matches!(
        v.launcher.as_ref().unwrap().phase,
        Phase::Unknown { .. }
    ));
    assert!(sock.is_empty(), "nothing went on the wire: {sock:?}");
}

/// A Starting attempt whose update is lost must stay escapable: Esc during
/// Submitting cancels the attempt and returns the sheet to Editing (retry
/// then arms a fresh id, one attempt each).
#[test]
fn submitting_sheet_offers_cancel_and_recovers_to_editing() {
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true)]);
    sync_catalog(&mut v);
    if let Some(l) = v.launcher.as_mut() {
        l.armed = Some(1);
        l.phase = Phase::Submitting { request_id: 1 };
    }
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, b"\x1b", &mut sock).await;
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
fn launcher_click_on_a_tab_switches_to_it() {
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true)]);
    sync_catalog(&mut v);
    let l = v.launcher.as_ref().unwrap();
    let sl = l.sheet_layout(&v).unwrap();
    let (_, r) = *sl
        .tabs
        .iter()
        .find(|(f, _)| *f == Focus::Message)
        .expect("the Message tab is in the bar");
    let rep = crate::mouse::MouseReport {
        kind: crate::proto::MouseKind::Press(crate::proto::MouseButton::Left),
        row: sl.origin.0 + 1 + r.y,
        col: sl.origin.1 + 1 + r.x,
        shift: false,
    };
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let consumed = super::agent_launcher::launcher_mouse(&mut v, rep, &mut sock)
            .await
            .unwrap();
        assert!(consumed, "a click on the sheet is consumed");
    });
    assert_eq!(
        v.launcher.as_ref().unwrap().focus,
        Focus::Message,
        "the click landed on the Message tab"
    );
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
fn the_values_strip_carries_choices_not_labels() {
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true)]);
    sync_catalog(&mut v);
    let strip = v.launcher.as_ref().unwrap().values_strip();
    for want in ["claude", "default", "thread", "claude decides"] {
        assert!(strip.contains(want), "want {want:?} in {strip:?}");
    }
}

#[test]
fn model_tab_lists_catalog_rows_and_picking_one_pins_the_row() {
    // The Model tab body lists the current harness default and that
    // harness's configured rows; committing a row pins its model, provider
    // and route together.
    let mut v = view_with_launcher();
    let mut rows = catalog(&[("claude", true, true)]).unwrap();
    if let CatalogOutcome::Ok(choices, _) = &mut rows {
        choices[0].models = vec![
            super::agent_launcher::ModelChoice {
                name: "claude-opus-5".into(),
                model: "claude-opus-5".into(),
                route: String::new(),
                provider: None,
                verdict: "ok".into(),
            },
            super::agent_launcher::ModelChoice {
                name: "qwen3-coder".into(),
                model: "qwen/qwen3-coder".into(),
                route: "openrouter/qwen/qwen3-coder".into(),
                provider: Some("openrouter".into()),
                verdict: "ok".into(),
            },
        ];
    }
    v.launcher_catalog = Some(rows);
    sync_catalog(&mut v);
    let mut l = v.launcher.take().unwrap();
    l.focus = Focus::Model;
    let (body, actions) = super::agent_launcher::tab_body_rows(&l, &v.launcher_catalog, &v.backlog);
    let enabled: Vec<String> = body
        .iter()
        .filter_map(|r| match r {
            crate::popup::PopupRow::Entry { label, enabled, .. } if *enabled => Some(label.clone()),
            _ => None,
        })
        .collect();
    assert!(
        enabled.contains(&"harness default".to_string())
            && enabled.contains(&"qwen3-coder".to_string()),
        "the Model tab lists the default + configured rows: {enabled:?}"
    );
    // Commit the OpenRouter route straight off the body.
    let target = body
        .iter()
        .position(
            |r| matches!(r, crate::popup::PopupRow::Entry { label, .. } if label == "qwen3-coder"),
        )
        .unwrap();
    let action = actions.get(target).cloned().flatten().unwrap();
    super::agent_launcher::apply_picker_action(&mut l, &v.launcher_catalog, action, 0);
    assert_eq!(l.draft.model, "qwen/qwen3-coder");
    assert_eq!(l.draft.model_row.as_deref(), Some("qwen3-coder"));
    assert_eq!(l.draft.provider, "openrouter");

    // The pick lands in the recent section at the top of the body.
    let (body, _) = super::agent_launcher::tab_body_rows(&l, &v.launcher_catalog, &v.backlog);
    assert!(matches!(
        body.first(),
        Some(crate::popup::PopupRow::Header(section)) if section == "recent"
    ));
    assert!(body.iter().any(
        |row| matches!(row, crate::popup::PopupRow::Entry { label, .. } if label == "qwen3-coder")
    ));
}

#[test]
fn provider_and_model_choices_come_from_configured_rows() {
    let mut v = view_with_launcher();
    let mut rows = catalog(&[("opencode", true, true)]).unwrap();
    if let CatalogOutcome::Ok(choices, _) = &mut rows {
        choices[0].models = super::agent_launcher::parse_opencode_models(
            "openrouter/qwen/qwen3-coder\nlocal/llama-3.3\n",
        );
    }
    v.launcher_catalog = Some(rows);
    sync_catalog(&mut v);

    let mut l = v.launcher.take().unwrap();
    l.focus = Focus::Provider;
    let (body, actions) = super::agent_launcher::tab_body_rows(&l, &v.launcher_catalog, &v.backlog);
    let provider_row = body
        .iter()
        .position(|row| matches!(row, crate::popup::PopupRow::Entry { label, .. } if label == "openrouter"))
        .unwrap();
    assert!(body.iter().all(|row| {
        !matches!(row, crate::popup::PopupRow::Entry { label, .. } if label == "zai")
    }));
    let action = actions[provider_row].clone().unwrap();
    super::agent_launcher::apply_picker_action(&mut l, &v.launcher_catalog, action, 0);
    assert_eq!(l.draft.provider, "openrouter");

    l.focus = Focus::Model;
    let (body, actions) = super::agent_launcher::tab_body_rows(&l, &v.launcher_catalog, &v.backlog);
    let model_row = body
        .iter()
        .position(|row| matches!(row, crate::popup::PopupRow::Entry { label, .. } if label == "openrouter/qwen/qwen3-coder"))
        .unwrap();
    let action = actions[model_row].clone().unwrap();
    super::agent_launcher::apply_picker_action(&mut l, &v.launcher_catalog, action, 0);
    let request = l.draft.request(3);
    assert_eq!(request.harness, "opencode");
    assert_eq!(l.draft.provider, "openrouter");
    assert_eq!(
        request.provider, None,
        "OpenCode carries provider/model in one id"
    );
    assert_eq!(
        request.model.as_deref(),
        Some("openrouter/qwen/qwen3-coder")
    );
}

#[test]
fn account_rows_supply_model_and_provider_options() {
    let configured = r#"{"value":[
      {"id":"anthropic-main","harness":"claude","model_name":"sonnet"},
      {"id":"openrouter-main","harness":"claude","route_provider_id":"openrouter","model_name":"qwen/qwen3-coder"},
      {"id":"codex-main","harness":"codex"}
    ]}"#;
    let by_harness = super::agent_launcher::parse_configured_account_models(configured).unwrap();
    let claude = &by_harness["claude"];
    assert_eq!(
        claude.len(),
        2,
        "only accounts with configured models appear"
    );
    assert!(claude
        .iter()
        .any(|model| { model.model == "sonnet" && model.provider.is_none() }));
    assert!(claude.iter().any(|model| {
        model.model == "qwen/qwen3-coder" && model.provider.as_deref() == Some("openrouter")
    }));
    assert!(
        !by_harness.contains_key("codex"),
        "no model row is invented"
    );

    let mut v = view_with_launcher();
    let mut catalog = catalog(&[("claude", true, true)]).unwrap();
    if let CatalogOutcome::Ok(rows, _) = &mut catalog {
        rows[0].models = claude.clone();
    }
    v.launcher_catalog = Some(catalog);
    sync_catalog(&mut v);
    let l = v.launcher.as_ref().unwrap();
    let sl = l.sheet_layout(&v).unwrap();
    assert!(
        sl.tabs.iter().any(|(f, _)| *f == Focus::Provider),
        "two providers give the bar a Provider tab"
    );
}

/// The regression that forced the tab rewrite: the unavailable row's LABEL
/// used to be ellipsized to fit the long error hint, so "model list
/// unavailable" never reached the screen in CI. Labels stay whole; the
/// hint truncates instead; the selection spans the full inner width.
#[test]
fn model_tab_label_survives_a_long_hint_and_selects_full_width() {
    let mut v = view_with_launcher();
    let mut choices = catalog(&[("claude", true, true)]).unwrap();
    if let CatalogOutcome::Ok(rows, models_err) = &mut choices {
        *models_err =
            Some("account records unavailable: Usage: fno-py config get [OPTIONS] {key}".into());
        let _ = rows;
    }
    v.launcher_catalog = Some(choices);
    sync_catalog(&mut v);
    if let Some(l) = v.launcher.as_mut() {
        l.focus = Focus::Model;
    }
    let l = v.launcher.as_ref().unwrap();
    let (body, _) = super::agent_launcher::tab_body_rows(l, &v.launcher_catalog, &v.backlog);
    assert!(body.iter().any(
        |row| matches!(row, crate::popup::PopupRow::Entry { label, .. } if label == "model list unavailable")
    ));
    let sl = l.sheet_layout(&v).unwrap();
    let inner_w = sl.framed_w.saturating_sub(2);
    let (rows_n, cols) = (v.term.0 as usize, v.term.1 as usize);
    let mut cells = vec![crate::proto::Cell::default(); rows_n * cols];
    l.paint_sheet(&v, &mut cells, rows_n, cols, &sl);
    let (oy, ox) = (sl.origin.0 as usize + 1, sl.origin.1 as usize + 1);
    let mut label_seen = false;
    for (_, r) in &sl.row_rects {
        let text: String = (0..inner_w)
            .map(|x| cells[(oy + r.y as usize) * cols + ox + r.x as usize + x].c)
            .collect();
        if text.contains("model list unavailable") {
            label_seen = true;
        }
    }
    assert!(label_seen, "the unavailable label renders whole");
    // The selection is a FULL-WIDTH rect: it spans the sheet's whole inner
    // width (the old popover styled only the row's content width).
    let r = sl.selected.expect("a list body always has a selection");
    assert_eq!(r.width as usize, inner_w, "the selection spans the body");
}

#[test]
fn degraded_inventory_names_the_failure_and_keeps_defaults() {
    // When the inventory read fails, the model list shows a disabled entry
    // carrying the reason and the harness default stays launchable;
    // No model row is fabricated when the configured inventory is absent.
    let mut v = view_with_launcher();
    v.launcher_catalog = Some(CatalogOutcome::Ok(
        vec![HarnessChoice {
            name: "claude".into(),
            native: true,
            installed: true,
            models: Vec::new(),
            models_error: None,
            efforts: Some(Vec::new()),
            permission_modes: Some(Vec::new()),
        }],
        Some("routing inventory unavailable".into()),
    ));
    sync_catalog(&mut v);
    if let Some(l) = v.launcher.as_mut() {
        l.focus = Focus::Model;
    }
    let l = v.launcher.as_ref().unwrap();
    let (rows, _) = super::agent_launcher::tab_body_rows(l, &v.launcher_catalog, &v.backlog);
    let disabled: Vec<&str> = rows
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
    let enabled: Vec<&str> = rows
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
        enabled.contains(&"harness default"),
        "the default row still launches: {enabled:?}"
    );
    assert!(
        !enabled.iter().any(|d| d.contains("type a model")),
        "no invented model row: {enabled:?}"
    );
    assert!(
        !v.launcher.as_ref().unwrap().draft.harnesses.is_empty(),
        "harness choices survive the degraded model list"
    );
}

#[test]
fn extra_flags_chip_parses_argv_without_shell_expansion() {
    let mut v = view_with_launcher();
    let (session, _wire_fixture) = current_wire_fixture();
    v.session = session;
    v.launcher_catalog = catalog(&[("claude", true, true)]);
    sync_catalog(&mut v);
    v.launcher.as_mut().unwrap().focus = Focus::ExtraFlags;
    let mut sock = Vec::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(
            &mut v,
            b"--agent abc --name 'two words' --label $HOME",
            &mut sock,
        )
        .await;
    });
    let draft = &v.launcher.as_ref().unwrap().draft;
    assert!(draft.extra_flags.starts_with("--agent abc"));
    let strip = v.launcher.as_ref().unwrap().values_strip();
    assert!(
        strip.contains("--agent abc"),
        "the flags value rides the values strip: {strip:?}"
    );

    v.launcher.as_mut().unwrap().focus = Focus::Message;
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, b"\r", &mut sock).await;
    });
    let mut wire = std::io::Cursor::new(sock);
    let crate::proto::ClientMsg::AgentLaunch(request) =
        crate::proto::read_msg_sync(&mut wire).unwrap()
    else {
        panic!("composer wrote a different client message");
    };
    assert_eq!(
        request.extra_flags,
        vec!["--agent", "abc", "--name", "two words", "--label", "$HOME"]
    );
}

#[test]
fn launch_extra_axes_require_a_stamped_compatible_server() {
    assert!(!super::agent_launcher::launch_extra_axes_supported(None));
    assert!(!super::agent_launcher::launch_extra_axes_supported(Some(
        90
    )));
    assert!(super::agent_launcher::launch_extra_axes_supported(Some(91)));
    assert!(super::agent_launcher::launch_extra_axes_supported(Some(92)));
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
    if let Some(l) = v.launcher.as_mut() {
        l.focus = Focus::Placement;
    }
    let l = v.launcher.as_ref().unwrap();
    let (rows, _) = super::agent_launcher::tab_body_rows(l, &v.launcher_catalog, &v.backlog);
    let labels: Vec<String> = rows
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
    let mut l = v.launcher.take().unwrap();
    super::agent_launcher::apply_picker_action(
        &mut l,
        &v.launcher_catalog,
        super::agent_launcher::PickerAction::Place(
            super::agent_launcher::Placement::ThreadSplitBeside,
        ),
        2,
    );
    v.launcher = Some(l.clone());
    let l = v.launcher.as_ref().unwrap();
    assert_eq!(l.draft.placement_portal, 2);
    let strip = l.values_strip();
    assert!(
        strip.contains("thread split beside"),
        "the values strip shows the picked view: {strip:?}"
    );
}

#[test]
fn editor_paints_prompt_marker_and_empty_draft_placeholder() {
    // The operator's scope add: a visible input marker before the first
    // message row, and dim placeholder text on an empty draft.
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true)]);
    sync_catalog(&mut v);
    if let Some(l) = v.launcher.as_mut() {
        l.focus = Focus::Message;
    }
    let l = v.launcher.as_ref().unwrap();
    let sl = l.sheet_layout(&v).unwrap();
    let inner_w = sl.framed_w.saturating_sub(2) as usize;
    let (rows_n, cols) = (v.term.0 as usize, v.term.1 as usize);
    let mut cells = vec![crate::proto::Cell::default(); rows_n * cols];
    l.paint_sheet(&v, &mut cells, rows_n, cols, &sl);
    let (oy, ox) = (sl.origin.0 as usize + 1, sl.origin.1 as usize + 1);
    let marker = (0..2)
        .map(|x| cells[(oy + sl.message.y as usize) * cols + ox + x].c)
        .collect::<String>();
    assert!(
        marker.contains('\u{276f}'),
        "prompt marker painted: {marker:?}"
    );
    let row: String = (0..inner_w)
        .map(|x| cells[(oy + sl.message.y as usize) * cols + ox + x].c)
        .collect();
    assert!(
        row.contains("/fno:target <node> or a task"),
        "placeholder on an empty draft: {row:?}"
    );
    // Typing replaces the placeholder and keeps the marker.
    type_message(&mut v, "ship it");
    let l = v.launcher.as_ref().unwrap();
    let sl = l.sheet_layout(&v).unwrap();
    let mut cells = vec![crate::proto::Cell::default(); rows_n * cols];
    l.paint_sheet(&v, &mut cells, rows_n, cols, &sl);
    let row: String = (0..inner_w)
        .map(|x| cells[(oy + sl.message.y as usize) * cols + ox + x].c)
        .collect();
    assert!(row.contains("ship it"), "draft paints: {row:?}");
    assert!(!row.contains("/fno:target"), "placeholder gone: {row:?}");
}

#[test]
fn typing_on_a_list_tab_filters_the_body() {
    // The harness tab narrows its configured choices in place; Backspace
    // widens the list again and clearing restores every row.
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true), ("codex", true, true)]);
    sync_catalog(&mut v);
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
    // Type `cod`: only the configured codex row survives.
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, b"cod", &mut sock).await;
    });
    let read = |v: &View| -> Vec<String> {
        let l = v.launcher.as_ref().unwrap();
        let (rows, _) = super::agent_launcher::tab_body_rows(l, &v.launcher_catalog, &v.backlog);
        rows.iter()
            .filter_map(|r| match r {
                crate::popup::PopupRow::Entry { label, .. } => Some(label.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        read(&v),
        vec!["codex"],
        "the query narrows the body: {:?}",
        read(&v)
    );
    assert_eq!(
        v.launcher.as_ref().unwrap().filter,
        "cod",
        "the query is live"
    );
    // Backspace once: the query `co` still narrows to codex.
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, &[0x7f], &mut sock).await;
    });
    assert_eq!(read(&v), vec!["codex"], "`co` still filters");
    // Clearing the query fully restores every row.
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, &[0x7f, 0x7f], &mut sock).await;
    });
    assert_eq!(read(&v).len(), 2, "widened");
}

#[test]
fn enter_commits_the_highlighted_row_under_an_active_filter() {
    // Filtering keeps the target/action mapping on the actual harness row.
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true), ("codex", true, true)]);
    sync_catalog(&mut v);
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
    // `co` leaves codex as the selected harness target.
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, b"co", &mut sock).await;
    });
    {
        let l = v.launcher.as_ref().unwrap();
        assert_eq!(l.filter, "co", "the query is live");
        let (rows, actions) =
            super::agent_launcher::tab_body_rows(l, &v.launcher_catalog, &v.backlog);
        assert_eq!(rows.len(), 1, "narrowed to one row: {rows:?}");
        let action = actions.first().cloned().flatten();
        assert!(
            matches!(
                action,
                Some(super::agent_launcher::PickerAction::SetHarness(ref h)) if h == "codex"
            ),
            "the highlighted row resolves SetHarness(codex): {action:?}"
        );
    }
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, &[0x0d], &mut sock).await;
    });
    let l = v.launcher.as_ref().unwrap();
    assert_eq!(l.draft.harness(), "codex", "the highlighted row committed");
}

#[test]
fn at_opens_the_node_picker_and_picking_inserts_the_id() {
    // Change 7: `@` in the message opens a node picker over the layout's
    // backlog cards; the glyph never lands in the draft; the pick inserts
    // `<id> ` at the cursor.
    let mut v = view_with_launcher();
    v.launcher_catalog = catalog(&[("claude", true, true)]);
    sync_catalog(&mut v);
    if let Some(l) = v.launcher.as_mut() {
        l.focus = Focus::Message;
    }
    // One backlog card for the picker to list (the fixture ships none).
    v.backlog = vec![crate::proto::BacklogCard {
        id: "x-6233".into(),
        slug: "a-node".into(),
        priority: "p1".into(),
        state: crate::proto::CardState::Ready,
        pane_id: None,
        attach_id: None,
        where_hint: None,
        project: None,
        lane: None,
        plan_path: None,
        head: false,
    }];
    type_message(&mut v, "plan ");
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let _ = super::agent_launcher::launcher_keys(&mut v, b"@", &mut sock).await;
    });
    let picker = v.launcher.as_ref().unwrap().picker.as_ref().unwrap();
    assert_eq!(
        picker.field,
        Focus::Message,
        "the node picker rides the message field"
    );
    let entry_count = picker
        .popup
        .rows
        .iter()
        .filter(|r| matches!(r, crate::popup::PopupRow::Entry { .. }))
        .count();
    assert_eq!(
        v.launcher.as_ref().unwrap().draft.message,
        "plan ",
        "the @ never lands in the draft"
    );
    // Pick the first card: its id lands at the cursor.
    let first_id: String = picker
        .actions
        .iter()
        .find_map(|a| match a {
            Some(super::agent_launcher::PickerAction::InsertNode(id)) => Some(id.clone()),
            _ => None,
        })
        .expect("the fixture carries at least one InsertNode row");
    assert!(entry_count >= 1, "cards list: {entry_count}");
    let mut l = v.launcher.take().unwrap();
    let action = super::agent_launcher::PickerAction::InsertNode(first_id.clone());
    super::agent_launcher::apply_picker_action(&mut l, &v.launcher_catalog, action, 0);
    v.launcher = Some(l);
    assert_eq!(
        v.launcher.as_ref().unwrap().draft.message,
        format!("plan {first_id} "),
        "the node id lands at the cursor"
    );
}

#[test]
fn open_with_binds_message_project_and_node() {
    // AC6-HP: the board prefill lands the message, the cursor at its end,
    // the node's project (appended when it was not a candidate) and the
    // node binding; the wire request carries the node. The phase resets to
    // Editing so the operator's next Launch submits fresh.
    let mut v = plain_view();
    open(&mut v);
    super::agent_launcher::open_with(
        &mut v,
        "/fno:target x-1".into(),
        Some("/r/footnote"),
        "x-1".into(),
    )
    .expect("a fresh draft yields");
    let l = v.launcher.as_ref().unwrap();
    assert_eq!(l.draft.message, "/fno:target x-1");
    assert_eq!(l.draft.cursor_chars, "/fno:target x-1".chars().count());
    let idx = l.draft.project_idx;
    assert_eq!(l.draft.projects[idx], "/r/footnote");
    assert_eq!(l.draft.node.as_deref(), Some("x-1"));
    assert_eq!(l.draft.request(9).node.as_deref(), Some("x-1"));
    assert_eq!(l.phase, Phase::Editing);
    assert_eq!(l.focus, Focus::Harness);
}

#[test]
fn open_with_keeps_a_retained_nonempty_draft() {
    // AC7-EDGE: a kept draft with typed text is never overwritten; the
    // error names the way out and the draft is unchanged.
    let mut v = plain_view();
    open(&mut v);
    if let Some(l) = v.launcher.as_mut() {
        l.draft.message = "fix the flake".into();
        l.draft.revision += 1;
    }
    let err = super::agent_launcher::open_with(
        &mut v,
        "/fno:target x-1".into(),
        Some("/r/footnote"),
        "x-1".into(),
    )
    .expect_err("a held draft refuses");
    assert!(
        err.contains("holds a draft"),
        "the refusal names the kept draft: {err}"
    );
    let l = v.launcher.as_ref().unwrap();
    assert_eq!(l.draft.message, "fix the flake");
    assert_eq!(l.draft.node, None);
}

#[test]
fn open_with_keeps_the_draft_while_an_attempt_is_in_flight() {
    // The in-flight guard: even an EMPTY draft does not yield while an
    // owned attempt is Starting. A seedless launch's outcome must fold
    // onto the dock it belongs to, never onto a fresh board prefill.
    let mut v = plain_view();
    open(&mut v);
    if let Some(l) = v.launcher.as_mut() {
        l.armed = Some(1);
    }
    apply_launch_update(
        &mut v,
        AgentLaunchUpdate {
            request_id: 1,
            state: LaunchState::Starting,
        },
    );
    let err =
        super::agent_launcher::open_with(&mut v, "/fno:target x-1".into(), None, "x-1".into())
            .expect_err("an in-flight attempt holds the draft");
    assert!(err.contains("holds a draft"), "err: {err}");
    let l = v.launcher.as_ref().unwrap();
    assert_eq!(l.phase, Phase::Submitting { request_id: 1 });
    assert_eq!(l.draft.node, None);
}

#[test]
fn open_with_replaces_after_a_launched_attempt() {
    // AC8-EDGE: a terminal `Launched` attempt makes way for another node's
    // prefill; the phase is Editing again.
    let mut v = plain_view();
    open(&mut v);
    if let Some(l) = v.launcher.as_mut() {
        l.draft.message = "/fno:target x-9".into();
        l.armed = Some(1);
    }
    apply_launch_update(
        &mut v,
        AgentLaunchUpdate {
            request_id: 1,
            state: LaunchState::Launched {
                name: "w".into(),
                pane: Some(3),
                seed_delivered: Some(true),
            },
        },
    );
    super::agent_launcher::open_with(&mut v, "/fno:target x-1".into(), None, "x-1".into())
        .expect("a launched draft yields");
    let l = v.launcher.as_ref().unwrap();
    assert_eq!(l.draft.message, "/fno:target x-1");
    assert_eq!(l.draft.node.as_deref(), Some("x-1"));
    assert_eq!(l.phase, Phase::Editing);
}

#[test]
fn request_drops_a_stale_node_binding_when_the_message_moves() {
    // AC9-EDGE: the binding rides only while the message's second word
    // (sentence punctuation trimmed) still names the node. A retarget, a
    // prose rewrite or an erasure never binds a stale node; appended
    // prose after the same id keeps it.
    let mut v = plain_view();
    open(&mut v);
    super::agent_launcher::open_with(&mut v, "/fno:target x-1".into(), None, "x-1".into())
        .expect("a fresh draft yields");
    let cases: &[(&str, Option<&str>)] = &[
        ("/fno:target x-2", None),
        ("fix the flake", None),
        ("", None),
        ("/fno:target x-1 focus on the flake", Some("x-1")),
        ("/fno:target x-1.", Some("x-1")),
    ];
    for (message, want) in cases {
        if let Some(l) = v.launcher.as_mut() {
            l.draft.message = message.to_string();
        }
        let req = v.launcher.as_ref().unwrap().draft.request(1);
        assert_eq!(
            req.node.as_deref(),
            *want,
            "message {message:?} binds {want:?}"
        );
    }
}
