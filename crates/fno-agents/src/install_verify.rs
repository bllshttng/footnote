//! Exec-proof of a deployed binary, with the poisoned-install-path repair.
//!
//! Measured 2026-09-16: an install that renames a new build over a path whose
//! old inode is still mapped leaves the kernel signature cache stale, and every
//! later exec of that path is SIGKILLed while `codesign -vvv` reads valid and a
//! byte-identical copy of the same file runs from another path. The repair
//! gives the path a fresh inode: copy to a sibling name in the same directory,
//! chmod 755, rename the sibling over the path. The probe runs once, repairs at
//! most once on the kill-shaped classifications, and re-probes; a timeout or a
//! clean nonzero exit never earns a repair, because repairing those would hide
//! a different bug.

use std::path::Path;
use std::time::Duration;

/// One bounded `version --json` probe of a deployed executable, classified.
#[derive(Debug, Clone, PartialEq)]
pub enum Probe {
    /// Clean answer carrying the build's `crates_rev` (and python script path).
    Ran { rev: String, script: Option<String> },
    /// Killed by signal `n` - the poisoned-path signature.
    Signal(i32),
    /// Exited with code `n`; `had_output` separates a real CLI error from silence.
    Exit { code: i32, had_output: bool },
    /// Exited 0 but produced no usable rev (empty, unparseable, dirty, or
    /// rev-stamp-less output); the payload is the instrument error.
    NoOutput(String),
    /// Did not exit within the budget.
    Timeout(Duration),
    /// Could not spawn at all (ENOEXEC, permissions, missing file).
    SpawnFailed(String),
}

impl Probe {
    /// The instrument-error string the classifier renders; None = Ran.
    pub fn instrument_error(&self) -> Option<String> {
        match self {
            Probe::Ran { .. } => None,
            Probe::Signal(n) => Some(format!("killed by signal {n} on `version --json`")),
            Probe::Exit { code, .. } => Some(format!("exited {code} on `version --json`")),
            Probe::NoOutput(msg) => Some(msg.clone()),
            Probe::Timeout(t) => Some(format!("hung on `version --json` (>{t:?})")),
            Probe::SpawnFailed(e) => Some(format!("could not be executed ({e})")),
        }
    }

    /// Short classification label, as it appears in repair evidence.
    pub fn describe(&self) -> String {
        match self {
            Probe::Ran { .. } => "ran".to_string(),
            Probe::Signal(n) => format!("signal({n})"),
            Probe::Exit { code, .. } => format!("exit({code})"),
            Probe::NoOutput(_) => "no_output".to_string(),
            Probe::Timeout(_) => "timeout".to_string(),
            Probe::SpawnFailed(_) => "spawn_failed".to_string(),
        }
    }

    /// Kill-shaped failures where the bytes are good and the path is the
    /// suspect: the only cases a repair would not hide a different bug. A
    /// timeout is a hang and a clean nonzero exit with output is a real CLI
    /// error; exit 137 is the shell-visible form of the same SIGKILL.
    pub fn repair_warranted(&self) -> bool {
        matches!(
            self,
            Probe::Signal(_) | Probe::SpawnFailed(_) | Probe::Exit { code: 137, .. }
        )
    }
}

