//! The journals the product writes: the repo's space (x-b1ee), resolved by the
//! same accessors the binary uses, so a fixture seeds and asserts where the
//! rows actually land instead of at the retired checkout path.

pub fn project_events(cwd: &std::path::Path) -> std::path::PathBuf {
    let path = fno_agents::paths::events_path(cwd);
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    path
}

pub fn project_ledger(cwd: &std::path::Path) -> std::path::PathBuf {
    let path = fno_agents::paths::ledger_path(cwd);
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    path
}
