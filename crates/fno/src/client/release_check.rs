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
        /// Notes parsed from the GitHub release body for the latest tag.
        /// Empty when the fetch or parse failed: notes degrade silently,
        /// the versions and the upgrade command never do.
        notes: Vec<ReleaseNotesSection>,
    },
    Degraded(String),
}

/// One curated section of a GitHub release body: the `### area` heading,
/// its `- bullet` lines, and prose before the first heading (area-less).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReleaseNotesSection {
    pub(crate) area: String,
    pub(crate) bullets: Vec<String>,
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(300);
const NOTES_TIMEOUT: Duration = Duration::from_secs(10);
/// The modal is a popup, not a changelog page: the newest sections and
/// bullets win, mirroring the readiness payload's ten-subject window.
const NOTES_MAX_BULLETS: usize = 10;
const NOTES_MAX_SECTIONS: usize = 6;

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
pub(crate) fn parse_uv_outdated(stdout: &str) -> Result<Option<(String, String)>, String> {
    let cleaned = strip_ansi(stdout);
    for line in cleaned.lines().map(str::trim) {
        if line.split_whitespace().next() != Some("fno") {
            continue;
        }
        let Some(rest) = line.strip_prefix("fno v") else {
            return Err("unrecognized fno outdated row".into());
        };
        let (installed, tail) = rest
            .split_once(' ')
            .ok_or_else(|| "missing latest version".to_string())?;
        let latest = tail
            .trim()
            .strip_prefix("[latest: ")
            .and_then(|value| value.strip_suffix(']'))
            .ok_or_else(|| "missing latest version".to_string())?;
        if installed.is_empty() || latest.trim().is_empty() {
            return Err("empty version in fno outdated row".into());
        }
        return Ok(Some((installed.to_string(), latest.trim().to_string())));
    }
    Ok(None)
}

/// The `(installed, latest)` pair from `brew outdated --json=v2`. A tapped
/// formula can report its full name (`owner/tap/fno`).
pub(crate) fn parse_brew_outdated(json: &str) -> Result<Option<(String, String)>, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("invalid JSON: {e}"))?;
    let formulae = value
        .get("formulae")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "missing formulae array".to_string())?;
    for formula in formulae {
        let name = formula
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "formula row is missing its name".to_string())?;
        if name != "fno" && !name.ends_with("/fno") {
            return Err(format!("unexpected formula in fno outdated output: {name}"));
        }
        let installed = formula
            .get("installed_versions")
            .and_then(serde_json::Value::as_array)
            .and_then(|versions| versions.first())
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "fno formula is missing an installed version".to_string())?;
        let latest = formula
            .get("current_version")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "fno formula is missing the current version".to_string())?;
        if installed.trim().is_empty() || latest.trim().is_empty() {
            return Err("fno formula has an empty version".into());
        }
        return Ok(Some((installed.to_string(), latest.to_string())));
    }
    Ok(None)
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
            Ok(Some((installed, latest))) => ReleaseOutcome::Newer {
                channel: Channel::Uv,
                installed,
                latest,
                notes: Vec::new(),
            },
            Ok(None) => ReleaseOutcome::Current {
                channel: Channel::Uv,
            },
            Err(e) => ReleaseOutcome::Degraded(format!("uv tool list --outdated: {e}")),
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

/// Parse the release body markdown: `### area` opens a section, `- `/`* `
/// is a bullet, prose before the first heading is the area-less intro, the
/// `##` title is skipped, the caps keep the modal a popup. No bullets, no
/// sections.
pub(crate) fn parse_release_body(body: &str) -> Vec<ReleaseNotesSection> {
    let mut sections: Vec<ReleaseNotesSection> = vec![ReleaseNotesSection {
        area: String::new(),
        bullets: Vec::new(),
    }];
    let mut total = 0usize;
    for line in body.lines() {
        if total >= NOTES_MAX_BULLETS {
            break;
        }
        let trimmed = line.trim_end();
        if let Some(area) = trimmed.strip_prefix("### ") {
            if sections.len() >= NOTES_MAX_SECTIONS {
                break;
            }
            sections.push(ReleaseNotesSection {
                area: area.trim().to_string(),
                bullets: Vec::new(),
            });
            continue;
        }
        if trimmed.starts_with("## ") {
            continue;
        }
        let bullet = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "));
        let text = match bullet {
            Some(b) => b.trim().to_string(),
            None => {
                if trimmed.trim().is_empty() || sections.last().is_some_and(|s| !s.area.is_empty())
                {
                    continue;
                }
                trimmed.trim().to_string()
            }
        };
        if text.is_empty() {
            continue;
        }
        if let Some(last) = sections.last_mut() {
            last.bullets.push(text);
            total += 1;
        }
    }
    sections.retain(|s| !s.bullets.is_empty());
    sections
}

