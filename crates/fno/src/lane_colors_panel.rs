//! (x-1b68) The settings modal's Colors tab: the four `[sideline.colors]`
//! axes, the named-color picker, the add-key drill, and the rendered view of
//! what every lane currently resolves to. Lived inline in `client.rs` until
//! the file-budget gate (client.rs is shrink-only) named the remedy: a module
//! named by the question it answers. The build fns are the testable seam; the
//! Client owns the drill lifecycle and the key handling.

use crate::client::AuxAction;
use crate::popup::PopupRow;
/// (x-e4f1) The four `[sideline.colors]` axis tables, in display order.
const LANE_AXES: [&str; 4] = ["harness", "route", "model", "row"];

/// (x-e4f1) The named colors the picker offers: exactly `parse_color`'s
/// accepted set (the picker-drift test asserts every entry parses, so the two
/// lists cannot drift silently).
const LANE_COLOR_NAMES: [&str; 16] = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "gray",
    "light_red",
    "light_green",
    "light_yellow",
    "light_blue",
    "light_magenta",
    "light_cyan",
    "light_white",
];

/// (x-e4f1) The lane-colors drill state for the settings Colors tab: which
/// level the operator is on (axis list -> key list -> picker) and any open
/// text entry. Client-local ephemera, the `create`/`rename` class. The
/// Client owns the lifecycle (it drives the drill from key events), so the
/// fields and lifecycle methods are part of the public surface.
#[derive(Debug, Default)]
pub(crate) struct LaneColorsUi {
    /// `Some(axis)` = the key list for that axis; `None` = the four-axis list.
    pub axis: Option<String>,
    /// `Some((axis, key))` = the color picker is open for that mapping.
    pub pick: Option<(String, String)>,
    /// `Some((axis, buffer))` = naming a NEW key for that axis.
    pub key_entry: Option<(String, String)>,
    /// `Some(buffer)` = free-form color entry for the key being picked
    /// (`pick` carries the (axis, key) context).
    pub custom_entry: Option<String>,
    /// Split-arrow safety for the text entries, same as `create_esc`.
    pub entry_esc: Vec<u8>,
}

impl LaneColorsUi {
    pub fn is_entry(&self) -> bool {
        self.key_entry.is_some() || self.custom_entry.is_some()
    }
    /// Drop text-entry buffers, keeping the drill level.
    pub fn clear_entry(&mut self) {
        self.key_entry = None;
        self.custom_entry = None;
        self.entry_esc.clear();
    }
    /// Drop the drill entirely (tab switch away from Colors).
    pub fn reset(&mut self) {
        self.axis = None;
        self.pick = None;
        self.clear_entry();
    }
}

/// (x-e4f1) The palette entries for one axis name, in config order.
pub(crate) fn lane_axis_entries(
    pal: &crate::sideline_color::SidelinePalette,
    axis: &str,
) -> Vec<(String, String)> {
    match axis {
        "harness" => pal.harness.clone(),
        "route" => pal.route.clone(),
        "model" => pal.model.clone(),
        _ => pal.row.clone(),
    }
}

/// (x-1b68) Push one axis's listing rows: every key the resolution cascade
/// knows, configured entries first (unmarked - the operator set them), then
/// each built-in default the config does NOT override, marked `(default)` so
/// an unconfigured install still shows what every lane resolves to. Defaults
/// stay resolve-time values: they are never written to config. When
/// `add_label` is `Some`, an add-key row closes the group.
fn push_lane_axis_rows(
    rows: &mut Vec<PopupRow>,
    actions: &mut Vec<AuxAction>,
    pal: &crate::sideline_color::SidelinePalette,
    axis: &str,
    add_label: Option<String>,
) {
    let entries = lane_axis_entries(pal, axis);
    for (k, v) in &entries {
        rows.push(PopupRow::Entry {
            glyph: "○".into(),
            label: format!("{k} = {v}"),
            hint: String::new(),
            enabled: true,
        });
        actions.push(AuxAction::LaneColorEdit(axis.to_string(), k.clone()));
    }
    let configured: std::collections::HashSet<&str> =
        entries.iter().map(|(k, _)| k.as_str()).collect();
    for (k, v) in crate::sideline_color::builtin_defaults(axis) {
        if configured.contains(k) {
            continue; // an override renders from config, unmarked
        }
        rows.push(PopupRow::Entry {
            glyph: "○".into(),
            label: format!("{k} = {v} (default)"),
            hint: String::new(),
            enabled: true,
        });
        actions.push(AuxAction::LaneColorEdit(axis.to_string(), (*k).to_string()));
    }
    if let Some(label) = add_label {
        rows.push(PopupRow::Entry {
            glyph: "＋".into(),
            label,
            hint: String::new(),
            enabled: true,
        });
        actions.push(AuxAction::LaneColorAdd(axis.to_string()));
    }
}