/// One bounded `<path> version --json` exec.
pub fn probe_exec(path: &Path, timeout: Duration) -> Probe {
    use std::io::Read;
    use std::process::{Command, Stdio};
    if !path.is_file() {
        return Probe::SpawnFailed("no file at path".to_string());
    }
    let mut child = match Command::new(path)
        .args(["version", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Probe::SpawnFailed(e.to_string()),
    };
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if started.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Probe::Timeout(timeout);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Probe::SpawnFailed(e.to_string()),
        }
    };
    let mut out = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut out);
    }
    let status = match status {
        Some(s) => s,
        None => return Probe::SpawnFailed("process vanished without a status".to_string()),
    };
    if !status.success() {
        #[cfg(unix)]
        if let Some(n) = std::os::unix::process::ExitStatusExt::signal(&status) {
            return Probe::Signal(n);
        }
        return Probe::Exit {
            code: status.code().unwrap_or(-1),
            had_output: !out.trim().is_empty(),
        };
    }
    if out.trim().is_empty() {
        return Probe::NoOutput("emitted no `version --json` output".to_string());
    }
    let data: serde_json::Value = match serde_json::from_str(&out) {
        Ok(v) => v,
        Err(_) => {
            return Probe::NoOutput("emitted unparseable `version --json` output".to_string())
        }
    };
    if !data.is_object() {
        return Probe::NoOutput("emitted unexpected `version --json` output".to_string());
    }
    if data.get("dirty").and_then(|d| d.as_bool()) == Some(true) {
        return Probe::NoOutput("was built from a dirty crates/ tree".to_string());
    }
    let rev = data
        .get("crates_rev")
        .and_then(|r| r.as_str())
        .filter(|r| !r.is_empty() && *r != "unknown")
        .map(|r| r.to_string());
    let Some(rev) = rev else {
        return Probe::NoOutput("carries no rev stamp (built outside a git checkout?)".to_string());
    };
    let script = data
        .get("python_script")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    Probe::Ran { rev, script }
}

/// Give `path` a fresh inode in place: copy to a sibling name in the same
/// directory, chmod 755, rename the sibling over the path. The sibling is
/// unlinked on any failure, leaving the original path and bytes intact.
pub fn repair_inode(path: &Path) -> Result<(), String> {
    let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) else {
        return Err(format!("{} has no parent directory", path.display()));
    };
    let Some(name) = path.file_name() else {
        return Err(format!("{} has no file name", path.display()));
    };
    let sibling = dir.join(format!(
        ".{}.inode-repair.{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    let fail = |step: &str, e: std::io::Error| -> String {
        let _ = std::fs::remove_file(&sibling);
        format!("{step} for inode repair failed ({e})")
    };
    std::fs::copy(path, &sibling).map_err(|e| fail("copy", e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sibling, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| fail("chmod 755", e))?;
    }
    std::fs::rename(&sibling, path).map_err(|e| fail("rename", e))?;
    Ok(())
}

/// Probe, classify, repair at most once, re-probe. The probe is a parameter so
/// a test drives every classification without needing a kernel kill.
#[derive(Debug, Clone, PartialEq)]
pub struct VerifyOutcome {
    pub rev: Option<String>,
    pub script: Option<String>,
    pub instrument_error: Option<String>,
    /// Classification that triggered the repair, when one ran and helped.
    pub repaired_after: Option<String>,
}

fn outcome_from(probe: &Probe) -> VerifyOutcome {
    match probe {
        Probe::Ran { rev, script } => VerifyOutcome {
            rev: Some(rev.clone()),
            script: script.clone(),
            instrument_error: None,
            repaired_after: None,
        },
        other => VerifyOutcome {
            rev: None,
            script: None,
            instrument_error: other.instrument_error(),
            repaired_after: None,
        },
    }
}

pub fn verify_and_repair(
    path: &Path,
    timeout: Duration,
    probe: impl Fn(&Path, Duration) -> Probe,
) -> VerifyOutcome {
    let first = probe(path, timeout);
    if !first.repair_warranted() {
        return outcome_from(&first);
    }
    let trigger = first.describe();
    if let Err(e) = repair_inode(path) {
        return VerifyOutcome {
            rev: None,
            script: None,
            instrument_error: Some(format!("inode repair failed after {trigger}: {e}")),
            repaired_after: None,
        };
    }
    let second = probe(path, timeout);
    match &second {
        Probe::Ran { rev, script } => VerifyOutcome {
            rev: Some(rev.clone()),
            script: script.clone(),
            instrument_error: None,
            repaired_after: Some(trigger),
        },
        other => {
            let err = other
                .instrument_error()
                .unwrap_or_else(|| format!("{other:?}"));
            VerifyOutcome {
                rev: None,
                script: None,
                instrument_error: Some(format!("{err} (still, after inode repair for {trigger})")),
                repaired_after: None,
            }
        }
    }
}
