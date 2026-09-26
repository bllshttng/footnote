//! The which-key modal's rows: the help surface for the prefix-chord table.
//!
//! Moved out of `client.rs` (file budget) beside the note it carries. The rows
//! come from `key_bindings`, the dispatcher's own table, so help can never
//! advertise an action the chord path cannot run.
//!
//! The modal grows one row per binding, and a test pins its trailing notes
//! above the 72-row fold. That is a real budget. A new binding spends it, and
//! the next one that overflows should reclaim a line here rather than move the
//! pin. (The global (no prefix) section spent five and moved the pin from 64.)

use super::*;

/// Build the modal's rows from [`key_bindings`] (the dispatcher's own table):
/// title, then each section's header + its bindings (key leading, action right),
/// its display-only meta rows, then a footer hint. `row_events` runs parallel to
/// `popup.rows` so a selected row's chord is one lookup away.
pub(crate) fn build_keys_modal() -> KeysModal {
    let mut rows: Vec<PopupRow> = Vec::new();
    let mut events: Vec<Option<Event>> = Vec::new();
    let mut add = |row: PopupRow, ev: Option<Event>| {
        rows.push(row);
        events.push(ev);
    };
    // The tail's scroll hint rides the title line: the modal grows one row
    // per binding, and the x7683 pin holds the notes above the 72-row fold,
    // so every row here is paid for. (The composer bindings spent this one.)
    add(
        PopupRow::Header("keybinds · esc close · wheel/pgup/pgdn scroll · ⏎ runs".into()),
        None,
    );
    let bindings = key_bindings();
    for section in [
        KeySection::GlobalNoPrefix,
        KeySection::Global,
        KeySection::Navigation,
        KeySection::WorkspacesTabs,
        KeySection::Panes,
        KeySection::SidelineRows,
    ] {
        // The Global header is reclaimed, not moved: the modal grows one row
        // per binding and the x7683 pin holds the notes above the 72-row
        // fold, and the title line already says what this list is. The
        // composer bindings spent the budget that removed it.
        if section != KeySection::Global {
            add(PopupRow::Header(section.title().into()), None);
        }
        for kb in bindings.iter().filter(|kb| kb.section == section) {
            add(
                PopupRow::Entry {
                    glyph: kb.disp.to_string(),
                    label: kb.label.to_string(),
                    // The stable id `[mux.keys]` names, beside the key it
                    // rebinds. The config contract promises this modal lists
                    // them.
                    hint: kb.action.to_string(),
                    enabled: true,
                },
                Some(kb.event.clone()),
            );
        }
        // Display-only rows (1-9 select tab, prefix-prefix literal): selectable
        // so the reference shows them, but not single-event chords, so Enter
        // BELs. No action id - `chord()` handles them structurally.
        for (disp, label, _) in meta_rows().iter().filter(|(_, _, s)| *s == section) {
            add(
                PopupRow::Entry {
                    glyph: disp.clone(),
                    label: label.clone(),
                    hint: String::new(),
                    enabled: true,
                },
                None,
            );
        }
    }
    // The right-click config note. The mux side works whenever the
    // bytes arrive (FNO_MUX_MOUSE_TRACE proves it either way); the terminals
    // that never send them are named so the operator configures the terminal,
    // or reaches for the no-config paths, instead of reading a dead feature.
    //
    // No Rule above this note. The pin below holds the whole block over the
    // 72-row fold, and the modal grows one row per binding, so a separator
    // here costs the same line a real key does. The header band already
    // separates it. Every new binding spends this budget; the next one that
    // overflows should reclaim a line rather than move the pin.
    // (The composer bindings spent it: the rule and the Terminal.app/Ghostty
    // lines were reclaimed to keep the pin honest.)
    add(
        PopupRow::Header("right-click works only where the terminal forwards it".into()),
        None,
    );
    // One line per terminal family, settings named not values: which value
    // restores forwarding is untested here, and a config line this text
    // cannot vouch for is the kind of confident wrong answer that cost a
    // whole diagnosis round already. Lines stay short: WIDTH_CAP is 60 and
    // a setting name past it truncates into a wrong hint. The tmux `m` /
    // long-press fallbacks live in the right-click meta row above, so the
    // tmux line carries only the setting it names.
    add(
        PopupRow::Header("Terminal.app never · iTerm2: report mouse · tmux: mouse off".into()),
        None,
    );
    add(
        PopupRow::Header("Ghostty binds it too · see right-click-action".into()),
        None,
    );
    // The glyph legend rides the modal tail, after the notes: the
    // x7683 pin holds the notes above the 72-row fold, and the legend is
    // reference material the same scroll reaches. Generated from the same
    // lattice table the rows and the header band render - one source, so
    // the modal cannot drift from what the screen draws. Inert rows.
    for row in glyph_legend::legend_rows() {
        add(row, None);
    }
    KeysModal {
        popup: Popup::new(rows, Anchor::Center),
        row_events: events,
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
