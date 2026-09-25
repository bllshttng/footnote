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

pub(crate) const PREFIX_PICKS: [&str; 4] = ["C-a", "C-b", "C-x", "C-t"];

pub(crate) fn build_prefix_settings_rows(live_prefix: &str) -> (Vec<PopupRow>, Vec<AuxAction>) {
    let mut rows = vec![PopupRow::Header(format!("prefix: {live_prefix}"))];
    let mut actions = Vec::new();
    for spec in PREFIX_PICKS {
        let active = live_prefix == spec;
        rows.push(PopupRow::Entry {
            glyph: if active { "●".into() } else { "○".into() },
            label: spec.into(),
            hint: if active {
                "active".into()
            } else {
                String::new()
            },
            enabled: true,
        });
        actions.push(AuxAction::ApplyPrefix(spec.into()));
    }
    (rows, actions)
}

impl View {
    /// Build the settings modal: general toggles plus theme and prefix pickers.
    pub(super) fn build_settings_modal(&self) -> AuxPopup {
        let tab = self.settings_tab;
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
                // The four shipped palettes; the active one is marked. Enter on a
                // name applies it (an explicit action, not a cursor-move preview).
                for name in crate::theme::THEME_NAMES {
                    let active = self.theme.name == name;
                    rows.push(PopupRow::Entry {
                        glyph: if active { "●".into() } else { "○".into() },
                        label: name.into(),
                        hint: if active {
                            "active".into()
                        } else {
                            String::new()
                        },
                        enabled: true,
                    });
                    actions.push(AuxAction::ApplyTheme(name.into()));
                }
            }
            SettingsTab::Keys => {
                (rows, actions) = build_prefix_settings_rows(&crate::keys::prefix_display());
            }
            SettingsTab::Colors => {
                (rows, actions) = crate::lane_colors_panel::build_lane_color_rows(
                    crate::sideline_color::palette(),
                    &self.lane,
                );
            }
        }
        let popup = Popup::new(rows, Anchor::Center)
            .title("settings")
            .tabs(vec![
                ("general".to_string(), tab == SettingsTab::General),
                ("theme".to_string(), tab == SettingsTab::Theme),
                ("keys".to_string(), tab == SettingsTab::Keys),
                ("colors".to_string(), tab == SettingsTab::Colors),
            ])
            .footer("tab switches section · esc close");
        AuxPopup { popup, actions }
    }

    /// Rebuild the settings modal after a toggle so its glyph reflects the new
    /// state, preserving the current selection (a keyboard toggle must re-toggle
    /// the SAME row on the next Enter, not reset to row 0).
    pub(super) fn reopen_settings_keeping_sel(&mut self) {
        let sel = self.aux.as_ref().map(|m| m.popup.sel).unwrap_or(0);
        let mut modal = self.build_settings_modal();
        let n = modal.popup.targets().len();
        modal.popup.sel = if n > 0 { sel.min(n - 1) } else { 0 };
        self.aux = Some(modal);
    }
}

/// The three persisting toggle arms of the settings modal, moved verbatim
/// from `client.rs`'s `execute_aux_action`. Each flips its flag, reports the
/// new state (a failed persist says "applied this session" honestly), and
/// rebuilds the modal in place so the glyph tracks the flag.
pub(super) async fn run_toggle(
    view: &mut View,
    action: AuxAction,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    match action {
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
