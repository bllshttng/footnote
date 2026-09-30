//! Read the narrow owner lease carried by fno-launched sandbox processes.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerLease {
    pub pid: u32,
    pub birth: u64,
    pub session: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerStatus {
    Alive,
    Dead,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerRead {
    Owner(OwnerLease),
    Ownerless,
    Unknown,
}

#[derive(serde::Deserialize)]
struct OwnerSidecar {
    server_pid: u32,
    server_birth: u64,
    owner_pid: u32,
    owner_birth: u64,
    owner_session: String,
}

pub fn owner_sidecar_path(socket: &Path) -> PathBuf {
    socket.with_extension("owner")
}

pub fn owner_lease_for_server(pid: u32, socket: &Path) -> OwnerRead {
    let path = owner_sidecar_path(socket);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let Some(lease) = crate::daemon::process_start_time(pid)
                .and_then(|birth| lease_from_sidecar(&bytes, pid, birth))
            else {
                return OwnerRead::Unknown;
            };
            OwnerRead::Owner(lease)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(target_os = "linux")]
            {
                match process_environment(pid) {
                    Some(bytes) => match lease_from_environment(&bytes) {
                        Some(lease) => OwnerRead::Owner(lease),
                        None if bytes.windows(10).any(|window| window == b"FNO_OWNER_") => {
                            OwnerRead::Unknown
                        }
                        None => OwnerRead::Ownerless,
                    },
                    None => OwnerRead::Unknown,
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = error;
                OwnerRead::Ownerless
            }
        }
        Err(_) => OwnerRead::Unknown,
    }
}

pub fn lease_from_sidecar(
    bytes: &[u8],
    expected_server_pid: u32,
    expected_birth: u64,
) -> Option<OwnerLease> {
    let sidecar = serde_json::from_slice::<OwnerSidecar>(bytes).ok()?;
    if sidecar.server_pid != expected_server_pid
        || sidecar.server_birth != expected_birth
        || sidecar.owner_pid <= 1
        || sidecar.owner_birth == 0
        || !valid_owner_session(&sidecar.owner_session)
    {
        return None;
    }
    Some(OwnerLease {
        pid: sidecar.owner_pid,
        birth: sidecar.owner_birth,
        session: sidecar.owner_session,
    })
}

#[cfg(target_os = "linux")]
fn process_environment(pid: u32) -> Option<Vec<u8>> {
    std::fs::read(format!("/proc/{pid}/environ")).ok()
}

pub fn lease_from_environment(bytes: &[u8]) -> Option<OwnerLease> {
    let mut pid = None;
    let mut birth = None;
    let mut session = None;
    for item in bytes.split(|byte| *byte == 0 || byte.is_ascii_whitespace()) {
        let Ok(item) = std::str::from_utf8(item) else {
            continue;
        };
        if let Some(value) = item.strip_prefix("FNO_OWNER_PID=") {
            pid = value.parse::<u32>().ok().filter(|value| *value > 1);
        } else if let Some(value) = item.strip_prefix("FNO_OWNER_BIRTH=") {
            birth = value.parse::<u64>().ok().filter(|value| *value > 0);
        } else if let Some(value) = item.strip_prefix("FNO_OWNER_SESSION=") {
            if valid_owner_session(value) {
                session = Some(value.to_string());
            }
        }
    }
    Some(OwnerLease {
        pid: pid?,
        birth: birth?,
        session: session?,
    })
}

fn valid_owner_session(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= 128
        && session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

pub fn owner_status(lease: &OwnerLease) -> OwnerStatus {
    match crate::daemon::process_start_time(lease.pid) {
        Some(birth) if birth == lease.birth => OwnerStatus::Alive,
        Some(_) => OwnerStatus::Dead,
        None => {
            let result = unsafe { libc::kill(lease.pid as libc::pid_t, 0) };
            if result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
                OwnerStatus::Unknown
            } else {
                OwnerStatus::Dead
            }
        }
    }
}
