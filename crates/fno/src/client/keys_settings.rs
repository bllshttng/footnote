//! The key config's `$EDITOR` door. The table itself lives in
//! `keys_modal.rs` - settings and the menu open the SAME table - so all
//! that remains here is naming the editor and reloading the keymap after
//! it closes.

use super::*;

/// Open the key config in `$EDITOR`, then reload the keymap from config.
/// Rebuilds whichever surface opened it, so the cursor lands back on a live
/// view: the settings modal rebuilds its rows, the which-key modal its table
/// (same filter).
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
    if view.aux.is_some() {
        view.reopen_settings_keeping_sel();
    }
    if let Some(m) = view.keys_modal.as_ref() {
        let filter = m.filter.clone();
        view.keys_modal = Some(keys_modal::keys_modal_with_filter(filter.as_deref()));
    }
}
