//! The settings modal, moved out of `client.rs` for the shrink-only
//! ratchet (the backlog board's wiring is paid for by this move). Behavior is
//! preserved: the same tabs, toggles, and rebuild-keeping-selection contract.
//! Everything here reaches the client's private items through `super::*`.

use super::*;

/// The settings modal's tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsTab {
    General,
    Theme,
    Keys,
    Colors,
}

impl SettingsTab {
    /// Tab's section cycle: General -> Theme -> Keys -> Colors -> General.
    pub(crate) fn next(self) -> Self {
        match self {
            SettingsTab::General => SettingsTab::Theme,
            SettingsTab::Theme => SettingsTab::Keys,
            SettingsTab::Keys => SettingsTab::Colors,
            SettingsTab::Colors => SettingsTab::General,
        }
    }
}

pub(crate) const ALL: [SettingsTab; 4] = [
    SettingsTab::General,
    SettingsTab::Theme,
    SettingsTab::Keys,
    SettingsTab::Colors,
];

impl View {
    /// One settings tab's rows and actions (extracted from
    /// `build_settings_modal` so every tab's width can be measured).
    pub(super) fn settings_rows_for(&self, tab: SettingsTab) -> (Vec<PopupRow>, Vec<AuxAction>) {
        let mut rows = Vec::new();
        let mut actions: Vec<AuxAction> = Vec::new();
        match tab {
            SettingsTab::General => {
                let toggle = |on: bool, label: &str| PopupRow::Entry {
                    glyph: if on { "☑".into() } else { "☐".into() },
                    label: label.into(),
                    hint: String::new(),
                    enabled: true,
                };
                rows.push(toggle(self.hover_focus, "focus follows mouse"));
                rows.push(toggle(self.status_on, "status row"));
                rows.push(toggle(
                    self.resource_meter_on,
                    "resource meter (needs macmon)",
                ));
                rows.push(toggle(self.confirm_lifecycle, "confirm before stop/remove"));
                rows.push(toggle(
                    self.sideline_layout == crate::sideline_color::SidelineLayout::Card,
                    "sideline card layout",
                ));
                actions.push(AuxAction::ToggleHoverFocus);
                actions.push(AuxAction::ToggleStatus);
                actions.push(AuxAction::ToggleResourceMeter);
                actions.push(AuxAction::ToggleConfirmLifecycle);
                actions.push(AuxAction::ToggleSidelineLayout);
            }
            SettingsTab::Theme => {
                if !matches!(&self.theme_import, theme_import_ui::ThemeImportUi::Idle) {
                    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                    (rows, actions) = theme_import_ui::rows(&self.theme_import, &cwd);
                } else {
                    // The shipped palettes first, then the user's own
                    // ([mux.themes], latched at startup); the active one is
                    // marked. Enter on a name applies it (an explicit action, not
                    // a cursor-move preview).
                    let mut names: Vec<String> = crate::theme::THEME_NAMES
                        .iter()
                        .map(|n| n.to_string())
                        .collect();
                    names.extend(self.user_themes.iter().map(|(n, _)| n.clone()));
                    for name in &names {
                        let active = self.theme.name == name.as_str();
                        rows.push(PopupRow::Entry {
                            glyph: if active { "●".into() } else { "○".into() },
                            label: name.clone(),
                            hint: if active {
                                "active".into()
                            } else {
                                String::new()
                            },
                            enabled: true,
                        });
                        actions.push(AuxAction::ApplyTheme(name.clone()));
                    }
                    rows.push(PopupRow::Entry {
                        glyph: "+".into(),
                        label: "add theme file".into(),
                        hint: String::new(),
                        enabled: true,
                    });
                    actions.push(AuxAction::ThemeImportOpen);
                }
            }
            SettingsTab::Keys => {
                (rows, actions) = keys_settings::rows(self);
            }
            SettingsTab::Colors => {
                (rows, actions) = crate::lane_colors_panel::build_lane_color_rows(
                    crate::sideline_color::palette(),
                    &self.lane,
                    &self.theme,
                );
            }
        }
        (rows, actions)
    }

