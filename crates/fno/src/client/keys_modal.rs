//! The which-key modal's rows: the help surface for the prefix-chord table.
//!
//! Moved out of `client.rs` (file budget) beside the note it carries. The rows
//! come from `key_bindings`, the dispatcher's own table, so help can never
//! advertise an action the chord path cannot run.
//!
//! The modal grows one row per binding, and a test pins its trailing notes
//! above the 64-row fold. That is a real budget. A new binding spends it, and
//! the next one that overflows should reclaim a line here rather than move the
//! pin.

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
    add(PopupRow::Header("keybinds  ·  esc close".into()), None);
    let bindings = key_bindings();
    for section in [
        KeySection::Global,
        KeySection::Navigation,
        KeySection::WorkspacesTabs,
        KeySection::Panes,
        KeySection::SidelineRows,
    ] {
        add(PopupRow::Header(section.title().into()), None);
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
    add(PopupRow::Rule, None);
    add(
        PopupRow::Header("scroll wheel · pgup/pgdn · ⏎/click/tap runs".into()),
        None,
    );
    // (x-7683) The right-click config note. The mux side works whenever the
    // bytes arrive (FNO_MUX_MOUSE_TRACE proves it either way); the terminals
    // that never send them are named so the operator configures the terminal,
    // or reaches for the no-config paths, instead of reading a dead feature.
    //
    // No Rule above this note. The pin below holds the whole block over the
    // 64-row fold, and the modal grows one row per binding, so a separator
    // here costs the same line a real key does. The header band already
    // separates it. Every new binding spends this budget; the next one that
    // overflows should reclaim a line rather than move the pin.
    add(
        PopupRow::Header("right-click works only where the terminal forwards it".into()),
        None,
    );
    add(
        PopupRow::Header("Terminal.app never does · iTerm2: report mouse events".into()),
        None,
    );
    // (x-b465) Ghostty joins the named list: it binds right-click to its own
    // context menu by default (`right-click-action`), measured against Warp on
    // the same build, where the identical press opens the menu. The SETTING is
    // named, not a value to set: which value restores forwarding is untested
    // here, and a config line this text cannot vouch for is the kind of
    // confident wrong answer that cost a whole diagnosis round already.
    add(
        PopupRow::Header("Ghostty binds it too · see right-click-action".into()),
        None,
    );
    add(
        PopupRow::Header(format!(
            "in tmux set mouse off · else m, or hold Left {}ms",
            MENU_LONG_PRESS.as_millis()
        )),
        None,
    );
    // (x-b5d1) The glyph legend rides the modal tail, after the notes: the
    // x7683 pin holds the notes above the 64-row fold, and the legend is
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
