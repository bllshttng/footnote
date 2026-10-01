//! The settings Keybindings tab: the prefix and every rebindable chord, a
//! "press the new key" capture that rebinds one through the keymap resolver,
//! and the key config opened in `$EDITOR`.

use super::input_field::back_row;
use super::*;
use crate::keys::KeySection;

/// An open capture: the action id being rebound ("prefix" for the prefix),
/// the split-arrow carry, and the list position to return to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyCapture {
    action: String,
    esc: Vec<u8>,
    sel: usize,
    scroll: usize,
}

const SECTIONS: [KeySection; 4] = [
    KeySection::Global,
    KeySection::Navigation,
    KeySection::WorkspacesTabs,
    KeySection::Panes,
];

fn entry(glyph: String, label: &str) -> PopupRow {
    PopupRow::Entry {
        glyph,
        label: label.into(),
        hint: String::new(),
        enabled: true,
    }
}

/// The label and current key the page shows for an action id.
fn describe(action: &str) -> (String, String) {
    if action == "prefix" {
        return ("prefix".into(), crate::keys::prefix_display());
    }
    crate::keys::editable_bindings()
        .into_iter()
        .find(|kb| kb.action == action)
        .map(|kb| (kb.label.to_string(), kb.disp))
        .unwrap_or_else(|| (action.to_string(), "unbound".into()))
}

pub(super) fn rows(view: &View) -> (Vec<PopupRow>, Vec<AuxAction>) {
    let mut rows = Vec::new();
    let mut actions = Vec::new();
    if let Some(capture) = &view.key_capture {
        let (label, now) = describe(&capture.action);
        rows.push(back_row());
        actions.push(AuxAction::SettingsBack);
        rows.push(PopupRow::Header(format!("press the new key for {label}")));
        rows.push(PopupRow::Info {
            label: "now".into(),
            value: now,
        });
        rows.push(PopupRow::Entry {
            glyph: " ".into(),
            label: "esc cancels · 1-9 and the prefix are refused".into(),
            hint: String::new(),
            enabled: false,
        });
        return (rows, actions);
    }
    rows.push(entry(crate::keys::prefix_display(), "prefix"));
    actions.push(AuxAction::KeyCapture("prefix".into()));
    let bindings = crate::keys::editable_bindings();
    for section in SECTIONS {
        rows.push(PopupRow::Header(String::new()));
        rows.push(PopupRow::Header(section.title().into()));
        for kb in bindings.iter().filter(|kb| kb.section == section) {
            rows.push(entry(kb.disp.clone(), kb.label));
            actions.push(AuxAction::KeyCapture(kb.action.into()));
        }
    }
    rows.push(PopupRow::Rule);
    rows.push(entry("✎".into(), "edit keys in $EDITOR"));
    actions.push(AuxAction::EditKeysFile);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    rows.push(PopupRow::Info {
        label: "file".into(),
        value: crate::digest_overlay::keys_file_path(&cwd)
            .display()
            .to_string(),
    });
    (rows, actions)
}

pub(super) fn open_capture(view: &mut View, action: String) {
    let (sel, scroll) = view
        .aux
        .as_ref()
        .map_or((0, 0), |m| (m.popup.sel, m.popup.scroll));
    view.key_capture = Some(KeyCapture {
        action,
        esc: Vec::new(),
        sel,
        scroll,
    });
    view.reopen_settings_keeping_sel();
}

/// Close an open capture and put the cursor back on the row it was opened
/// from, so the next edit starts there and not at the top of the list.
pub(super) fn close(view: &mut View) -> bool {
    let Some(capture) = view.key_capture.take() else {
        return false;
    };
    view.reopen_settings_keeping_sel();
    if let Some(m) = view.aux.as_mut() {
        m.popup.sel = capture.sel;
        m.popup.scroll = capture.scroll;
    }
    true
}

/// Keys while the capture is open. A lone Esc steps back; an arrow or other
/// escape sequence is dropped whole; any other byte is tried as the new key.
pub(super) async fn capture_keys(
    view: &mut View,
    bytes: &[u8],
    _sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let Some(capture) = view.key_capture.as_mut() else {
        return Ok(());
    };
    let keys = fold_search_input(&mut capture.esc, bytes);
    let mut keys = keys.into_iter().peekable();
    while let Some(key) = keys.next() {
        match key {
            // An SS3 arrow (application cursor mode) folds as Esc, `O`, and
            // a final byte: drop all three, as a CSI arrow is dropped.
            SearchKey::Esc if keys.peek() == Some(&SearchKey::Byte(b'O')) => {
                keys.next();
                keys.next();
            }
            SearchKey::Esc => {
                settings_modal::back(view);
                break;
            }
            SearchKey::Byte(b) => {
                if apply(view, b).await {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Try `byte` as the captured action's new key. True when it took (the
/// capture closes); a refusal keeps the capture open for another key.
async fn apply(view: &mut View, byte: u8) -> bool {
    let Some(action) = view.key_capture.as_ref().map(|c| c.action.clone()) else {
        return true;
    };
    let disp = crate::keys::key_disp(byte);
    if crate::keys::parse_key(&disp) != Some(byte) {
        view.set_notice("that key cannot be a chord".into());
        return false;
    }
    let (resolved, config_key) = if action == "prefix" {
        (crate::keys::resolve_prefix_change(&disp), "mux.prefix")
    } else {
        (crate::keys::resolve_rebind(&action, byte), "mux.keys")
    };
    let map = match resolved {
        Ok(map) => map,
        Err(refusal) => {
            view.set_notice(refusal);
            return false;
        }
    };
    crate::keys::reinstall(map.clone());
    let value = if action == "prefix" {
        disp.clone()
    } else {
        crate::keys::rebinds_json(&map)
    };
    let (label, _) = describe(&action);
    let notice = match spawn_config_set(config_key, &value).await {
        Err(_) => format!("{label}: {disp} applied this session; save failed"),
        Ok(()) => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            if crate::digest_overlay::keymap(&cwd).0 == map {
                format!("{label}: {disp}")
            } else {
                format!(
                    "{label}: {disp} saved, but the config the mux reads differs; check config layering"
                )
            }
        }
    };
    view.set_notice(notice);
    close(view);
    true
}

/// Open the key config in `$EDITOR`, then reload the keymap from config.
pub(super) async fn edit_keys_file(view: &mut View) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let path = crate::digest_overlay::keys_file_path(&cwd);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let edited = path.clone();
    let ok = tokio::task::spawn_blocking(move || editor::edit_file_suspended(&edited))
        .await
        .unwrap_or(false);
    let (map, warnings) = crate::digest_overlay::keymap(&cwd);
    crate::keys::reinstall(map);
    view.set_notice(match warnings.first() {
        Some(warning) => warning.0.clone(),
        None if ok => format!("keys reloaded from {}", path.display()),
        None => "editor: failed or exited non-zero; keys reloaded from config as it stands".into(),
    });
    view.reopen_settings_keeping_sel();
}