    /// Build the settings modal: general toggles plus theme and prefix pickers.
    pub(super) fn build_settings_modal(&self) -> AuxPopup {
        let tab = self.settings_tab;
        let (rows, actions) = self.settings_rows_for(tab);
        // One width across every tab (tabbed-modal width): the modal measures
        // all four and pins the widest, so switching tabs never resizes.
        let widest = ALL
            .iter()
            .map(|t| {
                let (rows, _) = self.settings_rows_for(*t);
                Popup::new(rows, Anchor::Center).content_width()
            })
            .max()
            .unwrap_or(0);
        // One width across every tab, with a floor at the popup width cap:
        // the review found both the toggles and the key table cramped at the
        // content's own minimum. render() clamps min_width to WIDTH_CAP, so
        // the cap IS the widest this modal can go.
        let popup = Popup::new(rows, Anchor::Center)
            .title("settings")
            .tabs(vec![
                ("general".to_string(), tab == SettingsTab::General),
                ("theme".to_string(), tab == SettingsTab::Theme),
                ("keybindings".to_string(), tab == SettingsTab::Keys),
                ("colors".to_string(), tab == SettingsTab::Colors),
            ])
            .footer(if self.settings_drilled() {
                "esc back"
            } else {
                "tab or click a section · esc close"
            })
            .min_width(widest.max(crate::popup::WIDTH_CAP))
            .plain_body();
        AuxPopup { popup, actions }
    }

    /// Rebuild the settings modal after a toggle so its glyph reflects the new
    /// state, preserving the current selection (a keyboard toggle must re-toggle
    /// the SAME row on the next Enter, not reset to row 0).
    pub(super) fn reopen_settings_keeping_sel(&mut self) {
        let (sel, scroll) = self
            .aux
            .as_ref()
            .map_or((0, 0), |m| (m.popup.sel, m.popup.scroll));
        let mut modal = self.build_settings_modal();
        let n = modal.popup.targets().len();
        modal.popup.sel = if n > 0 { sel.min(n - 1) } else { 0 };
        // Keep the scroll too, so a rebuild on a long page never jumps the
        // list back to its top under the cursor.
        modal.popup.scroll = scroll;
        modal.popup.follow_sel(self.term);
        self.aux = Some(modal);
    }

    /// Whether any settings page sits below its tab's top level.
    fn settings_drilled(&self) -> bool {
        let lane = &self.lane;
        self.key_capture.is_some()
            || !matches!(self.theme_import, theme_import_ui::ThemeImportUi::Idle)
            || lane.axis.is_some()
            || lane.pick.is_some()
            || lane.is_entry()
    }
}

/// Whether the open aux popup is the settings modal (the one with tabs).
fn is_settings(view: &View) -> bool {
    view.aux
        .as_ref()
        .is_some_and(|m| !m.popup.chrome.tabs.is_empty())
}

/// Put `tab` in front at its top level.
pub(super) fn switch_tab(view: &mut View, tab: SettingsTab) {
    view.settings_tab = tab;
    view.lane.reset();
    theme_import_ui::reset(view);
    view.key_capture = None;
    view.reopen_settings_keeping_sel();
}

/// A left press on a settings tab switches to it. True when the press hit
/// a tab.
pub(super) fn tap_tab(view: &mut View, row: u16, col: u16) -> bool {
    let Some(i) = view
        .aux
        .as_ref()
        .and_then(|m| m.popup.render(view.term).tab_at(row, col))
    else {
        return false;
    };
    if let Some(tab) = ALL.get(i) {
        switch_tab(view, *tab);
    }
    true
}

/// While a text field or key capture owns the keyboard, only the back and
/// file-picker rows take a click; acting on another row would leave the
/// field armed under a changed page.
pub(super) fn row_tap_allowed(view: &View, target: usize) -> bool {
    if !(view.lane.is_entry()
        || theme_import_ui::is_entry(&view.theme_import)
        || view.key_capture.is_some())
    {
        return true;
    }
    let action = view.aux.as_ref().and_then(|m| m.actions.get(target));
    matches!(action, Some(AuxAction::SettingsBack | AuxAction::ThemePick))
}

