//! The which-key modal's rows: the help surface for the prefix-chord table.
//!
//! Moved out of `client.rs` (file budget) beside the note it carries. The rows
//! come from `key_bindings`, the dispatcher's own table, so help can never
//! advertise an action the chord path cannot run.
//!
//! The modal renders as a plain-body popup: no inverse ground, the key column
//! bold accent, one filled band on the cursor row only, sections spaced by
//! blank lines, a 60%-of-terminal viewport with a scrollbar, and a live `/`
//! filter over key, label, and action id.

use super::*;

/// Build the modal from [`key_bindings`] (the dispatcher's own table),
/// unfiltered.
pub(crate) fn build_keys_modal() -> KeysModal {
    keys_modal_with_filter(None)
}

/// The modal's rows for a filter query: each section's header + its bindings
/// (key left in a fixed-width column, action right) + its display-only meta
/// rows, sections spaced by blank lines. `row_events` runs parallel to
/// `popup.rows` so a selected row's chord is one lookup away. A `Some` query
/// keeps only sections with a matching row and swaps the tail notes/legend for
/// nothing - the filter answers "where is this key", not "what else is here".
pub(crate) fn keys_modal_with_filter(filter: Option<&str>) -> KeysModal {
    let query = filter.map(str::to_lowercase);
    let matches = |hay: &str| {
        query
            .as_deref()
            .is_none_or(|q| hay.to_lowercase().contains(q))
    };
    let mut rows: Vec<PopupRow> = Vec::new();
    let mut events: Vec<Option<Event>> = Vec::new();
    let mut any_row = false;
    let mut edit_row: Option<usize> = None;
    // The prefix line the settings table used to lead with: the whole
    // table hangs off it, so it is the table's first row (US1). Inert
    // (LD7): a selectable prefix line would take the first selection and
    // answer Enter with a bell against the subtitle's promise.
    rows.push(PopupRow::Entry {
        glyph: crate::keys::prefix_display(),
        label: "prefix".into(),
        hint: String::new(),
        enabled: false,
    });
    events.push(None);
    let bindings = key_bindings();
    for section in [
        KeySection::GlobalNoPrefix,
        KeySection::Global,
        KeySection::Navigation,
        KeySection::WorkspacesTabs,
        KeySection::Panes,
        KeySection::SidelineRows,
        KeySection::Messages,
    ] {
        let mut section_rows: Vec<(PopupRow, Option<Event>)> = Vec::new();
        for kb in bindings.iter().filter(|kb| kb.section == section) {
            if !(matches(&kb.disp) || matches(kb.label) || matches(kb.action)) {
                continue;
            }
            section_rows.push((
                // The action id (`kb.action`) rides the filter, not the row:
                // the id is config grammar, the label is the readable phrase.
                PopupRow::Entry {
                    glyph: kb.disp.to_string(),
                    label: kb.label.to_string(),
                    hint: String::new(),
                    enabled: true,
                },
                Some(kb.event.clone()),
            ));
        }
        // Display-only rows (1-9 select tab, prefix-prefix literal): selectable
        // so the reference shows them, but not single-event chords, so Enter
        // BELs. No action id - `chord()` handles them structurally.
        for (disp, label, _) in meta_rows().iter().filter(|(_, _, s)| *s == section) {
            if !(matches(disp) || matches(label)) {
                continue;
            }
            section_rows.push((
                PopupRow::Entry {
                    glyph: disp.clone(),
                    label: label.clone(),
                    hint: String::new(),
                    enabled: true,
                },
                None,
            ));
        }
        if section_rows.is_empty() {
            continue;
        }
        // One blank line before each section header (the plain-body anatomy's
        // group spacing). An empty Header renders as an inert blank row.
        rows.push(PopupRow::Header(String::new()));
        events.push(None);
        any_row = true;
        rows.push(PopupRow::Header(section.title().into()));
        events.push(None);
        for (row, ev) in section_rows {
            rows.push(row);
            events.push(ev);
            any_row = true;
        }
    }
    if filter.is_some() && !any_row {
        // A filter that matches nothing still renders a body.
        rows.push(PopupRow::Header("no matching binding".into()));
        events.push(None);
    }
    if filter.is_none() {
        // The editor entry, carried from the settings table it replaced:
        // a FullWidth row reads as the button it is, and the label names
        // the editor that actually opens.
        edit_row = Some(rows.len());
        rows.push(PopupRow::FullWidth(format!(
            "[ edit keys in {} ]",
            editor::editor_name()
        )));
        events.push(None);
        // The right-click config note. The mux side works whenever the
        // bytes arrive (FNO_MUX_MOUSE_TRACE proves it either way); the terminals
        // that never send them are named so the operator configures the terminal,
        // or reaches for the no-config paths, instead of reading a dead feature.
        // Its own section, spaced like the binding sections.
        rows.push(PopupRow::Header(String::new()));
        events.push(None);
        rows.push(PopupRow::Header("right click".into()));
        events.push(None);
        rows.push(PopupRow::Header(
            "right-click works only where the terminal forwards it".into(),
        ));
        events.push(None);
        // One line per terminal family, settings named not values: which value
        // restores forwarding is untested here, and a config line this text
        // cannot vouch for is the kind of confident wrong answer that cost a
        // whole diagnosis round already. Lines stay short, so a setting name
        // stays on one line instead of wrapping mid-name.
        rows.push(PopupRow::Header(
            "Terminal.app never · iTerm2: report mouse · tmux: mouse off".into(),
        ));
        events.push(None);
        rows.push(PopupRow::Header(
            "Ghostty binds it too · see right-click-action".into(),
        ));
        events.push(None);
        // The glyph legend rides the modal tail, after the notes: reference
        // material the same scroll reaches. Generated from the same lattice
        // table the rows and the header band render - one source, so the modal
        // cannot drift from what the screen draws. Inert rows.
        for row in glyph_legend::legend_rows() {
            rows.push(row);
            events.push(None);
        }
    }
    // The chrome carries the grammar the old in-body title line used to:
    // title + esc chip, a dim hint under it, and a dim footer naming the
    // scroll and search keys. The body is plain with a 60%-of-terminal
    // viewport, so the table scrolls in a fixed window instead of growing one
    // row per binding.
    let mut popup = Popup::new(rows, Anchor::Center)
        .title("keybindings")
        .width_cap(usize::MAX)
        .plain_body()
        .body_cap_pct(60)
        .footer("j/k scroll · / filter · ⏎ run · esc close");
    popup = match filter {
        // The live query rides the hint line, the composer pickers' grammar.
        Some(q) => popup.subtitle(format!("/{q}")),
        None => popup.subtitle("⏎ runs the selected chord"),
    };
    KeysModal {
        popup,
        row_events: events,
        filter: filter.map(str::to_string),
        edit_row,
    }
}

