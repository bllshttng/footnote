//! The settings modal's Colors tab: the four `[sideline.colors]`
//! axes, the named-color picker, the add-key drill, and the rendered view of
//! what every lane currently resolves to. Lived inline in `client.rs` until
//! the file-budget gate (client.rs is shrink-only) named the remedy: a module
//! named by the question it answers. The build fns are the testable seam; the
//! Client owns the drill lifecycle and the key handling.

use crate::client::input_field::{back_row, InputField};
use crate::client::AuxAction;
use crate::popup::PopupRow;
use crate::proto::Color;
/// The four `[sideline.colors]` axis tables, in display order.
const LANE_AXES: [&str; 4] = ["harness", "route", "model", "row"];

/// The named colors the picker offers: exactly `parse_color`'s
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

/// The lane-colors drill state for the settings Colors tab: which
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
    /// `Some((axis, field))` = naming a NEW key for that axis.
    pub key_entry: Option<(String, InputField)>,
    /// `Some(field)` = free-form color entry for the key being picked
    /// (`pick` carries the (axis, key) context).
    pub custom_entry: Option<InputField>,
}

impl LaneColorsUi {
    pub fn is_entry(&self) -> bool {
        self.key_entry.is_some() || self.custom_entry.is_some()
    }
    /// Drop text-entry buffers, keeping the drill level.
    pub fn clear_entry(&mut self) {
        self.key_entry = None;
        self.custom_entry = None;
    }
    /// Drop the drill entirely (tab switch away from Colors).
    pub fn reset(&mut self) {
        self.axis = None;
        self.pick = None;
        self.clear_entry();
    }
}

/// The palette entries for one axis name, in config order.
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

/// One listing row for a configured value: a swatch in the color the ACTIVE
/// theme paints for that value - a hex paints itself, an ANSI name paints the
/// theme's Terminal 16 slot (the emulator's own slot when the theme defines
/// no palette). The resolved hex rides the label when it is known; the theme
/// changes, the squares repaint. An unparseable value renders a plain entry.
fn value_row(
    glyph: &str,
    base: String,
    v: &str,
    theme: &crate::theme::Theme,
    enabled: bool,
) -> PopupRow {
    let Some(c) = crate::sideline_color::parse_color(v) else {
        return PopupRow::Entry {
            glyph: glyph.into(),
            label: base,
            hint: String::new(),
            enabled,
        };
    };
    let painted = match c {
        Color::Rgb(..) => Some(c),
        Color::Indexed(n) => crate::theme::terminal16_slot(n, theme).or(Some(c)),
        Color::Default => None,
    };
    let label = match painted.and_then(crate::theme::color_hex) {
        Some(h) => format!("{base} {h}"),
        None => base,
    };
    PopupRow::SwatchEntry {
        glyph: glyph.into(),
        label,
        hint: String::new(),
        enabled,
        color: painted.unwrap(),
    }
}

/// Push one axis's listing rows: every key the resolution cascade
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
    theme: &crate::theme::Theme,
    add_label: Option<String>,
) {
    let entries = lane_axis_entries(pal, axis);
    for (k, v) in &entries {
        rows.push(value_row("○", format!("{k} = {v}"), v, theme, true));
        actions.push(AuxAction::LaneColorEdit(axis.to_string(), k.clone()));
    }
    let configured: std::collections::HashSet<&str> =
        entries.iter().map(|(k, _)| k.as_str()).collect();
    for (k, v) in crate::sideline_color::builtin_defaults(axis) {
        if configured.contains(k) {
            continue; // an override renders from config, unmarked
        }
        rows.push(value_row(
            "○",
            format!("{k} = {v} (default)"),
            v,
            theme,
            true,
        ));
        actions.push(AuxAction::LaneColorEdit(axis.to_string(), (*k).to_string()));
    }
    if let Some(label) = add_label {
        rows.push(PopupRow::Entry {
            glyph: "+".into(),
            label,
            hint: String::new(),
            enabled: true,
        });
        actions.push(AuxAction::LaneColorAdd(axis.to_string()));
    }
}

/// The color currently configured for one (axis, key), if any.
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

/// Merge one (key, color) into an axis block and serialize the WHOLE
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

