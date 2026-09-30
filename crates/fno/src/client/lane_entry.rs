use super::*;

/// Keys while a lane-colors text entry is open: printable/Backspace
/// edit the buffer, Enter submits, Esc cancels back to the underlying drill
/// level. Modeled on [`create_keys`] (`fold_search_input` + per-key re-check),
/// with the settings modal staying open underneath. Enter on an EMPTY buffer
/// keeps the entry open; Enter on a custom entry validates through
/// `parse_color` and saves or refuses with a notice.
pub(super) async fn lane_entry_keys(
    view: &mut View,
    bytes: &[u8],
    _sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.lane.entry_esc);
    let keys = fold_search_input(&mut esc, bytes);
    view.lane.entry_esc = esc;
    for key in keys {
        // Re-read the mode each key: a submit or Esc mid-chunk closes it, and
        // the rest of the chunk must be swallowed, never forwarded.
        if !view.lane.is_entry() {
            break;
        }
        match key {
            SearchKey::Esc => {
                view.lane.clear_entry();
                view.reopen_settings_keeping_sel();
                break;
            }
            SearchKey::Byte(b'\r' | b'\n') => {
                if let Some((axis, buf)) = view.lane.key_entry.clone() {
                    // Naming a NEW key: an empty buffer keeps the entry
                    // open (the create_keys shape); a typed name opens the
                    // picker for it.
                    let name = buf.trim().to_string();
                    if name.is_empty() {
                        continue;
                    }
                    view.lane.clear_entry();
                    view.lane.pick = Some((axis, name));
                    view.reopen_settings_keeping_sel();
                } else if let Some(buf) = view.lane.custom_entry.clone() {
                    // Free-form color: validate, then save through the
                    // same path the picker rows use.
                    let text = buf.trim().to_string();
                    if let Some((axis, key)) = view.lane.pick.clone() {
                        view.lane.clear_entry();
                        if crate::sideline_color::parse_color(&text).is_some() {
                            lane_color_save(view, &axis, &key, &text).await?;
                        } else {
                            view.set_notice(format!(
                                "{axis}.{key}: invalid color (name, indexed(n), #rrggbb)"
                            ));
                            view.reopen_settings_keeping_sel();
                        }
                    }
                }
            }
            SearchKey::Byte(0x7f | 0x08) => {
                if let Some((_, buf)) = view.lane.key_entry.as_mut() {
                    buf.pop();
                } else if let Some(buf) = view.lane.custom_entry.as_mut() {
                    buf.pop();
                }
            }
            SearchKey::Byte(b @ 0x20..=0x7e) => {
                // Same bound as the create overlay: a key name or color
                // string never needs to grow without limit.
                if let Some((_, buf)) = view.lane.key_entry.as_mut() {
                    if buf.len() < MAX_SEARCH_QUERY {
                        buf.push(b as char);
                    }
                } else if let Some(buf) = view.lane.custom_entry.as_mut() {
                    if buf.len() < MAX_SEARCH_QUERY {
                        buf.push(b as char);
                    }
                }
            }
            SearchKey::Byte(_) => {}
        }
    }
    Ok(StdinFlow::Continue)
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