/// One printable byte while the modal is open, in the modal's own grammar:
/// `/` enters the live filter, j/k move the cursor (and keep scrolling into
/// the inert tail at either end), any other byte falls through to the chord
/// dispatch. Returns whether the modal consumed the byte.
pub(crate) fn keys_modal_byte(view: &mut View, b: u8) -> bool {
    let filtering = view.keys_modal.as_ref().is_some_and(|m| m.filter.is_some());
    if filtering {
        // Filter input: printable bytes edit the query, backspace pops it
        // (an empty backspace exits the filter), anything else is inert. A
        // changed query rebuilds the rows.
        let mut edited = false;
        if let Some(m) = view.keys_modal.as_mut() {
            match b {
                0x7f | 0x08 => {
                    let q = m.filter.as_mut().expect("filtering");
                    if q.pop().is_none() {
                        m.filter = None;
                    }
                    edited = true;
                }
                0x15 => {
                    m.filter.as_mut().expect("filtering").clear();
                    edited = true;
                }
                _ if b.is_ascii_graphic() || b == b' ' => {
                    m.filter.as_mut().expect("filtering").push(b as char);
                    edited = true;
                }
                _ => {}
            }
        }
        if edited {
            view.keys_modal = Some(keys_modal_with_filter(
                view.keys_modal.as_ref().and_then(|m| m.filter.as_deref()),
            ));
        }
        return true;
    }
    match b {
        // The search key: enter filter mode (empty query).
        b'/' => {
            view.keys_modal = Some(keys_modal_with_filter(Some("")));
            true
        }
        // The modal's scroll keys (the footer names them). At either end of
        // the selectable rows the key keeps scrolling, so the inert tail
        // (the right-click notes, the glyph legend) stays keyboard-reachable.
        b'j' | b'k' => {
            let down = b == b'j';
            if let Some(m) = view.keys_modal.as_mut() {
                let before = m.popup.selected();
                m.popup.nav(if down { NavDir::Down } else { NavDir::Up });
                if m.popup.selected() == before {
                    m.popup.scroll_by(if down { 1 } else { -1 });
                }
            }
            view.follow_modal_selection();
            true
        }
        _ => false,
    }
}

