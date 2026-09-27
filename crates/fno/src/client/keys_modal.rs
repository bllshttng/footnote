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
    let bindings = key_bindings();
    for section in [
        KeySection::GlobalNoPrefix,
        KeySection::Global,
        KeySection::Navigation,
        KeySection::WorkspacesTabs,
        KeySection::Panes,
        KeySection::SidelineRows,
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
        // The right-click config note. The mux side works whenever the
        // bytes arrive (FNO_MUX_MOUSE_TRACE proves it either way); the terminals
        // that never send them are named so the operator configures the terminal,
        // or reaches for the no-config paths, instead of reading a dead feature.
        rows.push(PopupRow::Header(
            "right-click works only where the terminal forwards it".into(),
        ));
        events.push(None);
        // One line per terminal family, settings named not values: which value
        // restores forwarding is untested here, and a config line this text
        // cannot vouch for is the kind of confident wrong answer that cost a
        // whole diagnosis round already. Lines stay short: WIDTH_CAP is 60 and
        // a setting name past it truncates into a wrong hint.
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
        .title("keybinds")
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
            // Any esc-close chrome target (footer words, title-bar chip)
            // closes the modal; checked before the entry routers.
            if view
                .keys_modal
                .as_ref()
                .is_some_and(|m| view.chrome_close_hit(&m.popup, rep.row, rep.col))
            {
                view.keys_modal = None;
                return Ok(StdinFlow::Continue);
            }
            match view.keys_modal_hit(rep.row, rep.col) {
                Some(t) => {
                    if let Some(m) = view.keys_modal.as_mut() {
                        m.popup.select(t);
                    }
                    if matches!(
                        super::keys_modal_execute_selected(view, scanner, sock_w).await?,
                        DispatchFlow::Detach
                    ) {
                        return Ok(StdinFlow::Detach);
                    }
                }
                None => {
                    // A click inside the block that hit no target (a header, a border)
                    // is swallowed; only a click OFF the modal dismisses.
                    if !view.keys_modal_block_contains(rep.row, rep.col) {
                        view.keys_modal = None;
                    }
                }
            }
        }
        _ => {}
    }
    Ok(StdinFlow::Continue)
}
