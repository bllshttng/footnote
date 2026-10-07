use std::path::{Path, PathBuf};

/// Every file a session leaves beside its socket. Removal and manual recovery
/// both use this list so sidecars cannot drift apart.
pub fn session_files(socket: &Path) -> [PathBuf; 5] {
    [
        socket.to_path_buf(),
        super::version_sidecar_path(socket),
        super::pid_sidecar_path(socket),
        super::owner_sidecar_path(socket),
        visible_sidecar_path(socket),
    ]
}

/// The panes on screen in any attached client (`<name>.visible.json`). It goes
/// with the server: a list left behind would name panes nobody watches.
pub fn visible_sidecar_path(socket: &Path) -> PathBuf {
    socket.with_extension("visible.json")
}

/// Remove all session files during graceful shutdown or stale takeover.
pub fn remove_session_files(socket: &Path) -> std::io::Result<()> {
    for path in session_files(socket) {
        if let Err(error) = std::fs::remove_file(path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error);
            }
        }
    }
    Ok(())
}