/// Step the settings modal back one level. False at a tab's top level (or
/// when the open popup is not settings), so the caller closes instead.
pub(super) fn back(view: &mut View) -> bool {
    if !is_settings(view) {
        return false;
    }
    let lane_step = |l: &mut crate::lane_colors_panel::LaneColorsUi| {
        l.custom_entry.take().is_some()
            || l.key_entry.take().is_some()
            || l.pick.take().is_some()
            || l.axis.take().is_some()
    };
    if keys_settings::close(view) {
        return true;
    }
    let stepped = (!matches!(view.theme_import, theme_import_ui::ThemeImportUi::Idle) && {
        theme_import_ui::reset(view);
        true
    }) || lane_step(&mut view.lane);
    if !stepped {
        return false;
    }
    view.reopen_settings_keeping_sel();
    true
}

/// Feed one read to the open settings text field or key capture. `None`
/// when neither is open, so the caller runs its own keys. Every chunk ends
/// with a rebuild, so the field row always paints the text so far.
pub(super) async fn field_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<Option<StdinFlow>, String> {
    if !is_settings(view) {
        return Ok(None);
    }
    if view.key_capture.is_some() {
        keys_settings::capture_keys(view, bytes, sock_w).await?;
    } else {
        let events = if let Some(f) = view.lane.custom_entry.as_mut() {
            f.feed(bytes)
        } else if let Some((_, f)) = view.lane.key_entry.as_mut() {
            f.feed(bytes)
        } else if let theme_import_ui::ThemeImportUi::Entry(f) = &mut view.theme_import {
            f.feed(bytes)
        } else {
            return Ok(None);
        };
        for event in events {
            match event {
                input_field::FieldEvent::Cancel => {
                    back(view);
                    break;
                }
                input_field::FieldEvent::Submit(text) => {
                    let submitted = if view.lane.is_entry() {
                        lane_entry::submit(view, text).await?
                    } else {
                        theme_import_ui::submit(view, text)
                    };
                    if submitted {
                        break;
                    }
                }
            }
        }
    }
    if view.aux.is_some() {
        view.reopen_settings_keeping_sel();
    }
    Ok(Some(StdinFlow::Continue))
}