/// The release body from a `releases/tags/<tag>` API reply, or None: a
/// malformed reply costs the notes, never the modal.
pub(crate) fn parse_release_json(json_text: &str) -> Option<Vec<ReleaseNotesSection>> {
    let value: serde_json::Value = serde_json::from_str(json_text).ok()?;
    let body = value.get("body")?.as_str()?;
    let sections = parse_release_body(body);
    (!sections.is_empty()).then_some(sections)
}

/// One notes fetch per client per version: the memo keys on the tag so a
/// re-probe never spends the unauthenticated per-IP budget again. Only
/// successes cache; a failed fetch retries on the next probe.
static NOTES_MEMO: std::sync::Mutex<Option<(String, Vec<ReleaseNotesSection>)>> =
    std::sync::Mutex::new(None);

/// The newest published release's notes for `v{latest}`; any failure
/// answers empty and the modal shows versions without notes.
async fn fetch_release_notes(latest: &str) -> Vec<ReleaseNotesSection> {
    if let Ok(memo) = NOTES_MEMO.lock() {
        if let Some((tag, notes)) = memo.as_ref() {
            if tag == latest {
                return notes.clone();
            }
        }
    }
    let url = format!(
        "https://api.github.com/repos/{}/releases/tags/v{latest}",
        crate::update_prebuilt::release_repo()
    );
    let args = [
        "-sS",
        "--max-time",
        "10",
        "-H",
        "Accept: application/vnd.github+json",
        &url,
    ];
    let notes = match run(Path::new("curl"), &args, NOTES_TIMEOUT).await {
        Ok(json_text) => parse_release_json(&json_text).unwrap_or_default(),
        Err(_) => return Vec::new(),
    };
    if !notes.is_empty() {
        if let Ok(mut memo) = NOTES_MEMO.lock() {
            *memo = Some((latest.to_string(), notes.clone()));
        }
    }
    notes
}

/// Fill the notes a release-modal shows for a newer release. Only Newer
/// fetches: a current install never pays the network round.
async fn attach_notes(outcome: ReleaseOutcome) -> ReleaseOutcome {
    match outcome {
        ReleaseOutcome::Newer {
            channel,
            installed,
            latest,
            ..
        } => ReleaseOutcome::Newer {
            channel,
            installed,
            notes: fetch_release_notes(&latest).await,
            latest,
        },
        other => other,
    }
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
        let outcome = match run(Path::new("brew"), &args, PROBE_TIMEOUT).await {
            Err(e) => ReleaseOutcome::Degraded(format!("brew outdated: {e}")),
            Ok(json) => match parse_brew_outdated(&json) {
                Ok(Some((installed, latest))) => ReleaseOutcome::Newer {
                    channel: Channel::Brew,
                    installed,
                    latest,
                    notes: Vec::new(),
                },
                Ok(None) => ReleaseOutcome::Current {
                    channel: Channel::Brew,
                },
                Err(e) => ReleaseOutcome::Degraded(format!("brew outdated: {e}")),
            },
        };
        return attach_notes(outcome).await;
    }
    let Some(uv) = uv else {
        return ReleaseOutcome::NotApplicable;
    };
    let dir = match run(&uv, &["tool", "dir", "--color", "never"], PROBE_TIMEOUT).await {
        Ok(d) => d.trim().to_string(),
        Err(e) => return ReleaseOutcome::Degraded(format!("uv tool dir: {e}")),
    };
    let receipt = std::fs::read_to_string(Path::new(&dir).join("fno/uv-receipt.toml")).ok();
    let outcome = uv_outcome(receipt.as_deref(), || async {
        run(
            &uv,
            &["tool", "list", "--outdated", "--color", "never"],
            PROBE_TIMEOUT,
        )
        .await
    })
    .await;
    attach_notes(outcome).await
}

