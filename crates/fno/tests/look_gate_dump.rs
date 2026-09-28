//! Look-gate dump: render the PR's surfaces through the real paint path
//! (`Popup::render` -> per-char `Role`s -> `cell_style` via `chrome::blit`)
//! and write the cell buffers as JSON when `FNO_LOOK_DUMP` names a directory.
//! Without the env var the test only asserts the paint invariants, so CI runs
//! it as a plain regression test.

use fno::chrome;
use fno::popup::{Anchor, Popup, PopupRow};
use fno::theme::{cell_style, Theme};

struct Surface {
    name: &'static str,
    framed: chrome::Framed,
}

fn settings_like() -> Surface {
    // A Full-chrome tabbed modal: title, tabs, key rows with right-aligned
    // hints, a rule, a disabled row, footer with the esc-close words.
    let rows = vec![
        PopupRow::Header("general".to_string()),
        PopupRow::Entry {
            glyph: "\u{25cf}".to_string(),
            label: "hover focus".to_string(),
            hint: "on".to_string(),
            enabled: true,
        },
        PopupRow::Entry {
            glyph: "\u{25cb}".to_string(),
            label: "retire paused sessions".to_string(),
            hint: "off".to_string(),
            enabled: true,
        },
        PopupRow::Rule,
        PopupRow::Header("keys".to_string()),
        PopupRow::Entry {
            glyph: " ".to_string(),
            label: "open settings".to_string(),
            hint: "prefix s".to_string(),
            enabled: false,
        },
    ];
    let popup = Popup::new(rows, Anchor::Center)
        .title("settings")
        .tabs(vec![
            ("general".to_string(), true),
            ("theme".to_string(), false),
            ("keys".to_string(), false),
        ])
        .footer("tab switches section \u{b7} esc close")
        .min_width(50)
        .plain_body();
    let rendered = popup.render((40, 100));
    Surface {
        name: "settings-full",
        framed: to_framed(&rendered),
    }
}

fn menu_bare() -> Surface {
    // A Bare-level context menu: the esc chip rides the top border now.
    let rows = vec![
        PopupRow::Entry {
            glyph: "\u{2022}".to_string(),
            label: "rename".to_string(),
            hint: "".to_string(),
            enabled: true,
        },
        PopupRow::Entry {
            glyph: "\u{2022}".to_string(),
            label: "new workspace".to_string(),
            hint: "".to_string(),
            enabled: true,
        },
        PopupRow::Entry {
            glyph: "\u{2022}".to_string(),
            label: "close".to_string(),
            hint: "".to_string(),
            enabled: false,
        },
    ];
    let popup = Popup::new(rows, Anchor::At { row: 1, col: 1 });
    let rendered = popup.render((24, 60));
    Surface {
        name: "menu-bare",
        framed: to_framed(&rendered),
    }
}

fn name_input() -> Surface {
    let rows = vec![PopupRow::FullWidth("workspace-name_".to_string())];
    let popup = Popup::new(rows, Anchor::Center)
        .title("new workspace")
        .footer("enter names it \u{b7} esc discards");
    let rendered = popup.render((24, 70));
    Surface {
        name: "name-input",
        framed: to_framed(&rendered),
    }
}

/// The rendered popup's framed lines, rehoused for `chrome::blit`.
fn to_framed(rendered: &fno::popup::Rendered) -> chrome::Framed {
    chrome::Framed {
        width: rendered.width,
        lines: rendered
            .lines
            .iter()
            .map(|l| chrome::FramedLine {
                text: l.text.clone(),
                roles: l.roles.clone(),
                hits: vec![],
            })
            .collect(),
    }
}

fn dump_surface(dir: &std::path::Path, theme_name: &str, s: &Surface, t: &Theme) {
    let rows = s.framed.lines.len();
    let cols = s.framed.width;
    let mut cells = vec![fno::proto::Cell::default(); rows * cols];
    chrome::blit(&mut cells, rows, cols, (0, 0), &s.framed, t);
    let doc = serde_json::json!({
        "theme": theme_name,
        "surface": s.name,
        "rows": rows,
        "cols": cols,
        "border": format!("{:?}", t.border),
        "brand": format!("{:?}", t.brand),
        "cells": cells.iter().map(|c| serde_json::json!({
            "c": c.c,
            "fg": format!("{:?}", c.fg),
            "bg": format!("{:?}", c.bg),
            "flags": c.flags,
        })).collect::<Vec<_>>(),
    });
    let path = dir.join(format!("{}-{}.json", theme_name, s.name));
    std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
}

#[test]
fn look_gate_surfaces_dump_or_assert() {
    let themes = [
        Theme::from_name("footnote-superscript").0,
        Theme::from_name("footnote-paper").0,
        Theme::from_name("catppuccin").0,
        Theme::from_name("tokyo-night").0,
        Theme::from_name("gruvbox").0,
        Theme::from_name("terminal").0,
    ];
    // The dump directory arrives as a marker file (some runners sanitize the
    // environment): its first line is the output path.
    let dir = std::fs::read_to_string(".fno/look-dump-dir")
        .ok()
        .map(|p| std::path::PathBuf::from(p.trim()));
    for t in &themes {
        for s in [settings_like(), menu_bare(), name_input()] {
            // Invariant every theme must hold: no cell blits with an INVERSE
            // flag - the ground is the one body fill, chrome is explicit.
            let rows = s.framed.lines.len();
            let cols = s.framed.width;
            let mut cells = vec![fno::proto::Cell::default(); rows * cols];
            chrome::blit(&mut cells, rows, cols, (0, 0), &s.framed, t);
            for c in &cells {
                assert!(
                    c.flags & fno::proto::cell_flags::INVERSE == 0 || t.inherit,
                    "{}: unexpected INVERSE cell under {}",
                    s.name,
                    t.name
                );
            }
            if let Some(dir) = &dir {
                std::fs::create_dir_all(dir).unwrap();
                dump_surface(dir, t.name, &s, t);
            }
        }
    }
}