/// Which-key modal keys (US3). Esc closes; arrows/pgup scroll+select;
/// Enter/`click` run the selected row; a bound printable key runs immediately
/// through the shared chord dispatch (which-key), an unbound one dismisses. Esc
/// is folded like every other overlay (carried across reads) so a split arrow
/// sequence can never leak its tail into a pane (codex P2). No key ever reaches
/// a pane.
pub(crate) async fn keys_modal_keys(
    view: &mut View,
    scanner: &mut Scanner,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.keys_modal_esc);
    let toks = fold_modal_keys(&mut esc, bytes);
    view.keys_modal_esc = esc;
    for tok in toks {
        if view.keys_modal.is_none() {
            break; // closed mid-chunk: swallow the rest, never forward
        }
        match tok {
            ModalKey::Esc => settings_modal::dismiss_keys_modal(view),
            ModalKey::Up => {
                if let Some(m) = view.keys_modal.as_mut() {
                    m.popup.nav(NavDir::Up);
                }
                view.follow_modal_selection();
            }
            ModalKey::Down => {
                if let Some(m) = view.keys_modal.as_mut() {
                    m.popup.nav(NavDir::Down);
                }
                view.follow_modal_selection();
            }
            ModalKey::Left => {
                if let Some(m) = view.keys_modal.as_mut() {
                    m.popup.nav(NavDir::Left);
                }
            }
            ModalKey::Right => {
                if let Some(m) = view.keys_modal.as_mut() {
                    m.popup.nav(NavDir::Right);
                }
            }
            // The table has no split cells, so the folded Shift+arrow is
            // inert here - swallowed, never leaked and never a dismissal.
            ModalKey::ShiftArrow(_) => {}
            ModalKey::PageUp => {
                let page = (view.term.0 as isize - 2).max(1);
                if let Some(m) = view.keys_modal.as_mut() {
                    m.popup.scroll_by(-page);
                    m.popup.clamp_sel_to_view(view.term); // Enter never runs an off-screen row
                }
            }
            ModalKey::PageDown => {
                let page = (view.term.0 as isize - 2).max(1);
                if let Some(m) = view.keys_modal.as_mut() {
                    m.popup.scroll_by(page);
                    m.popup.clamp_sel_to_view(view.term);
                }
            }
            ModalKey::Enter => {
                if matches!(
                    keys_modal_execute_selected(view, scanner, sock_w).await?,
                    DispatchFlow::Detach
                ) {
                    return Ok(StdinFlow::Detach);
                }
            }
            ModalKey::Byte(b) => {
                if !keys_modal_byte(view, b) {
                    match resolve_chord(b) {
                        // Unbound key dismisses (AC2-EDGE): no action fires.
                        Event::Bell => settings_modal::dismiss_keys_modal(view),
                        // Bound key runs immediately through the SAME dispatch
                        // a typed chord uses (Locked 3), then the modal closes.
                        ev => {
                            view.keys_modal = None;
                            // Parity with a typed chord: modal execution arms any
                            // repeatable event too (the scanner never saw this byte).
                            scanner.arm_if_repeat(&ev, Instant::now());
                            if matches!(
                                dispatch_event(view, ev, sock_w).await?,
                                DispatchFlow::Detach
                            ) {
                                return Ok(StdinFlow::Detach);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(StdinFlow::Continue)
}

/// Run the modal's selected row (Enter/click) through the shared dispatch, then
/// close - a header/meta row with no chord BELs and stays open (nothing ran, so
/// the "execute always closes" invariant is not tripped). Returns the dispatch
/// flow so a detach chord (prefix+d) run from the modal actually detaches.
async fn keys_modal_execute_selected(
    view: &mut View,
    scanner: &mut Scanner,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<DispatchFlow, String> {
    // The edit button is the one row that is not a chord: it opens the key
    // config in $EDITOR and returns to the table (which edit_keys_file
    // rebuilds), the same stay-put the settings table's entry had.
    let on_edit_row = view.keys_modal.as_ref().is_some_and(|m| {
        m.popup
            .selected()
            .is_some_and(|(ri, _)| Some(ri) == m.edit_row)
    });
    if on_edit_row {
        keys_settings::edit_keys_file(view).await;
        return Ok(DispatchFlow::Continue);
    }
    let ev = view.keys_modal.as_ref().and_then(|m| {
        m.popup
            .selected()
            .and_then(|(ri, _)| m.row_events.get(ri).cloned().flatten())
    });
    match ev {
        Some(ev) => {
            view.keys_modal = None;
            // Parity with a typed chord: modal execution arms any repeatable
            // event too (the scanner never saw a key here).
            scanner.arm_if_repeat(&ev, Instant::now());
            dispatch_event(view, ev, sock_w).await
        }
        None => {
            let _ = raw_out(b"\x07");
            Ok(DispatchFlow::Continue)
        }
    }
}

/// One mouse report while the which-key modal is open (US3): hover moves
/// the selection, the wheel scrolls, a left click on a row runs it, a click off
/// the popup dismisses (click-elsewhere).
pub(crate) async fn keys_modal_mouse(
    view: &mut View,
    scanner: &mut Scanner,
    rep: crate::mouse::MouseReport,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    match rep.kind {
        MouseKind::Move => {
            if let Some(t) = view.keys_modal_hit(rep.row, rep.col) {
                if let Some(m) = view.keys_modal.as_mut() {
                    m.popup.select(t);
                }
            }
        }
        MouseKind::WheelUp => {
            if let Some(m) = view.keys_modal.as_mut() {
                m.popup.scroll_by(-3);
            }
        }
        MouseKind::WheelDown => {
            if let Some(m) = view.keys_modal.as_mut() {
                m.popup.scroll_by(3);
            }
        }
        MouseKind::Press(MouseButton::Left) => {
            match view.keys_modal_hit(rep.row, rep.col) {
                Some(t) => {
                    if let Some(m) = view.keys_modal.as_mut() {
                        m.popup.select(t);
                    }
                    if matches!(
                        keys_modal_execute_selected(view, scanner, sock_w).await?,
                        DispatchFlow::Detach
                    ) {
                        return Ok(StdinFlow::Detach);
                    }
                }
                None => {
                    // A click inside the block that hit no target (a header, a border)
                    // is swallowed; only a click OFF the modal dismisses.
                    if !view.keys_modal_block_contains(rep.row, rep.col) {
                        settings_modal::dismiss_keys_modal(view);
                    }
                }
            }
        }
        _ => {}
    }
    Ok(StdinFlow::Continue)
}
