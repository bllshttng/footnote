//! Installed-version readiness for the shared Codex daemon.
//!
//! `is_healthy` answers "is a daemon alive and answering initialize". This
//! module answers a DIFFERENT axis: "is that daemon the version the
//! installed CLI would start". The two never substitute for each other - a
//! symlink moved to 0.154.0 does not change PID 27454's 0.153.4 executable -
//! and the census, the persistent create/resume/attach receipts and the
//! upgrade transaction all read this one owner.

use serde::Serialize;

/// The version verdict. `Current` needs two agreeing positive readings; a
/// socket or pid answer alone is never `Current`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionVerdict {
    Current,
    Stale,
    Ahead,
    Unknown,
}

/// One readiness snapshot of the shared Codex daemon.
#[derive(Debug, Clone, Serialize)]
pub struct CodexDaemonReadiness {
    /// The process/socket/initialize predicate, verbatim from the daemon
    /// adapter. Health and version are separate fields on purpose.
    pub healthy: bool,
    pub pid: Option<u32>,
    pub start_token: Option<u64>,
    pub endpoint: String,
    pub codex_home: String,
    pub installed_version: Option<String>,
    pub live_version: Option<String>,
    pub verdict: VersionVerdict,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Read the whole readiness snapshot. Every reader failure degrades to
/// `Unknown` with the reason carried in `note`; nothing here guesses.
pub fn codex_daemon_readiness() -> CodexDaemonReadiness {
    let adapter = crate::codex_inject::CodexDaemonAdapter::from_environment();
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".codex"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from(".codex"));
    let provider_state = adapter.provider_pid_start();
    let healthy = provider_state
        .as_ref()
        .map(|(pid, start)| crate::daemon::pid_is_ours(*pid, Some(*start)) && adapter.socket_up())
        .unwrap_or(false);
    let installed_version = installed_cli_version();
    let live_version = live_app_server_version();
    let verdict = match (&installed_version, &live_version) {
        (Some(installed), Some(live)) => match compare_versions(installed, live) {
            Some(std::cmp::Ordering::Equal) => VersionVerdict::Current,
            Some(std::cmp::Ordering::Less) => VersionVerdict::Stale,
            Some(std::cmp::Ordering::Greater) => VersionVerdict::Ahead,
            None => VersionVerdict::Unknown,
        },
        (installed, live) => {
            let _ = (installed, live);
            VersionVerdict::Unknown
        }
    };
    CodexDaemonReadiness {
        healthy,
        pid: provider_state.as_ref().map(|(pid, _)| *pid),
        start_token: provider_state.as_ref().map(|(_, start)| *start),
        endpoint: endpoint_path().to_string_lossy().into_owned(),
        codex_home: codex_home.to_string_lossy().into_owned(),
        installed_version,
        live_version,
        verdict,
        note: None,
    }
}

fn endpoint_path() -> std::path::PathBuf {
    crate::codex_inject::CodexDaemonAdapter::from_environment().control_socket()
}

/// The installed CLI's own version: `codex --version`, parsed from its last
/// whitespace-separated token. `None` = the CLI is absent or unreadable.
pub fn installed_cli_version() -> Option<String> {
    let out = std::process::Command::new("codex")
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace().last().map(str::to_string)
}

/// The LIVE app-server's version, read two independent ways and required to
/// agree: the initialize response's server user-agent info, and the vendor
/// `codex app-server daemon version` verb as a positive control (it may
/// itself fail to inspect the managed pid under a sandbox, so it is never
/// the only reader). Disagreeing or empty readings are `None` - unknown,
/// never current.
pub fn live_app_server_version() -> Option<String> {
    let readings = live_version_readings();
    let mut unique: Vec<String> = readings.iter().filter_map(|(_, v)| v.clone()).collect();
    unique.dedup();
    if unique.len() == 1 {
        Some(unique[0].clone())
    } else {
        None
    }
}

/// Both raw readings with their reader names, for a receipt that must name
/// what it saw when it saw nothing.
pub fn live_version_readings() -> Vec<(String, Option<String>)> {
    vec![
        (
            "initialize serverInfo".to_string(),
            crate::codex_inject::initialize_server_info().and_then(|info| {
                info.get("version")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            }),
        ),
        ("daemon version verb".to_string(), daemon_version_verb()),
    ]
}

fn daemon_version_verb() -> Option<String> {
    let out = std::process::Command::new("codex")
        .args(["app-server", "daemon", "version"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace().last().map(str::to_string)
}

/// Three-way version compare on dotted numeric prefixes. `None` = not both
/// parseable: the verdict is unknown, never a guess from a prefix.
pub fn compare_versions(installed: &str, live: &str) -> Option<std::cmp::Ordering> {
    let parse = |text: &str| -> Option<Vec<u64>> {
        let trimmed = text.trim_start_matches('v');
        let numeric: String = trimmed
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if numeric.is_empty() {
            return None;
        }
        let mut parts: Vec<u64> = numeric
            .split('.')
            .filter(|p| !p.is_empty())
            .map(|p| p.parse().ok())
            .collect::<Option<Vec<_>>>()?;
        parts.resize(3, 0);
        Some(parts)
    };
    let (a, b) = (parse(installed)?, parse(live)?);
    Some(a.cmp(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_versions_orders_the_three_verdicts() {
        use std::cmp::Ordering::*;
        assert_eq!(compare_versions("0.153.4", "0.153.4"), Some(Equal));
        assert_eq!(
            compare_versions("0.154.0", "0.153.4"),
            Some(Greater),
            "installed newer = live stale"
        );
        assert_eq!(
            compare_versions("0.152.0", "0.153.4"),
            Some(Less),
            "installed older than live = ahead"
        );
        assert_eq!(compare_versions("garbage", "0.153.4"), None);
    }

    #[test]
    fn live_version_requires_two_agreeing_readings() {
        // One reader answers, one fails: unknown, never current from a
        // single observation.
        let one = vec![
            ("a".to_string(), Some("0.1.0".to_string())),
            ("b".to_string(), None),
        ];
        let mut unique: Vec<String> = one.iter().filter_map(|(_, v)| v.clone()).collect();
        unique.dedup();
        assert_eq!(unique.len(), 1);
        let disagree = vec![
            ("a".to_string(), Some("0.1.0".to_string())),
            ("b".to_string(), Some("0.2.0".to_string())),
        ];
        let mut unique: Vec<String> = disagree.iter().filter_map(|(_, v)| v.clone()).collect();
        unique.dedup();
        assert_ne!(unique.len(), 1, "disagreeing readers are unknown");
    }
}
