use super::*;

/// Enter on an open lane-colors field; true ends the chunk. An empty submit
/// keeps the field open; a key name opens the picker for it; a custom color
/// is validated through `parse_color`, then saved, or refused with a notice
/// and the field kept open for a fix.
pub(super) async fn submit(view: &mut View, text: String) -> Result<bool, String> {
    if text.is_empty() {
        return Ok(false);
    }
    if let Some((axis, _)) = view.lane.key_entry.take() {
        view.lane.pick = Some((axis, text));
    } else if let Some((axis, key)) = view.lane.pick.clone() {
        if crate::sideline_color::parse_color(&text).is_none() {
            view.set_notice(format!(
                "{axis}.{key}: invalid color (name, indexed(n), #rrggbb)"
            ));
            return Ok(true);
        }
        view.lane.custom_entry = None;
        view.lane.pick = None;
        lane_color_save(view, &axis, &key, &text).await?;
    }
    Ok(true)
}

/// Persist one lane color through the CLI block-replace form and
/// reload the palette so it goes live without a restart. The merge source is
/// re-read fresh first, so a config change written by another process since
/// the palette loaded is not clobbered by the whole-block replace.
pub(super) async fn lane_color_save(
    view: &mut View,
    axis: &str,
    key: &str,
    color: &str,
) -> Result<(), String> {
    crate::sideline_color::reload_palette();
    use crate::lane_colors_panel as panel;
    let json = panel::merged_axis_json(
        &panel::lane_axis_entries(crate::sideline_color::palette(), axis),
        key,
        color,
    );
    let notice = match spawn_config_set(&format!("sideline.colors.{axis}"), &json).await {
        Ok(()) => {
            crate::sideline_color::reload_palette();
            // Verify at the palette's own source: the CLI write and the
            // palette read can land in different config layers (a concurrent
            // block-replace, or a project config shadowing the global write).
            // A lost write is surfaced here, never silently swallowed.
            if panel::current_lane_color(crate::sideline_color::palette(), axis, key).as_deref()
                == Some(color)
            {
                format!("{axis}.{key}: {color}")
            } else {
                format!(
                    "{axis}.{key}: save did not stick in the config the sideline reads; check config layering"
                )
            }
        }
        Err(_) => format!("{axis}.{key}: save failed"),
    };
    view.set_notice(notice);
    view.reopen_settings_keeping_sel();
    Ok(())
}
