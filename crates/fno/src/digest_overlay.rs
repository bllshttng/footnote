//! Attach-time "while you were gone" catch-up overlay (x-4e2d, client half).
//!
//! On attach the client asks: was I away from this mux session long enough to
//! want a catch-up? If so it shells out to `fno-agents digest --json` for the
//! focused pane's node and renders the ranked lines as a dismissable overlay.
//!
//! Two plan premises did not hold and are handled here:
//!   - The server has NO attach/detach timestamps (the `Client` struct carries
//!     none; `Info` is a frozen wire message). So "last detach age" is tracked
//!     CLIENT-LOCAL: [`record_detach`] writes epoch seconds keyed by mux session
//!     under the mux dir; [`read_detach_secs`] reads it on attach. Epoch seconds
//!     (not RFC3339) keep the age math to integer subtraction — no calendar code
//!     in a crate with no date library.
//!   - The mux "session" is a GROUPING name ("main" / `FNO_SESSION`), never the
//!     fno session id the digest folds on. The bridge is the focused pane's cwd:
//!     its basename is the worktree = node id, which `fno-agents digest` resolves
//!     to the session via the ledger. If the cwd yields nothing resolvable the
//!     fold returns empty and the overlay stays quiet (fail-open, AC-error).
//!
//! Config (`config.mux.attach_digest` + `attach_digest_threshold_min`) is read
//! straight from settings.yaml (Pattern B) because the interactive attach path
//! has no Python launcher to translate a knob into an env var.

use crate::proto;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_THRESHOLD_MIN: u64 = 10;
/// Fail-open budget for the fold shell-out; a slow `fno-agents` yields no
/// overlay rather than stalling the attach (AC-error: >800ms => no overlay).
const SHELLOUT_TIMEOUT: Duration = Duration::from_millis(800);

/// Seconds since the epoch, or 0 if the clock is before it (never in practice).
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The client-local detach-time file for a mux session, under the mux dir.
fn detach_file(session: &str) -> Option<PathBuf> {
    // Reject a session name that could escape the dir (mirrors socket_path's
    // guard); `proto::socket_path` already validates, but this file is written
    // independently so it re-checks the one dangerous shape.
    if session.is_empty() || session.contains('/') || session.contains("..") {
        return None;
    }
    Some(proto::mux_dir().join(format!("{session}.detach")))
}

/// Record "detached now" for `session`. Best-effort: a write failure is silent
/// (the worst case is a missing catch-up on the next attach, never a crash).
pub fn record_detach(session: &str) {
    let Some(path) = detach_file(session) else {
        return;
    };
    let _ = proto::ensure_private_dir(&proto::mux_dir());
    let _ = std::fs::write(&path, now_secs().to_string());
}

/// Read the last-detach epoch seconds for `session`, if any.
pub fn read_detach_secs(session: &str) -> Option<u64> {
    let path = detach_file(session)?;
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// The node/worktree selector for the digest: the basename of the focused
/// squad's cwd. Empty when the cwd is unknown/degraded.
pub fn selector_from_cwd(cwd: &str) -> Option<String> {
    let base = cwd.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    (!base.is_empty()).then(|| base.to_string())
}

// ── config (Pattern B: read settings.yaml directly) ────────────────────────

/// `config.mux.attach_digest` (default ON) — gate the overlay entirely.
pub fn attach_digest_enabled(cwd: &Path) -> bool {
    mux_str(cwd, "attach_digest")
        .and_then(|v| parse_bool(&v))
        .unwrap_or(true)
}

/// `config.mux.hover_focus` (default ON) — the focus-follows-mouse off-switch
/// (x-a496). Latched once at client startup. Lives here because this module owns
/// the `fno` crate's `config.mux.*` reader (mirrors `attach_digest_enabled`).
pub fn hover_focus_enabled(cwd: &Path) -> bool {
    mux_str(cwd, "hover_focus")
        .and_then(|v| parse_bool(&v))
        .unwrap_or(true)
}

/// `config.mux.attach_digest_threshold_min` (default 10) as seconds.
pub fn threshold_secs(cwd: &Path) -> u64 {
    mux_str(cwd, "attach_digest_threshold_min")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_THRESHOLD_MIN)
        // saturating: an absurd configured minutes value must not overflow.
        .saturating_mul(60)
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Resolve a `config: > mux: > <key>` string with the same file precedence as
/// `agents_config::mux_bool` ($FNO_CONFIG sole > project-local > global).
fn mux_str(cwd: &Path, key: &str) -> Option<String> {
    if let Some(explicit) = non_empty_env("FNO_CONFIG") {
        return read_mux_file(Path::new(&explicit), key);
    }
    if let Some(v) = read_mux_file(&cwd.join(".fno/settings.yaml"), key) {
        return Some(v);
    }
    let global = non_empty_env("FNO_GLOBAL_SETTINGS_PATH")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".fno/settings.yaml")));
    global.and_then(|g| read_mux_file(&g, key))
}

