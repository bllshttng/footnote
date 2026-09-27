//! Quarantine of interrupted-write temp files: the `.tmp.`/`.part` debris
//! a daemon crash leaves beside its targets, moved under
//! `~/.fno/.interrupted-writes/` so a reader never half-sees a write.
//! Moved out of daemon.rs (over the line budget) with its behavior intact.

use crate::events::EventEmitter;
use crate::paths::AgentsHome;
use crate::state;
use serde_json::json;

pub(crate) fn quarantine_interrupted_write_temps(
    home: &AgentsHome,
    emitter: &EventEmitter,
) -> Vec<String> {
    let mut found = Vec::new();
    let state_root = home.root().parent().unwrap_or(home.root());
    let quarantine = state_root.join(".interrupted-writes");
    for dir in [home.root(), state_root] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if !kind.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !(name.starts_with('.') && (name.contains(".tmp.") || name.ends_with(".part"))) {
                continue;
            }
            let target_name = name
                .strip_prefix('.')
                .and_then(|name| name.split_once(".tmp.").map(|(target, _)| target))
                .or_else(|| {
                    name.strip_prefix('.')
                        .and_then(|name| name.strip_suffix(".part"))
                });
            let Some(target_name) = target_name else {
                continue;
            };
            let target = dir.join(target_name);
            let Ok(Some(_lock)) = state::try_lock_path_exclusive(&target) else {
                continue;
            };
            if !entry.path().exists() {
                continue;
            }
            let _ = std::fs::create_dir_all(&quarantine);
            let dest = quarantine.join(format!("{}-{}", crate::daemon::now_compact(), name));
            let outcome = if std::fs::rename(entry.path(), &dest).is_ok() {
                "quarantined"
            } else {
                "detected"
            };
            let _ = emitter.emit(
                "daemon_recovery_interrupted_temp",
                &json!({"name": name, "outcome": outcome, "quarantined_to": dest}),
            );
            found.push(name);
        }
    }
    found
}
