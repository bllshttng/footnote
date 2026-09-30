use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

/// The owner lease beside an owner-bound sandbox session socket.
pub fn owner_sidecar_path(socket: &Path) -> PathBuf {
    socket.with_extension("owner")
}

/// Persist both process births so the Rust sweep can reject reused pids.
pub fn write_owner_sidecar(
    socket: &Path,
    owner_pid: u32,
    owner_birth: u64,
    owner_session: &str,
) -> std::io::Result<()> {
    let Some(server_birth) = super::pid_start_time(std::process::id()) else {
        return Err(std::io::Error::other("server process birth is unreadable"));
    };
    let bytes = serde_json::to_vec(&serde_json::json!({
        "server_pid": std::process::id(),
        "server_birth": server_birth,
        "owner_pid": owner_pid,
        "owner_birth": owner_birth,
        "owner_session": owner_session,
    }))
    .map_err(std::io::Error::other)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(owner_sidecar_path(socket))?;
    file.write_all(&bytes)
}