/// Build the settings Colors tab rows for the current drill level:
/// axis list -> key list -> picker -> (replacing the picker) the free-form
/// color entry. The free function is the testable seam: `palette()` is a
/// process-global cache, so tests pass a literal palette instead of seeding
/// the cache. `theme` is the active theme: its Terminal 16 palette paints
/// the swatches and their hexes.
pub(crate) fn build_lane_color_rows(
    pal: &crate::sideline_color::SidelinePalette,
    ui: &LaneColorsUi,
    theme: &crate::theme::Theme,
) -> (Vec<PopupRow>, Vec<AuxAction>) {
    let mut rows = Vec::new();
    let mut actions = Vec::new();
    if ui.pick.is_some() || ui.key_entry.is_some() || ui.axis.is_some() {
        rows.push(back_row());
        actions.push(AuxAction::SettingsBack);
    }
    if let Some((axis, key)) = &ui.pick {
        if let Some(field) = &ui.custom_entry {
            // Free-form entry replaces the picker view; the field owns the
            // keyboard.
            rows.push(field.row(&format!("{axis}.{key}")));
            return (rows, actions);
        }
        // Picker level: the named colors, each with a swatch in the theme's
        // color for that slot; the current value marked.
        rows.push(PopupRow::Header(format!("{axis}.{key}")));
        rows.push(PopupRow::Rule);
        let current = current_lane_color(pal, axis, key);
        for name in LANE_COLOR_NAMES {
            let active = current.as_deref() == Some(name);
            rows.push(value_row(
                if active { "●" } else { "○" },
                name.to_string(),
                name,
                theme,
                true,
            ));
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
    if let Some((axis, field)) = &ui.key_entry {
        // Key-naming field + every key this axis resolves for (configured
        // and default), so the operator names one that is new.
        rows.push(field.row(&format!("{axis} key")));
        rows.push(PopupRow::Rule);
        push_lane_axis_rows(&mut rows, &mut actions, pal, axis, theme, None);
        return (rows, actions);
    }
    if let Some(axis) = &ui.axis {
        // Key list: this axis's mappings + the add-key row.
        rows.push(PopupRow::Header(axis.clone()));
        rows.push(PopupRow::Rule);
        push_lane_axis_rows(
            &mut rows,
            &mut actions,
            pal,
            axis,
            theme,
            Some("add key".into()),
        );
        return (rows, actions);
    }
    // Axis list: every axis's mappings grouped under its header, each group
    // followed by its add-key row. No top "colors" header: the tab strip
    // already names this section (the settings-tab cleanup).
    for axis in LANE_AXES {
        rows.push(PopupRow::Header((*axis).into()));
        push_lane_axis_rows(
            &mut rows,
            &mut actions,
            pal,
            axis,
            theme,
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

    // A literal palette for the lane-colors tests; the process
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

    fn sup() -> Theme {
        Theme::from_name("footnote-superscript").0
    }

    fn paper() -> Theme {
        Theme::from_name("footnote-paper").0
    }

    #[test]
    fn lane_colors_axis_list_groups_every_axis_under_its_header() {
        let pal = lane_pal(&[("zai", "green")]);
        let (rows, actions) = build_lane_color_rows(&pal, &LaneColorsUi::default(), &sup());
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
        // The one configured mapping renders with the theme's slot hex beside
        // the name (slot 2 = green, dark #9cc49c) and opens the picker.
        assert!(rows.iter().any(
            |r| matches!(r, PopupRow::SwatchEntry { label, .. } if label == "zai = green #9cc49c")
        ));
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
        // The ANSI names STAY; each name gains a filled
        // square painted in the active theme's Terminal 16 slot and the
        // resolved hex beside it. The pick still writes the name.
        let pal = lane_pal(&[("zai", "green")]);
        let ui = LaneColorsUi {
            pick: Some(("route".into(), "zai".into())),
            ..Default::default()
        };
        let (rows, actions) = build_lane_color_rows(&pal, &ui, &sup());
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
        // Every picker row paints the theme's slot color and carries the
        // resolved hex in its label: green (slot 2, dark) reads `green
        // #9cc49c`.
        for (n, swatch) in rows.iter().filter_map(|r| match r {
            PopupRow::SwatchEntry { label, color, .. } => Some((label.clone(), *color)),
            _ => None,
        }) {
            let name = n.split(' ').next().unwrap_or_default();
            let slot = LANE_COLOR_NAMES.iter().position(|c| c == &name).unwrap() as u8;
            assert_eq!(
                Some(swatch),
                crate::theme::terminal16_slot(slot, &sup()),
                "swatch paints the theme slot for {name}"
            );
            let hex = crate::theme::color_hex(swatch).unwrap();
            assert!(n.ends_with(&hex), "hex {hex} beside the name: {n}");
        }
        // The current value is marked, not just listed.
        assert!(rows.iter().any(|r| matches!(
            r,
            PopupRow::SwatchEntry { glyph, .. } if glyph == "●"
        )));
        // The free-form entry is offered beside the names.
        assert!(actions
        .iter()
        .any(|a| matches!(a, AuxAction::LaneColorCustom(axis, key) if axis == "route" && key == "zai")));
    }

    #[test]
    fn picker_squares_repaint_when_the_theme_changes() {
        // The squares paint the ACTIVE theme's slot - the
        // same name resolves to a different square under the paper twin.
        let ui = LaneColorsUi {
            pick: Some(("route".into(), "zai".into())),
            ..Default::default()
        };
        let (dark, _) = build_lane_color_rows(&Default::default(), &ui, &sup());
        let (light, _) = build_lane_color_rows(&Default::default(), &ui, &paper());
        let pick = |rows: &[PopupRow], want: &str| {
            rows.iter().find_map(|r| match r {
                PopupRow::SwatchEntry { label, color, .. } if label.starts_with(want) => {
                    Some(*color)
                }
                _ => None,
            })
        };
        assert_ne!(
            pick(&dark, "blue"),
            pick(&light, "blue"),
            "the blue square repaints under the other theme"
        );
        assert_ne!(
            pick(&dark, "red"),
            pick(&light, "red"),
            "the red square repaints under the other theme"
        );
    }

    // An unconfigured install renders every built-in default the
    // cascade knows, marked, instead of four empty groups.
    #[test]
    fn unconfigured_palette_renders_the_cascade_defaults_marked() {
        let (rows, actions) =
            build_lane_color_rows(&Default::default(), &LaneColorsUi::default(), &sup());
        let defaults = [
            "zai = green (default) #9cc49c",
            "openrouter = magenta (default) #d7a6c6",
            "openai = blue (default) #9fb8e5",
            "anthropic = cyan (default) #83c6bd",
            "codex = blue (default) #9fb8e5",
            "agy = yellow (default) #c5b784",
            "opencode = light_magenta (default) #d092b9",
            "cursor = light_blue (default) #8ca8e2",
            "pi = light_yellow (default) #b8a965",
        ];
        for want in defaults {
            assert!(
                rows.iter()
                    .any(|r| matches!(r, PopupRow::SwatchEntry { label, .. } if label == want)),
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
        |r| matches!(r, PopupRow::SwatchEntry { label, .. } if label.contains("(default)") && (label.starts_with("model") || label.contains("add")))
    ));
    }

    // A configured key renders from config, unmarked, and suppresses
    // its default row - what changed is visible against what is in effect.
    #[test]
    fn a_configured_override_renders_unmarked_and_hides_its_default_row() {
        let pal = lane_pal(&[("zai", "red")]);
        let (rows, actions) = build_lane_color_rows(&pal, &LaneColorsUi::default(), &sup());
        assert!(
            rows.iter().any(
                |r| matches!(r, PopupRow::SwatchEntry { label, .. } if label == "zai = red #e1a6a3")
            ),
            "the override renders from config"
        );
        assert!(
            !rows.iter().any(
                |r| matches!(r, PopupRow::SwatchEntry { label, .. } if label.contains("zai = green"))
            ),
            "the overridden default row is suppressed"
        );
        assert!(
            rows.iter()
                .any(|r| matches!(r, PopupRow::SwatchEntry { label, .. }
                if label == "openrouter = magenta (default) #d7a6c6")),
            "the untouched defaults still render marked"
        );
        // Nothing in this render path writes config.
        assert!(actions.iter().all(|a| matches!(
            a,
            AuxAction::LaneColorEdit(_, _) | AuxAction::LaneColorAdd(_)
        )));
    }

    // The REAL render path, not the row builder: the Colors tab
    // rendered through Popup::render + popup::draw (what the live client
    // calls), on a short viewport so the scrollbar appears. Every row must
    // close its right border on the same column - add-key rows carrying the
    // fullwidth glyph and plain rows alike - and the wide glyph must claim
    // its spacer cell.
    #[test]
    fn colors_tab_render_path_keeps_the_right_border_on_one_column() {
        let (rows, _) =
            build_lane_color_rows(&Default::default(), &LaneColorsUi::default(), &sup());
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
                matches!(at(c0 + w - 1), '│' | '╮' | '╯'),
                "row {i} closes its right border on one column: {:?}",
                line.text
            );
            if line.text.contains("+ add") {
                add_rows += 1;
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
        assert!(
            painted.contains("#d092b9"),
            "resolved hex visible on screen"
        );
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
