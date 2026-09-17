//! Is a newer published fno release available for this install? The
//! update modal's release answer. `fno doctor update --check` reads the
//! source checkout only, so on a `uv tool install fno` or Homebrew install it
//! can only degrade. This module asks the package manager that owns the
//! install instead, and names the one command that upgrades it.

use std::path::Path;
use std::time::Duration;

/// The package manager that owns a release install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Channel {
    Uv,
    Brew,
}

impl Channel {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Channel::Uv => "uv",
            Channel::Brew => "brew",
        }
    }

    pub(crate) fn upgrade_argv(self) -> &'static [&'static str] {
        match self {
            Channel::Uv => &["uv", "tool", "upgrade", "fno"],
            Channel::Brew => &["brew", "upgrade", "fno"],
        }
    }

    pub(crate) fn upgrade_command(self) -> String {
        self.upgrade_argv().join(" ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReleaseOutcome {
    /// A source install, or no channel fno can upgrade. No menu row.
    NotApplicable,
    Current {
        channel: Channel,
    },
    Newer {
        channel: Channel,
        installed: String,
        latest: String,
    },
    Degraded(String),
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(300);

/// `Some(true)` when the uv receipt installed fno from an index (so
/// `uv tool upgrade` can move it), `Some(false)` for a directory, path, git,
/// url or editable source, `None` when the receipt has no fno entry.
pub(crate) fn receipt_is_index(receipt: &str) -> Option<bool> {
    let value: toml::Value = toml::from_str(receipt).ok()?;
    let reqs = value.get("tool")?.get("requirements")?.as_array()?;
    let fno = reqs
        .iter()
        .find(|r| r.get("name").and_then(|n| n.as_str()) == Some("fno"))?;
    let local = ["directory", "path", "git", "url", "editable"]
        .iter()
        .any(|k| fno.get(k).is_some());
    Some(!local)
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The `(installed, latest)` pair from `uv tool list --outdated`, whose fno
/// line reads `fno v0.3.1 [latest: 0.3.2]`.
pub(crate) fn parse_uv_outdated(stdout: &str) -> Option<(String, String)> {
    strip_ansi(stdout).lines().find_map(|line| {
        let rest = line.trim().strip_prefix("fno v")?;
        let (installed, tail) = rest.split_once(' ')?;
        let latest = tail.trim().strip_prefix("[latest: ")?.strip_suffix(']')?;
        Some((installed.to_string(), latest.trim().to_string()))
    })
}

/// The `(installed, latest)` pair from `brew outdated --json=v2`. A tapped
/// formula can report its full name (`owner/tap/fno`).
pub(crate) fn parse_brew_outdated(json: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    value["formulae"].as_array()?.iter().find_map(|f| {
        let name = f["name"].as_str()?;
        if name != "fno" && !name.ends_with("/fno") {
            return None;
        }
        let installed = f["installed_versions"].as_array()?.first()?.as_str()?;
        let latest = f["current_version"].as_str()?;
        Some((installed.to_string(), latest.to_string()))
    })
}

/// Fold a uv receipt and the outdated run into an outcome. `outdated` is only
/// called for an index receipt, so a source install never pays for it.
pub(crate) async fn uv_outcome<F, Fut>(receipt: Option<&str>, outdated: F) -> ReleaseOutcome
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<String, String>>,
{
    let Some(receipt) = receipt else {
        return ReleaseOutcome::NotApplicable;
    };
    if receipt_is_index(receipt) != Some(true) {
        return ReleaseOutcome::NotApplicable;
    }
    match outdated().await {
        Err(e) => ReleaseOutcome::Degraded(format!("uv tool list --outdated: {e}")),
        Ok(stdout) => match parse_uv_outdated(&stdout) {
            Some((installed, latest)) => ReleaseOutcome::Newer {
                channel: Channel::Uv,
                installed,
                latest,
            },
            None => ReleaseOutcome::Current {
                channel: Channel::Uv,
            },
        },
    }
}

/// Run a command with a bound; `Ok(stdout)` only on exit 0.
async fn run(program: &Path, args: &[&str], bound: Duration) -> Result<String, String> {
    let mut command = crate::process_admission::tokio_command(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    let output = match tokio::time::timeout(bound, fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => return Err(format!("timed out after {}s", bound.as_secs())),
    };
    if !output.status.success() {
        return Err(format!("exit {}", output.status.code().unwrap_or(-1)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Ask the channel that owns this install whether a newer release exists.
/// Runs off the UI loop; every failure degrades, never hangs.
pub(crate) async fn probe_release() -> ReleaseOutcome {
    let found = tokio::task::spawn_blocking(|| {
        let brew = crate::bootstrap::resolved_python_script()
            .and_then(|p| std::fs::canonicalize(p).ok())
            .is_some_and(|p| p.to_string_lossy().contains("/Cellar/fno/"));
        (brew, crate::bootstrap::find_uv())
    })
    .await;
    let Ok((brew, uv)) = found else {
        return ReleaseOutcome::NotApplicable;
    };
    if brew {
        let args = ["outdated", "--json=v2", "--formula", "fno"];
        return match run(Path::new("brew"), &args, PROBE_TIMEOUT).await {
            Err(e) => ReleaseOutcome::Degraded(format!("brew outdated: {e}")),
            Ok(json) => match parse_brew_outdated(&json) {
                Some((installed, latest)) => ReleaseOutcome::Newer {
                    channel: Channel::Brew,
                    installed,
                    latest,
                },
                None => ReleaseOutcome::Current {
                    channel: Channel::Brew,
                },
            },
        };
    }
    let Some(uv) = uv else {
        return ReleaseOutcome::NotApplicable;
    };
    let dir = match run(&uv, &["tool", "dir", "--color", "never"], PROBE_TIMEOUT).await {
        Ok(d) => d.trim().to_string(),
        Err(e) => return ReleaseOutcome::Degraded(format!("uv tool dir: {e}")),
    };
    let receipt = std::fs::read_to_string(Path::new(&dir).join("fno/uv-receipt.toml")).ok();
    uv_outcome(receipt.as_deref(), || async {
        run(
            &uv,
            &["tool", "list", "--outdated", "--color", "never"],
            PROBE_TIMEOUT,
        )
        .await
    })
    .await
}

/// Run the channel's upgrade off the UI loop and return one notice line.
pub(crate) async fn run_upgrade_verb(channel: Channel) -> String {
    let argv = channel.upgrade_argv();
    let args: Vec<&str> = argv[1..]
        .iter()
        .filter(|a| !a.is_empty())
        .copied()
        .collect();
    let mut command = crate::process_admission::tokio_command(argv[0]);
    command
        .args(&args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    let output = match tokio::time::timeout(UPGRADE_TIMEOUT, fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return format!("upgrade failed ({}): {e}", channel.upgrade_command()),
        Err(_) => return format!("upgrade timed out after {}s", UPGRADE_TIMEOUT.as_secs()),
    };
    let last = |bytes: &[u8]| {
        strip_ansi(&String::from_utf8_lossy(bytes))
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(str::to_string)
    };
    let line = last(&output.stdout)
        .or_else(|| last(&output.stderr))
        .unwrap_or_default();
    if output.status.success() {
        format!("upgrade ok: {line}")
    } else {
        format!(
            "upgrade failed (exit {}): {line}",
            output.status.code().unwrap_or(-1)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn index_receipt_and_outdated_line_read_newer() {
        let receipt = "[tool]\nrequirements = [{ name = \"fno\" }]\n";
        assert_eq!(receipt_is_index(receipt), Some(true));
        let outcome = uv_outcome(Some(receipt), || async {
            Ok("graphifyy v0.9.57 [latest: 0.9.63]\nfno v0.3.1 [latest: 0.3.2]\n".to_string())
        })
        .await;
        assert_eq!(
            outcome,
            ReleaseOutcome::Newer {
                channel: Channel::Uv,
                installed: "0.3.1".into(),
                latest: "0.3.2".into(),
            }
        );
    }

    #[tokio::test]
    async fn directory_receipt_is_not_applicable_and_never_runs_outdated() {
        let receipt = "[tool]\nrequirements = [{ name = \"fno\", directory = \"/x/cli\" }]\n";
        assert_eq!(receipt_is_index(receipt), Some(false));
        let outcome = uv_outcome(Some(receipt), || async {
            panic!("uv tool list --outdated must not run for a source install")
        })
        .await;
        assert_eq!(outcome, ReleaseOutcome::NotApplicable);
    }

    #[tokio::test]
    async fn failed_outdated_run_degrades_naming_the_command() {
        let receipt = "[tool]\nrequirements = [{ name = \"fno\" }]\n";
        let outcome = uv_outcome(Some(receipt), || async { Err("exit 2".to_string()) }).await;
        match outcome {
            ReleaseOutcome::Degraded(r) => assert!(r.contains("uv tool list --outdated"), "{r}"),
            other => panic!("expected Degraded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn current_when_fno_absent_from_outdated_and_missing_receipt_not_applicable() {
        let receipt = "[tool]\nrequirements = [{ name = \"fno\" }]\n";
        let outcome = uv_outcome(Some(receipt), || async {
            Ok("\u{1b}[1mgraphifyy\u{1b}[0m v0.9.57 [latest: 0.9.63]\n".to_string())
        })
        .await;
        assert_eq!(
            outcome,
            ReleaseOutcome::Current {
                channel: Channel::Uv
            }
        );
        let none = uv_outcome(None, || async { Ok(String::new()) }).await;
        assert_eq!(none, ReleaseOutcome::NotApplicable);
        assert_eq!(receipt_is_index("[tool]\nrequirements = []\n"), None);
    }

    #[test]
    fn parsers_strip_ansi_and_read_tapped_brew_formula() {
        assert_eq!(
            parse_uv_outdated("\u{1b}[1mfno\u{1b}[0m v0.3.1 [latest: 0.3.2]"),
            Some(("0.3.1".into(), "0.3.2".into()))
        );
        assert_eq!(
            parse_uv_outdated("fno v0.3.1 \u{1b}[2m[latest: 0.3.2]\u{1b}[0m"),
            Some(("0.3.1".into(), "0.3.2".into()))
        );
        let brew = r#"{"formulae":[{"name":"bllshttng/tap/fno","installed_versions":["0.3.1"],"current_version":"0.3.2"}],"casks":[]}"#;
        assert_eq!(
            parse_brew_outdated(brew),
            Some(("0.3.1".into(), "0.3.2".into()))
        );
        assert_eq!(parse_brew_outdated(r#"{"formulae":[],"casks":[]}"#), None);
        assert_eq!(Channel::Brew.upgrade_command(), "brew upgrade fno");
        assert_eq!(Channel::Uv.upgrade_command(), "uv tool upgrade fno");
    }
}
