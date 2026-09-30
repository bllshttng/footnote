//! Read the narrow owner lease carried by fno-launched sandbox processes.

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

pub fn process_environment(pid: u32) -> Option<Vec<u8>> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read(format!("/proc/{pid}/environ")).ok()
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("ps")
            .args(["eww", "-p", &pid.to_string(), "-o", "command="])
            .output()
            .ok()?;
        output.status.success().then_some(output.stdout)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
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
            if !value.trim().is_empty() {
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

pub fn owner_lease(pid: u32) -> Option<OwnerLease> {
    lease_from_environment(&process_environment(pid)?)
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