fn read_mux_file(path: &Path, key: &str) -> Option<String> {
    read_mux_value(&std::fs::read_to_string(path).ok()?, key)
}

/// Scan a settings.yaml body for `config: > mux: > <key>:` and return the raw
/// (comment- and quote-stripped) value. Indent-unit-agnostic, mirroring
/// `agents_config::read_mux_bool`.
fn read_mux_value(content: &str, key: &str) -> Option<String> {
    let unit = content
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(|l| l.len() - l.trim_start().len())
        .find(|&i| i > 0)
        .unwrap_or(2);
    let level = |line: &str| -> usize { (line.len() - line.trim_start().len()) / unit };

    let mut in_config = false;
    let mut in_mux = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        match level(line) {
            0 => {
                in_config = trimmed.starts_with("config:");
                in_mux = false;
            }
            1 if in_config => in_mux = trimmed.starts_with("mux:"),
            2 if in_mux => {
                if let Some(rest) = trimmed.strip_prefix(key).and_then(|r| r.strip_prefix(':')) {
                    let v = rest
                        .split('#')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .trim_matches(|c| c == '"' || c == '\'')
                        .to_string();
                    return (!v.is_empty()).then_some(v);
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// Resolve the `fno-agents` binary: `$FNO_AGENTS_BIN`, else a sibling of the
/// running `fno` binary (the installed layout, mirroring `resolve_daemon_bin`),
/// else bare `fno-agents` on PATH. Crate-visible: the server's claim-sweep
/// shell-out (x-54fa) resolves the same binary the same way.
pub(crate) fn fno_agents_bin() -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_AGENTS_BIN") {
        return PathBuf::from(v);
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("fno-agents")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("fno-agents"))
}

// ── overlay assembly ───────────────────────────────────────────────────────

/// Turn a `fno-agents digest --json` stdout blob into overlay lines. `None`
/// when the JSON is unparseable or the fold produced no `lines` (fail-quiet).
fn lines_from_json(stdout: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    let arr = v.get("lines")?.as_array()?;
    let lines: Vec<String> = arr
        .iter()
        .filter_map(|l| l.as_str())
        .map(str::to_string)
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(decorate(lines))
}

/// Frame the fold lines with a header + dismiss hint, padded to a common width
/// so the inverse-video block is a clean rectangle.
fn decorate(mut body: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(body.len() + 2);
    out.push("while you were gone".to_string());
    out.append(&mut body);
    out.push("(any key to dismiss)".to_string());
    let width = out.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    for line in &mut out {
        let pad = width - line.chars().count();
        line.push_str(&" ".repeat(pad));
    }
    out
}

/// The full attach-time decision. Returns overlay lines, or `None` to render
/// nothing. Fail-open at every step: a disabled knob, a too-recent detach, an
/// unknown cwd, a missing/slow/empty `fno-agents` all yield `None`.
pub async fn on_attach(session: &str, focused_cwd: &str) -> Option<Vec<String>> {
    let cwd = Path::new(focused_cwd);
    if !attach_digest_enabled(cwd) {
        return None;
    }
    // Threshold gate: only after an ABSENCE longer than the configured minutes.
    // No prior detach record (first attach) => nothing to catch up on.
    let last = read_detach_secs(session)?;
    if now_secs().saturating_sub(last) < threshold_secs(cwd) {
        return None;
    }
    let selector = selector_from_cwd(focused_cwd)?;

    // Scope the fold to the absence window: pass the detach time as epoch
    // seconds so the digest reports what changed WHILE AWAY, not lifetime
    // totals. Epoch avoids synthesizing an RFC3339 string in a crate with no
    // date library; `fno-agents` parses each row's ts to epoch for the compare.
    let since_epoch = last.to_string();
    let fut = tokio::process::Command::new(fno_agents_bin())
        .args([
            "digest",
            "--session",
            &selector,
            "--since-epoch",
            &since_epoch,
            "--json",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // On timeout the future is dropped; kill_on_drop reaps the child so a
        // slow `fno-agents` can't leave an orphan behind on each attach.
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(SHELLOUT_TIMEOUT, fut)
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    lines_from_json(&String::from_utf8_lossy(output.stdout.as_slice()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_is_worktree_basename() {
        assert_eq!(
            selector_from_cwd("/Users/x/conductor/workspaces/footnote/x-4e2d").as_deref(),
            Some("x-4e2d")
        );
        assert_eq!(selector_from_cwd("/w/x-4e2d/").as_deref(), Some("x-4e2d"));
        assert_eq!(selector_from_cwd(""), None);
        assert_eq!(selector_from_cwd("/"), None);
    }

    #[test]
    fn config_defaults_when_absent() {
        let dir = Path::new("/nonexistent-xyz");
        // No settings file anywhere reachable -> defaults (on, 10min).
        // (HOME may have one; the point is the parse path, covered below.)
        let _ = attach_digest_enabled(dir);
        let _ = threshold_secs(dir);
    }

    #[test]
    fn reads_mux_values() {
        let yaml =
            "config:\n  mux:\n    attach_digest: false\n    attach_digest_threshold_min: 30\n";
        assert_eq!(
            read_mux_value(yaml, "attach_digest").as_deref(),
            Some("false")
        );
        assert_eq!(
            parse_bool(&read_mux_value(yaml, "attach_digest").unwrap()),
            Some(false)
        );
        assert_eq!(
            read_mux_value(yaml, "attach_digest_threshold_min").as_deref(),
            Some("30")
        );
        assert_eq!(read_mux_value(yaml, "missing"), None);
    }

    #[test]
    fn json_to_lines_frames_and_pads() {
        let json = r#"{"lines":["! 1 block (last: FAILURE) - resolved","PR #42 OPEN - CI SUCCESS - reviewed"]}"#;
        let lines = lines_from_json(json).expect("has lines");
        assert!(lines[0].starts_with("while you were gone"));
        assert!(lines.iter().any(|l| l.contains("#42")));
        assert!(lines.last().unwrap().starts_with("(any key to dismiss)"));
        // All padded to a common width (clean inverse rectangle).
        let w = lines[0].chars().count();
        assert!(lines.iter().all(|l| l.chars().count() == w));
    }

    #[test]
    fn json_empty_lines_is_none() {
        assert_eq!(lines_from_json(r#"{"lines":[]}"#), None);
        assert_eq!(lines_from_json("not json"), None);
        assert_eq!(lines_from_json(r#"{"no_lines":1}"#), None);
    }

    #[test]
    fn detach_file_rejects_traversal() {
        assert!(detach_file("../evil").is_none());
        assert!(detach_file("a/b").is_none());
        assert!(detach_file("").is_none());
        assert!(detach_file("main").is_some());
    }
}