/// Run one settings row's action. A toggle flips its flag, reports the new
/// state (a failed persist says "applied this session" honestly), and
/// rebuilds the modal in place so the glyph tracks the flag.
pub(super) async fn run_action(
    view: &mut View,
    action: AuxAction,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    match action {
        AuxAction::ToggleHoverFocus => {
            view.hover_focus = !view.hover_focus;
            let enabled = if view.hover_focus { "true" } else { "false" };
            let notice = match spawn_config_set("mux.hover_focus", enabled).await {
                Ok(()) => format!("focus follows mouse: {enabled}"),
                Err(_) => "focus follows mouse applied this session; save failed".into(),
            };
            view.set_notice(notice);
            view.reopen_settings_keeping_sel();
        }
        AuxAction::ApplyTheme(name) => theme_ground::apply(view, &name).await?,
        AuxAction::ThemeImportOpen => theme_import_ui::open(view),
        AuxAction::ThemeImportSave => theme_import_ui::save(view).await?,
        AuxAction::ThemeImportCancel => theme_import_ui::cancel(view),
        AuxAction::ThemePick => theme_import_ui::pick(view),
        AuxAction::SettingsBack => {
            back(view);
        }
        AuxAction::KeyCapture(action) => keys_settings::open_capture(view, action),
        AuxAction::EditKeysFile => keys_settings::edit_keys_file(view).await,
        // Edit and Add leave `lane.axis` alone, so back returns to the
        // level the user came from.
        AuxAction::LaneColorEdit(axis, key) => {
            view.lane.pick = Some((axis, key));
            view.reopen_settings_keeping_sel();
        }
        AuxAction::LaneColorAdd(axis) => {
            let field = input_field::InputField::new("new key name, e.g. glm-5", MAX_SEARCH_QUERY);
            view.lane.key_entry = Some((axis, field));
            view.reopen_settings_keeping_sel();
        }
        AuxAction::LaneColorCustom(axis, key) => {
            view.lane.pick = Some((axis, key));
            view.lane.custom_entry = Some(input_field::InputField::new(
                "name, indexed(n) or #rrggbb",
                MAX_SEARCH_QUERY,
            ));
            view.reopen_settings_keeping_sel();
        }
        AuxAction::LaneColorSet(axis, key, color) => {
            view.lane.pick = None;
            lane_entry::lane_color_save(view, &axis, &key, &color).await?;
        }
        AuxAction::ToggleStatus => {
            view.status_on = !view.status_on;
            // The status row changed the content area; report the new size so the
            // panes reflow (same accounting as Event::ToggleStatus).
            let (r, c) = view.content_dims();
            write_msg(sock_w, &ClientMsg::Resize { rows: r, cols: c })
                .await
                .map_err(|e| format!("resize send failed: {e}"))?;
            let enabled = if view.status_on { "true" } else { "false" };
            let notice = match spawn_config_set("mux.status_row", enabled).await {
                Ok(()) => format!("status row: {enabled}"),
                Err(_) => "status row applied this session; save failed".into(),
            };
            view.set_notice(notice);
            view.reopen_settings_keeping_sel();
        }
        AuxAction::ToggleConfirmLifecycle => {
            view.confirm_lifecycle = !view.confirm_lifecycle;
            view_store::save_confirm_lifecycle(view.confirm_lifecycle);
            let enabled = if view.confirm_lifecycle {
                "true"
            } else {
                "false"
            };
            view.set_notice(format!("confirm before stop/remove: {enabled}"));
            view.reopen_settings_keeping_sel();
        }
        AuxAction::ToggleResourceMeter => {
            view.resource_meter_on = !view.resource_meter_on;
            view.resource_meter_gate
                .store(view.resource_meter_on, std::sync::atomic::Ordering::Relaxed);
            // The run loop owns the spawn (one-shot via resource_meter_sampling);
            // clearing the text here means the row reads "sensor unavailable"
            // until the first sample lands - never a stale reading.
            view.resource_meter_sampling = view.resource_meter_on;
            view.resource_meter_text = None;
            let enabled = if view.resource_meter_on {
                "true"
            } else {
                "false"
            };
            let notice = match spawn_config_set("resource_meter.enabled", enabled).await {
                Ok(()) => format!("resource meter: {enabled}"),
                Err(_) => "resource meter applied this session; save failed".into(),
            };
            view.set_notice(notice);
            view.reopen_settings_keeping_sel();
        }
        AuxAction::ToggleSidelineLayout => {
            // Flip the in-memory shape first (the sideline reads this field
            // per render), then persist through the CLI and invalidate the
            // palette cache. A failed save keeps the shape and says so, the
            // same posture as the theme toggle.
            view.sideline_layout = match view.sideline_layout {
                crate::sideline_color::SidelineLayout::Card => {
                    crate::sideline_color::SidelineLayout::List
                }
                crate::sideline_color::SidelineLayout::List => {
                    crate::sideline_color::SidelineLayout::Card
                }
            };
            crate::sideline_color::reload_palette();
            let shape = if view.sideline_layout == crate::sideline_color::SidelineLayout::Card {
                "card"
            } else {
                "list"
            };
            let notice = match spawn_config_set("sideline.layout", shape).await {
                Ok(()) => format!("sideline layout: {shape}"),
                Err(_) => "sideline layout applied this session; save failed".into(),
            };
            view.set_notice(notice);
            view.reopen_settings_keeping_sel();
        }
        _ => {}
    }
    Ok(())
}