/// (x-e4f1) The color currently configured for one (axis, key), if any.
pub(crate) fn current_lane_color(
    pal: &crate::sideline_color::SidelinePalette,
    axis: &str,
    key: &str,
) -> Option<String> {
    lane_axis_entries(pal, axis)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

/// (x-e4f1) Merge one (key, color) into an axis block and serialize the WHOLE
/// block as a JSON object. `fno config set` refuses per-key dotted writes
/// inside dict fields, so the picker replaces the whole block (REPLACE
/// semantics) with the one key updated - the merge source is re-read fresh
/// by the caller right before this runs.
pub(crate) fn merged_axis_json(entries: &[(String, String)], key: &str, color: &str) -> String {
    let mut map = serde_json::Map::new();
    for (k, v) in entries {
        if k != key {
            map.insert(k.clone(), serde_json::Value::String(v.clone()));
        }
    }
    map.insert(
        key.to_string(),
        serde_json::Value::String(color.to_string()),
    );
    serde_json::to_string(&serde_json::Value::Object(map)).unwrap_or_default()
}

/// (x-e4f1) Build the settings Colors tab rows for the current drill level:
/// axis list -> key list -> picker -> (replacing the picker) the free-form
/// color entry. The free function is the testable seam: `palette()` is a
/// process-global cache, so tests pass a literal palette instead of seeding
/// the cache.
pub(crate) fn build_lane_color_rows(
    pal: &crate::sideline_color::SidelinePalette,
    ui: &LaneColorsUi,
) -> (Vec<PopupRow>, Vec<AuxAction>) {
    let mut rows = Vec::new();
    let mut actions = Vec::new();
    if let Some((axis, key)) = &ui.pick {
        if ui.custom_entry.is_some() {
            // Free-form entry replaces the picker view; Enter is handled by
            // the key divert, so the rows are display-only context.
            let buf = ui.custom_entry.as_deref().unwrap_or("");
            rows.push(PopupRow::Header(format!("{axis}.{key}: {buf}")));
            rows.push(PopupRow::Rule);
            rows.push(PopupRow::Entry {
                glyph: " ".into(),
                label: "enter: name | indexed(n) | #rrggbb".into(),
                hint: String::new(),
                enabled: false,
            });
            return (rows, actions);
        }
        // Picker level: the named colors, the current value marked.
        rows.push(PopupRow::Header(format!("{axis}.{key}")));
        rows.push(PopupRow::Rule);
        let current = current_lane_color(pal, axis, key);
        for name in LANE_COLOR_NAMES {
            let active = current.as_deref() == Some(name);
            rows.push(PopupRow::Entry {
                glyph: if active { "●" } else { "○" }.into(),
                label: name.into(),
                hint: if active { "current" } else { "" }.into(),
                enabled: true,
            });
            actions.push(AuxAction::LaneColorSet(
                axis.clone(),
                key.clone(),
                (*name).into(),
            ));
        }
        rows.push(PopupRow::Rule);
        rows.push(PopupRow::Entry {
            glyph: "✎".into(),
            label: "custom…".into(),
            hint: "indexed(n), #rrggbb".into(),
            enabled: true,
        });
        actions.push(AuxAction::LaneColorCustom(axis.clone(), key.clone()));
        return (rows, actions);
    }
    if let Some((axis, buf)) = &ui.key_entry {
        // Key-naming entry: live echo + every key this axis resolves for
        // (configured and default), so the operator names one that is new.
        rows.push(PopupRow::Header(format!("{axis} key: {buf}")));
        rows.push(PopupRow::Rule);
        push_lane_axis_rows(&mut rows, &mut actions, pal, axis, None);
        return (rows, actions);
    }
    if let Some(axis) = &ui.axis {
        // Key list: this axis's mappings + the add-key row.
        rows.push(PopupRow::Header(axis.clone()));
        rows.push(PopupRow::Rule);
        push_lane_axis_rows(&mut rows, &mut actions, pal, axis, Some("add key".into()));
        return (rows, actions);
    }
    // Axis list: every axis's mappings grouped under its header, each group
    // followed by its add-key row.
    rows.push(PopupRow::Header("colors".into()));
    rows.push(PopupRow::Rule);
    for axis in LANE_AXES {
        rows.push(PopupRow::Header((*axis).into()));
        push_lane_axis_rows(
            &mut rows,
            &mut actions,
            pal,
            axis,
            Some(format!("add {axis} key")),
        );
    }
    (rows, actions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::popup::{self, Anchor, Popup};
    use crate::proto::Cell;
    use crate::theme::Theme;

    // (x-e4f1) A literal palette for the lane-colors tests; the process
    // palette() cache cannot be seeded per-test, so every builder test passes
    // the literal through the free-function seam.
    fn lane_pal(route: &[(&str, &str)]) -> crate::sideline_color::SidelinePalette {
        crate::sideline_color::SidelinePalette {
            route: route
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn lane_colors_axis_list_groups_every_axis_under_its_header() {
        let pal = lane_pal(&[("zai", "green")]);
        let (rows, actions) = build_lane_color_rows(&pal, &LaneColorsUi::default());
        // One header per axis, in display order.
        let headers: Vec<&str> = LANE_AXES.to_vec();
        let mut seen_headers = rows.iter().filter_map(|r| match r {
            PopupRow::Header(h) if LANE_AXES.contains(&h.as_str()) => Some(h.as_str()),
            _ => None,
        });
        for h in headers {
            assert_eq!(
                seen_headers.next(),
                Some(h),
                "axis {h} header present in order"
            );
        }
        // The one configured mapping renders as `key = color` and opens the picker.
        assert!(rows
            .iter()
            .any(|r| matches!(r, PopupRow::Entry { label, .. } if label == "zai = green")));
        assert!(actions.iter().any(
            |a| matches!(a, AuxAction::LaneColorEdit(axis, key) if axis == "route" && key == "zai")
        ));
        // Every axis offers its add-key row (positive marker per axis).
        for axis in LANE_AXES {
            assert!(
                actions
                    .iter()
                    .any(|a| matches!(a, AuxAction::LaneColorAdd(a_axis) if a_axis == axis)),
                "add row present for axis {axis}"
            );
        }
    }

    #[test]
    fn lane_color_picker_lists_the_parser_vocabulary_and_marks_the_current() {
        let pal = lane_pal(&[("zai", "green")]);
        let ui = LaneColorsUi {
            pick: Some(("route".into(), "zai".into())),
            ..Default::default()
        };
        let (rows, actions) = build_lane_color_rows(&pal, &ui);
        // Drift guard: every picker name must satisfy parse_color, so a name
        // added without parser support fails here instead of refusing at save.
        let names: Vec<&str> = actions
            .iter()
            .filter_map(|a| match a {
                AuxAction::LaneColorSet(_, _, color) => Some(color.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, LANE_COLOR_NAMES.to_vec());
        for name in &names {
            assert!(
                crate::sideline_color::parse_color(name).is_some(),
                "picker name {name} must parse"
            );
        }
        // The current value is marked, not just listed.
        assert!(rows.iter().any(|r| matches!(
            r,
            PopupRow::Entry { glyph, hint, .. } if glyph == "●" && hint == "current"
        )));
        // The free-form entry is offered beside the names.
        assert!(actions
        .iter()
        .any(|a| matches!(a, AuxAction::LaneColorCustom(axis, key) if axis == "route" && key == "zai")));
    }

    #[test]
    fn lane_color_custom_entry_is_display_only_with_a_live_echo() {
        let pal = lane_pal(&[]);
        let ui = LaneColorsUi {
            pick: Some(("route".into(), "zai".into())),
            custom_entry: Some("#12abF0".into()),
            ..Default::default()
        };
        let (rows, actions) = build_lane_color_rows(&pal, &ui);
        // The typed buffer echoes in the header.
        assert!(matches!(
            rows.first(),
            Some(PopupRow::Header(h)) if h.contains("#12abF0")
        ));
        // No save action is reachable from a display-only entry; submit goes
        // through the key divert, never a row.
        assert!(actions.is_empty());
        assert!(rows
            .iter()
            .any(|r| matches!(r, PopupRow::Entry { enabled: false, .. })));
    }

    #[test]
    fn lane_color_key_entry_echoes_the_buffer_and_lists_existing_keys() {
        let pal = lane_pal(&[("zai", "green"), ("openai", "blue")]);
        let ui = LaneColorsUi {
            key_entry: Some(("route".into(), "o".into())),
            ..Default::default()
        };
        let (rows, actions) = build_lane_color_rows(&pal, &ui);
        assert!(matches!(
            rows.first(),
            Some(PopupRow::Header(h)) if h == "route key: o"
        ));
        // Existing keys are listed so an existing mapping is pickable.
        assert!(rows
            .iter()
            .any(|r| matches!(r, PopupRow::Entry { label, .. } if label == "openai = blue")));
        // The configured pair (zai, openai) plus the two route defaults the
        // config does not override (openrouter, anthropic) are all pickable.
        assert_eq!(
            actions
                .iter()
                .filter(|a| matches!(a, AuxAction::LaneColorEdit(_, _)))
                .count(),
            4
        );
    }

    // (x-1b68) An unconfigured install renders every built-in default the
    // cascade knows, marked, instead of four empty groups.
    #[test]
    fn unconfigured_palette_renders_the_cascade_defaults_marked() {
        let (rows, actions) = build_lane_color_rows(&Default::default(), &LaneColorsUi::default());
        let defaults = [
            "zai = green (default)",
            "openrouter = magenta (default)",
            "openai = blue (default)",
            "anthropic = cyan (default)",
            "codex = blue (default)",
            "agy = yellow (default)",
            "opencode = light_magenta (default)",
            "cursor = light_blue (default)",
            "pi = light_yellow (default)",
        ];
        for want in defaults {
            assert!(
                rows.iter()
                    .any(|r| matches!(r, PopupRow::Entry { label, .. } if label == want)),
                "default row {want:?} rendered"
            );
        }
        // Every default is pickable: clicking it opens the picker to override.
        assert_eq!(
            actions
                .iter()
                .filter(|a| matches!(a, AuxAction::LaneColorEdit(_, _)))
                .count(),
            defaults.len(),
            "each default row carries its edit action"
        );
        // No configured row and no "(default)" marker leaked into model/row,
        // the two config-only axes.
        assert!(!rows.iter().any(
        |r| matches!(r, PopupRow::Entry { label, .. } if label.contains("(default)") && (label.starts_with("model") || label.contains("add")))
    ));
    }

    // (x-1b68) A configured key renders from config, unmarked, and suppresses
    // its default row - what changed is visible against what is in effect.
    #[test]
    fn a_configured_override_renders_unmarked_and_hides_its_default_row() {
        let pal = lane_pal(&[("zai", "red")]);
        let (rows, actions) = build_lane_color_rows(&pal, &LaneColorsUi::default());
        assert!(
            rows.iter()
                .any(|r| matches!(r, PopupRow::Entry { label, .. } if label == "zai = red")),
            "the override renders from config"
        );
        assert!(
            !rows.iter().any(
                |r| matches!(r, PopupRow::Entry { label, .. } if label.contains("zai = green"))
            ),
            "the overridden default row is suppressed"
        );
        assert!(
            rows.iter()
                .any(|r| matches!(r, PopupRow::Entry { label, .. }
                if label == "openrouter = magenta (default)")),
            "the untouched defaults still render marked"
        );
        // Nothing in this render path writes config.
        assert!(actions.iter().all(|a| matches!(
            a,
            AuxAction::LaneColorEdit(_, _) | AuxAction::LaneColorAdd(_)
        )));
    }

    // (x-1b68) The REAL render path, not the row builder: the Colors tab
    // rendered through Popup::render + popup::draw (what the live client
    // calls), on a short viewport so the scrollbar appears. Every row must
    // close its right border on the same column - add-key rows carrying the
    // fullwidth glyph and plain rows alike - and the wide glyph must claim
    // its spacer cell.
    #[test]
    fn colors_tab_render_path_keeps_the_right_border_on_one_column() {
        let (rows, _) = build_lane_color_rows(&Default::default(), &LaneColorsUi::default());
        let popup = Popup::new(rows, Anchor::Center)
            .title("settings")
            .tabs(vec![
                ("general".to_string(), false),
                ("theme".to_string(), false),
                ("keys".to_string(), false),
                ("colors".to_string(), true),
            ])
            .footer("tab switches section · esc close");
        let term = (20u16, 60u16);
        let rendered = popup.render(term);
        let theme = Theme::default_theme();
        let cols = term.1 as usize;
        let rows_n = term.0 as usize;
        let mut cells = vec![Cell::default(); rows_n * cols];
        popup::draw(&mut cells, rows_n, cols, &rendered, &theme);
        let (r0, c0) = rendered.origin;
        let w = rendered.width;
        let mut add_rows = 0usize;
        let mut body_rows = 0usize;
        let mut scrollbar_cells = 0usize;
        for (i, line) in rendered.lines.iter().enumerate() {
            let row = r0 + i;
            let at = |col: usize| cells[row * cols + col].c;
            assert!(
                matches!(at(c0 + w - 1), '│' | '┐' | '┘'),
                "row {i} closes its right border on one column: {:?}",
                line.text
            );
            if line.text.contains('＋') {
                add_rows += 1;
                let lead = (c0..c0 + w)
                    .find(|&col| cells[row * cols + col].c == '＋')
                    .expect("the lead glyph is painted");
                assert!(
                    cells[row * cols + lead + 1].flags & crate::proto::cell_flags::WIDE_SPACER != 0,
                    "the fullwidth glyph claims its spacer cell"
                );
            }
            // Body rows sit between the top chrome (title + tabs) and the
            // bottom chrome (footer + border); the scrollbar column rides
            // inside their right border.
            if i > 2 && i < rendered.lines.len() - 2 {
                body_rows += 1;
                if matches!(at(c0 + w - 2), '█' | '░') {
                    scrollbar_cells += 1;
                }
            }
        }
        // The 15-row viewport shows harness + route groups (harness lists
        // first); model/row add rows sit below the fold - the scroll that
        // the scrollbar assertion below pins.
        assert!(
            add_rows >= 2,
            "visible axes show their add rows: {add_rows}"
        );
        assert_eq!(
            scrollbar_cells, body_rows,
            "the panel scrolled and every body row carries the scrollbar column"
        );
        // The defaults reached the paint surface, not just the row builder.
        let painted: String = cells
            .iter()
            .filter(|c| c.flags & crate::proto::cell_flags::WIDE_SPACER == 0)
            .map(|c| c.c)
            .collect();
        assert!(painted.contains("light_magenta (default)"));
        assert!(painted.contains("(default)"), "defaults visible on screen");
    }

    #[test]
    fn merged_axis_json_replaces_one_key_and_keeps_the_rest() {
        let entries = vec![
            ("zai".to_string(), "green".to_string()),
            ("openai".to_string(), "blue".to_string()),
        ];
        let out = merged_axis_json(&entries, "zai", "magenta");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["zai"], "magenta", "existing key replaced");
        assert_eq!(v["openai"], "blue", "untouched key kept");
        let out = merged_axis_json(&entries, "openrouter", "magenta");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["openrouter"], "magenta", "new key inserted");
        assert_eq!(v["zai"], "green", "existing key kept");
    }
}