/// Run the channel's upgrade off the UI loop and return one notice line.
pub(crate) async fn run_upgrade_verb(channel: Channel) -> String {
    let argv = channel.upgrade_argv();
    // The probe may have found uv in an install dir off PATH; spawn that one.
    let program = match channel {
        Channel::Uv => tokio::task::spawn_blocking(crate::bootstrap::find_uv)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| argv[0].into()),
        Channel::Brew => argv[0].into(),
    };
    let mut command = crate::process_admission::tokio_command(program);
    command
        .args(&argv[1..])
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
                notes: Vec::new(),
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
    async fn malformed_uv_fno_row_degrades_instead_of_being_current() {
        let receipt = "[tool]\nrequirements = [{ name = \"fno\" }]\n";
        let outcome = uv_outcome(Some(receipt), || async {
            Ok("fno v0.3.1 [newer: 0.3.2]".to_string())
        })
        .await;
        assert!(
            matches!(&outcome, ReleaseOutcome::Degraded(reason) if reason.contains("uv tool list --outdated")),
            "{outcome:?}"
        );
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
    fn malformed_brew_output_is_not_a_current_result() {
        assert!(parse_brew_outdated("{").is_err());
        assert!(parse_brew_outdated(r#"{"formulae":[{"name":"fno"}],"casks":[]}"#).is_err());
        assert!(parse_brew_outdated(
            r#"{"formulae":[{"name":"other","installed_versions":["0.1"],"current_version":"0.2"}],"casks":[]}"#
        )
        .is_err());
        assert!(parse_brew_outdated(
            r#"{"formulae":[{"name":"fno","installed_versions":[""],"current_version":"0.2"}],"casks":[]}"#
        )
        .is_err());
        assert!(parse_brew_outdated(r#"{"formulae":[],"casks":[]}"#)
            .unwrap()
            .is_none());
    }

    #[test]
    fn parsers_strip_ansi_and_read_tapped_brew_formula() {
        assert_eq!(
            parse_uv_outdated("\u{1b}[1mfno\u{1b}[0m v0.3.1 [latest: 0.3.2]").unwrap(),
            Some(("0.3.1".into(), "0.3.2".into()))
        );
        assert_eq!(
            parse_uv_outdated("fno v0.3.1 \u{1b}[2m[latest: 0.3.2]\u{1b}[0m").unwrap(),
            Some(("0.3.1".into(), "0.3.2".into()))
        );
        let brew = r#"{"formulae":[{"name":"bllshttng/tap/fno","installed_versions":["0.3.1"],"current_version":"0.3.2"}],"casks":[]}"#;
        assert_eq!(
            parse_brew_outdated(brew).unwrap(),
            Some(("0.3.1".into(), "0.3.2".into()))
        );
        assert_eq!(
            parse_brew_outdated(r#"{"formulae":[],"casks":[]}"#).unwrap(),
            None
        );
        assert_eq!(Channel::Brew.upgrade_command(), "brew upgrade fno");
        assert_eq!(Channel::Uv.upgrade_command(), "uv tool upgrade fno");
    }

    #[test]
    fn release_body_parses_caps_and_degrades() {
        let body = "## v0.4.1\n\n42 merged pull requests since v0.4.0.\n\n### mux\n\n- Portals open\n- Themes switch live\n\n### backlog\n\n- Rank orders the board\n";
        let sections = parse_release_body(body);
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].area, "");
        assert_eq!(
            sections[0].bullets,
            vec!["42 merged pull requests since v0.4.0."]
        );
        assert_eq!(sections[1].area, "mux");
        assert_eq!(
            sections[1].bullets,
            vec!["Portals open", "Themes switch live"]
        );
        assert_eq!(sections[2].bullets, vec!["Rank orders the board"]);
        let long = "### area\n".to_string() + &"- bullet\n".repeat(NOTES_MAX_BULLETS + 5);
        let parsed = parse_release_body(&long);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].bullets.len(), NOTES_MAX_BULLETS);
        assert!(parse_release_body("## v0.4.1\n").is_empty());
        assert!(parse_release_json("{\"body\": 3}").is_none());
        assert!(parse_release_json("{").is_none());
    }
}
